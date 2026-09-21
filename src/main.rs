use std::cell::Cell;
use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use dunnage::config::{self, Config};
use dunnage::eco::cargo::home as cargo_home;
use dunnage::engine;
use dunnage::inventory::{Inventory, Target};
use dunnage::session::{self, Control, Observer, Request, RunReport, Session, Settings};
use dunnage::tmpdir;
use indicatif::{ProgressBar, ProgressStyle};

mod daemon;

const BYTES_PER_GIB: f64 = (1u64 << 30) as f64;
/// Exit code when a profile dir was skipped because a build holds its lock, or another run of
/// the tool holds the run lock.
const BUSY_EXIT: u8 = 2;
const SECS_PER_DAY: u64 = 24 * 60 * 60;

/// Shrink Cargo target directories without slowing builds
#[derive(Parser)]
#[command(name = "dunnage", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// The arguments, without the subcommand name cargo puts first: a `cargo-dunnage` link to this
/// binary is run by `cargo dunnage <args>` as `cargo-dunnage dunnage <args>`. No subcommand of
/// ours is called `dunnage`, so dropping it cannot take anything else away.
fn args() -> Vec<std::ffi::OsString> {
    let mut args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|first| first == "dunnage") {
        args.remove(1);
    }
    args
}

#[derive(Subcommand)]
enum Cmd {
    /// List cargo target dirs under the roots: real size, families, what the passes could win
    Status {
        /// Print the inventory as JSON
        #[arg(long)]
        json: bool,
        /// List every build dir, not only the largest of each checkout and ecosystem
        #[arg(long)]
        all: bool,
        /// Also measure the cargo home's unpacked sources, which costs another walk
        /// [default: $CARGO_HOME, else ~/.cargo]
        #[arg(long, value_name = "DIR", num_args = 0..=1, default_missing_value = "")]
        cargo_home: Option<PathBuf>,
        /// Dirs to search; a target dir itself works too [default: .]
        #[arg(value_name = "ROOT")]
        roots: Vec<PathBuf>,
    },
    /// Read the manifests and cargo configs under the roots and say what makes their targets
    /// bigger than they need to be; changes nothing
    Advise {
        /// Print the findings as JSON
        #[arg(long)]
        json: bool,
        /// Dirs to search; a target dir itself works too [default: .]
        #[arg(value_name = "ROOT")]
        roots: Vec<PathBuf>,
    },
    /// Plan and apply the passes, one family of targets at a time; profile dirs with a running
    /// build are skipped
    Run(Box<RunArgs>),
    /// Git worktrees that start warm
    #[command(subcommand)]
    Worktree(WorktreeCmd),
    /// Run the passes as build dirs go cold, as a service; `daemon install` sets that up
    #[command(subcommand)]
    Daemon(DaemonCmd),
    /// Clone a sibling checkout's target into a fresh one, so its first build starts warm
    Seed {
        /// Where to copy from: a checkout or a target dir [default: the family's newest target]
        #[arg(long, value_name = "DIR")]
        from: Option<PathBuf>,
        /// Report what would be copied without touching anything
        #[arg(long)]
        dry_run: bool,
        /// Content-hash cache [default: ~/.cache/dunnage/hashes-v1.bin]
        #[arg(long, value_name = "FILE")]
        index: Option<PathBuf>,
        /// The checkout to seed [default: .]
        #[arg(value_name = "DIR")]
        dir: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum WorktreeCmd {
    /// `git worktree add`, then `seed` the new worktree from the repository's newest target
    Add {
        /// Seed as a dry run: the worktree is still added
        #[arg(long)]
        dry_run: bool,
        /// Content-hash cache [default: ~/.cache/dunnage/hashes-v1.bin]
        #[arg(long, value_name = "FILE")]
        index: Option<PathBuf>,
        /// Passed to `git worktree add` as they are
        #[arg(
            value_name = "GIT ARGS",
            required = true,
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        git_args: Vec<std::ffi::OsString>,
    },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Stay in the foreground and run the config's passes over its `roots` whenever a build dir
    /// built since the last look has gone cold. Lossy passes run only if the config enables them
    Run {
        /// Configuration file [default: $XDG_CONFIG_HOME/dunnage/config.toml]
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
        /// Content-hash cache; the state file sits next to it
        /// [default: ~/.cache/dunnage/hashes-v1.bin]
        #[arg(long, value_name = "FILE")]
        index: Option<PathBuf>,
        /// Look once, run if anything is due, write the state file and exit
        #[arg(long)]
        once: bool,
    },
    /// Write a launchd agent (macOS) or a systemd user unit (Linux) that keeps `daemon run`
    /// going at low priority, and start it
    Install {
        /// Passed on to `daemon run`
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
        /// Passed on to `daemon run`
        #[arg(long, value_name = "FILE")]
        index: Option<PathBuf>,
        /// Print the unit and where it would go; write and start nothing
        #[arg(long)]
        print: bool,
    },
    /// Stop the daemon and remove its unit
    Remove,
    /// Whether the unit is installed, the last run, and which build dirs are due when
    Status {
        /// Content-hash cache the daemon was started with
        /// [default: ~/.cache/dunnage/hashes-v1.bin]
        #[arg(long, value_name = "FILE")]
        index: Option<PathBuf>,
        /// Print the state file as it is
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::Args)]
struct RunArgs {
    /// Report what would change without touching anything
    #[arg(long)]
    dry_run: bool,
    /// Enable a lossy pass (deletes rebuildable data); repeatable
    #[arg(long, value_name = "PASS")]
    lossy: Vec<String>,
    /// Run only this pass; repeatable [default: every pass not gated by `--lossy`]
    #[arg(long, value_name = "PASS")]
    pass: Vec<String>,
    /// With `--lossy evict`: remove profile dirs not built for this many days
    #[arg(long, value_name = "DAYS")]
    evict_idle_days: Option<u64>,
    /// With `--lossy evict`: then remove least recently built profile dirs until all targets
    /// under the roots fit into this many GiB
    #[arg(long, value_name = "GIB")]
    evict_max_total_gib: Option<u64>,
    /// With `--lossy evict`: once every profile dir of a target is evicted, remove the target dir
    /// itself, so `doc/`, `package/` and `tmp/` go with it
    #[arg(long)]
    evict_whole_target: bool,
    /// With `--lossy incremental`: drop the incremental cache of profile dirs not built for
    /// this many days
    #[arg(long, value_name = "DAYS")]
    incremental_idle_days: Option<u64>,
    /// With `--lossy orphans`: also remove the build dirs of projects whose manifest is gone —
    /// deleted, renamed, or absent on this branch — once nothing was built there for this many
    /// days. Without it they are only reported
    #[arg(long, value_name = "DAYS")]
    orphans_project_idle_days: Option<u64>,
    /// With `--lossy tmpdir`: remove top-level entries of the temp dir with nothing inside
    /// modified for this many days and no path in them held by a process
    #[arg(long, value_name = "DAYS")]
    tmpdir_idle_days: Option<u64>,
    /// Leave files younger than this alone, in seconds; both lossless passes [default: 3600]
    #[arg(long, value_name = "SECS")]
    min_age: Option<u64>,
    /// Leave files smaller than this alone; both lossless passes [default: 8192 / 4096]
    #[arg(long, value_name = "BYTES")]
    min_size: Option<u64>,
    /// Also compress the cargo home's unpacked sources (`registry/src`, `git/checkouts`)
    /// under cargo's own `.package-cache` lock [default: $CARGO_HOME, else ~/.cargo]
    #[arg(long, value_name = "DIR", num_args = 0..=1, default_missing_value = "")]
    cargo_home: Option<PathBuf>,
    /// Also compress a content-addressed store: `GOCACHE`, `~/.cabal/store`, Zig's `o/`. No lock
    /// exists there, so only files older than an hour are touched. Repeatable
    #[arg(long, value_name = "DIR")]
    store: Vec<PathBuf>,
    /// Also compress Go's caches, as `go env` names them: `GOCACHE` as a store, and the unpacked
    /// modules of `GOMODCACHE`, whose read-only dirs are made writable for a moment each
    #[arg(long)]
    go: bool,
    /// Compare every target under the roots with every other, not only the targets of one
    /// repository: unrelated projects do share artifacts, at the price of one wider lock
    #[arg(long)]
    across_families: bool,
    /// HAZARD. Where the filesystem cannot share blocks (ext4, NTFS), let `dedupe` share equal
    /// build artifacts as hardlinks instead. rustc rewrites its outputs in place, so a later
    /// build that rewrites one linked artifact rewrites every other name for it, in every target
    /// sharing it. Off by default, and the cargo home's sources are shared without it
    #[arg(long)]
    link_artifacts: bool,
    /// Content-hash cache [default: ~/.cache/dunnage/hashes-v1.bin]
    #[arg(long, value_name = "FILE")]
    index: Option<PathBuf>,
    /// Walk the roots for build dirs even if the last walk is recent enough to reuse
    #[arg(long)]
    rediscover: bool,
    /// Configuration file [default: $XDG_CONFIG_HOME/dunnage/config.toml]
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Print the report as JSON instead of a table
    #[arg(long)]
    json: bool,
    /// Dirs to search; a target dir itself works too [default: `roots` from the config]
    #[arg(value_name = "ROOT")]
    roots: Vec<PathBuf>,
}

fn main() -> ExitCode {
    let Cli { cmd } = Cli::parse_from(args());
    let done = match cmd {
        Cmd::Status {
            json,
            all,
            cargo_home,
            roots,
        } => status(json, all, cargo_home, roots).map(|()| Done::Everything),
        Cmd::Advise { json, roots } => advise(json, roots).map(|()| Done::Everything),
        Cmd::Run(args) => run(*args),
        Cmd::Daemon(cmd) => match cmd {
            DaemonCmd::Run {
                config,
                index,
                once,
            } => daemon::run(config.as_deref(), index, once),
            DaemonCmd::Install {
                config,
                index,
                print,
            } => daemon::service::install(config.as_deref(), index.as_deref(), print),
            DaemonCmd::Remove => daemon::service::remove(),
            DaemonCmd::Status { index, json } => daemon::status(index, json),
        }
        .map(|()| Done::Everything),
        Cmd::Seed {
            from,
            dry_run,
            index,
            dir,
        } => seed_into(from, dry_run, index, dir),
        Cmd::Worktree(WorktreeCmd::Add {
            dry_run,
            index,
            git_args,
        }) => worktree_add(dry_run, index, &git_args),
    };
    match done {
        Ok(Done::Everything) => ExitCode::SUCCESS,
        // A cron job wants to tell "nothing to do" from "a build was in the way".
        Ok(Done::LeftBusy) => ExitCode::from(BUSY_EXIT),
        Err(error) => {
            eprintln!("error: {error:#}");
            // Another run is working on the same targets: the same "try again later".
            let busy = matches!(
                error.downcast_ref::<dunnage::Error>(),
                Some(dunnage::Error::RunLockHeld(_))
            );
            if busy {
                ExitCode::from(BUSY_EXIT)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

/// What a command finished with; the difference is visible in the exit code.
enum Done {
    Everything,
    LeftBusy,
}

impl Done {
    fn busy_if(left_busy: bool) -> Self {
        if left_busy {
            Self::LeftBusy
        } else {
            Self::Everything
        }
    }
}

/// `--cargo-home` without a value means "the one cargo would use".
fn home_flag(flag: Option<PathBuf>) -> Option<PathBuf> {
    flag.and_then(|flag| cargo_home::path((!flag.as_os_str().is_empty()).then_some(flag)))
}

/// The roots named on the command line, else `roots` from the config file, else `.`.
fn roots_or_config(roots: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    let roots = if roots.is_empty() {
        match config::default_path() {
            Some(path) => Config::load(&path)?.roots,
            None => Vec::new(),
        }
    } else {
        roots
    };
    Ok(if roots.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        roots
    })
}

/// A session on `index`, or on the default one, kept as `config` says.
fn open(index: Option<PathBuf>, config: &Config) -> Result<Session> {
    Ok(Session::open(Settings::from_config(
        index_path(index)?,
        config,
    )))
}

/// `--index`, else the default one.
fn index_path(index: Option<PathBuf>) -> Result<PathBuf> {
    match index {
        Some(path) => Ok(path),
        None => session::default_index().context("HOME is not set; pass --index"),
    }
}

/// The file `--config` names, which must be there, else the default one if there is one.
fn load_config(path: Option<&Path>) -> Result<Config> {
    Ok(match path {
        // A file the command line names and that is not there is a mistake, not a default.
        Some(path) => {
            ensure!(path.exists(), "no config file at {}", path.display());
            Config::load(path)?
        }
        None => match config::default_path() {
            Some(path) => Config::load(&path)?,
            None => Config::default(),
        },
    })
}

fn gib(bytes: u64) -> String {
    format!("{:.2} GiB", bytes as f64 / BYTES_PER_GIB)
}

/// What the filesystem under a target cannot do, in the words of the passes it silences.
/// `None` when it can do everything, which needs no line.
fn missing_caps(caps: &dunnage::sys::Caps) -> Option<&'static str> {
    match (caps.clone, caps.compress) {
        (true, true) => None,
        (true, false) => {
            Some("this filesystem has no transparent compression: compress finds nothing here")
        }
        (false, true) => Some(
            "this filesystem shares no blocks: dedupe links cargo home sources, artifacts only with --link-artifacts",
        ),
        (false, false) => Some(
            "this filesystem neither shares blocks nor compresses: compress finds nothing here, dedupe only links cargo home sources",
        ),
    }
}

fn status(json: bool, all: bool, home: Option<PathBuf>, roots: Vec<PathBuf>) -> Result<()> {
    // The same `roots` key `run` uses; `.` stays the fallback when there is none.
    let roots = roots_or_config(roots)?;
    // Read-only and stateless: no index, so a missing `$HOME` is no reason to stop.
    let session = Session::open(Settings::default());
    let spinner = spinner(!json, "reading the build dirs");
    let inventory = session.inventory(&roots, home_flag(home).as_deref());
    spinner.finish_and_clear();
    let inventory = inventory?;
    if json {
        println!("{}", serde_json::to_string_pretty(&inventory)?);
        return Ok(());
    }
    print_inventory(&inventory, all);
    Ok(())
}

/// Build dirs listed per checkout and ecosystem without `--all`; the rest are one line.
const STATUS_LIMIT: usize = 5;

/// Family, then checkout, then a subtotal per ecosystem over its largest build dirs.
fn print_inventory(inventory: &Inventory, all: bool) {
    let now = session::now_unix();
    type Key<'a> = (&'a Option<PathBuf>, &'a Option<PathBuf>, &'static str);
    let mut groups: BTreeMap<Key, Vec<&Target>> = BTreeMap::new();
    for target in &inventory.targets {
        groups
            .entry((&target.family, &target.checkout, target.ecosystem))
            .or_default()
            .push(target);
    }
    let (mut family, mut checkout) = (None, None);
    for ((group_family, group_checkout, ecosystem), mut targets) in groups {
        if family != Some(group_family) {
            family = Some(group_family);
            checkout = None;
            match group_family {
                Some(dir) => println!("family {}", dir.display()),
                None => println!("no family"),
            }
        }
        if checkout != Some(group_checkout) {
            checkout = Some(group_checkout);
            match group_checkout {
                Some(dir) => println!("  checkout {}", dir.display()),
                None => println!("  no checkout"),
            }
        }
        targets.sort_by(|a, b| (b.allocated_bytes, &a.root).cmp(&(a.allocated_bytes, &b.root)));
        let bytes: u64 = targets.iter().map(|t| t.allocated_bytes).sum();
        println!(
            "    {ecosystem}: {} build dirs, {}",
            targets.len(),
            gib(bytes)
        );
        let shown = if all { targets.len() } else { STATUS_LIMIT };
        for target in targets.iter().take(shown) {
            print_target(target, now);
        }
        if targets.len() > shown {
            let rest = &targets[shown..];
            println!(
                "      {} more, {}: --all lists them",
                rest.len(),
                gib(rest.iter().map(|t| t.allocated_bytes).sum())
            );
        }
    }
    let sum = |field: fn(&Target) -> u64| inventory.targets.iter().map(field).sum::<u64>();
    println!(
        "{} targets, {} on disk ({} logical)",
        inventory.targets.len(),
        gib(sum(|t| t.allocated_bytes)),
        gib(sum(|t| t.logical_bytes))
    );
    if let Some(home) = &inventory.cargo_home {
        println!(
            "cargo home {}: {} on disk, not compressed yet: {}",
            home.home.display(),
            gib(home.allocated_bytes),
            gib(home.compressible_bytes)
        );
    }
    println!(
        "orphaned worktrees: {}; not compressed yet: {}; dedupe candidates (upper bound): {}",
        gib(sum(|t| if t.orphaned { t.allocated_bytes } else { 0 })),
        gib(sum(|t| t.compressible_bytes)),
        gib(sum(|t| t.dedupe_candidate_bytes))
    );
}

/// One build dir: size, age, its place in the checkout, and what the passes should know.
fn print_target(target: &Target, now: u64) {
    let built = target.last_built_unix.map_or("never built".into(), |at| {
        format!("built {}d ago", now.saturating_sub(at) / SECS_PER_DAY)
    });
    let orphaned = if target.orphaned {
        "  ORPHANED"
    } else if target.project_gone {
        "  PROJECT GONE"
    } else {
        ""
    };
    let place = target.position.as_ref().unwrap_or(&target.root);
    println!(
        "      {:>11}  {built:<16}{orphaned}  {}",
        gib(target.allocated_bytes),
        place.display()
    );
    // Only worth a line when something is missing: a filesystem that does both is the case the
    // numbers above already assume.
    if let Some(missing) = missing_caps(&target.caps) {
        println!("      {:>11}  {missing}", "");
    }
    if target.stale_units > 0 {
        let units: usize = target.toolchains.iter().map(|built| built.units).sum();
        println!(
            "      {:>11}  {} of {units} units built by an older rustc ({} toolchains)",
            format!("~{}", gib(target.stale_bytes_estimate)),
            target.stale_units,
            target.toolchains.len()
        );
    }
}

/// `dunnage advise`: how to print what the session found.
fn advise(json: bool, roots: Vec<PathBuf>) -> Result<()> {
    let roots = roots_or_config(roots)?;
    let session = Session::open(Settings::default());
    let spinner = spinner(!json, "reading manifests and configs");
    let advice = session.advise(&roots, cargo_home::path(None).as_deref());
    spinner.finish_and_clear();
    let advice = advice?;
    for warning in &advice.warnings {
        eprintln!("warning: {warning}");
    }
    let (findings, notes) = (&advice.findings, &advice.notes);
    if json {
        let report = serde_json::json!({ "findings": findings, "notes": notes });
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    let mut file = None;
    for finding in findings {
        if file != Some(&finding.file) {
            file = Some(&finding.file);
            println!("{}", finding.file.display());
        }
        println!("  {}: {}", finding.key, finding.note);
    }
    if !notes.is_empty() {
        println!("from the inventory");
        for note in notes {
            println!("  {}: {}", note.about, note.note);
        }
    }
    if findings.is_empty() && notes.is_empty() {
        println!("nothing to change");
    }
    Ok(())
}

/// `dunnage seed`: the session does the copy; this prints it.
fn seed_into(
    from: Option<PathBuf>,
    dry_run: bool,
    index: Option<PathBuf>,
    dir: Option<PathBuf>,
) -> Result<Done> {
    let checkout = dir.unwrap_or_else(|| PathBuf::from("."));
    let session = open(index, &load_config(None)?)?;
    let spinner = spinner(true, "seeding");
    let done = session.seed(&checkout, from.as_deref(), dry_run);
    spinner.finish_and_clear();
    print_seedings(&done?, dry_run)
}

/// `dunnage worktree add`: the session adds and seeds; this says what came of each.
fn worktree_add(
    dry_run: bool,
    index: Option<PathBuf>,
    git_args: &[std::ffi::OsString],
) -> Result<Done> {
    let session = open(index, &load_config(None)?)?;
    let added = session.worktree_add(Path::new("."), git_args, dry_run)?;
    println!("added worktree {}", added.worktree.display());
    let seeding = added.seeding.with_context(|| {
        format!(
            "worktree {} is there, but seeding it failed",
            added.worktree.display()
        )
    })?;
    if seeding.is_empty() {
        println!("  nothing to seed from: no other checkout of this repository has a target there");
        return Ok(Done::Everything);
    }
    print_seedings(&seeding, dry_run)
}

/// Every position seeded; busy when a build held any source unit.
fn print_seedings(done: &[session::Seeding], dry_run: bool) -> Result<Done> {
    for seeding in done {
        print_seeding(seeding, dry_run);
    }
    Ok(Done::busy_if(
        done.iter().any(|seeding| !seeding.seeded.busy.is_empty()),
    ))
}

fn print_seeding(done: &session::Seeding, dry_run: bool) {
    let seeded = &done.seeded;
    let verb = if dry_run { "would copy" } else { "copied" };
    println!(
        "{} from {}: {verb} {} files and {} symlinks, {} that the clones share with it",
        done.target.display(),
        seeded.source.display(),
        seeded.files,
        seeded.symlinks,
        gib(seeded.bytes)
    );
    for dir in &seeded.busy {
        println!("  busy, not copied: {}", dir.display());
    }
}

/// Flags over the config file: a flag always wins.
fn request(args: RunArgs, config: &Config) -> Request {
    let mut request = Request::from_config(config);
    if !args.lossy.is_empty() {
        request.lossy = args.lossy;
    }
    request.passes = args.pass;
    if let Some(days) = args.evict_idle_days {
        request.evict.idle_days = Some(days);
    }
    if let Some(gib) = args.evict_max_total_gib {
        request.evict.max_total_bytes = Some(session::gib_to_bytes(gib));
    }
    request.evict_whole_target |= args.evict_whole_target;
    if let Some(days) = args.incremental_idle_days {
        request.incremental_idle_days = Some(days);
    }
    if let Some(days) = args.orphans_project_idle_days {
        request.orphans_project_idle_days = Some(days);
    }
    if let Some(days) = args.tmpdir_idle_days {
        request.tmpdir_idle_days = Some(days);
    }
    request.tmpdir = Some(dunnage::sys::temp_dir());
    if let Some(secs) = args.min_age {
        request.min_age = Some(Duration::from_secs(secs));
    }
    if let Some(bytes) = args.min_size {
        request.min_size = Some(bytes);
    }
    request.cargo_home = home_flag(args.cargo_home);
    if !args.store.is_empty() {
        request.stores = args.store;
    }
    request.across_families |= args.across_families;
    request.link_artifacts = args.link_artifacts;
    request.until_settled = true;
    request.rediscover = args.rediscover;
    if !args.roots.is_empty() {
        request.roots = args.roots;
    }
    request
}

/// `run --go`: `GOCACHE` as a store and `GOMODCACHE` as the module cache, where `go env` says
/// they are.
fn go_caches(request: &mut Request) -> Result<()> {
    let out = std::process::Command::new("go")
        .args(["env", "GOCACHE", "GOMODCACHE"])
        .output()
        .context("`--go` asks `go env` where the caches are, and `go` did not start")?;
    ensure!(
        out.status.success(),
        "`go env` failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    let text =
        String::from_utf8(out.stdout).context("`go env` printed a path that is not UTF-8")?;
    let mut lines = text.lines();
    let (cache, modcache) = (lines.next().unwrap_or(""), lines.next().unwrap_or(""));
    // `GOCACHE=off` is a build without a cache.
    if !cache.is_empty() && cache != "off" {
        request.stores.push(PathBuf::from(cache));
    }
    ensure!(!modcache.is_empty(), "`go env GOMODCACHE` printed nothing");
    request.go_modcache = Some(PathBuf::from(modcache));
    Ok(())
}

fn run(args: RunArgs) -> Result<Done> {
    let config = load_config(args.config.as_deref())?;
    let (dry_run, json, index) = (args.dry_run, args.json, args.index.clone());
    let go = args.go;
    let mut request = request(args, &config);
    if go {
        go_caches(&mut request)?;
    }
    // Before anything else can fail, so a mistyped flag is named even without a `$HOME`.
    request.check()?;
    let session = open(index, &config)?;
    let table = Table {
        link_warning: Cell::new(request.link_artifacts),
        dry_run,
    };
    let quiet = session::Quiet;
    let progress = Progress {
        bar: spinner(!json, "starting"),
        table: if json { &quiet } else { &table },
    };
    let control = Control {
        observer: &progress,
        ..Control::default()
    };
    let report = if dry_run {
        session.plan(&request, &control)
    } else {
        session.apply(&request, &control)
    };
    progress.bar.finish_and_clear();
    let report = report?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&JsonReport::new(&report))?
        );
    } else {
        for note in &report.compress_notes {
            println!("compress backend: {note}");
        }
        println!("files hashed: {}", report.files_hashed);
        if !report.walked && !report.groups.is_empty() {
            println!("build dirs from the last walk of the roots; --rediscover walks them again");
        }
        if let Some(tmpdir) = &report.tmpdir {
            print_tmpdir(tmpdir, dry_run);
        }
    }
    Ok(Done::busy_if(report.left_busy))
}

/// A spinner on stderr while `what` goes on: hidden unless `show` and stderr is a terminal, so a
/// pipe, a script and `--json` see nothing of it. Cleared before anything is printed.
fn spinner(show: bool, what: &'static str) -> ProgressBar {
    if !show || !std::io::stderr().is_terminal() {
        return ProgressBar::hidden();
    }
    let bar = ProgressBar::new_spinner()
        .with_style(
            ProgressStyle::with_template("{spinner} {elapsed} {wide_msg}")
                .expect("a valid template"),
        )
        .with_message(what);
    bar.enable_steady_tick(Duration::from_millis(100));
    bar
}

/// The spinner, then a bar over the groups, around the table: every table line is printed with
/// the bar lifted, so the two never share a line.
struct Progress<'a> {
    bar: ProgressBar,
    table: &'a dyn Observer,
}

impl Observer for Progress<'_> {
    fn step(&self, what: &str) {
        self.bar.set_message(what.to_owned());
    }

