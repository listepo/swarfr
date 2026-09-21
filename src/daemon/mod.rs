//! `dunnage daemon`: the passes of `dunnage run`, started when a build dir has gone cold rather
//! than by the clock. Everything it changes, it changes through [`Session::apply`] with the
//! `Request` the config file makes; what is its own is when to call it, and the state file that
//! `daemon status` reads. No IPC: a manual run and the daemon meet at the session's run lock.

pub mod service;

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{fs, thread};

use anyhow::{Context, Result, ensure};
use dunnage::compress;
use dunnage::eco::{self, Guard};
use dunnage::engine::{self, QUIET_MIN_AGE};
use dunnage::session::{self, Control, Observer, Request, RunReport, Session, Settings};
use dunnage::{sys, tmpdir};
use serde::{Deserialize, Serialize};

/// Next to the hash index, like the run lock.
pub const STATE_FILE: &str = "daemon.json";
const DEFAULT_INTERVAL: Duration = Duration::from_secs(10 * 60);
const DEFAULT_REDISCOVER: Duration = Duration::from_secs(6 * 60 * 60);
/// Low: a build that finds a group holding its lock waits this long, and one action more.
const DEFAULT_LOCK_BUDGET: Duration = Duration::from_secs(2);
/// How often `tmpdir`, when the config enables it, cleans the temp dir.
const TMPDIR_EVERY: u64 = 24 * 60 * 60;

/// A unit the daemon knows, and where it stands.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unit {
    pub dir: PathBuf,
    /// The adapter that claimed it, by name.
    pub ecosystem: String,
    /// The latest build seen, as unix seconds.
    pub last_built_unix: Option<u64>,
    /// When its files will have gone cold: the last build plus min-age.
    pub due_unix: Option<u64>,
    /// The build the passes last finished with, by its `last_built_unix`.
    pub visited_build_unix: Option<u64>,
}

impl Unit {
    /// Built since the passes last finished with it.
    fn pending(&self) -> bool {
        self.last_built_unix.is_some() && self.last_built_unix > self.visited_build_unix
    }

    fn ready(&self, now: u64) -> bool {
        self.pending() && self.due_unix.is_some_and(|due| due <= now)
    }
}

