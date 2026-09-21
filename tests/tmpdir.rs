//! The `tmpdir` pass on temp dirs standing in for `$TMPDIR`. Nothing here looks at the real one.
#![cfg(unix)]

use std::fs::{self, File};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime};

use dunnage::session::{Request, Session, Settings};
use dunnage::sys;
use dunnage::tmpdir::{self, Kept};
use tempfile::TempDir;
use walkdir::WalkDir;

const DAY: Duration = Duration::from_secs(24 * 60 * 60);
const IDLE_DAYS: u64 = 7;

fn root() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    (tmp, root)
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text.repeat(1000)).unwrap();
}

/// Everything under `path`, contents before their dir, moved back `days`: a dir's mtime changes
/// when an entry is made in it, not when a file in it is touched. A symlink's or a socket's own
/// time is set by `touch -h`: std cannot, and a socket does not open.
fn age(path: &Path, days: u32) {
    let when = SystemTime::now() - DAY * days;
    let reference = path.with_extension("reference");
    File::create(&reference)
        .unwrap()
        .set_modified(when)
        .unwrap();
    for entry in WalkDir::new(path).contents_first(true) {
        let entry = entry.unwrap();
        let kind = entry.file_type();
        if !kind.is_file() && !kind.is_dir() {
            let touched = Command::new("touch")
                .arg("-h")
                .arg("-r")
                .arg(&reference)
                .arg(entry.path())
                .status()
                .unwrap();
            assert!(touched.success());
        } else {
            File::open(entry.path())
                .unwrap()
                .set_modified(when)
                .unwrap();
        }
    }
    fs::remove_file(reference).unwrap();
    if let Some(parent) = path.parent() {
        // Making and removing the reference moved the parent's mtime; that is the fake tmp dir.
        File::open(parent)
            .unwrap()
            .set_modified(SystemTime::now())
            .unwrap();
    }
}

fn run(dir: &Path, in_use: Option<&[PathBuf]>, keep: &[PathBuf], dry_run: bool) -> tmpdir::Report {
    tmpdir::run(dir, IDLE_DAYS, in_use, keep, SystemTime::now(), dry_run).unwrap()
}

fn removed(report: &tmpdir::Report) -> Vec<&Path> {
    report
        .removed
        .iter()
        .map(|entry| entry.path.as_path())
        .collect()
}

#[test]
fn old_entries_go_and_one_with_anything_young_inside_stays() {
    let (_tmp, dir) = root();
    write(&dir.join("old.log"), "old ");
    write(&dir.join("old/a/b.txt"), "old ");
    write(&dir.join("young/c.txt"), "new ");
    // Deep inside an old tree, one file written within the limit keeps all of it.
    write(&dir.join("deep/x/y/z/fresh.txt"), "new ");
    write(&dir.join("deep/x/stale.txt"), "old ");
    for name in ["old.log", "old", "deep"] {
        age(&dir.join(name), 10);
    }
    let fresh = dir.join("deep/x/y/z/fresh.txt");
    File::open(&fresh)
        .unwrap()
        .set_modified(SystemTime::now() - DAY)
        .unwrap();
    // Just inside the limit is not old either.
    write(&dir.join("six-days/d.txt"), "old ");
    age(&dir.join("six-days"), 6);

    let dry = run(&dir, Some(&[]), &[], true);
    assert_eq!(removed(&dry), [dir.join("old"), dir.join("old.log")]);
    assert!(
        dir.join("old/a/b.txt").is_file(),
        "a dry run removes nothing"
    );
    assert_eq!(dry.young, 3, "{dry:?}");
    assert!(dry.freed_bytes() > 0);
    let newest = dry.removed[0].newest_unix;
    let ten_days_ago = SystemTime::now() - DAY * 10;
    let ten_days_ago = ten_days_ago.duration_since(SystemTime::UNIX_EPOCH).unwrap();
    assert!(newest.abs_diff(ten_days_ago.as_secs()) < 60, "{newest}");

    let done = run(&dir, Some(&[]), &[], false);
    assert_eq!(removed(&done), removed(&dry));
    assert!(done.failed.is_empty() && done.kept.is_empty(), "{done:?}");
    assert!(!dir.join("old").exists() && !dir.join("old.log").exists());
    for kept in [
        "young/c.txt",
        "deep/x/stale.txt",
        "deep/x/y/z/fresh.txt",
        "six-days/d.txt",
    ] {
        assert!(dir.join(kept).is_file(), "{kept}");
    }
}

