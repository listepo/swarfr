//! `swarfr seed` on a real `git worktree` of the cargo fixture. Everything is in temp dirs.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use predicates::str::contains;
use swarfr::eco::cargo::{ARTIFACT_LOCK_FILE, BUILD_LOCK_FILE, CARGO, LOCK_FILE};
use swarfr::index::HashIndex;
use swarfr::seed;
use tempfile::TempDir;

mod common;
use common::{Fixture, allocated_bytes, stale_units_at, swarfr};

const EXIT_FAILURE: i32 = 1;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=swarfr", "-c", "user.email=swarfr@invalid"])
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// The fixture as a git repository with one worktree, built once in the repository itself.
/// Returns the worktree's own copy of the workspace, which has no target dir yet.
struct Family {
    fixture: Fixture,
    _worktree_tmp: TempDir,
    worktree_ws: PathBuf,
}

impl Family {
    fn new() -> Self {
        let fixture = Fixture::new();
        git(&fixture.root, &["init", "-b", "main"]);
        // Committed before anything is built, so no target dir can reach the worktree.
        git(&fixture.root, &["add", "."]);
        git(&fixture.root, &["commit", "-m", "fixture"]);
        let tmp = TempDir::new().unwrap();
        let worktree = tmp.path().canonicalize().unwrap().join("wt");
        git(
            &fixture.root,
            &["worktree", "add", worktree.to_str().unwrap()],
        );
        fixture.build(&fixture.target());
        Self {
            fixture,
            _worktree_tmp: tmp,
            worktree_ws: worktree.join("ws"),
        }
    }

    fn source(&self) -> PathBuf {
        self.fixture.target()
    }

    fn seeded(&self) -> PathBuf {
        self.worktree_ws.join("target")
    }
}

fn nowhere(root: &Path) -> HashIndex {
    HashIndex::load(&root.join("index-that-does-not-exist.bin"))
}

/// What a seeded worktree actually starts with: measured, not assumed.
#[test]
fn a_seeded_worktree_reuses_what_it_can() {
    if !common::filesystem_can(|caps| caps.clone, "share blocks") {
        return;
    }
    let family = Family::new();
    let mut index = nowhere(&family.worktree_ws);

    let seeded = seed::seed(
        &family.worktree_ws,
        &family.source(),
        &CARGO,
        &mut index,
        false,
    )
    .unwrap();

    assert_eq!(seeded.source, family.source());
    assert!(seeded.files > 0, "{seeded:?}");
    assert!(seeded.busy.is_empty(), "{seeded:?}");
    // The copy is a clone of the original: same bytes, same size, a separate inode.
    let rlib = |target: &Path| {
        let deps = fs::read_dir(target.join("debug/deps")).unwrap();
        deps.filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|kind| kind == "rlib"))
            .unwrap()
    };
    let (original, copy) = (rlib(&family.source()), rlib(&family.seeded()));
    assert_eq!(fs::read(&original).unwrap(), fs::read(&copy).unwrap());
    assert_eq!(allocated_bytes(&family.seeded()), seeded.bytes);
    // The copy keeps the source's stamp: one that looks newer makes the
    // seeded worktree look freshly built, and cargo rebuilds the units the
    // seeding was for.
    let stamps = (
        fs::metadata(&original).unwrap().modified().unwrap(),
        fs::metadata(&copy).unwrap().modified().unwrap(),
    );
    assert_eq!(stamps.0, stamps.1);
}

#[test]
fn the_cache_and_the_lock_files_are_left_behind() {
    let family = Family::new();
    let mut index = nowhere(&family.worktree_ws);
    assert!(family.source().join("debug/incremental").is_dir());

    seed::seed(
        &family.worktree_ws,
        &family.source(),
        &CARGO,
        &mut index,
        false,
    )
    .unwrap();

    assert!(!family.seeded().join("debug/incremental").exists());
    let seeded_debug = family.seeded().join("debug");
    for name in [LOCK_FILE, BUILD_LOCK_FILE, ARTIFACT_LOCK_FILE] {
        assert!(
            !seeded_debug.join(name).exists(),
            "seed must not copy cargo's {name}"
        );
    }
    assert!(family.seeded().join("CACHEDIR.TAG").is_file());
}

#[test]
fn a_dry_run_copies_nothing_and_an_existing_target_is_refused() {
    let family = Family::new();
    let mut index = nowhere(&family.worktree_ws);

    let planned = seed::seed(
        &family.worktree_ws,
        &family.source(),
        &CARGO,
        &mut index,
        true,
    )
    .unwrap();

    assert!(planned.files > 0);
    assert!(!family.seeded().exists());

    seed::seed(
        &family.worktree_ws,
        &family.source(),
        &CARGO,
        &mut index,
        false,
    )
    .unwrap();
    let again = seed::seed(
        &family.worktree_ws,
        &family.source(),
        &CARGO,
        &mut index,
        false,
    );
    assert_eq!(
        again.unwrap_err().kind(),
        std::io::ErrorKind::AlreadyExists,
        "a target that is already there is never merged into"
    );
}

