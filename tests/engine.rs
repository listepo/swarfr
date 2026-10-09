//! One test per safety invariant of `DESIGN.md`, on throwaway dirs only.

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use swarfr::eco::cargo;
use swarfr::eco::cargo::{BUILD_LOCK_FILE, CARGO, LOCK_FILE};
use swarfr::eco::{Ecosystem, Guard};
use swarfr::engine::{
    self, Action, Interrupt, Interrupted, Options, Pass, Replace, Report, Share, Skip,
};
use swarfr::model::Stamp;
use swarfr::model::{self, Profile, TMP_PREFIX};
use tempfile::TempDir;

mod common;
use common::{BIN, BUILD_SLEEP_ENV, Fixture, POLL, ino, run_unbusy};

const CONTENT: &[u8] = b"same bytes in both files";
const MEMBER_MODE: u32 = 0o640;
const OLD_MTIME: Duration = Duration::from_secs(1_000_000_000);

struct FnPass<F> {
    lossy: bool,
    plan: F,
}

impl<F: Fn(&[Profile]) -> Vec<Action>> Pass for FnPass<F> {
    fn name(&self) -> &'static str {
        "test"
    }
    fn lossy(&self) -> bool {
        self.lossy
    }
    fn plan(&self, profiles: &[Profile]) -> Vec<Action> {
        (self.plan)(profiles)
    }
}

/// Plans: replace the inode of file `member` with a clone of file `source`, if both were scanned.
fn replace_by_name(profiles: &[Profile], source: &str, member: &str) -> Vec<Action> {
    share_by_name(profiles, source, member, Share::Clone)
}

/// The same, sharing the way `how` says.
fn share_by_name(profiles: &[Profile], source: &str, member: &str, how: Share) -> Vec<Action> {
    let find = |name: &str| {
        profiles
            .iter()
            .flat_map(|p| &p.inodes)
            .find(|i| i.paths.iter().any(|p| p.file_name().unwrap() == name))
    };
    let (Some(source), Some(member)) = (find(source), find(member)) else {
        return Vec::new();
    };
    vec![Action::Replace(Replace {
        source: source.paths[0].clone(),
        source_stamp: source.stamp.clone(),
        member: member.clone(),
        how,
    })]
}

fn run(profile: &Path, passes: &[&dyn Pass], opts: &Options) -> Report {
    run_unbusy(|| engine::run(&[profile.to_path_buf()], passes, opts, &CARGO).unwrap())
}

fn run_replace(profile: &Path, opts: &Options) -> Report {
    let pass = FnPass {
        lossy: false,
        plan: |p: &[Profile]| replace_by_name(p, "canon", "member"),
    };
    run(profile, &[&pass], opts)
}

/// A profile dir with `canon` and `member` (same bytes, different inodes); `member` is old.
fn profile() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().canonicalize().unwrap().join("debug");
    fs::create_dir_all(dir.join("deps")).unwrap();
    File::create(dir.join(LOCK_FILE)).unwrap();
    fs::write(dir.join("canon"), CONTENT).unwrap();
    let member = dir.join("deps/member");
    fs::write(&member, CONTENT).unwrap();
    fs::set_permissions(&member, fs::Permissions::from_mode(MEMBER_MODE)).unwrap();
    let mtime = SystemTime::UNIX_EPOCH + OLD_MTIME;
    File::open(&member).unwrap().set_modified(mtime).unwrap();
    (tmp, dir)
}

fn only_skip(report: &Report) -> &Skip {
    assert_eq!(report.passes[0].applied, 0);
    &report.passes[0].skipped[0].1
}

#[test]
fn replaces_whole_hardlink_group_and_keeps_mtime_and_mode() {
    if !common::filesystem_can(|caps| caps.clone, "share blocks") {
        return;
    }
    let (_tmp, dir) = profile();
    let member = dir.join("deps/member");
    let link = dir.join("member-link");
    fs::hard_link(&member, &link).unwrap();
    let old_ino = ino(&member);

    let report = run_replace(&dir, &Options::default());

    assert_eq!(report.passes[0].applied, 1, "{report:?}");
    assert_ne!(ino(&member), old_ino);
    assert_eq!(ino(&member), ino(&link), "the group must stay one inode");
    assert_ne!(
        ino(&member),
        ino(&dir.join("canon")),
        "a clone, not a hardlink"
    );
    let meta = fs::metadata(&member).unwrap();
    assert_eq!(meta.modified().unwrap(), SystemTime::UNIX_EPOCH + OLD_MTIME);
    assert_eq!(meta.mode() & 0o7777, MEMBER_MODE);
    assert_eq!(fs::read(&member).unwrap(), CONTENT);
    assert!(model::scan(&dir, &CARGO).unwrap().stale_temps.is_empty());
}

