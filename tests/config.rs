//! `config.toml`, the JSON report and the exit codes. Every run here gets a config home of its
//! own, so nothing depends on the machine's configuration.

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use predicates::str::contains;
use tempfile::TempDir;

mod common;
use common::{fake_target, swarfr};

const EXIT_FAILURE: i32 = 1;
const EXIT_BUSY: i32 = 2;
const IDLE_DAYS: u64 = 7;

fn root() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    (tmp, root)
}

/// Writes `body` to `<root>/config-home/swarfr/config.toml` and returns the config home.
fn config_home(root: &Path, body: &str) -> PathBuf {
    let home = root.join("config-home");
    fs::create_dir_all(home.join("swarfr")).unwrap();
    fs::write(home.join("swarfr/config.toml"), body).unwrap();
    home
}

/// A config that evicts idle profiles under `root`, with the roots in the file rather than on
/// the command line.
fn evicting(root: &Path) -> String {
    format!(
        "roots = [\"{}\"]\nlossy = [\"evict\"]\n[evict]\nidle-days = {IDLE_DAYS}\n",
        common::toml_basic(root)
    )
}

#[test]
fn the_file_supplies_the_roots_and_the_lossy_pass() {
    let (_tmp, root) = root();
    let idle = fake_target(&root, "idle", 64, IDLE_DAYS + 1);
    let home = config_home(&root, &evicting(&root));

    swarfr(&home)
        .args(["run", "--index"])
        .arg(root.join("index.bin"))
        .assert()
        .success()
        .stdout(contains("evict: planned 1"));

    assert!(!idle.exists());
}

#[test]
fn a_flag_wins_over_the_file() {
    let (_tmp, root) = root();
    let idle = fake_target(&root, "idle", 64, IDLE_DAYS + 1);
    let home = config_home(&root, &evicting(&root));

    swarfr(&home)
        .args(["run", "--evict-idle-days", "999", "--index"])
        .arg(root.join("index.bin"))
        .assert()
        .success()
        .stdout(contains("evict: planned 0"));

    assert!(idle.join("deps/libx.rlib").exists());
}

/// A/B: the same tree and the same config twice, `skip` the only difference.
#[test]
fn ab_a_skipped_family_is_left_alone() {
    let (_tmp_a, control) = root();
    let (_tmp_b, treatment) = root();
    let mut homes = Vec::new();
    for root in [&control, &treatment] {
        fake_target(root, "idle", 64, IDLE_DAYS + 1);
        homes.push(config_home(root, &evicting(root)));
    }
    // The family of a target without a repository is the target dir itself.
    let family = control.join("idle/target");
    let skipping = format!(
        "{}[family.\"{}\"]\nskip = true\n",
        evicting(&control),
        common::toml_basic(&family)
    );
    fs::write(homes[0].join("swarfr/config.toml"), skipping).unwrap();

    for (home, root) in homes.iter().zip([&control, &treatment]) {
        swarfr(home)
            .args(["run", "--index"])
            .arg(root.join("index.bin"))
            .assert()
            .success();
    }

    assert!(control.join("idle/target/debug/deps/libx.rlib").exists());
    assert!(!treatment.join("idle/target/debug").exists());
}

#[test]
fn a_broken_file_names_itself_and_the_key() {
    let (_tmp, root) = root();
    fake_target(&root, "p", 64, 0);
    let home = config_home(&root, "min-aeg = 60\n");

    swarfr(&home)
        .args(["run", "--index"])
        .arg(root.join("index.bin"))
        .arg(&root)
        .assert()
        .code(EXIT_FAILURE)
        .stderr(contains("config.toml"))
        .stderr(contains("min-aeg"));
}

#[test]
fn a_file_named_on_the_command_line_must_exist() {
    let (_tmp, root) = root();
    let home = config_home(&root, "");

    swarfr(&home)
        .args(["run", "--config"])
        .arg(root.join("nowhere.toml"))
        .arg(&root)
        .assert()
        .code(EXIT_FAILURE)
        .stderr(contains("no config file at"));
}

#[test]
fn the_json_report_is_machine_readable() {
    let (_tmp, root) = root();
    let idle = fake_target(&root, "idle", 64, IDLE_DAYS + 1);
    let home = config_home(&root, &evicting(&root));

    let out = swarfr(&home)
        .args(["run", "--json", "--dry-run", "--index"])
        .arg(root.join("index.bin"))
        .output()
        .unwrap();

    assert!(out.status.success());
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["dry_run"], true);
    let group = &report["groups"][0];
    assert_eq!(group["family"], idle.parent().unwrap().to_str().unwrap());
    assert!(group["busy"].as_array().unwrap().is_empty());
    let pass = &group["passes"][0];
    assert_eq!(pass["name"], "evict");
    assert_eq!(pass["planned"], 1);
    assert_eq!(pass["applied"], 0);
    assert_eq!(pass["removals"][0]["path"], idle.to_str().unwrap());
    let reason = pass["removals"][0]["reason"].as_str().unwrap();
    assert!(reason.contains("idle for"), "{reason}");
    assert!(idle.exists(), "a dry run changes nothing");
}

#[test]
fn a_busy_profile_leaves_its_own_exit_code() {
    let (_tmp, root) = root();
    let idle = fake_target(&root, "idle", 64, IDLE_DAYS + 1);
    let home = config_home(&root, &evicting(&root));
    // What cargo holds for the length of a build.
    let build = File::options()
        .write(true)
        .open(idle.join(".cargo-lock"))
        .unwrap();
    build.lock().unwrap();

    swarfr(&home)
        .args(["run", "--index"])
        .arg(root.join("index.bin"))
        .assert()
        .code(EXIT_BUSY)
        .stdout(contains("busy, skipped"));

    assert!(idle.join("deps/libx.rlib").exists());
}
