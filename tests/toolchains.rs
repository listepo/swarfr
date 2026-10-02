//! The orphan-toolchain report, over a target a real cargo built. A second toolchain is
//! simulated the only way that is honest here: by rewriting the compiler hash cargo wrote into
//! half the fingerprints, which is exactly what a real upgrade leaves behind — units whose
//! `rustc` no longer matches the one in use. Nothing is removed by any of this.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

mod common;
use common::{Fixture, swarfr};

/// Fingerprint JSON files of the target, sorted, oldest-looking first.
fn fingerprints(profile: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = walkdir::WalkDir::new(profile.join(".fingerprint"))
        .into_iter()
        .filter_map(Result::ok)
        .map(|entry| entry.path().to_path_buf())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    found.sort();
    found
}

/// Rewrites the compiler hash in `count` fingerprints and dates them back a month: units the
/// previous toolchain built and this one never touched again.
fn age_out(files: &[PathBuf], count: usize) {
    let built = SystemTime::now() - Duration::from_secs(30 * 24 * 60 * 60);
    for path in files.iter().take(count) {
        let mut json: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        json["rustc"] = Value::from(1u64);
        fs::write(path, serde_json::to_vec(&json).unwrap()).unwrap();
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(built))
            .unwrap();
    }
}

fn status_json(fixture: &Fixture) -> Value {
    let config_home = fixture.root.join("config-home");
    fs::create_dir_all(&config_home).unwrap();
    let out = swarfr(&config_home)
        .args(["status", "--json"])
        .arg(&fixture.root)
        .output()
        .unwrap();
    assert!(out.status.success());
    serde_json::from_slice(&out.stdout).unwrap()
}

fn target_of(report: &Value) -> &Value {
    &report["targets"].as_array().unwrap()[0]
}

#[test]
fn one_toolchain_is_reported_as_nothing_at_all() {
    let fixture = Fixture::new();
    fixture.build(&fixture.target());

    let report = status_json(&fixture);

    let target = target_of(&report);
    assert_eq!(target["stale_units"], 0);
    assert_eq!(target["stale_bytes_estimate"], 0);
    let toolchains = target["toolchains"].as_array().unwrap();
    assert_eq!(toolchains.len(), 1, "one rustc built this: {toolchains:?}");
    assert!(toolchains[0]["units"].as_u64().unwrap() > 0);
}

#[test]
fn units_of_the_previous_toolchain_are_counted_and_priced() {
    let fixture = Fixture::new();
    fixture.build(&fixture.target());
    let files = fingerprints(&fixture.target().join("debug"));
    let old = files.len() / 2;
    assert!(old > 0, "the fixture must build several units");
    age_out(&files, old);

    let report = status_json(&fixture);

    let target = target_of(&report);
    assert_eq!(target["stale_units"], old);
    let toolchains = target["toolchains"].as_array().unwrap();
    assert_eq!(toolchains.len(), 2, "{toolchains:?}");
    assert_eq!(
        toolchains[0]["units"].as_u64().unwrap() as usize,
        files.len() - old,
        "the compiler in use comes first"
    );
    // The estimate is the profile's bytes in the share of the units; it cannot be more.
    let estimate = target["stale_bytes_estimate"].as_u64().unwrap();
    assert!(estimate > 0 && estimate < target["allocated_bytes"].as_u64().unwrap());
}

#[test]
fn advise_names_the_target_and_says_what_it_would_take() {
    let fixture = Fixture::new();
    fixture.build(&fixture.target());
    let files = fingerprints(&fixture.target().join("debug"));
    age_out(&files, files.len() / 2);
    let config_home = fixture.root.join("config-home");
    fs::create_dir_all(&config_home).unwrap();

    let out = swarfr(&config_home)
        .args(["advise", "--json"])
        .arg(&fixture.root)
        .output()
        .unwrap();

    assert!(out.status.success());
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let note = report["notes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|note| {
            note["note"]
                .as_str()
                .unwrap()
                .contains("no longer the one cargo uses")
        })
        .unwrap_or_else(|| panic!("no toolchain note in {}", report["notes"]));
    assert_eq!(note["about"], fixture.target().to_str().unwrap());
    assert!(note["note"].as_str().unwrap().contains("cargo clean"));
}