    fn groups(&self, total: usize) {
        self.bar.set_length(total as u64);
        self.bar.set_style(
            ProgressStyle::with_template("{spinner} {elapsed} [{bar:30}] {pos}/{len} {wide_msg}")
                .expect("a valid template")
                .progress_chars("=> "),
        );
    }

    fn group(&self, group: &Path) {
        self.bar.set_message(group.display().to_string());
        self.bar.suspend(|| self.table.group(group));
    }

    fn report(&self, group: &Path, report: &engine::Report) {
        self.bar.suspend(|| self.table.report(group, report));
        self.bar.inc(1);
    }
}

/// The table a run prints as it goes.
struct Table {
    /// Still to be said, once, before the first group.
    link_warning: Cell<bool>,
    dry_run: bool,
}

impl Observer for Table {
    fn group(&self, group: &Path) {
        if self.link_warning.replace(false) {
            eprintln!(
                "--link-artifacts: equal artifacts may become one inode where the filesystem \
                 cannot share blocks. A build that rewrites one of them rewrites the others."
            );
        }
        println!("{}", group.display());
    }

    fn report(&self, _group: &Path, report: &engine::Report) {
        print_report(report, self.dry_run);
    }
}

fn print_report(report: &engine::Report, dry_run: bool) {
    for dir in &report.busy {
        println!("  busy, skipped: {}", dir.display());
    }
    for dir in &report.quiet {
        println!("  no build lock, weaker checks: {}", dir.display());
    }
    if report.temps_removed > 0 {
        println!("  stale temp files removed: {}", report.temps_removed);
    }
    for pass in &report.passes {
        println!(
            "  {}: planned {} ({} bytes), applied {} ({} bytes), skipped {}",
            pass.name,
            pass.planned,
            pass.planned_bytes,
            pass.applied,
            pass.freed_bytes,
            pass.skipped.len()
        );
        let verb = if dry_run { "would remove" } else { "remove" };
        for (dir, reason) in &pass.removals {
            println!("    {verb} {}: {reason}", dir.display());
        }
        for (path, skip) in &pass.skipped {
            println!("    skipped {}: {skip:?}", path.display());
        }
    }
}

