//! The lossy `incremental` pass: on fake profile dirs for the accounting, and on the real cargo
//! fixture for what dropping the cache costs. Everything lives in temp dirs.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use predicates::str::contains;
use swarfr::eco::cargo::incremental::{self, Incremental};
use swarfr::eco::cargo::{CARGO, LOCK_FILE};
use swarfr::engine::{self, Options, Report};
use swarfr::inventory;
use tempfile::TempDir;

mod common;
use common::{Fixture, allocated_bytes, fake_target, run_unbusy, swarfr as swarfr_in};

const KIB: usize = 1024;
const CACHE_KIB: usize = 256;
const IDLE_DAYS: u64 = 7;
const EXIT_FAILURE: i32 = 1;

fn root() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    (tmp, root)
}

fn now_unix() -> u64 {
    let since_epoch = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH);
    since_epoch.unwrap().as_secs()
}

/// A fake target with an incremental cache, both as old as the rest of the profile: the cache
/// is created first, so `fake_target` ages it with everything else it finds.
fn idle_target(root: &Path, name: &str, days: u64) -> PathBuf {
    let cache = root.join(name).join("target/debug/incremental");
    fs::create_dir_all(&cache).unwrap();
    fs::write(cache.join("s-fx-1a2b.bin"), vec![7; CACHE_KIB * KIB]).unwrap();
    fake_target(root, name, 64, days)
}

fn named() -> Options {
    Options {
        dry_run: false,
        lossy: vec![incremental::NAME.to_string()],
        ..Options::default()
    }
}

/// What the CLI does: inventory of `root`, selection, one engine run over every profile dir.
fn run(root: &Path, idle_days: u64, opts: &Options) -> Report {
    run_unbusy(|| {
        let inventory = inventory::inventory(&[root.to_path_buf()]).unwrap();
        let profiles: Vec<_> = inventory
            .targets
            .into_iter()
            .flat_map(|target| target.profiles)
            .collect();
        let dirs: Vec<PathBuf> = profiles.iter().map(|profile| profile.dir.clone()).collect();
        let pass = Incremental::new(incremental::select(&profiles, now_unix(), idle_days));
        engine::run(&dirs, &[&pass], opts, &CARGO).unwrap()
    })
}

#[test]
fn an_idle_cache_goes_and_the_rest_of_the_profile_stays() {
    let (_tmp, root) = root();
    let idle = idle_target(&root, "idle", IDLE_DAYS + 1);
    let fresh = idle_target(&root, "fresh", 1);

    let report = run(&root, IDLE_DAYS, &named());

    let pass = &report.passes[0];
    assert_eq!((pass.planned, pass.applied), (1, 1), "{pass:?}");
    assert!(pass.freed_bytes >= (CACHE_KIB * KIB) as u64);
    assert_eq!(pass.removals[0].0, idle.join("incremental"));
    assert!(
        pass.removals[0].1.contains("no build for 8 days"),
        "{pass:?}"
    );
    assert!(!idle.join("incremental").exists());
    // Only the cache: the artifacts, the lock file and the target itself are untouched.
    assert!(idle.join("deps/libx.rlib").exists());
    assert!(idle.join(LOCK_FILE).exists());
    assert!(fresh.join("incremental").exists());
}

#[test]
fn a_profile_without_a_cache_is_never_planned() {
    let (_tmp, root) = root();
    let bare = fake_target(&root, "bare", 64, IDLE_DAYS + 1);

    let report = run(&root, IDLE_DAYS, &named());

    assert_eq!(report.passes[0].planned, 0, "{report:?}");
    assert!(bare.join("deps/libx.rlib").exists());
}

#[test]
fn nothing_happens_unless_the_pass_is_named_and_a_dry_run_only_lists() {
    let (_tmp, root) = root();
    let idle = idle_target(&root, "idle", IDLE_DAYS + 1);

    let unnamed = run(&root, IDLE_DAYS, &Options::default());
    assert!(unnamed.passes.is_empty(), "{unnamed:?}");

    let dry = Options {
        dry_run: true,
        ..named()
    };
    let listed = run(&root, IDLE_DAYS, &dry);
    let pass = &listed.passes[0];
    assert_eq!((pass.planned, pass.applied), (1, 0), "{pass:?}");
    assert_eq!(pass.removals.len(), 1);
    assert!(pass.planned_bytes >= (CACHE_KIB * KIB) as u64);
    assert!(idle.join("incremental").exists());
}