/// The link fallback (`T22`), which is the only way to share on a filesystem without
/// copy-on-write: one inode under every name of the group, and the *later* of the two
/// modification times on it, because a name that suddenly reads older than what it was built
/// from is a name cargo rebuilds. Filesystem-independent, so this runs everywhere.
#[test]
fn a_link_puts_the_group_on_one_inode_and_keeps_the_later_mtime() {
    let (_tmp, dir) = profile();
    let (canon, member) = (dir.join("canon"), dir.join("deps/member"));
    let link = dir.join("member-link");
    fs::hard_link(&member, &link).unwrap();
    // One inode carries one mode, so the engine only links files that already agree on it.
    let mode = fs::metadata(&canon).unwrap().mode() & 0o7777;
    fs::set_permissions(&member, fs::Permissions::from_mode(mode)).unwrap();
    // And the member is the newer of the two here, so its time is the one that must survive.
    let newer = SystemTime::now() + Duration::from_secs(600);
    File::open(&member).unwrap().set_modified(newer).unwrap();

    let pass = FnPass {
        lossy: false,
        plan: |p: &[Profile]| share_by_name(p, "canon", "member", Share::Link),
    };
    let report = run(&dir, &[&pass], &Options::default());

    assert_eq!(report.passes[0].applied, 1, "{report:?}");
    assert_eq!(
        ino(&member),
        ino(&canon),
        "one inode, not a copy of the bytes"
    );
    assert_eq!(ino(&member), ino(&link), "the whole group follows");
    assert_eq!(fs::read(&member).unwrap(), CONTENT);
    assert_eq!(
        fs::metadata(&member).unwrap().modified().unwrap(),
        newer,
        "the shared inode keeps the later time, for both names"
    );
    assert!(model::scan(&dir, &CARGO).unwrap().stale_temps.is_empty());
}

/// `last_built` is the newest top-level child of the profile. Replacing a file inside `deps/`
/// moves that directory's mtime; without putting it back, an idle profile looks freshly built.
#[test]
fn a_replace_does_not_move_the_profile_last_built() {
    let (_tmp, dir) = profile();
    let (canon, member) = (dir.join("canon"), dir.join("deps/member"));
    let mode = fs::metadata(&canon).unwrap().mode() & 0o7777;
    fs::set_permissions(&member, fs::Permissions::from_mode(mode)).unwrap();
    let old = SystemTime::UNIX_EPOCH + OLD_MTIME;
    for path in [dir.join("deps"), canon, dir.join(LOCK_FILE)] {
        File::open(&path).unwrap().set_modified(old).unwrap();
    }
    let before = cargo::last_built(&dir).unwrap();
    assert_eq!(before, OLD_MTIME.as_secs());

    let pass = FnPass {
        lossy: false,
        plan: |p: &[Profile]| share_by_name(p, "canon", "member", Share::Link),
    };
    let report = run(&dir, &[&pass], &Options::default());

    assert_eq!(report.passes[0].applied, 1, "{report:?}");
    assert_eq!(
        cargo::last_built(&dir).unwrap(),
        before,
        "renaming inside deps/ must not make the profile look freshly built"
    );
}

/// Modes are not negotiable: the fixture's member is `0o640` and `canon` is not, and one inode
/// cannot hold both. Nothing is linked and nothing is touched.
#[test]
fn a_link_refuses_to_change_a_files_mode() {
    let (_tmp, dir) = profile();
    let member = dir.join("deps/member");
    let before = ino(&member);

    let pass = FnPass {
        lossy: false,
        plan: |p: &[Profile]| share_by_name(p, "canon", "member", Share::Link),
    };
    let report = run(&dir, &[&pass], &Options::default());

    assert_eq!(*only_skip(&report), Skip::ModeMismatch);
    assert_eq!(ino(&member), before);
    assert_eq!(fs::metadata(&member).unwrap().mode() & 0o7777, MEMBER_MODE);
    assert_eq!(fs::read(&member).unwrap(), CONTENT);
}

