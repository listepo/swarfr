//! `--evict-whole-target`: once every profile dir of a target is evicted, the target dir goes
//! with them. Everything here is a fake target inside a temp dir.

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use predicates::str::contains;
use tempfile::TempDir;

mod common;
use common::{fake_profile, fake_target, swarfr};

const IDLE_DAYS: u64 = 7;

fn root() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    (tmp, root)
}

/// A target with two idle profile dirs and the things a target holds outside them.
fn two_profiles(root: &Path, name: &str) -> PathBuf {
    let target = fake_target(root, name, 64, IDLE_DAYS + 1)
        .parent()
        .unwrap()
        .to_path_buf();
    fake_profile(&target, "release", 64, IDLE_DAYS + 2);
    fs::create_dir_all(target.join("doc")).unwrap();
    fs::write(target.join("doc/index.html"), "docs").unwrap();
    target
}

/// A config home of its own, so nothing here reads the machine's configuration.
fn home(root: &Path) -> PathBuf {
    let home = root.join("config-home");
    fs::create_dir_all(&home).unwrap();
    home
}

fn evict(root: &Path, extra: &[&str]) -> assert_cmd::Command {
    let mut cmd = swarfr(&home(root));
    cmd.args(["run", "--lossy", "evict", "--evict-idle-days"])
        .arg(IDLE_DAYS.to_string())
        .args(extra)
        .args(["--index"])
        .arg(root.join("index.bin"))
        .arg(root);
    cmd
}

/// A/B: the same tree twice, `--evict-whole-target` the only difference. Without it a target is
/// emptied profile by profile and everything outside them survives; with it nothing is left.
#[test]
fn ab_only_the_whole_target_run_takes_what_is_outside_the_profiles() {
    let (_tmp_a, control) = root();
    let (_tmp_b, treatment) = root();
    let (kept, gone) = (two_profiles(&control, "p"), two_profiles(&treatment, "p"));

    evict(&control, &[]).assert().success();
    evict(&treatment, &["--evict-whole-target"])
        .assert()
        .success()
        .stdout(contains("evict: planned 1"));

    assert!(!kept.join("debug").exists(), "the profiles go either way");
    assert!(!kept.join("release").exists());
    assert!(kept.join("doc/index.html").is_file(), "but nothing else");
    assert!(kept.join("CACHEDIR.TAG").is_file());
    assert!(!gone.exists(), "the whole target dir is removed");
    assert!(
        gone.parent().unwrap().is_dir(),
        "and the project around it stays"
    );
}

#[test]
fn a_busy_profile_keeps_the_target_and_the_free_profiles_are_still_evicted() {
    let (_tmp, root) = root();
    let target = two_profiles(&root, "p");
    // What cargo holds for the length of a build.
    let build = File::options()
        .write(true)
        .open(target.join("release/.cargo-lock"))
        .unwrap();
    build.lock().unwrap();

    evict(&root, &["--evict-whole-target"]).assert().code(2);

    assert!(target.is_dir(), "a target with a build running is kept");
    assert!(target.join("release/deps/libx.rlib").is_file());
    assert!(
        !target.join("debug").exists(),
        "the free profile still goes"
    );
}

/// A target only partly idle is not taken whole, however the cap is set.
#[test]
fn a_fresh_profile_keeps_its_target() {
    let (_tmp, root) = root();
    let target = two_profiles(&root, "p");
    fake_profile(&target, "bench", 64, 0);

    evict(&root, &["--evict-whole-target"]).assert().success();

    assert!(target.join("bench/deps/libx.rlib").is_file());
    assert!(target.join("doc/index.html").is_file());
    assert!(!target.join("debug").exists());
    assert!(!target.join("release").exists());
}

#[test]
fn the_config_file_can_ask_for_it_too() {
    let (_tmp, root) = root();
    let target = two_profiles(&root, "p");
    let home = home(&root);
    fs::create_dir_all(home.join("swarfr")).unwrap();
    fs::write(
        home.join("swarfr/config.toml"),
        format!(
            "roots = [\"{}\"]\nlossy = [\"evict\"]\n\
             [evict]\nidle-days = {IDLE_DAYS}\nwhole-target = true\n",
            common::toml_basic(&root)
        ),
    )
    .unwrap();

    swarfr(&home)
        .args(["run", "--index"])
        .arg(root.join("index.bin"))
        .assert()
        .success();

    assert!(!target.exists());
}