/// What `--lossy tmpdir` removed, or would, and what old entries it left and why.
fn print_tmpdir(report: &tmpdir::Report, dry_run: bool) {
    println!("{}", report.dir.display());
    if report.unsure {
        println!("  tmpdir: which files processes hold open cannot be told here; nothing removed");
        return;
    }
    let now = session::now_unix();
    let verb = if dry_run { "would remove" } else { "removed" };
    println!(
        "  tmpdir: {verb} {} entries, {}; {} old kept, {} modified within {} days",
        report.removed.len(),
        gib(report.freed_bytes()),
        report.kept.len(),
        report.young,
        report.idle_days
    );
    for entry in &report.removed {
        println!(
            "    {verb} {}: {}, nothing modified for {} days",
            entry.path.display(),
            gib(entry.allocated_bytes),
            now.saturating_sub(entry.newest_unix) / 86_400
        );
    }
    for (path, why) in &report.kept {
        let why = match why {
            tmpdir::Kept::InUse => "a process holds a path in it",
            tmpdir::Kept::Socket => "holds a socket",
            tmpdir::Kept::OtherFilesystem => "reaches into another filesystem",
            tmpdir::Kept::Protected => "protected by a flag, as macOS guards its own",
            tmpdir::Kept::Unreadable => "unreadable, or dated in the future",
            tmpdir::Kept::Kept => "holds the hash index",
        };
        println!("    kept {}: {why}", path.display());
    }
    for (path, error) in &report.failed {
        println!("    failed {}: {error}", path.display());
    }
}

