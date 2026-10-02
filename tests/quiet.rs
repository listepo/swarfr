//! The safety tier for build systems without a lock (`Guard::Quiet`), with a test adapter over
//! throwaway dirs.

use std::cell::RefCell;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::time::{Duration, SystemTime};

use swarfr::dedupe::{DEFAULT_MIN_SIZE, Dedupe};
use swarfr::eco::{Ecosystem, Guard, Policy, Sharing};
use swarfr::engine::{self, Action, Options, Pass, Skip};
use swarfr::index::HashIndex;
use swarfr::model::Profile;
use tempfile::TempDir;

mod common;

const BIG: usize = 3 * DEFAULT_MIN_SIZE as usize;
const OLD_MTIME: Duration = Duration::from_secs(1_000_000_000);
/// A process name nothing runs under: the check can look, and finds no build.
const NO_TOOL: &[&str] = &["swarfr-test-no-such-tool"];

/// A build system with no lock, whose tool runs under the names in `tools`.
struct Lockless {
    tools: &'static [&'static str],
}

impl Ecosystem for Lockless {
    fn name(&self) -> &'static str {
        "lockless"
    }
    fn units(&self, build_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
        Ok(vec![build_dir.to_path_buf()])
    }
    fn guard(&self, _unit: &Path) -> Guard {
        Guard::Quiet
    }
    fn tools(&self) -> &'static [&'static str] {
        self.tools
    }
    fn policy(&self) -> Policy {
        Policy {
            share: Sharing::ClonesOnly,
        }
    }
}

fn write_old(path: &Path, content: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
    File::open(path)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + OLD_MTIME)
        .unwrap();
}

/// Units `a` and `b` under a fresh temp dir.
fn units() -> (TempDir, PathBuf, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let (a, b) = (root.join("a"), root.join("b"));
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    (tmp, a, b)
}

/// A lossy pass that removes every unit it sees.
struct RemoveAll;

impl Pass for RemoveAll {
    fn name(&self) -> &'static str {
        "remove-all"
    }
    fn lossy(&self) -> bool {
        true
    }
    fn plan(&self, profiles: &[Profile]) -> Vec<Action> {
        profiles
            .iter()
            .map(|profile| Action::Remove {
                dir: profile.dir.clone(),
                reason: "test".into(),
            })
            .collect()
    }
}

#[test]
fn a_file_rewritten_during_a_run_keeps_the_newer_bytes() {
    if !common::filesystem_can(|caps| caps.clone, "share blocks") {
        return;
    }
    const FILES: usize = 300;
    const WRITES: usize = 50;
    let (tmp, a, b) = units();
    for unit in [&a, &b] {
        for i in 0..FILES {
            write_old(&unit.join(format!("f{i}")), &vec![(i % 251) as u8; BIG]);
        }
    }
    let hot = a.join("f0");
    let newest = |i: usize| format!("{i:>width$}", width = BIG).into_bytes();

    let start = Arc::new(Barrier::new(2));
    let writer = {
        let (start, hot) = (Arc::clone(&start), hot.clone());
        std::thread::spawn(move || {
            start.wait();
            for i in 0..WRITES {
                fs::write(&hot, newest(i)).unwrap();
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };
    let index = RefCell::new(HashIndex::load(&tmp.path().join("index.bin")));
    let mut dedupe = Dedupe::new(&index);
    dedupe.min_age = Duration::ZERO;
    let eco = Lockless { tools: NO_TOOL };
    start.wait();
    let report = engine::run(
        &[a.clone(), b.clone()],
        &[&dedupe],
        &Options::default(),
        &eco,
    )
    .unwrap();
    writer.join().unwrap();

    assert_eq!(fs::read(&hot).unwrap(), newest(WRITES - 1));
    assert!(report.busy.is_empty(), "{report:?}");
    assert_eq!(report.quiet, [a.clone(), b.clone()]);
    // Every pair but the hot one is shared.
    assert!(report.passes[0].applied >= FILES - 1, "{report:?}");
    assert_eq!(fs::read(b.join("f0")).unwrap(), vec![0; BIG]);
}

#[test]
fn lossy_passes_leave_a_unit_with_a_young_file_alone() {
    let (_tmp, a, b) = units();
    write_old(&a.join("old"), b"built long ago");
    write_old(&b.join("old"), b"built long ago");
    fs::write(a.join("young"), b"maybe still being written").unwrap();
    let eco = Lockless { tools: NO_TOOL };
    let opts = Options {
        lossy: vec!["remove-all".into()],
        ..Options::default()
    };

    let report = engine::run(&[a.clone(), b.clone()], &[&RemoveAll], &opts, &eco).unwrap();

    let pass = &report.passes[0];
    assert!(
        pass.skipped.contains(&(a.clone(), Skip::Unsure)),
        "{pass:?}"
    );
    assert!(a.join("young").exists());
    if cfg!(windows) {
        // No process check there: every quiet unit is unsure.
        assert!(b.exists());
    } else {
        assert!(!b.exists(), "{pass:?}");
    }
}

#[test]
fn an_adapter_naming_no_tool_never_gets_a_lossy_pass() {
    let (_tmp, a, _b) = units();
    write_old(&a.join("old"), b"built long ago");
    let eco = Lockless { tools: &[] };
    let opts = Options {
        lossy: vec!["remove-all".into()],
        ..Options::default()
    };

    let report = engine::run(std::slice::from_ref(&a), &[&RemoveAll], &opts, &eco).unwrap();

    assert_eq!(report.passes[0].skipped, [(a.clone(), Skip::Unsure)]);
    assert!(a.join("old").exists());
}

#[cfg(unix)]
#[test]
fn a_build_tool_running_in_the_unit_makes_it_busy() {
    use std::process::Command;
    use std::time::Instant;

    const TOOL: &[&str] = &["sleep"];
    let (_tmp, a, _b) = units();
    write_old(&a.join("old"), b"built long ago");
    let mut tool = Command::new("sleep")
        .arg("30")
        .current_dir(&a)
        .spawn()
        .unwrap();
    // The child is seen once it has become `sleep`.
    let started = Instant::now();
    while swarfr::sys::tool_running(&a, TOOL) != Some(true)
        && started.elapsed() < Duration::from_secs(5)
    {
        std::thread::sleep(common::POLL);
    }
    let eco = Lockless { tools: TOOL };
    let opts = Options {
        lossy: vec!["remove-all".into()],
        ..Options::default()
    };

    let report = engine::run(std::slice::from_ref(&a), &[&RemoveAll], &opts, &eco);
    tool.kill().unwrap();
    tool.wait().unwrap();
    let report = report.unwrap();

    assert_eq!(report.busy, std::slice::from_ref(&a));
    assert!(report.quiet.is_empty());
    assert!(a.join("old").exists());
}
