//! The lossy `evict` pass on fake profile dirs in temp dirs. Nothing here points at a real target.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use predicates::str::contains;
use swarfr::eco::cargo::{CARGO, LOCK_FILE};
use swarfr::engine::{self, Options, Report};
use swarfr::evict::{self, Evict, Limits};
use swarfr::inventory;
use tempfile::TempDir;

mod common;
use common::{fake_target, run_unbusy, swarfr as swarfr_in};

const IDLE_DAYS: u64 = 14;
const KIB: usize = 1024;
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

fn named() -> Options {
    Options {
        dry_run: false,
        lossy: vec![evict::NAME.to_string()],
        ..Options::default()
    }
}

/// What the CLI does: inventory of `root`, selection, one engine run over every profile dir.
/// All of it again on a retry: a removed profile dir is no longer there to lock.
fn run(root: &Path, limits: Limits, opts: &Options) -> Report {
    run_unbusy(|| {
        let inventory = inventory::inventory(&[root.to_path_buf()]).unwrap();
        let profiles: Vec<_> = inventory
            .targets
            .into_iter()
            .flat_map(|target| target.profiles)
            .collect();
        let dirs: Vec<PathBuf> = profiles.iter().map(|profile| profile.dir.clone()).collect();
        let pass = Evict::new(evict::select(&profiles, now_unix(), limits));
        engine::run(&dirs, &[&pass], opts, &CARGO).unwrap()
    })
}

const IDLE: Limits = Limits {
    idle_days: Some(IDLE_DAYS),
    max_total_bytes: None,
};

#[test]
fn idle_profile_is_removed_whole_and_a_fresh_one_stays() {
    let (_tmp, root) = root();
    let old = fake_target(&root, "old", 64, IDLE_DAYS + 1);
    let fresh = fake_target(&root, "fresh", 64, 1);

    let report = run(&root, IDLE, &named());

    let pass = &report.passes[0];
    assert_eq!((pass.planned, pass.applied), (1, 1), "{pass:?}");
    assert!(pass.freed_bytes >= 64 * KIB as u64);
    assert_eq!(pass.removals[0].0, old);
    assert!(pass.removals[0].1.contains("idle for 15 days"), "{pass:?}");
    assert!(!old.exists());
    assert!(old.parent().unwrap().join("CACHEDIR.TAG").exists());
    assert!(fresh.join("deps/libx.rlib").exists());
}

#[test]
fn nothing_happens_unless_the_pass_is_named_and_a_dry_run_only_lists() {
    let (_tmp, root) = root();
    let old = fake_target(&root, "old", 64, IDLE_DAYS + 1);

    let unnamed = run(&root, IDLE, &Options::default());
    assert!(unnamed.passes.is_empty(), "{unnamed:?}");

    let dry = Options {
        dry_run: true,
        ..named()
    };
    let listed = run(&root, IDLE, &dry);
    let pass = &listed.passes[0];
    assert_eq!((pass.planned, pass.applied), (1, 0), "{pass:?}");
    assert_eq!(pass.removals.len(), 1);
    assert!(pass.planned_bytes >= 64 * KIB as u64);
    assert!(old.join("deps/libx.rlib").exists());
}

#[test]
fn a_profile_with_a_running_build_is_not_touched() {
    let (_tmp, root) = root();
    let old = fake_target(&root, "old", 64, IDLE_DAYS + 1);
    // What cargo does for the length of a build. Opening the lock file does not age the dir.
    let build = File::options()
        .write(true)
        .open(old.join(LOCK_FILE))
        .unwrap();
    build.lock().unwrap();

    let report = run(&root, IDLE, &named());

    assert_eq!(report.busy, std::slice::from_ref(&old));
    assert_eq!(report.passes[0].planned, 0);
    assert!(old.join("deps/libx.rlib").exists());
}

#[test]
fn a_profile_built_after_the_inventory_is_kept() {
    let (_tmp, root) = root();
    let old = fake_target(&root, "old", 64, IDLE_DAYS + 1);
    let inventory = inventory::inventory(std::slice::from_ref(&root)).unwrap();
    let profiles = inventory.targets[0].profiles.clone();
    let pass = Evict::new(evict::select(&profiles, now_unix(), IDLE));

    // A build slips in between the inventory and our lock.
    fs::write(old.join("fresh-artifact"), b"new").unwrap();
    let report =
        run_unbusy(|| engine::run(std::slice::from_ref(&old), &[&pass], &named(), &CARGO).unwrap());

    assert_eq!(report.passes[0].planned, 0, "{report:?}");
    assert!(old.join("deps/libx.rlib").exists());
}

#[test]
fn size_cap_removes_the_least_recently_built_first() {
    let (_tmp, root) = root();
    let oldest = fake_target(&root, "a", 512, 3);
    let middle = fake_target(&root, "b", 512, 2);
    let newest = fake_target(&root, "c", 512, 1);
    // Room for one profile and a bit, not for two.
    let cap = Limits {
        idle_days: None,
        max_total_bytes: Some(768 * KIB as u64),
    };

    let report = run(&root, cap, &named());

    let pass = &report.passes[0];
    assert_eq!(pass.applied, 2, "{pass:?}");
    assert!(pass.removals.iter().all(|(_, why)| why.contains("cap")));
    assert!(!oldest.exists() && !middle.exists());
    assert!(newest.join("deps/libx.rlib").exists());
}

#[test]
fn cli_evicts_only_when_asked_with_a_limit() {
    let (_tmp, root) = root();
    let old = fake_target(&root, "old", 64, IDLE_DAYS + 1);
    let index = root.join("index.bin");
    let swarfr = || swarfr_in(&root);

    swarfr()
        .args(["run", "--lossy", "evict", "--index"])
        .arg(&index)
        .arg(&root)
        .assert()
        .code(EXIT_FAILURE)
        .stderr(contains("need each other"));
    swarfr()
        .args(["run", "--evict-idle-days", "14", "--index"])
        .arg(&index)
        .arg(&root)
        .assert()
        .code(EXIT_FAILURE)
        .stderr(contains("need each other"));
    assert!(old.exists());

    swarfr()
        .args([
            "run",
            "--dry-run",
            "--lossy",
            "evict",
            "--evict-idle-days",
            "14",
            "--index",
        ])
        .arg(&index)
        .arg(&root)
        .assert()
        .success()
        .stdout(contains("would remove"))
        .stdout(contains("idle for 15 days"));
    assert!(old.exists());

    swarfr()
        .args([
            "run",
            "--lossy",
            "evict",
            "--evict-idle-days",
            "14",
            "--index",
        ])
        .arg(&index)
        .arg(&root)
        .assert()
        .success()
        .stdout(contains("evict: planned 1"));
    assert!(!old.exists());
}
