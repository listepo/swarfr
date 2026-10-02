//! `swarfr advise` on fake targets in temp dirs. The command reads files and prints; these
//! tests also check that it writes nothing at all.

use std::fs;
use std::path::{Path, PathBuf};

use predicates::str::contains;
use tempfile::TempDir;

mod common;
use common::{fake_target, swarfr};

/// A manifest with nothing tuned, which is what most projects have.
const PLAIN: &str = "[package]\nname = \"p\"\nversion = \"0.1.0\"\n";
/// The same project after taking every piece of advice.
const TUNED: &str = "[package]\nname = \"p\"\nversion = \"0.1.0\"\n\
                     [profile.dev]\ndebug = \"line-tables-only\"\n\
                     [profile.dev.package.\"*\"]\ndebug = false\n\
                     [profile.release]\nstrip = \"debuginfo\"\n";

fn root() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    (tmp, root)
}

/// A project with a target, a manifest and whatever config files the test wants.
fn project(root: &Path, manifest: &str, config: Option<&str>) -> PathBuf {
    let profile = fake_target(root, "p", 64, 1);
    let project = root.join("p");
    fs::write(project.join("Cargo.toml"), manifest).unwrap();
    if let Some(config) = config {
        fs::create_dir_all(project.join(".cargo")).unwrap();
        fs::write(project.join(".cargo/config.toml"), config).unwrap();
    }
    // An incremental cache, so the inventory has something to report about it.
    fs::create_dir_all(profile.join("incremental/p-1")).unwrap();
    fs::write(profile.join("incremental/p-1/x.bin"), vec![7; 64 * 1024]).unwrap();
    project
}

/// `advise` with a cargo home and a config home of its own, so nothing here reads the machine's.
fn advise(root: &Path) -> assert_cmd::Command {
    let home = root.join("config-home");
    fs::create_dir_all(&home).unwrap();
    let mut cmd = swarfr(&home);
    cmd.env("CARGO_HOME", root.join("cargo-home"))
        .arg("advise")
        .arg(root);
    cmd
}

/// Every path under `dir`, with its size and mtime: what must not change.
fn snapshot(dir: &Path) -> Vec<String> {
    let mut seen: Vec<String> = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .map(|entry| {
            let meta = entry.metadata().unwrap();
            format!("{} {:?}", entry.path().display(), meta.len())
        })
        .collect();
    seen.sort();
    seen
}

/// A/B: the same tree twice, the manifest the only difference. The plain one is told what to
/// change and the tuned one is told nothing about its profiles.
#[test]
fn ab_a_tuned_manifest_gets_no_profile_advice() {
    let (_tmp_a, plain) = root();
    let (_tmp_b, tuned) = root();
    project(&plain, PLAIN, None);
    project(&tuned, TUNED, None);

    let a = advise(&plain).output().unwrap();
    let b = advise(&tuned).output().unwrap();

    let (a, b) = (
        String::from_utf8(a.stdout).unwrap(),
        String::from_utf8(b.stdout).unwrap(),
    );
    for key in [
        "profile.dev.debug",
        "profile.dev.package.\"*\".debug",
        "profile.release.strip",
    ] {
        assert!(a.contains(key), "{a}");
        assert!(!b.contains(key), "{b}");
    }
    // What is true of the tree, not of the manifest, is said either way.
    assert!(a.contains("incremental caches") && b.contains("incremental caches"));
}

#[test]
fn every_finding_names_the_file_and_the_key_it_is_about() {
    let (_tmp, root) = root();
    let project = project(&root, PLAIN, Some("[unstable]\nno-embed-metadata = true\n"));

    advise(&root)
        .assert()
        .success()
        .stdout(contains(project.join("Cargo.toml").display().to_string()))
        .stdout(contains("profile.dev.debug: full debuginfo"))
        .stdout(contains(
            project.join(".cargo/config.toml").display().to_string(),
        ))
        .stdout(contains("unstable: a stable toolchain ignores"))
        .stdout(contains("no-embed-metadata"))
        // The cargo home is advised on although it has no config file at all.
        .stdout(contains("cache.auto-clean-frequency"));
}

#[test]
fn it_changes_nothing_on_disk() {
    let (_tmp, root) = root();
    project(&root, PLAIN, Some("[build]\nincremental = true\n"));
    // Built first: preparing the command creates the config home the run reads.
    let mut cmd = advise(&root);
    let before = snapshot(&root);

    cmd.assert().success();

    assert_eq!(snapshot(&root), before);
}

#[test]
fn the_json_report_is_machine_readable() {
    let (_tmp, root) = root();
    let project = project(&root, PLAIN, None);

    let out = advise(&root).arg("--json").output().unwrap();

    assert!(out.status.success());
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let findings = report["findings"].as_array().unwrap();
    let manifest = project.join("Cargo.toml");
    let first = findings
        .iter()
        .find(|finding| finding["file"] == manifest.to_str().unwrap())
        .unwrap();
    assert_eq!(first["key"], "profile.dev.debug");
    assert!(first["note"].as_str().unwrap().contains("line-tables-only"));
    let notes = report["notes"].as_array().unwrap();
    assert!(
        notes
            .iter()
            .any(|note| note["about"] == "incremental caches"),
        "{notes:?}"
    );
}

#[test]
fn a_broken_file_is_a_warning_and_not_the_end_of_the_run() {
    let (_tmp, root) = root();
    project(&root, "[package\nname =", None);

    advise(&root)
        .assert()
        .success()
        .stderr(contains("Cargo.toml"))
        .stdout(contains("cache.auto-clean-frequency"));
}
