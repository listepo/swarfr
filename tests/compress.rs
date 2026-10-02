//! The compress pass, alone and in front of dedupe, on throwaway dirs only.
#![cfg(target_os = "macos")]

use std::cell::RefCell;
use std::fs::{self, File};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use swarfr::compress::{Compress, DEFAULT_MIN_AGE, DEFAULT_MIN_SIZE};
use swarfr::dedupe::Dedupe;
use swarfr::eco::cargo::{CARGO, LOCK_FILE};
use swarfr::engine::{self, Options, PassReport, Skip};
use swarfr::index::{HASH_BYTES, HashIndex};
use swarfr::model::{Stamp, TMP_PREFIX};
use swarfr::sys;
use tempfile::TempDir;

mod common;
use common::{Fixture, allocated_bytes, ino, run_unbusy};

const LINE: &[u8] = b"swarfr: a line of text that compresses very well\n";
const BIG: usize = 8 * DEFAULT_MIN_SIZE as usize;
/// Real targets hold rlibs of this size; T2 saw a 119 MB file left uncompressed.
const HUGE: usize = 130 << 20;
const MODE: u32 = 0o640;
const OLD_MTIME: Duration = Duration::from_secs(1_000_000_000);
const XORSHIFT_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

fn text(len: usize) -> Vec<u8> {
    LINE.iter().copied().cycle().take(len).collect()
}

/// Bytes no codec gets anything out of.
fn noise(len: usize) -> Vec<u8> {
    let mut state = XORSHIFT_SEED;
    let mut bytes = Vec::with_capacity(len + size_of::<u64>());
    while bytes.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    bytes.truncate(len);
    bytes
}

fn write_old(path: &Path, content: &[u8]) {
    fs::write(path, content).unwrap();
    let file = File::open(path).unwrap();
    file.set_modified(SystemTime::UNIX_EPOCH + OLD_MTIME)
        .unwrap();
}

fn profile(root: &Path, name: &str) -> PathBuf {
    let dir = root.join(name).join("debug");
    fs::create_dir_all(dir.join("deps")).unwrap();
    File::create(dir.join(LOCK_FILE)).unwrap();
    dir
}

fn root() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    (tmp, root)
}

fn is_compressed(path: &Path) -> bool {
    sys::flags(path, &fs::metadata(path).unwrap()) & sys::COMPRESSED != 0
}

fn temps_left(dir: &Path) -> usize {
    let names = walkdir::WalkDir::new(dir).into_iter().map(|e| e.unwrap());
    names
        .filter(|e| e.file_name().to_string_lossy().starts_with(TMP_PREFIX))
        .count()
}

struct Outcome {
    compress: PassReport,
    dedupe: PassReport,
    hashed: usize,
    notes: Vec<String>,
}

/// Compress, then dedupe, the way `swarfr run` registers them.
fn run(dirs: &[PathBuf], index: &RefCell<HashIndex>, min_age: Duration) -> Outcome {
    let (mut compress, mut dedupe) = (Compress::new(index), Dedupe::new(index));
    compress.min_age = min_age;
    dedupe.min_age = min_age;
    let mut report = run_unbusy(|| {
        engine::run(dirs, &[&compress, &dedupe], &Options::default(), &CARGO).unwrap()
    });
    assert!(report.busy.is_empty());
    let dedupe_report = report.passes.remove(1);
    Outcome {
        compress: report.passes.remove(0),
        dedupe: dedupe_report,
        hashed: dedupe.hashed(),
        notes: compress.notes(),
    }
}

#[test]
fn fixture_target_shrinks_and_stays_fresh() {
    let fixture = Fixture::new();
    let target = fixture.target();
    fixture.build(&target);
    let profile = target.join("debug");
    let before = allocated_bytes(&profile);
    let index = RefCell::default();

    let first = run(std::slice::from_ref(&profile), &index, Duration::ZERO);

    assert!(first.compress.applied > 0, "{:?}", first.notes);
    let after = allocated_bytes(&profile);
    assert!(after < before, "{before} -> {after}");
    assert_eq!(before - after, first.compress.freed_bytes);
    assert_eq!(temps_left(&profile), 0);
    fixture.assert_fresh(&target);

    let second = run(std::slice::from_ref(&profile), &index, Duration::ZERO);
    assert_eq!(second.compress.applied, 0, "{:?}", second.compress);
}

#[test]
fn hardlink_group_stays_one_inode_and_keeps_mtime_and_mode() {
    let (_tmp, root) = root();
    let dir = profile(&root, "a");
    let (top, dep) = (dir.join("libbig.rlib"), dir.join("deps/libbig-1.rlib"));
    write_old(&dep, &text(BIG));
    fs::set_permissions(&dep, fs::Permissions::from_mode(MODE)).unwrap();
    fs::hard_link(&dep, &top).unwrap();
    let old_ino = ino(&dep);

    let outcome = run(
        std::slice::from_ref(&dir),
        &RefCell::default(),
        Duration::ZERO,
    );

    assert_eq!(outcome.compress.applied, 1, "{:?}", outcome.notes);
    assert!(outcome.compress.freed_bytes > 0);
    assert_eq!(ino(&top), ino(&dep));
    assert_ne!(ino(&dep), old_ino);
    assert!(is_compressed(&dep));
    assert_eq!(fs::read(&top).unwrap(), text(BIG));
    let meta = fs::metadata(&top).unwrap();
    assert_eq!(meta.modified().unwrap(), SystemTime::UNIX_EPOCH + OLD_MTIME);
    assert_eq!(meta.mode() & 0o7777, MODE);
    assert_eq!(temps_left(&dir), 0);
}