#[test]
fn a_busy_source_profile_is_reported_and_not_copied() {
    let family = Family::new();
    let mut index = nowhere(&family.worktree_ws);
    let profile = family.source().join("debug");
    // What cargo holds for the length of a build.
    let _build = common::hold_unit_lock(&profile);

    let seeded = seed::seed(
        &family.worktree_ws,
        &family.source(),
        &CARGO,
        &mut index,
        false,
    )
    .unwrap();

    assert_eq!(seeded.busy, [profile]);
    assert!(!family.seeded().join("debug").exists());
    assert!(family.seeded().join("CACHEDIR.TAG").is_file());
}

#[test]
fn the_source_is_chosen_inside_the_family_and_its_copies_are_shared_in_the_index() {
    if !common::filesystem_can(|caps| caps.clone, "share blocks") {
        return;
    }
    let family = Family::new();
    let mut index = nowhere(&family.worktree_ws);
    // What `run` leaves behind: the source files hashed, none of them shared yet.
    let rlib = family.source().join("debug/deps");
    let mut source_stamps = Vec::new();
    for entry in fs::read_dir(&rlib).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            let stamp = swarfr::model::Stamp::read(&path).unwrap();
            index.put(&stamp, [7; 32], false);
            source_stamps.push(stamp);
        }
    }
    assert!(!source_stamps.is_empty());

    assert_eq!(
        seed::choose(&family.worktree_ws, &CARGO),
        Some(family.source())
    );
    seed::seed(
        &family.worktree_ws,
        &family.source(),
        &CARGO,
        &mut index,
        false,
    )
    .unwrap();

    for stamp in &source_stamps {
        assert!(index.get(stamp).unwrap().shared, "the source is shared now");
    }
    for entry in fs::read_dir(family.seeded().join("debug/deps")).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            let stamp = swarfr::model::Stamp::read(&path).unwrap();
            let entry = index.get(&stamp).expect("the copy is in the index");
            assert!(entry.shared, "and so is the copy");
            assert_eq!(entry.hash, [7; 32]);
        }
    }
}

/// A/B: the same fresh worktree twice, seeding the only difference. What a seeded worktree
/// keeps is measured here rather than assumed: a path dependency and the workspace members are
/// compiled again, because their absolute paths are part of the fingerprint and a worktree is
/// somewhere else on disk. What survives is everything whose path did not change.
#[test]
fn ab_a_seeded_worktree_builds_less_than_an_empty_one() {
    let control = Family::new();
    let treatment = Family::new();
    let mut index = nowhere(&treatment.worktree_ws);

    seed::seed(
        &treatment.worktree_ws,
        &treatment.source(),
        &CARGO,
        &mut index,
        false,
    )
    .unwrap();

    let empty = stale_units_at(&control.worktree_ws, &control.seeded());
    let seeded = stale_units_at(&treatment.worktree_ws, &treatment.seeded());
    assert!(
        seeded.len() < empty.len(),
        "seeded {seeded:?} vs empty {empty:?}"
    );
    // The fixture has no registry dependencies (it builds `--offline` from path deps only),
    // so this is the floor of the win, not its size on a real workspace.
    assert!(
        seeded.iter().any(|unit| unit.contains("fx_dep")),
        "a path dependency moves with the worktree and is rebuilt: {seeded:?}"
    );
}

#[test]
fn cli_seeds_from_a_named_checkout_and_says_what_it_did() {
    let family = Family::new();
    let home = family.worktree_ws.join("config-home");
    let index = family.worktree_ws.join("index.bin");

    swarfr(&home)
        .args(["seed", "--dry-run", "--from"])
        .arg(family.fixture.root.join("ws"))
        .arg("--index")
        .arg(&index)
        .arg(&family.worktree_ws)
        .assert()
        .success()
        .stdout(contains("would copy"));
    assert!(!family.seeded().exists());

    swarfr(&home)
        .args(["seed", "--index"])
        .arg(&index)
        .arg(&family.worktree_ws)
        .assert()
        .success()
        .stdout(contains("copied"));
    assert!(family.seeded().join("CACHEDIR.TAG").is_file());

    swarfr(&home)
        .args(["seed", "--index"])
        .arg(&index)
        .arg(&family.worktree_ws)
        .assert()
        .code(EXIT_FAILURE)
        .stderr(contains("already has a target dir"));
}
