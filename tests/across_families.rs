//! Dedupe across families: two projects that share nothing but their bytes. Each fixture is a
//! real cargo build in a temp dir of its own, so neither has a repository and each is its own
//! family — which is exactly the case one run per family cannot reach.

use std::fs;
use std::path::{Path, PathBuf};

use common::{Fixture, ino, swarfr};

mod common;

/// Big enough for dedupe's size floor, and the same bytes in both targets: what the same
/// version of the same crate, built the same way, leaves in two unrelated projects.
const SHARED: &[u8] = &[7; 64 * 1024];
const SHARED_FILE: &str = "debug/deps/libshared.rlib";

/// Two built fixtures with one identical artifact each.
fn pair() -> (Fixture, Fixture) {
    let pair = (Fixture::new(), Fixture::new());
    for fixture in [&pair.0, &pair.1] {
        fixture.build(&fixture.target());
        fs::write(fixture.target().join(SHARED_FILE), SHARED).unwrap();
    }
    pair
}

fn shared(fixture: &Fixture) -> PathBuf {
    fixture.target().join(SHARED_FILE)
}

/// `run` over both fixtures, with a config home and an index of its own.
fn run(tmp: &Path, roots: [&Fixture; 2], extra: &[&str]) -> assert_cmd::Command {
    let config_home = tmp.join("config-home");
    fs::create_dir_all(&config_home).unwrap();
    let mut cmd = swarfr(&config_home);
    cmd.args(["run", "--pass", "dedupe", "--min-age", "0", "--index"])
        .arg(tmp.join("index.bin"))
        .args(extra)
        .arg(roots[0].root.as_path())
        .arg(roots[1].root.as_path());
    cmd
}

/// A/B: the same two fixtures, the same run, `--across-families` the only difference. A clone
/// is a new inode sharing the old one's blocks, so what proves the sharing is that the second
/// copy's inode was replaced — and that its bytes and its mtime were not.
#[test]
fn ab_only_across_families_shares_the_file_and_neither_build_goes_stale() {
    if !common::filesystem_can(|caps| caps.clone, "share blocks") {
        return;
    }
    let (control_a, control_b) = pair();
    let (treatment_a, treatment_b) = pair();
    let before = |fixture: &Fixture| {
        let path = shared(fixture);
        let meta = fs::metadata(&path).unwrap();
        (ino(&path), meta.modified().unwrap())
    };
    let (control_ino, _) = before(&control_b);
    let (treatment_ino, treatment_mtime) = before(&treatment_b);

    run(&control_a.root, [&control_a, &control_b], &[])
        .assert()
        .success();
    run(
        &treatment_a.root,
        [&treatment_a, &treatment_b],
        &["--across-families"],
    )
    .assert()
    .success();

    assert_eq!(
        ino(&shared(&control_b)),
        control_ino,
        "one run per family cannot see the other project"
    );
    assert_ne!(
        ino(&shared(&treatment_b)),
        treatment_ino,
        "across families the second copy became a clone of the first"
    );
    assert_eq!(fs::read(shared(&treatment_b)).unwrap(), SHARED);
    assert_eq!(
        fs::metadata(shared(&treatment_b))
            .unwrap()
            .modified()
            .unwrap(),
        treatment_mtime,
        "an mtime that moves is a rebuild"
    );
    for fixture in [&treatment_a, &treatment_b, &control_a, &control_b] {
        fixture.assert_fresh(&fixture.target());
    }
}

#[test]
fn the_report_names_the_group_once_instead_of_every_family() {
    let (a, b) = pair();

    let out = run(&a.root, [&a, &b], &["--across-families", "--json"])
        .output()
        .unwrap();

    assert!(out.status.success());
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let groups = report["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "{groups:?}");
    assert_eq!(groups[0]["family"], "<across families>");
}