#[test]
fn busy_profile_is_skipped_untouched() {
    let (_tmp, dir) = profile();
    let stale = dir.join(format!("{TMP_PREFIX}crashed"));
    fs::write(&stale, b"x").unwrap();
    let held = File::open(dir.join(LOCK_FILE)).unwrap();
    held.lock().unwrap();
    let old_ino = ino(&dir.join("deps/member"));

    let report = run_replace(&dir, &Options::default());

    assert_eq!(report.busy, [dir.as_path()]);
    assert_eq!(report.passes[0].planned, 0);
    assert_eq!(ino(&dir.join("deps/member")), old_ino);
    assert!(stale.exists());
}

#[test]
fn stale_temps_are_removed_but_not_on_dry_run() {
    let (_tmp, dir) = profile();
    let stale = dir.join("deps").join(format!("{TMP_PREFIX}crashed"));
    fs::write(&stale, b"x").unwrap();
    let dry = Options {
        dry_run: true,
        ..Options::default()
    };

    assert_eq!(run(&dir, &[], &dry).temps_removed, 0);
    assert!(stale.exists());
    let report = run(&dir, &[], &Options::default());
    assert_eq!(report.temps_removed, 1, "{report:?}");
    assert!(!stale.exists());
}

#[test]
fn dry_run_plans_but_applies_nothing() {
    let (_tmp, dir) = profile();
    let old_ino = ino(&dir.join("deps/member"));
    let dry = Options {
        dry_run: true,
        ..Options::default()
    };

    let report = run_replace(&dir, &dry);

    assert_eq!((report.passes[0].planned, report.passes[0].applied), (1, 0));
    assert_eq!(ino(&dir.join("deps/member")), old_ino);
}

#[test]
fn member_changed_after_the_scan_is_skipped() {
    let (_tmp, dir) = profile();
    let member = dir.join("deps/member");
    let pass = FnPass {
        lossy: false,
        plan: |p: &[Profile]| {
            let actions = replace_by_name(p, "canon", "member");
            // rustc rewrites the file between plan and apply
            let mut file = File::options().append(true).open(&member).unwrap();
            file.write_all(b"!").unwrap();
            actions
        },
    };

    let report = run(&dir, &[&pass], &Options::default());

    assert_eq!(only_skip(&report), &Skip::Changed);
    assert!(fs::read(&member).unwrap().ends_with(b"!"));
}

#[test]
fn group_with_a_link_outside_the_profile_is_skipped() {
    let (tmp, dir) = profile();
    fs::hard_link(dir.join("deps/member"), tmp.path().join("outside")).unwrap();

    assert_eq!(
        only_skip(&run_replace(&dir, &Options::default())),
        &Skip::ForeignLinks
    );
}

#[cfg(target_os = "macos")]
#[test]
fn flagged_member_is_skipped() {
    let (_tmp, dir) = profile();
    let status = Command::new("chflags")
        .arg("nodump")
        .arg(dir.join("deps/member"))
        .status()
        .unwrap();
    assert!(status.success());

    assert_eq!(
        only_skip(&run_replace(&dir, &Options::default())),
        &Skip::Flags
    );
}

#[test]
fn paths_outside_locked_profiles_are_refused() {
    let (tmp, dir) = profile();
    let outside = tmp.path().canonicalize().unwrap().join("outside");
    fs::write(&outside, CONTENT).unwrap();
    let pass = FnPass {
        lossy: false,
        plan: |p: &[Profile]| {
            let mut actions = replace_by_name(p, "canon", "member");
            if let Action::Replace(replace) = &mut actions[0] {
                replace.member.paths = vec![outside.clone()];
            }
            actions
        },
    };

    let report = run(&dir, &[&pass], &Options::default());

    assert_eq!(only_skip(&report), &Skip::Unlocked);
}

#[test]
fn lossy_pass_runs_only_when_named() {
    if !common::filesystem_can(|caps| caps.clone, "share blocks") {
        return;
    }
    let (_tmp, dir) = profile();
    let pass = FnPass {
        lossy: true,
        plan: |p: &[Profile]| replace_by_name(p, "canon", "member"),
    };
    let enabled = Options {
        lossy: vec!["test".into()],
        ..Options::default()
    };

    let off = run(&dir, &[&pass], &Options::default());
    let on = run(&dir, &[&pass], &enabled);

    assert!(off.passes.is_empty());
    assert_eq!(on.passes[0].applied, 1);
}

