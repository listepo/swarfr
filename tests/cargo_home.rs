//! Compressing the cargo home. Everything here is a fake home in a temp dir; the machine's own
//! `~/.cargo` is never the subject of a test.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use predicates::str::contains;
use tempfile::TempDir;

mod common;
use common::{allocated_bytes, swarfr};

const EXIT_BUSY: i32 = 2;
/// Above the compress pass's floor, and text enough to compress well.
const SOURCE: usize = 64 * 1024;

fn home() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().canonicalize().unwrap().join("cargo-home");
    let index = home.join("registry/src/index.crates.io-1949cf8c6b5b557f/serde-1.0.0");
    fs::create_dir_all(index.join("src")).unwrap();
    // What cargo writes when it has finished extracting a crate; its absence is what makes
    // cargo extract the crate again.
    fs::write(index.join(".cargo-ok"), "{\"v\":1}").unwrap();
    fs::write(
        index.join("src/lib.rs"),
        "pub fn de() {}\n".repeat(SOURCE / 14),
    )
    .unwrap();
    let checkout = home.join("git/checkouts/some-repo-0000/abcdef0");
    fs::create_dir_all(&checkout).unwrap();
    fs::write(
        checkout.join("lib.rs"),
        "pub fn git() {}\n".repeat(SOURCE / 16),
    )
    .unwrap();
    // Already-compressed archives cargo keeps beside the sources: never touched.
    let cache = home.join("registry/cache/index.crates.io-1949cf8c6b5b557f");
    fs::create_dir_all(&cache).unwrap();
    fs::write(cache.join("serde-1.0.0.crate"), vec![3; SOURCE]).unwrap();
    File::create(home.join(".package-cache")).unwrap();
    (tmp, home)
}

/// Every file under `dir` with its content and mtime: what compression must not change.
fn contents(dir: &Path) -> BTreeMap<PathBuf, (Vec<u8>, SystemTime)> {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            let meta = entry.metadata().unwrap();
            let body = fs::read(entry.path()).unwrap();
            (entry.path().to_path_buf(), (body, meta.modified().unwrap()))
        })
        .collect()
}

/// `run --cargo-home <home>` with a config home of its own.
fn run(home: &Path, extra: &[&str]) -> assert_cmd::Command {
    let config_home = home.parent().unwrap().join("config-home");
    fs::create_dir_all(&config_home).unwrap();
    let mut cmd = swarfr(&config_home);
    cmd.args(["run", "--cargo-home"])
        .arg(home)
        .args(extra)
        .args(["--min-age", "0", "--index"])
        .arg(home.parent().unwrap().join("index.bin"));
    cmd
}

/// Files under `dir` the filesystem is holding compressed.
fn compressed_files(dir: &Path) -> usize {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| {
            entry.metadata().is_ok_and(|meta| {
                swarfr::sys::flags(entry.path(), &meta) & swarfr::sys::COMPRESSED != 0
            })
        })
        .count()
}

/// A/B: two identical homes, `--cargo-home` the only difference. Only the named one shrinks,
/// and not one byte of content or one mtime changes with it.
#[test]
fn ab_only_the_named_home_is_compressed_and_nothing_in_it_changes() {
    if !common::filesystem_can(|caps| caps.compress, "compress") {
        return;
    }
    let (_tmp_a, control) = home();
    let (_tmp_b, treatment) = home();
    let before = contents(&treatment);
    let sources = |home: &Path| allocated_bytes(&home.join("registry/src"));

    // The control still has to be a run: same flags, no `--cargo-home`.
    let config_home = control.parent().unwrap().join("config-home");
    fs::create_dir_all(&config_home).unwrap();
    swarfr(&config_home)
        .args(["run", "--min-age", "0", "--index"])
        .arg(control.parent().unwrap().join("index.bin"))
        .arg(&control)
        .assert()
        .failure()
        .stderr(contains("no cargo target dirs found"));
    run(&treatment, &[]).assert().success();

    // btrfs compresses and still reports the uncompressed size in `st_blocks`, so on a
    // filesystem like that the win is real and the number cannot show it. What is observable
    // everywhere is the flag: the sources came back compressed.
    if swarfr::sys::ALLOCATED_SHOWS_COMPRESSION {
        assert!(
            sources(&treatment) < sources(&control),
            "{} vs {}",
            sources(&treatment),
            sources(&control)
        );
    } else {
        assert!(
            compressed_files(&treatment.join("registry/src")) > 0,
            "nothing in the sources carries the compressed flag"
        );
    }
    assert_eq!(contents(&treatment), before, "content and mtimes are kept");
}