/// `--json`: the same report as the table, for a script that has to act on it.
#[derive(serde::Serialize)]
struct JsonReport<'a> {
    dry_run: bool,
    groups: Vec<JsonGroup<'a>>,
    compress_notes: &'a [String],
    files_hashed: usize,
    /// The roots were walked for build dirs, rather than the last walk's list used.
    walked: bool,
    /// `--lossy tmpdir`, when it ran.
    tmpdir: Option<&'a tmpdir::Report>,
}

impl<'a> JsonReport<'a> {
    fn new(report: &'a RunReport) -> Self {
        Self {
            dry_run: report.dry_run,
            groups: report
                .groups
                .iter()
                .map(|(group, report)| JsonGroup::new(group, report))
                .collect(),
            compress_notes: &report.compress_notes,
            files_hashed: report.files_hashed,
            walked: report.walked,
            tmpdir: report.tmpdir.as_ref(),
        }
    }
}

#[derive(serde::Serialize)]
struct JsonGroup<'a> {
    family: &'a Path,
    busy: &'a [PathBuf],
    /// Worked on without a lock: `DESIGN.md`, "Safety tier without a build lock".
    quiet: &'a [PathBuf],
    temps_removed: usize,
    passes: Vec<JsonPass<'a>>,
}

impl<'a> JsonGroup<'a> {
    fn new(family: &'a Path, report: &'a engine::Report) -> Self {
        Self {
            family,
            busy: &report.busy,
            quiet: &report.quiet,
            temps_removed: report.temps_removed,
            passes: report.passes.iter().map(JsonPass::new).collect(),
        }
    }
}

#[derive(serde::Serialize)]
struct JsonPass<'a> {
    name: &'a str,
    planned: usize,
    planned_bytes: u64,
    applied: usize,
    freed_bytes: u64,
    removals: Vec<JsonNote<'a>>,
    skipped: Vec<JsonNote<'a>>,
}

impl<'a> JsonPass<'a> {
    fn new(pass: &'a engine::PassReport) -> Self {
        let removals = pass.removals.iter().map(|(path, reason)| JsonNote {
            path,
            reason: reason.clone(),
        });
        let skipped = pass.skipped.iter().map(|(path, skip)| JsonNote {
            path,
            reason: format!("{skip:?}"),
        });
        Self {
            name: pass.name,
            planned: pass.planned,
            planned_bytes: pass.planned_bytes,
            applied: pass.applied,
            freed_bytes: pass.freed_bytes,
            removals: removals.collect(),
            skipped: skipped.collect(),
        }
    }
}

#[derive(serde::Serialize)]
struct JsonNote<'a> {
    path: &'a Path,
    reason: String,
}
