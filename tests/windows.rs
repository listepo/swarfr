//! What only Windows can show: NTFS compression through the engine, a sharing violation on
//! `rename`, and a family key spelled the way git prints it.
//!
//! Point `TEMP` at a ReFS volume (a Dev Drive) to run the clone half. On NTFS the clone tests
//! in `src/sys/windows.rs` assert the refusal, and the tests here assert the compression.

#![cfg(windows)]

use std::fs::{self, File};
use std::os::windows::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use swarfr::compress::Compress;
use swarfr::eco::cargo::{CARGO, LOCK_FILE};
use swarfr::engine::{self, Options, Share};
use swarfr::index::HashIndex;
use swarfr::inventory;
use swarfr::model::{self, Profile};
use swarfr::session::Request;
use swarfr::sys;
use tempfile::TempDir;

mod common;
use common::{fake_target, git, ino, set_mtime};

use swarfr::config::Config;
use swarfr::engine::{Action, Pass, Replace};

/// `FILE_SHARE_READ | FILE_SHARE_WRITE`, and not `FILE_SHARE_DELETE`. A `rename` over a file
/// opened this way is a sharing violation.
const SHARE_READ_WRITE: u32 = 0x1 | 0x2;
const REPETITIVE: &[u8] = &[9; 128 * 1024];
const OLD: Duration = Duration::from_secs(1_000_000_000);

struct FnPass<F> {
    plan: F,
}

impl<F: Fn(&[Profile]) -> Vec<Action>> Pass for FnPass<F> {
    fn name(&self) -> &'static str {
        "test"
    }
    fn plan(&self, profiles: &[Profile]) -> Vec<Action> {
        (self.plan)(profiles)
    }
}

fn profile() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("debug");
    fs::create_dir_all(dir.join("deps")).unwrap();
    File::create(dir.join(LOCK_FILE)).unwrap();
    fs::write(dir.join("canon"), REPETITIVE).unwrap();
    let member = dir.join("deps/member");
    fs::write(&member, REPETITIVE).unwrap();
    set_mtime(&member, SystemTime::UNIX_EPOCH + OLD);
    (tmp, dir)
}

/// The compress pass on NTFS has to copy, because the volume cannot clone, and the copy it
/// swaps in is smaller on disk and still the same bytes.
#[test]
fn compress_copies_then_shrinks_and_a_second_run_finds_nothing() {
    let (_tmp, dir) = profile();
    if !sys::caps(&dir).compress {
        eprintln!("this volume does not compress; point TEMP at an NTFS volume");
        return;
    }
    let before = sys::allocated(
        &dir.join("canon"),
        &fs::metadata(dir.join("canon")).unwrap(),
    );
    let index = std::cell::RefCell::new(HashIndex::default());
    let mut compress = Compress::new(&index);
    compress.min_age = Duration::ZERO;

    let report = engine::run(
        std::slice::from_ref(&dir),
        &[&compress],
        &Options::default(),
        &CARGO,
    )
    .unwrap();

    assert!(
        report.passes[0].applied >= 1,
        "{report:?} {:?}",
        compress.notes()
    );
    let canon = dir.join("canon");
    assert_eq!(fs::read(&canon).unwrap(), REPETITIVE);
    let after = sys::allocated(&canon, &fs::metadata(&canon).unwrap());
    assert!(after < before, "{before} -> {after}");
    assert_ne!(
        sys::flags(&canon, &fs::metadata(&canon).unwrap()) & sys::COMPRESSED,
        0
    );
    assert!(model::scan(&dir, &CARGO).unwrap().stale_temps.is_empty());

    let again = engine::run(&[dir], &[&compress], &Options::default(), &CARGO).unwrap();
    assert_eq!(again.passes[0].applied, 0, "{again:?}");
}

/// An editor or a scanner that holds the destination without `FILE_SHARE_DELETE` makes the
/// `rename` a sharing violation. That is a skipped file, not a failed run.
#[test]
fn a_rename_blocked_by_sharing_is_a_skip() {
    let (_tmp, dir) = profile();
    let member = dir.join("deps/member");
    let held = File::options()
        .read(true)
        .share_mode(SHARE_READ_WRITE)
        .open(&member)
        .unwrap();
    let pass = FnPass {
        plan: |profiles: &[Profile]| {
            let find = |name: &str| {
                profiles
                    .iter()
                    .flat_map(|profile| &profile.inodes)
                    .find(|inode| {
                        inode
                            .paths
                            .iter()
                            .any(|path| path.file_name().unwrap() == name)
                    })
            };
            let (Some(source), Some(member)) = (find("canon"), find("member")) else {
                return Vec::new();
            };
            vec![Action::Replace(Replace {
                source: source.paths[0].clone(),
                source_stamp: source.stamp.clone(),
                member: member.clone(),
                how: Share::Link,
            })]
        },
    };

    let report = engine::run(
        std::slice::from_ref(&dir),
        &[&pass],
        &Options::default(),
        &CARGO,
    )
    .unwrap();

    assert_eq!(report.passes[0].applied, 0, "{report:?}");
    assert!(
        report.passes[0]
            .skipped
            .iter()
            .any(|(_, skip)| *skip == swarfr::engine::Skip::Busy),
        "{report:?}"
    );
    assert_ne!(
        ino(&member),
        ino(&dir.join("canon")),
        "the link did not land"
    );
    drop(held);
}

/// `canonicalize` returns `\\?\C:\…` and git prints `C:/…`. A family key written the second
/// way still skips the family.
#[test]
fn a_slashed_family_key_skips_the_verbatim_family() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let repo = base.join("repo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("README"), "x\n").unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "init"]);
    fake_target(&base, "repo", 1, 0);

    let found = inventory::inventory(std::slice::from_ref(&base)).unwrap();
    let target = found
        .targets
        .iter()
        .find(|target| {
            target.root.ends_with("repo/target") || target.root.ends_with(r"repo\target")
        })
        .expect("the fake target");
    let family = target.family.clone().unwrap();
    let slashed = sys::plain(&family).to_string_lossy().replace('\\', "/");
    assert!(
        family.to_string_lossy().starts_with(r"\\?\"),
        "the stored family is the verbatim form, got {}",
        family.display()
    );
    assert_ne!(slashed, family.to_string_lossy());

    let config_path = base.join("config.toml");
    fs::write(
        &config_path,
        format!("[family.\"{slashed}\"]\nskip = true\n"),
    )
    .unwrap();
    let request = Request::from_config(&Config::load(&config_path).unwrap());

    assert!(
        !request.keeps(target),
        "{slashed} should skip {}",
        family.display()
    );
}
