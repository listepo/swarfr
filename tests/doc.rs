//! The lossy `doc` pass on the built fixture. `target/doc` goes, the build graph does not.

use std::fs;
use std::path::Path;

use predicates::str::contains;

mod common;
use common::{Fixture, allocated_bytes, swarfr};

const EXIT_BUSY: i32 = 2;

/// The fixture, built, with rustdoc output beside its profile dirs.
fn documented() -> Fixture {
    let fixture = Fixture::new();
    fixture.build(&fixture.target());
    let doc = fixture
        .cargo(&fixture.target(), &["doc", "--no-deps"])
        .status()
        .unwrap();
    assert!(doc.success(), "cargo doc");
    assert!(fixture.target().join("doc").is_dir());
    fixture
}

/// `run` with a config home of its own, so nothing here reads the machine's configuration.
fn run(fixture: &Fixture, root: &Path, extra: &[&str]) -> assert_cmd::Command {
    let home = root.join("config-home");
    fs::create_dir_all(&home).unwrap();
    let mut cmd = swarfr(&home);
    cmd.args(["run", "--index"])
        .arg(root.join("index.bin"))
        .args(extra)
        .arg(fixture.target());
    cmd
}

/// A/B: the same built fixture twice, `--lossy doc` the only difference. Both keep every unit
/// cargo needs; only the named run loses `doc/`.
#[test]
fn ab_only_the_named_run_removes_the_docs() {
    let control = documented();
    let treatment = documented();

    run(&control, &control.root, &[]).assert().success();
    run(&treatment, &treatment.root, &["--lossy", "doc"])
        .assert()
        .success()
        .stdout(contains("doc: planned 1"));

    assert!(control.target().join("doc/fx/index.html").is_file());
    assert!(!treatment.target().join("doc").exists());
    // Docs are not part of the build graph, so nothing is stale on either side.
    control.assert_fresh(&control.target());
    treatment.assert_fresh(&treatment.target());
}

#[test]
fn a_dry_run_lists_the_dir_with_its_size_and_removes_nothing() {
    let fixture = documented();
    let bytes = allocated_bytes(&fixture.target().join("doc"));
    assert!(bytes > 0);

    let out = run(
        &fixture,
        &fixture.root,
        &["--lossy", "doc", "--dry-run", "--json"],
    )
    .output()
    .unwrap();

    assert!(out.status.success());
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let pass = report["groups"][0]["passes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|pass| pass["name"] == "doc")
        .unwrap();
    assert_eq!(pass["planned"], 1);
    assert_eq!(pass["planned_bytes"], bytes);
    assert_eq!(pass["applied"], 0);
    let removal = &pass["removals"][0];
    assert_eq!(
        removal["path"],
        fixture.target().join("doc").to_str().unwrap()
    );
    assert!(removal["reason"].as_str().unwrap().contains("rustdoc"));
    assert!(fixture.target().join("doc").is_dir());
}

#[test]
fn a_target_with_a_running_build_keeps_its_docs() {
    let fixture = documented();
    // What cargo holds for the length of a build.
    let _build = common::hold_unit_lock(&fixture.target().join("debug"));

    run(&fixture, &fixture.root, &["--lossy", "doc"])
        .assert()
        .code(EXIT_BUSY);

    assert!(fixture.target().join("doc/fx/index.html").is_file());
}