#[test]
fn the_already_packed_crates_are_left_alone() {
    let (_tmp, home) = home();
    let cache = home.join("registry/cache");
    let before = allocated_bytes(&cache);

    run(&home, &[]).assert().success();

    assert_eq!(allocated_bytes(&cache), before);
}

#[test]
fn a_held_package_cache_stops_the_run_without_touching_anything() {
    let (_tmp, home) = home();
    let before = allocated_bytes(&home.join("registry/src"));
    // What cargo holds while it fetches or extracts.
    let cargo = File::options()
        .write(true)
        .open(home.join(".package-cache"))
        .unwrap();
    cargo.lock().unwrap();

    run(&home, &[])
        .assert()
        .code(EXIT_BUSY)
        .stdout(contains("busy, skipped"));

    assert_eq!(allocated_bytes(&home.join("registry/src")), before);
}

#[test]
fn a_dry_run_reports_the_home_as_its_own_group_and_changes_nothing() {
    if !common::filesystem_can(|caps| caps.compress, "compress") {
        return;
    }
    let (_tmp, home) = home();
    let before = contents(&home);

    let out = run(&home, &["--dry-run", "--json"]).output().unwrap();

    assert!(out.status.success());
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let group = report["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|group| group["family"] == home.to_str().unwrap())
        .unwrap();
    let passes = group["passes"].as_array().unwrap();
    // The two lossless passes, and nothing that deletes: these are sources, not build output.
    let names: Vec<&str> = passes.iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["compress", "dedupe"], "{passes:?}");
    assert!(passes[0]["planned"].as_u64().unwrap() >= 2);
    for pass in passes {
        assert_eq!(pass["applied"], 0, "a dry run applies nothing: {pass:?}");
    }
    assert_eq!(contents(&home), before);
}

#[test]
fn a_home_cargo_has_never_used_is_refused() {
    let (_tmp, home) = home();
    fs::remove_file(home.join(".package-cache")).unwrap();

    run(&home, &[])
        .assert()
        .failure()
        .stderr(contains(".package-cache"));
}

#[test]
fn status_measures_the_home_only_when_asked() {
    let (_tmp, home) = home();
    let config_home = home.parent().unwrap().join("config-home");
    fs::create_dir_all(&config_home).unwrap();

    let out = swarfr(&config_home)
        .args(["status", "--json", "--cargo-home"])
        .arg(&home)
        .arg(&home)
        .output()
        .unwrap();

    assert!(out.status.success());
    let inventory: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let stats = &inventory["cargo_home"];
    assert_eq!(stats["home"], home.to_str().unwrap());
    assert!(stats["allocated_bytes"].as_u64().unwrap() > 0);
    let compressible = stats["compressible_bytes"].as_u64().unwrap();
    if swarfr::sys::caps(&home).compress {
        assert!(compressible > 0);
    } else {
        // Nothing here is compressible if the filesystem does not compress, whatever the
        // files' sizes say.
        assert_eq!(compressible, 0);
    }

    let plain = swarfr(&config_home)
        .args(["status", "--json"])
        .arg(&home)
        .output()
        .unwrap();
    let inventory: serde_json::Value = serde_json::from_slice(&plain.stdout).unwrap();
    assert!(inventory.get("cargo_home").is_none(), "{inventory}");
}
