//! What the passes do on the filesystem they are actually standing on.
//!
//! This is the A/B for `T20`, and the variable is the filesystem rather than a flag: the same
//! fixture, the same run, btrfs or XFS as one side and ext4 as the other. Each test states both
//! outcomes and picks by [`sys::caps`], so it is a real assertion on every filesystem instead of
//! a skip on the ones that cannot win — on ext4 "planned nothing and touched nothing" is the
//! whole point, and a pass that copied files there would fail this.
//!
//! Point `TMPDIR` at a mount to choose a side:
//!
//! ```sh
//! TMPDIR=/mnt/btrfs cargo test --test caps   # sharing and compression
//! TMPDIR=/mnt/ext4  cargo test --test caps   # neither
//! ```

use std::fs;

use common::{Fixture, ino, swarfr};
use swarfr::sys;

mod common;

/// Identical bytes in two files of one target, large enough for both passes' size floors.
const TWIN: &[u8] = &[9; 128 * 1024];

fn plant_twins(fixture: &Fixture) -> (std::path::PathBuf, std::path::PathBuf) {
    let deps = fixture.target().join("debug/deps");
    let (a, b) = (deps.join("libtwin-a.rlib"), deps.join("libtwin-b.rlib"));
    fs::write(&a, TWIN).unwrap();
    fs::write(&b, TWIN).unwrap();
    (a, b)
}

fn run(fixture: &Fixture, pass: &str) -> serde_json::Value {
    let config_home = fixture.root.join("config-home");
    fs::create_dir_all(&config_home).unwrap();
    let out = swarfr(&config_home)
        .args(["run", "--pass", pass, "--min-age", "0", "--json", "--index"])
        .arg(fixture.root.join("index.bin"))
        .arg(&fixture.root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn planned(report: &serde_json::Value, pass: &str) -> u64 {
    report["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| group["passes"].as_array().unwrap())
        .find(|p| p["name"] == pass)
        .unwrap_or_else(|| panic!("no {pass} pass in {report}"))["planned"]
        .as_u64()
        .unwrap()
}

/// Where blocks can be shared, the twin becomes a clone; where they cannot, nothing is planned
/// and nothing is read — a "clone" that copies the bytes again is the one outcome that would be
/// worse than doing nothing.
#[test]
fn dedupe_shares_where_it_can_and_stands_still_where_it_cannot() {
    let fixture = Fixture::new();
    fixture.build(&fixture.target());
    let (a, b) = plant_twins(&fixture);
    let before = (ino(&a), ino(&b));
    let sharing = sys::caps(&fixture.target()).clone;

    let report = run(&fixture, "dedupe");

    if sharing {
        // The twins, and whatever else a real build left identical: the count is not the
        // point, the pair is.
        assert!(planned(&report, "dedupe") >= 1, "{report}");
        assert_eq!(ino(&a), before.0, "the canonical is not touched");
        assert_ne!(ino(&b), before.1, "the twin became a clone of it");
    } else {
        assert_eq!(planned(&report, "dedupe"), 0, "{report}");
        assert_eq!((ino(&a), ino(&b)), before, "nothing was replaced");
    }
    assert_eq!(fs::read(&a).unwrap(), TWIN);
    assert_eq!(fs::read(&b).unwrap(), TWIN);
    fixture.assert_fresh(&fixture.target());
}

/// The same for compression: a filesystem without it must not be handed a single copy to make.
#[test]
fn compress_plans_only_where_the_filesystem_compresses() {
    let fixture = Fixture::new();
    fixture.build(&fixture.target());
    plant_twins(&fixture);
    let compresses = sys::caps(&fixture.target()).compress;

    let report = run(&fixture, "compress");

    let planned = planned(&report, "compress");
    if compresses {
        assert!(planned > 0, "{report}");
    } else {
        assert_eq!(planned, 0, "{report}");
    }
    fixture.assert_fresh(&fixture.target());
}

/// `status` says what the filesystem cannot do, and says nothing when it can do everything.
#[test]
fn status_names_what_the_filesystem_cannot_do() {
    let fixture = Fixture::new();
    fixture.build(&fixture.target());
    let config_home = fixture.root.join("config-home");
    fs::create_dir_all(&config_home).unwrap();

    let out = swarfr(&config_home)
        .arg("status")
        .arg(&fixture.root)
        .output()
        .unwrap();

    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    let caps = sys::caps(&fixture.target());
    assert_eq!(
        text.contains("this filesystem"),
        !(caps.clone && caps.compress),
        "{caps:?} in:\n{text}"
    );
}