#[test]
fn scan_does_not_follow_symlinks() {
    let (tmp, dir) = profile();
    let outside = tmp.path().join("outside-dir");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret"), b"x").unwrap();
    symlink(&outside, dir.join("link-dir")).unwrap();
    symlink(outside.join("secret"), dir.join("link-file")).unwrap();

    let scan = model::scan(&dir, &CARGO).unwrap();

    let names: Vec<_> = scan
        .inodes
        .iter()
        .flat_map(|i| &i.paths)
        .map(|p| p.file_name().unwrap().to_str().unwrap())
        .collect();
    assert_eq!(names, ["canon", "member"]);
}

#[test]
fn profile_dirs_refuses_a_dir_cargo_did_not_tag() {
    let (_tmp, dir) = profile();
    let target = dir.parent().unwrap();
    assert!(cargo::profile_dirs(target).is_err(), "no tag at all");
    fs::write(
        target.join("CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55",
    )
    .unwrap();
    assert!(cargo::profile_dirs(target).is_err(), "someone else's tag");
    fs::write(target.join("CACHEDIR.TAG"), "# tag created by cargo.").unwrap();
    assert_eq!(cargo::profile_dirs(target).unwrap(), [dir.as_path()]);
}

#[test]
fn profile_dirs_finds_a_build_lock() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("build");
    let dir = target.join("debug");
    fs::create_dir_all(&dir).unwrap();
    fs::write(target.join("CACHEDIR.TAG"), "# tag created by cargo.").unwrap();
    File::create(dir.join(BUILD_LOCK_FILE)).unwrap();

    assert_eq!(cargo::profile_dirs(&target).unwrap(), [dir.as_path()]);
    assert_eq!(CARGO.guard(&dir), Guard::Lock(dir.join(BUILD_LOCK_FILE)));
}

// --- a real cargo build ---

const BUILD_SCRIPT_SLEEP_SECS: &str = "3";
const BUSY_TIMEOUT: Duration = Duration::from_secs(120);

#[test]
fn running_build_is_not_disturbed_and_replaced_artifacts_stay_fresh() {
    if !common::filesystem_can(|caps| caps.clone, "share blocks") {
        return;
    }
    let fixture = Fixture::new();
    let target = fixture.target();
    let profile = target.join("debug");

    // Invariant 1 against the real thing: while cargo builds, the profile reads as busy.
    let mut build = fixture
        .cargo(&target, &["build"])
        .env(BUILD_SLEEP_ENV, BUILD_SCRIPT_SLEEP_SECS)
        .spawn()
        .unwrap();
    let started = Instant::now();
    let mut seen_busy = false;
    while !seen_busy && build.try_wait().unwrap().is_none() {
        assert!(started.elapsed() < BUSY_TIMEOUT);
        // Err: cargo has not created the lock file yet.
        if let Ok(report) = engine::run(
            std::slice::from_ref(&profile),
            &[],
            &Options::default(),
            &CARGO,
        ) {
            seen_busy = !report.busy.is_empty();
        }
        std::thread::sleep(POLL);
    }
    assert!(build.wait().unwrap().success());
    assert!(seen_busy, "never saw cargo holding the lock");
    // Cargo's real tag and lock file are what `profile_dirs` expects.
    assert_eq!(cargo::profile_dirs(&target).unwrap(), [profile.as_path()]);
    fixture.build(&target);

    // Freshness: replace the final binary's hardlink group (`fx` and `deps/fx-<hash>`).
    fs::copy(profile.join(BIN), profile.join("canon")).unwrap();
    let old_ino = ino(&profile.join(BIN));
    let pass = FnPass {
        lossy: false,
        plan: |p: &[Profile]| replace_by_name(p, "canon", BIN),
    };
    let report = run(&profile, &[&pass], &Options::default());
    assert_eq!(report.passes[0].applied, 1, "{report:?}");
    assert_ne!(ino(&profile.join(BIN)), old_ino);

    fixture.assert_fresh(&target);
}

/// Replaces `member` and `second` with clones of `canon`, and raises `stop` after the first.
struct StopAfterOne<'a> {
    stop: &'a AtomicBool,
}

impl Pass for StopAfterOne<'_> {
    fn name(&self) -> &'static str {
        "test"
    }
    fn plan(&self, profiles: &[Profile]) -> Vec<Action> {
        let mut actions = replace_by_name(profiles, "canon", "member");
        actions.extend(replace_by_name(profiles, "canon", "second"));
        actions
    }
    fn replaced(&self, _replace: &Replace, _new: &Stamp) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[test]