#[test]
fn a_profile_with_a_running_build_is_not_touched() {
    let (_tmp, root) = root();
    let idle = idle_target(&root, "idle", IDLE_DAYS + 1);
    let build = File::options()
        .write(true)
        .open(idle.join(LOCK_FILE))
        .unwrap();
    build.lock().unwrap();

    let report = run(&root, IDLE_DAYS, &named());

    assert_eq!(report.busy, std::slice::from_ref(&idle));
    assert_eq!(report.passes[0].planned, 0);
    assert!(idle.join("incremental").exists());
}

#[test]
fn a_profile_built_after_the_inventory_keeps_its_cache() {
    let (_tmp, root) = root();
    let idle = idle_target(&root, "idle", IDLE_DAYS + 1);
    let inventory = inventory::inventory(std::slice::from_ref(&root)).unwrap();
    let profiles = inventory.targets[0].profiles.clone();
    let pass = Incremental::new(incremental::select(&profiles, now_unix(), IDLE_DAYS));

    // A build slips in between the inventory and our lock.
    fs::write(idle.join("fresh-artifact"), b"new").unwrap();
    let report = run_unbusy(|| {
        engine::run(std::slice::from_ref(&idle), &[&pass], &named(), &CARGO).unwrap()
    });

    assert_eq!(report.passes[0].planned, 0, "{report:?}");
    assert!(idle.join("incremental").exists());
}

/// A/B: the same tree twice, the pass the only difference.
#[test]
fn ab_only_the_named_run_frees_the_cache() {
    let (_tmp_a, control) = root();
    let (_tmp_b, treatment) = root();
    for root in [&control, &treatment] {
        idle_target(root, "idle", IDLE_DAYS + 1);
    }
    let before = allocated_bytes(&control.join("idle"));
    assert_eq!(before, allocated_bytes(&treatment.join("idle")));

    run(&control, IDLE_DAYS, &Options::default());
    run(&treatment, IDLE_DAYS, &named());

    assert_eq!(allocated_bytes(&control.join("idle")), before);
    let freed = before - allocated_bytes(&treatment.join("idle"));
    assert!(freed >= (CACHE_KIB * KIB) as u64, "freed {freed} bytes");
    // The difference is the cache and nothing else.
    let cache = control.join("idle/target/debug/incremental");
    assert_eq!(freed, allocated_bytes(&cache));
}

/// What dropping a real cache costs: measured, not assumed.
#[test]
fn dropping_a_real_cache_rebuilds_nothing() {
    let fixture = Fixture::new();
    let target = fixture.target();
    fixture.build(&target);
    let cache = target.join("debug/incremental");
    assert!(cache.is_dir(), "cargo built nothing incrementally");
    let before = allocated_bytes(&target);

    // `--incremental-idle-days 0`: this profile was built seconds ago and is still chosen.
    let report = run(&fixture.root, 0, &named());

    let pass = &report.passes[0];
    assert_eq!((pass.planned, pass.applied), (1, 1), "{pass:?}");
    assert!(!cache.exists());
    assert!(allocated_bytes(&target) < before);
    // Nothing is rebuilt at all: the cache is not part of cargo's fingerprint, so the cost
    // lands on the next edit of a workspace member, as one non-incremental rebuild of it.
    fixture.assert_fresh(&target);
}

#[test]
fn cli_drops_the_cache_only_when_asked_with_a_limit() {
    let (_tmp, root) = root();
    let idle = idle_target(&root, "idle", IDLE_DAYS + 1);
    let index = root.join("index.bin");
    let swarfr = || {
        let mut cmd = swarfr_in(&root);
        cmd.args(["run", "--pass", "incremental", "--index"]);
        cmd.arg(&index);
        cmd
    };

    swarfr()
        .args(["--lossy", "incremental"])
        .arg(&root)
        .assert()
        .code(EXIT_FAILURE)
        .stderr(contains("need each other"));
    swarfr()
        .args(["--incremental-idle-days", "7"])
        .arg(&root)
        .assert()
        .code(EXIT_FAILURE)
        .stderr(contains("need each other"));
    assert!(idle.join("incremental").exists());

    swarfr()
        .args([
            "--dry-run",
            "--lossy",
            "incremental",
            "--incremental-idle-days",
            "7",
        ])
        .arg(&root)
        .assert()
        .success()
        .stdout(contains("would remove"))
        .stdout(contains("no build for 8 days"));
    assert!(idle.join("incremental").exists());

    swarfr()
        .args(["--lossy", "incremental", "--incremental-idle-days", "7"])
        .arg(&root)
        .assert()
        .success()
        .stdout(contains("incremental: planned 1"));
    assert!(!idle.join("incremental").exists());
    assert!(idle.join("deps/libx.rlib").exists());
}