/// Kills the process when the test ends, whichever way it ends.
struct Held(Child);

impl Drop for Held {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Until `paths_in_use` sees `path`: a process takes a moment to start and open it.
fn in_use_with(dir: &Path, path: &Path) -> Vec<PathBuf> {
    for _ in 0..200 {
        let paths = sys::paths_in_use(dir).expect("this platform reports open files");
        if paths.iter().any(|held| held == path) {
            return paths;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("{} never showed up as in use", path.display());
}

#[test]
fn an_entry_a_process_holds_a_file_or_its_current_dir_in_stays() {
    let (_tmp, dir) = root();
    let log = dir.join("tailed/logs/app.log");
    write(&log, "old ");
    write(&dir.join("worked-in/data.txt"), "old ");
    for name in ["tailed", "worked-in"] {
        age(&dir.join(name), 10);
    }
    let tail = Held(
        Command::new("tail")
            .arg("-f")
            .arg(&log)
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let sleeper = Held(
        Command::new("sleep")
            .arg("60")
            .current_dir(dir.join("worked-in"))
            .spawn()
            .unwrap(),
    );
    in_use_with(&dir, &log);
    let in_use = in_use_with(&dir, &dir.join("worked-in"));

    let report = run(&dir, Some(&in_use), &[], false);
    assert!(report.removed.is_empty(), "{report:?}");
    assert_eq!(
        report.kept,
        [
            (dir.join("tailed"), Kept::InUse),
            (dir.join("worked-in"), Kept::InUse)
        ]
    );

    drop((tail, sleeper));
    let in_use = sys::paths_in_use(&dir).unwrap();
    let report = run(&dir, Some(&in_use), &[], false);
    assert_eq!(
        removed(&report),
        [dir.join("tailed"), dir.join("worked-in")]
    );
}

#[test]
fn a_socket_a_kept_path_or_nothing_known_about_open_files_keeps_an_entry() {
    let (_tmp, dir) = root();
    // A server's socket outlives nothing here, but Linux would not name it as held.
    fs::create_dir_all(dir.join("server")).unwrap();
    drop(UnixListener::bind(dir.join("server/s")).unwrap());
    write(&dir.join("cache/hashes.bin"), "index ");
    write(&dir.join("plain/f.txt"), "old ");
    for name in ["server", "cache", "plain"] {
        age(&dir.join(name), 10);
    }

    let report = run(&dir, None, &[], false);
    assert!(report.unsure && report.removed.is_empty(), "{report:?}");
    assert!(dir.join("plain/f.txt").is_file());

    let report = run(&dir, Some(&[]), &[dir.join("cache/hashes.bin")], false);
    assert_eq!(removed(&report), [dir.join("plain")]);
    assert_eq!(
        report.kept,
        [
            (dir.join("cache"), Kept::Kept),
            (dir.join("server"), Kept::Socket)
        ]
    );
}

#[test]
fn links_go_themselves_and_what_they_point_at_stays() {
    let (_tmp, dir) = root();
    let (_elsewhere_tmp, elsewhere) = root();
    write(&elsewhere.join("keep/me.txt"), "mine ");
    symlink(elsewhere.join("keep"), dir.join("top-link")).unwrap();
    fs::create_dir_all(dir.join("links")).unwrap();
    symlink(elsewhere.join("keep"), dir.join("links/inner")).unwrap();
    // A dir its owner made read-only, as a Go module cache does, goes too.
    write(&dir.join("read-only/mod/go.mod"), "module x ");
    for name in ["top-link", "links", "read-only"] {
        age(&dir.join(name), 10);
    }
    fs::set_permissions(dir.join("read-only/mod"), fs::Permissions::from_mode(0o555)).unwrap();

    let report = run(&dir, Some(&[]), &[], false);
    assert!(report.failed.is_empty(), "{report:?}");
    assert_eq!(
        removed(&report),
        [
            dir.join("links"),
            dir.join("read-only"),
            dir.join("top-link")
        ]
    );
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    assert!(elsewhere.join("keep/me.txt").is_file());
}

fn session(state: &Path) -> Session {
    Session::open(Settings {
        index: state.join("hashes.bin"),
        ..Settings::default()
    })
}

#[test]
fn a_run_cleans_the_temp_dir_it_is_given_without_any_root() {
    let (_tmp, dir) = root();
    let (_state_tmp, state) = root();
    write(&dir.join("left-behind/f.txt"), "old ");
    age(&dir.join("left-behind"), 30);
    let request = Request {
        lossy: vec![tmpdir::NAME.into()],
        tmpdir: Some(dir.clone()),
        tmpdir_idle_days: Some(IDLE_DAYS),
        ..Request::default()
    };

    let plan = session(&state).plan(&request, &Default::default()).unwrap();
    let planned = plan.tmpdir.unwrap();
    assert_eq!(removed(&planned), [dir.join("left-behind")]);
    assert!(dir.join("left-behind").exists());
    let done = session(&state)
        .apply(&request, &Default::default())
        .unwrap();
    assert_eq!(
        removed(done.tmpdir.as_ref().unwrap()),
        [dir.join("left-behind")]
    );
    assert!(!dir.join("left-behind").exists());

    // Named as the only pass, as the daemon runs it.
    write(&dir.join("again/f.txt"), "old ");
    age(&dir.join("again"), 30);
    let only = Request {
        passes: vec![tmpdir::NAME.into()],
        ..request
    };
    let done = session(&state).apply(&only, &Default::default()).unwrap();
    assert_eq!(removed(done.tmpdir.as_ref().unwrap()), [dir.join("again")]);
    assert!(done.groups.is_empty());
}

#[test]
fn the_pass_and_its_days_need_each_other_and_a_real_temp_dir() {
    let (_tmp, dir) = root();
    let (_state_tmp, state) = root();
    let request = Request {
        lossy: vec![tmpdir::NAME.into()],
        tmpdir: Some(dir.clone()),
        tmpdir_idle_days: Some(IDLE_DAYS),
        ..Request::default()
    };
    assert!(request.check().is_ok());
    let no_days = Request {
        tmpdir_idle_days: None,
        ..request.clone()
    };
    assert!(no_days.check().is_err());
    let no_pass = Request {
        lossy: Vec::new(),
        ..request.clone()
    };
    assert!(no_pass.check().is_err());
    let no_dir = Request {
        tmpdir: None,
        ..request.clone()
    };
    assert!(no_dir.check().is_err());
    let root_dir = Request {
        tmpdir: Some(PathBuf::from("/")),
        ..request
    };
    let error = session(&state)
        .plan(&root_dir, &Default::default())
        .unwrap_err()
        .to_string();
    assert!(error.contains("filesystem root"), "{error}");
}

/// macOS keeps per-service dirs in the user's temp dir under `sunlnk`; any flag that forbids a
/// removal keeps the entry, instead of a removal that empties it and then fails. `uchg` is the
/// one a user may set.
#[cfg(target_os = "macos")]
#[test]
fn an_entry_with_a_flag_that_forbids_removal_stays_whole() {
    let (_tmp, dir) = root();
    write(&dir.join("guarded/a.txt"), "old ");
    write(&dir.join("guarded/locked.txt"), "old ");
    age(&dir.join("guarded"), 10);
    let chflags = |flag: &str| {
        let done = Command::new("chflags")
            .arg(flag)
            .arg(dir.join("guarded/locked.txt"))
            .status()
            .unwrap();
        assert!(done.success());
    };
    chflags("uchg");

    let report = run(&dir, Some(&[]), &[], false);
    chflags("nouchg");
    assert_eq!(report.kept, [(dir.join("guarded"), Kept::Protected)]);
    assert!(report.failed.is_empty(), "{report:?}");
    assert!(
        dir.join("guarded/a.txt").is_file(),
        "nothing inside was removed"
    );
}