#[test]
fn small_hot_shared_and_flagged_files_are_not_planned() {
    let (_tmp, root) = root();
    let dir = profile(&root, "a");
    write_old(&dir.join("small"), &text(DEFAULT_MIN_SIZE as usize - 1));
    fs::write(dir.join("hot"), text(BIG)).unwrap();
    let (shared, flagged) = (dir.join("shared"), dir.join("flagged"));
    write_old(&shared, &text(BIG));
    write_old(&flagged, &text(BIG));
    let chflags = Command::new("chflags").arg("nodump").arg(&flagged).status();
    assert!(chflags.unwrap().success());
    // A clone made by dedupe: compressing it would only un-share it.
    let mut index = HashIndex::default();
    index.put(&Stamp::read(&shared).unwrap(), [0; HASH_BYTES], true);

    let outcome = run(&[dir], &RefCell::new(index), DEFAULT_MIN_AGE);

    assert_eq!(outcome.compress.planned, 0, "{:?}", outcome.compress);
}

#[test]
fn incompressible_file_keeps_its_inode() {
    let (_tmp, root) = root();
    let dir = profile(&root, "a");
    let path = dir.join("deps/noise.bin");
    write_old(&path, &noise(BIG));
    let old_ino = ino(&path);

    let outcome = run(
        std::slice::from_ref(&dir),
        &RefCell::default(),
        Duration::ZERO,
    );

    assert_eq!(outcome.compress.applied, 0);
    assert_eq!(
        outcome.compress.skipped,
        [(path.clone(), Skip::NotCompressed)]
    );
    assert!(
        !outcome.notes.is_empty(),
        "the backend's reason is reported"
    );
    assert_eq!(ino(&path), old_ino);
    assert_eq!(temps_left(&dir), 0);
}

#[test]
fn a_file_as_large_as_a_real_rlib_compresses() {
    let (_tmp, root) = root();
    let dir = profile(&root, "a");
    let path = dir.join("deps/libhuge.rlib");
    write_old(&path, &text(HUGE));

    let outcome = run(&[dir], &RefCell::default(), Duration::ZERO);

    assert_eq!(outcome.compress.applied, 1, "{:?}", outcome.notes);
    assert!(is_compressed(&path));
    assert_eq!(fs::metadata(&path).unwrap().len(), HUGE as u64);
}

#[test]
fn dedupe_after_compress_shares_compressed_clones_and_the_second_run_is_idle() {
    let (_tmp, root) = root();
    let dirs = [profile(&root, "a"), profile(&root, "b")];
    for dir in &dirs {
        write_old(&dir.join("deps/libx.rlib"), &text(BIG));
    }
    let index = RefCell::default();

    let first = run(&dirs, &index, Duration::ZERO);

    assert_eq!(first.compress.applied, 2, "{:?}", first.notes);
    assert_eq!(first.dedupe.applied, 1, "{:?}", first.dedupe);
    for dir in &dirs {
        let path = dir.join("deps/libx.rlib");
        assert!(
            is_compressed(&path),
            "a clone of a compressed file is compressed"
        );
        assert_eq!(fs::read(path).unwrap(), text(BIG));
    }

    let second = run(&dirs, &index, Duration::ZERO);
    let planned = (second.compress.planned, second.dedupe.planned);
    assert_eq!((planned, second.hashed), ((0, 0), 0));
}

#[test]
fn a_compressed_file_keeps_its_hash_in_the_index() {
    let (_tmp, root) = root();
    let dirs = [profile(&root, "a"), profile(&root, "b")];
    // Same size, different content: dedupe hashes both and shares nothing.
    write_old(&dirs[0].join("deps/libx.rlib"), &text(BIG));
    write_old(&dirs[1].join("deps/libx.rlib"), &[b'x'; BIG]);
    let index = RefCell::default();
    let mut dedupe = Dedupe::new(&index);
    dedupe.min_age = Duration::ZERO;
    run_unbusy(|| engine::run(&dirs, &[&dedupe], &Options::default(), &CARGO).unwrap());
    assert_eq!(dedupe.hashed(), 2);

    let outcome = run(&dirs, &index, Duration::ZERO);

    let said = (&outcome.compress, &outcome.notes);
    assert_eq!(outcome.compress.applied, 2, "{said:?}");
    assert_eq!(outcome.hashed, 0, "the entries moved to the new inodes");
    assert_eq!(index.borrow().len(), 2);
}
