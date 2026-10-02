//! Full output lives in `tests/cmd/*.trycmd`; exit codes and messages that carry paths are here.

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use tempfile::TempDir;

mod common;
use common::{fake_target, swarfr};

const EXIT_FAILURE: i32 = 1;

/// A config home of its own, so no machine's `config.toml` reaches these runs.
fn empty_config() -> (TempDir, Command) {
    let tmp = TempDir::new().unwrap();
    let cmd = swarfr(tmp.path());
    (tmp, cmd)
}

#[test]
fn cli_output() {
    trycmd::TestCases::new().case("tests/cmd/*.trycmd");
}

#[test]
fn unknown_lossy_pass_fails_before_anything_is_touched() {
    let tmp = TempDir::new().unwrap();
    let index = tmp.path().join("index.bin");
    swarfr(tmp.path())
        .args(["run", "--lossy", "nope", "--index"])
        .arg(&index)
        .arg(tmp.path())
        .assert()
        .code(EXIT_FAILURE)
        .stderr(contains("unknown lossy pass `nope`"));
    assert!(!index.exists());
}

#[test]
fn run_without_a_target_under_the_root_fails() {
    let tmp = TempDir::new().unwrap();
    swarfr(tmp.path())
        .args(["run", "--index"])
        .arg(tmp.path().join("index.bin"))
        .arg(tmp.path())
        .assert()
        .code(EXIT_FAILURE)
        .stderr(contains("no cargo target dirs found"));
}

#[test]
fn status_of_a_root_without_targets_succeeds() {
    let tmp = TempDir::new().unwrap();
    swarfr(tmp.path())
        .arg("status")
        .arg(tmp.path())
        .assert()
        .success()
        .stdout(contains("0 targets"));
}

#[test]
fn missing_root_is_named_in_the_error() {
    let (_tmp, mut swarfr) = empty_config();
    swarfr
        .args(["status", "/nonexistent-swarfr-root"])
        .assert()
        .code(EXIT_FAILURE)
        .stderr(contains("/nonexistent-swarfr-root"));
}

#[test]
fn passes_and_size_floors_are_picked_by_flag() {
    if !common::filesystem_can(
        |caps| caps.clone && caps.compress,
        "share blocks or compress",
    ) {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    fake_target(&root, "p", 64, 0);
    let index = root.join("index.bin");
    let compress_only = |extra: &[&str]| {
        let mut cmd = swarfr(&root);
        cmd.args(["run", "--pass", "compress"])
            .args(extra)
            .arg("--index")
            .arg(&index)
            .arg(&root);
        cmd
    };

    swarfr(&root)
        .args(["run", "--pass", "nope", "--index"])
        .arg(&index)
        .arg(&root)
        .assert()
        .code(EXIT_FAILURE)
        .stderr(contains("unknown pass `nope`"));
    assert!(!index.exists());

    // Only the named pass runs, and the just-written artifact is below both default floors.
    compress_only(&[])
        .assert()
        .success()
        .stdout(contains("compress: planned 0"))
        .stdout(contains("dedupe:").not());
    compress_only(&["--min-age", "0", "--min-size", "1000000"])
        .assert()
        .success()
        .stdout(contains("compress: planned 0"));
    compress_only(&["--min-age", "0"])
        .assert()
        .success()
        .stdout(contains("compress: planned 1"));
}

#[test]
fn run_without_roots_and_without_a_config_says_so() {
    let (_tmp, mut swarfr) = empty_config();
    swarfr
        .arg("run")
        .assert()
        .code(EXIT_FAILURE)
        .stderr(contains("no roots"));
}

/// `cargo swarfr <args>` runs a `cargo-swarfr` link to the binary as
/// `cargo-swarfr swarfr <args>`; the extra first argument must not be taken for a command.
#[test]
fn the_name_cargo_puts_first_is_dropped() {
    let (_tmp, mut swarfr) = empty_config();

    swarfr
        .args(["swarfr", "--version"])
        .assert()
        .success()
        .stdout(contains("swarfr"));
}