fn a_stop_raised_mid_run_leaves_every_file_old_or_new() {
    // The stop is raised by the first replace; without clones nothing is replaced.
    if !common::filesystem_can(|caps| caps.clone, "share blocks") {
        return;
    }
    let (_tmp, dir) = profile();
    let second = dir.join("deps/second");
    fs::write(&second, CONTENT).unwrap();
    let (member_ino, second_ino) = (ino(&dir.join("deps/member")), ino(&second));
    let stop = AtomicBool::new(false);
    let pass = StopAfterOne { stop: &stop };
    let interrupt = Interrupt {
        stop: Some(&stop),
        ..Interrupt::default()
    };

    let report = run_unbusy(|| {
        engine::run_with(
            std::slice::from_ref(&dir),
            &[&pass],
            &Options::default(),
            &CARGO,
            interrupt,
        )
        .unwrap()
    });

    assert_eq!(report.interrupted, Some(Interrupted::Stopped));
    assert_eq!(report.passes[0].applied, 1, "{report:?}");
    // The first file is new, the second is the old one, and both hold the same bytes.
    assert_ne!(ino(&dir.join("deps/member")), member_ino);
    assert_eq!(ino(&second), second_ino);
    assert_eq!(fs::read(dir.join("deps/member")).unwrap(), CONTENT);
    assert_eq!(fs::read(&second).unwrap(), CONTENT);
    let leftovers = fs::read_dir(dir.join("deps"))
        .unwrap()
        .filter(|entry| {
            let name = entry.as_ref().unwrap().file_name();
            name.to_string_lossy().starts_with(TMP_PREFIX)
        })
        .count();
    assert_eq!(leftovers, 0, "no temp file is left behind");
}

/// Whether `a` and `b` were scanned as one inode.
fn one_inode(profiles: &[Profile], a: &str, b: &str) -> bool {
    profiles.iter().flat_map(|p| &p.inodes).any(|inode| {
        let named = |name: &str| inode.paths.iter().any(|p| p.file_name().unwrap() == name);
        named(a) && named(b)
    })
}

/// Links `second` to `canon` only once `member` is linked to it: work the second pass of a
/// round makes for the first pass of the next.
fn chained_passes() -> (impl Pass, impl Pass) {
    let after = FnPass {
        lossy: false,
        plan: |p: &[Profile]| {
            if one_inode(p, "canon", "member") && !one_inode(p, "canon", "second") {
                share_by_name(p, "canon", "second", Share::Link)
            } else {
                Vec::new()
            }
        },
    };
    let first = FnPass {
        lossy: false,
        plan: |p: &[Profile]| {
            if one_inode(p, "canon", "member") {
                Vec::new()
            } else {
                share_by_name(p, "canon", "member", Share::Link)
            }
        },
    };
    (after, first)
}

/// A profile dir with three equal files of one mode, each its own inode.
fn three_equal_files() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().canonicalize().unwrap().join("debug");
    fs::create_dir_all(&dir).unwrap();
    File::create(dir.join(LOCK_FILE)).unwrap();
    for name in ["canon", "member", "second"] {
        fs::write(dir.join(name), CONTENT).unwrap();
    }
    (tmp, dir)
}

#[test]
fn until_settled_runs_again_while_a_round_applies_anything() {
    let (_tmp, dir) = three_equal_files();
    let (after, first) = chained_passes();
    let opts = Options {
        until_settled: true,
        ..Options::default()
    };

    let report = run(&dir, &[&after, &first], &opts);

    assert_eq!(ino(&dir.join("second")), ino(&dir.join("canon")));
    assert_eq!(report.rounds, 3, "two that apply, one that finds nothing");
    let applied: Vec<_> = report
        .passes
        .iter()
        .map(|p| (p.planned, p.applied))
        .collect();
    assert_eq!(applied, [(1, 1), (1, 1)], "summed over the rounds");
}

#[test]
fn one_round_without_until_settled_and_on_a_dry_run() {
    for opts in [
        Options::default(),
        Options {
            until_settled: true,
            dry_run: true,
            ..Options::default()
        },
    ] {
        let (_tmp, dir) = three_equal_files();
        let (after, first) = chained_passes();

        let report = run(&dir, &[&after, &first], &opts);

        assert_eq!(report.rounds, 1, "{opts:?}");
        assert_ne!(
            ino(&dir.join("second")),
            ino(&dir.join("canon")),
            "{opts:?}"
        );
    }
}