/// `daemon.json`: what `daemon status` prints, and what a restarted daemon starts from.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub pid: u32,
    pub updated_unix: u64,
    pub discovered_unix: u64,
    pub next_wake_unix: u64,
    pub units: Vec<Unit>,
    pub last_run: Option<LastRun>,
    /// Why the last run did not happen: another run held the run lock, or it failed.
    pub last_error: Option<String>,
    /// The last time `tmpdir` cleaned the temp dir.
    pub last_tmpdir: Option<TmpDirRun>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TmpDirRun {
    pub finished_unix: u64,
    pub removed: usize,
    pub freed_bytes: u64,
    /// Old entries left, because a process held them or for another reason.
    pub kept: usize,
    /// Nobody could say which files processes hold, so nothing was removed.
    pub unsure: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct LastRun {
    pub started_unix: u64,
    pub finished_unix: u64,
    /// Every pass that ran, summed over the groups.
    pub passes: Vec<PassSummary>,
    /// Units a build was seen in; they stay pending.
    pub busy: Vec<PathBuf>,
    pub left_busy: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PassSummary {
    pub name: String,
    pub applied: usize,
    pub freed_bytes: u64,
}

impl TmpDirRun {
    fn new(report: &tmpdir::Report) -> Self {
        eprintln!(
            "{}: removed {} entries, {} freed, {} old kept",
            report.dir.display(),
            report.removed.len(),
            crate::gib(report.freed_bytes()),
            report.kept.len()
        );
        Self {
            finished_unix: session::now_unix(),
            removed: report.removed.len(),
            freed_bytes: report.freed_bytes(),
            kept: report.kept.len(),
            unsure: report.unsure,
        }
    }
}

impl LastRun {
    fn new(started_unix: u64, report: &RunReport) -> Self {
        let mut passes: Vec<PassSummary> = Vec::new();
        for pass in report.groups.iter().flat_map(|(_, group)| &group.passes) {
            match passes.iter_mut().find(|sum| sum.name == pass.name) {
                Some(sum) => {
                    sum.applied += pass.applied;
                    sum.freed_bytes += pass.freed_bytes;
                }
                None => passes.push(PassSummary {
                    name: pass.name.to_owned(),
                    applied: pass.applied,
                    freed_bytes: pass.freed_bytes,
                }),
            }
        }
        Self {
            started_unix,
            finished_unix: session::now_unix(),
            passes,
            busy: busy(report).into_iter().cloned().collect(),
            left_busy: report.left_busy,
        }
    }
}

fn busy(report: &RunReport) -> Vec<&PathBuf> {
    report
        .groups
        .iter()
        .flat_map(|(_, group)| &group.busy)
        .collect()
}

/// The build dirs under `roots`, split into units, with what `old` knew about each.
fn discover(roots: &[PathBuf], old: &[Unit]) -> Vec<Unit> {
    let mut units = Vec::new();
    for (dir, eco) in eco::discover(roots) {
        let Ok(found) = eco.units(&dir) else { continue };
        for dir in found {
            let visited = old.iter().find(|unit| unit.dir == dir);
            units.push(Unit {
                ecosystem: eco.name().to_owned(),
                visited_build_unix: visited.and_then(|unit| unit.visited_build_unix),
                dir,
                ..Unit::default()
            });
        }
    }
    units
}

/// Each unit's last build, and when its files will be old enough for the passes: `min_age`
/// after it, or the quiet tier's floor where no lock guards the unit.
fn refresh(units: &mut [Unit], min_age: Duration) {
    for unit in units {
        let Some(eco) = eco::named(&unit.ecosystem) else {
            continue;
        };
        let floor = if eco.guard(&unit.dir) == Guard::Quiet {
            min_age.max(QUIET_MIN_AGE)
        } else {
            min_age
        };
        unit.last_built_unix = eco.last_used(&unit.dir);
        unit.due_unix = unit
            .last_built_unix
            .map(|built| built.saturating_add(floor.as_secs()));
    }
}

/// The units in `ready` the run finished with. None when a group let go early: which of its
/// units it reached, the report does not say.
fn mark_visited(units: &mut [Unit], ready: &[PathBuf], report: &RunReport) {
    let mut last: BTreeMap<&Path, &engine::Report> = BTreeMap::new();
    for (group, group_report) in &report.groups {
        last.insert(group, group_report);
    }
    if report.stopped || last.values().any(|group| group.interrupted.is_some()) {
        return;
    }
    let busy = busy(report);
    for unit in units.iter_mut().filter(|unit| ready.contains(&unit.dir)) {
        if !busy.contains(&&unit.dir) {
            unit.visited_build_unix = unit.last_built_unix;
        }
    }
}

/// The next due time still ahead, capped by the interval and the next discovery. A unit whose
/// due time has passed and that is still pending was busy: it waits for the interval.
fn next_wake(units: &[Unit], now: u64, interval: Duration, next_discovery: u64) -> u64 {
    units
        .iter()
        .filter(|unit| unit.pending())
        .filter_map(|unit| unit.due_unix)
        .filter(|&due| due > now)
        .chain([now.saturating_add(interval.as_secs()), next_discovery])
        .min()
        .unwrap_or(next_discovery)
}

fn secs(value: Option<u64>, default: Duration) -> Duration {
    value.map_or(default, Duration::from_secs)
}

pub fn state_path(index: &Path) -> PathBuf {
    index.with_file_name(STATE_FILE)
}

fn load_state(path: &Path) -> State {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Written whole and renamed into place: `daemon status` never reads half a file.
fn write_state(path: &Path, state: &State) -> Result<()> {
    let dir = path.parent().context("the state file has no parent dir")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, serde_json::to_vec_pretty(state)?)
        .with_context(|| format!("writing {}", temp.display()))?;
    fs::rename(&temp, path).with_context(|| format!("writing {}", path.display()))
}

/// What the passes did, a line each, on stderr: launchd and journald keep it.
struct Log;

impl Observer for Log {
    fn report(&self, group: &Path, report: &engine::Report) {
        let group = group.display();
        for dir in &report.busy {
            eprintln!("{group}: busy, skipped: {}", dir.display());
        }
        for pass in &report.passes {
            if pass.applied > 0 {
                eprintln!(
                    "{group}: {}: applied {}, {} bytes freed",
                    pass.name, pass.applied, pass.freed_bytes
                );
            }
            for (dir, reason) in &pass.removals {
                eprintln!("{group}: {}: remove {}: {reason}", pass.name, dir.display());
            }
        }
        if let Some(why) = &report.interrupted {
            eprintln!("{group}: let go early: {why:?}");
        }
    }
}

/// `dunnage daemon run`: in the foreground until killed, or one look with `once`.
pub fn run(config: Option<&Path>, index: Option<PathBuf>, once: bool) -> Result<()> {
    let config = crate::load_config(config)?;
    let mut request = Request::from_config(&config);
    request.until_settled = true;
    request.tmpdir = Some(sys::temp_dir());
    request.check()?;
    // The temp dir on its own, once a day: every run the build dirs start would walk it again.
    let tmpdir_request = request
        .lossy
        .iter()
        .any(|name| name == tmpdir::NAME)
        .then(|| Request {
            lossy: vec![tmpdir::NAME.into()],
            passes: vec![tmpdir::NAME.into()],
            tmpdir: request.tmpdir.clone(),
            tmpdir_idle_days: request.tmpdir_idle_days,
            ..Request::default()
        });
    request.lossy.retain(|name| name != tmpdir::NAME);
    request.tmpdir_idle_days = None;
    ensure!(
        !request.roots.is_empty(),
        "the daemon works on `roots` from the config file, and it names none"
    );
    let index = crate::index_path(index)?;
    let path = state_path(&index);
    let session = Session::open(Settings::from_config(index, &config));
    let interval = secs(config.daemon.interval_secs, DEFAULT_INTERVAL);
    let rediscover = secs(config.daemon.rediscover_secs, DEFAULT_REDISCOVER);
    let min_age = request.min_age.unwrap_or(compress::DEFAULT_MIN_AGE);
    let control = Control {
        observer: &Log,
        lock_budget: Some(secs(config.daemon.lock_budget_secs, DEFAULT_LOCK_BUDGET)),
        ..Control::default()
    };

    let mut state = load_state(&path);
    state.pid = std::process::id();
    let mut next_discovery = 0;
    let mut next_tmpdir = state
        .last_tmpdir
        .as_ref()
        .map_or(0, |run| run.finished_unix.saturating_add(TMPDIR_EVERY));
    loop {
        let now = session::now_unix();
        if let Some(cleanup) = tmpdir_request.as_ref().filter(|_| now >= next_tmpdir) {
            match session.apply(cleanup, &control) {
                Ok(report) => {
                    state.last_tmpdir = report.tmpdir.as_ref().map(TmpDirRun::new);
                    state.last_error = None;
                }
                Err(error) => {
                    eprintln!("error: {error:#}");
                    state.last_error = Some(format!("{error:#}"));
                }
            }
            // A failed cleanup is tried again tomorrow, not on every wake.
            next_tmpdir = now.saturating_add(TMPDIR_EVERY);
        }
        if now >= next_discovery {
            state.units = discover(&request.roots, &state.units);
            state.discovered_unix = now;
            next_discovery = now.saturating_add(rediscover.as_secs());
            // The run walks too, or a build dir found here would be marked visited unseen.
            request.rediscover = true;
        }
        refresh(&mut state.units, min_age);
        let ready: Vec<PathBuf> = state
            .units
            .iter()
            .filter(|unit| unit.ready(now))
            .map(|unit| unit.dir.clone())
            .collect();
        if !ready.is_empty() {
            eprintln!(
                "{} build dirs have gone cold since their last visit",
                ready.len()
            );
            match session.apply(&request, &control) {
                Ok(report) => {
                    mark_visited(&mut state.units, &ready, &report);
                    request.rediscover = false;
                    state.last_run = Some(LastRun::new(now, &report));
                    state.last_error = None;
                }
                // Another run holds the run lock, or this one failed: the units stay pending.
                Err(error) => {
                    eprintln!("error: {error:#}");
                    state.last_error = Some(format!("{error:#}"));
                }
            }
        }
        let now = session::now_unix();
        state.next_wake_unix = next_wake(&state.units, now, interval, next_discovery);
        if tmpdir_request.is_some() {
            state.next_wake_unix = state.next_wake_unix.min(next_tmpdir.max(now));
        }
        state.updated_unix = now;
        write_state(&path, &state)?;
        if once {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(
            state.next_wake_unix.saturating_sub(now).max(1),
        ));
    }
}

/// `3d 4h`, `5h 12m`, `7m 3s`: the two largest units.
fn span(secs: u64) -> String {
    let parts = [
        (secs / 86_400, "d"),
        (secs / 3600 % 24, "h"),
        (secs / 60 % 60, "m"),
        (secs % 60, "s"),
    ];
    let first = parts.iter().position(|(n, _)| *n > 0).unwrap_or(3);
    parts[first..]
        .iter()
        .take(2)
        .map(|(n, unit)| format!("{n}{unit}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `dunnage daemon status`: the service unit, and the state file as the daemon last wrote it.
pub fn status(index: Option<PathBuf>, json: bool) -> Result<()> {
    let path = state_path(&crate::index_path(index)?);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            ensure!(!json, "no daemon state at {}", path.display());
            service::print_installed();
            println!("no state at {}: the daemon has not run", path.display());
            return Ok(());
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    if json {
        println!("{}", text.trim_end());
        return Ok(());
    }
    let state: State =
        serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))?;
    let now = session::now_unix();
    let ago = |unix: u64| span(now.saturating_sub(unix));
    service::print_installed();
    println!(
        "state {} (pid {}, written {} ago)",
        path.display(),
        state.pid,
        ago(state.updated_unix)
    );
    match &state.last_run {
        Some(run) => {
            println!("last run {} ago", ago(run.finished_unix));
            for pass in &run.passes {
                println!(
                    "  {}: applied {}, {} freed",
                    pass.name,
                    pass.applied,
                    crate::gib(pass.freed_bytes)
                );
            }
            for dir in &run.busy {
                println!("  busy: {}", dir.display());
            }
        }
        None => println!("no run yet"),
    }
    if let Some(run) = &state.last_tmpdir {
        if run.unsure {
            println!(
                "temp dir {} ago: open files unknown here, nothing removed",
                ago(run.finished_unix)
            );
        } else {
            println!(
                "temp dir {} ago: removed {} entries, {} freed, {} old kept",
                ago(run.finished_unix),
                run.removed,
                crate::gib(run.freed_bytes),
                run.kept
            );
        }
    }
    if let Some(error) = &state.last_error {
        println!("last error: {error}");
    }
    let pending: Vec<&Unit> = state.units.iter().filter(|unit| unit.pending()).collect();
    println!(
        "{} build dirs known, {} built since their last visit",
        state.units.len(),
        pending.len()
    );
    for unit in pending {
        let when = match unit.due_unix {
            Some(due) if due > now => format!("due in {}", span(due - now)),
            _ => "due now".to_owned(),
        };
        println!("  {} ({}): {when}", unit.dir.display(), unit.ecosystem);
    }
    if state.next_wake_unix > now {
        println!("next look in {}", span(state.next_wake_unix - now));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(built: Option<u64>, due: Option<u64>, visited: Option<u64>) -> Unit {
        Unit {
            dir: PathBuf::from("/t/debug"),
            ecosystem: "cargo".to_owned(),
            last_built_unix: built,
            due_unix: due,
            visited_build_unix: visited,
        }
    }

    #[test]
    fn a_unit_is_ready_once_built_since_its_visit_and_cold() {
        assert!(!unit(None, None, None).pending(), "never built");
        assert!(unit(Some(100), Some(200), None).ready(200));
        assert!(!unit(Some(100), Some(200), None).ready(199), "still warm");
        assert!(!unit(Some(100), Some(200), Some(100)).pending(), "visited");
        assert!(
            unit(Some(150), Some(250), Some(100)).ready(300),
            "built again"
        );
    }

    #[test]
    fn the_daemon_wakes_at_the_next_due_time_within_the_interval() {
        let interval = Duration::from_secs(600);
        let units = [
            unit(Some(100), Some(1300), None),
            // Past due and still pending: it was busy, and waits for the interval.
            unit(Some(100), Some(900), None),
            // Visited: its due time means nothing.
            unit(Some(100), Some(1100), Some(100)),
        ];
        assert_eq!(next_wake(&units, 1000, interval, 5000), 1300);
        assert_eq!(next_wake(&units[1..], 1000, interval, 5000), 1600);
        assert_eq!(next_wake(&units[1..], 1000, interval, 1200), 1200);
    }

    #[test]
    fn a_run_that_let_go_early_marks_nothing_visited() {
        let dir = PathBuf::from("/t/debug");
        let ready = [dir.clone()];
        let mut units = [unit(Some(100), Some(200), None)];
        let mut report = RunReport {
            groups: vec![(PathBuf::from("/t"), engine::Report::default())],
            ..RunReport::default()
        };
        report.groups[0].1.interrupted = Some(engine::Interrupted::OutOfBudget);
        mark_visited(&mut units, &ready, &report);
        assert_eq!(units[0].visited_build_unix, None);

        // Visited once more after the other groups, and done.
        report
            .groups
            .push((PathBuf::from("/t"), engine::Report::default()));
        mark_visited(&mut units, &ready, &report);
        assert_eq!(units[0].visited_build_unix, Some(100));
    }

    #[test]
    fn a_busy_unit_stays_pending() {
        let dir = PathBuf::from("/t/debug");
        let mut units = [unit(Some(100), Some(200), None)];
        let report = RunReport {
            groups: vec![(
                PathBuf::from("/t"),
                engine::Report {
                    busy: vec![dir.clone()],
                    ..engine::Report::default()
                },
            )],
            left_busy: true,
            ..RunReport::default()
        };
        mark_visited(&mut units, &[dir], &report);
        assert!(units[0].pending());
    }

    #[test]
    fn spans_read_as_their_two_largest_units() {
        assert_eq!(span(0), "0s");
        assert_eq!(span(59), "59s");
        assert_eq!(span(3 * 3600 + 5 * 60 + 7), "3h 5m");
        assert_eq!(span(2 * 86_400 + 60), "2d 0h");
    }
}
