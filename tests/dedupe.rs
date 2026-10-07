//! The dedupe pass and its hash index, on throwaway dirs only.

use std::cell::RefCell;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use swarfr::dedupe::{DEFAULT_MIN_SIZE, Dedupe};
use swarfr::eco::cargo::{CARGO, LOCK_FILE};
use swarfr::engine::{self, Options, PassReport};
use swarfr::index::HashIndex;
use tempfile::TempDir;

mod common;
use common::{Fixture, ino, run_unbusy};

const BIG: usize = 3 * DEFAULT_MIN_SIZE as usize;
const OLD_MTIME: Duration = Duration::from_secs(1_000_000_000);
const NEWER_MTIME: Duration = Duration::from_secs(1_100_000_000);
const NEWEST_MTIME: Duration = Duration::from_secs(1_200_000_000);

fn write_at(path: &Path, content: &[u8], mtime: Duration) {
    fs::write(path, content).unwrap();
    common::set_mtime(path, SystemTime::UNIX_EPOCH + mtime);
}

/// Profile dirs `a/debug` and `b/debug`, each holding the same `deps/libx.rlib`.
fn two_profiles() -> (TempDir, Vec<PathBuf>) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let dirs: Vec<PathBuf> = ["a", "b"]
        .iter()
        .map(|t| root.join(t).join("debug"))
        .collect();
    for dir in &dirs {
        fs::create_dir_all(dir.join("deps")).unwrap();
        File::create(dir.join(LOCK_FILE)).unwrap();
        write_at(&dir.join("deps/libx.rlib"), &vec![7; BIG], OLD_MTIME);
    }
    (tmp, dirs)
}

/// One run with the index at `index`; returns the dedupe report and how many files were hashed.
fn run(dirs: &[PathBuf], index: &Path, min_age: Duration) -> (PassReport, usize) {
    let cache = RefCell::new(HashIndex::load(index));
    let mut dedupe = Dedupe::new(&cache);
    dedupe.min_age = min_age;
    let mut report =
        run_unbusy(|| engine::run(dirs, &[&dedupe], &Options::default(), &CARGO).unwrap());
    assert!(report.busy.is_empty());
    let hashed = dedupe.hashed();
    cache.borrow().save(index).unwrap();
    (report.passes.remove(0), hashed)
}

#[test]
fn equal_files_are_cloned_once_and_the_second_run_does_nothing() {
    if !common::filesystem_can(|caps| caps.clone, "share blocks") {
        return;
    }
    let (tmp, dirs) = two_profiles();
    let index = tmp.path().join("cache/index.bin");
    let (a, b) = (
        dirs[0].join("deps/libx.rlib"),
        dirs[1].join("deps/libx.rlib"),
    );
    // Same size as nothing else: must never be read.
    write_at(&dirs[0].join("unique"), &vec![1; BIG + 1], OLD_MTIME);
    let b_ino = ino(&b);

    let (first, hashed) = run(&dirs, &index, Duration::ZERO);

    assert_eq!(
        (first.planned, first.applied, hashed),
        (1, 1, 2),
        "{first:?}"
    );
    let meta = fs::metadata(&a).unwrap();
    assert_eq!(first.freed_bytes, swarfr::sys::allocated(&a, &meta));
    assert_ne!(ino(&b), b_ino, "b is now a clone of a");
    assert_ne!(ino(&a), ino(&b));
    assert_eq!(fs::read(&b).unwrap(), vec![7; BIG]);
    let mtime = fs::metadata(&b).unwrap().modified().unwrap();
    assert_eq!(mtime, SystemTime::UNIX_EPOCH + OLD_MTIME);

    let (second, hashed) = run(&dirs, &index, Duration::ZERO);

    assert_eq!((second.planned, hashed), (0, 0), "{second:?}");
}

#[test]
fn only_a_rewritten_file_is_hashed_again() {
    if !common::filesystem_can(|caps| caps.clone, "share blocks") {
        return;
    }
    let (tmp, dirs) = two_profiles();
    let index = tmp.path().join("index.bin");
    let b = dirs[1].join("deps/libx.rlib");
    run(&dirs, &index, Duration::ZERO);

    // A rebuild produced different bytes of the same size.
    write_at(&b, &vec![8; BIG], NEWER_MTIME);
    let (changed, hashed) = run(&dirs, &index, Duration::ZERO);
    assert_eq!((changed.planned, hashed), (0, 1), "{changed:?}");

    // A rebuild produced the old bytes again: cloned from the shared copy, which stays put.
    let a_ino = ino(&dirs[0].join("deps/libx.rlib"));
    write_at(&b, &vec![7; BIG], NEWEST_MTIME);
    let (same, hashed) = run(&dirs, &index, Duration::ZERO);
    assert_eq!((same.applied, hashed), (1, 1), "{same:?}");
    assert_eq!(ino(&dirs[0].join("deps/libx.rlib")), a_ino);
}

#[test]
fn hot_and_small_files_are_left_alone() {
    let (tmp, dirs) = two_profiles();
    let index = tmp.path().join("index.bin");
    for dir in &dirs {
        fs::write(dir.join("hot"), vec![2; BIG]).unwrap();
        write_at(&dir.join("small"), &[3; 16], OLD_MTIME);
        fs::remove_file(dir.join("deps/libx.rlib")).unwrap();
    }

    let (report, hashed) = run(&dirs, &index, Duration::from_secs(60 * 60));

    assert_eq!((report.planned, hashed), (0, 0), "{report:?}");
}

#[test]
fn index_survives_a_round_trip_and_ignores_garbage() {
    if !common::filesystem_can(|caps| caps.clone, "share blocks") {
        return;
    }
    let (tmp, dirs) = two_profiles();
    let index = tmp.path().join("index.bin");
    run(&dirs, &index, Duration::ZERO);

    let loaded = HashIndex::load(&index);
    assert_eq!(loaded.len(), 2);
    let copy = tmp.path().join("copy.bin");
    loaded.save(&copy).unwrap();
    assert_eq!(HashIndex::load(&copy), loaded);

    let mut bytes = fs::read(&index).unwrap();
    bytes.pop();
    fs::write(&index, bytes).unwrap();
    assert!(HashIndex::load(&index).is_empty(), "truncated");
    fs::write(&index, b"not an index").unwrap();
    assert!(HashIndex::load(&index).is_empty(), "foreign");
    assert!(HashIndex::load(&tmp.path().join("missing")).is_empty());
}

// --- two real cargo builds of the same sources ---

#[test]
fn sibling_cargo_targets_share_artifacts_and_stay_fresh() {
    if !common::filesystem_can(|caps| caps.clone, "share blocks") {
        return;
    }
    let fixture = Fixture::new();
    let targets = [fixture.root.join("target-a"), fixture.root.join("target-b")];
    for target in &targets {
        fixture.build(target);
    }
    let dirs: Vec<PathBuf> = targets.iter().map(|t| t.join("debug")).collect();

    let (report, _) = run(&dirs, &fixture.root.join("index.bin"), Duration::ZERO);

    assert!(
        report.applied >= 1,
        "two builds of one source share nothing: {report:?}"
    );
    assert!(report.skipped.is_empty(), "{report:?}");
    for target in &targets {
        fixture.assert_fresh(target);
    }
}
