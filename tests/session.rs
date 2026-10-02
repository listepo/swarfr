//! The library boundary: whole runs driven through `Session`, with no binary involved.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use swarfr::Error;
use swarfr::engine::{Interrupted, Report};
use swarfr::session::{Control, Observer, Request, Session, Settings};
use tempfile::TempDir;

mod common;
use common::{Fixture, fake_target};

/// A session whose index and run lock live in `state`, never under the real `$HOME`.
fn session(state: &Path) -> Session {
    Session::open(Settings {
        index: state.join("hashes.bin"),
        ..Settings::default()
    })
}

/// Every lossless pass, on everything regardless of age.
fn lossless(roots: &[PathBuf]) -> Request {
    Request {
        roots: roots.to_vec(),
        min_age: Some(Duration::ZERO),
        ..Request::default()
    }
}

/// Remembers the groups it was told about, in order.
#[derive(Default)]
struct Groups(RefCell<Vec<PathBuf>>);

impl Observer for Groups {
    fn group(&self, group: &Path) {
        self.0.borrow_mut().push(group.to_path_buf());
    }
}

#[test]
fn a_whole_run_goes_through_the_session_and_leaves_the_build_fresh() {
    let fx = Fixture::new();
    let target = fx.target();
    fx.build(&target);
    let state = TempDir::new().unwrap();
    let session = session(state.path());
    let groups = Groups::default();
    let control = Control {
        observer: &groups,
        ..Control::default()
    };

    let report = session
        .apply(&lossless(std::slice::from_ref(&fx.root)), &control)
        .unwrap();

    assert_eq!(groups.0.borrow().len(), 1, "one family, one group");
    assert_eq!(report.groups.len(), 1);
    let (_, group) = &report.groups[0];
    let passes: Vec<_> = group.passes.iter().map(|pass| pass.name).collect();
    assert_eq!(passes, ["compress", "dedupe"], "the lossy ones stay off");
    assert!(!report.left_busy && !report.stopped);
    assert!(
        state.path().join("hashes.bin").is_file(),
        "the index is saved"
    );
    fx.assert_fresh(&target);
}

/// While one session works, it calls another: that one must be refused, not interleaved.
struct Intruder<'a> {
    other: &'a Session,
    request: &'a Request,
    refused: Cell<Option<bool>>,
}

impl Observer for Intruder<'_> {
    fn group(&self, _group: &Path) {
        let result = self.other.apply(self.request, &Control::default());
        self.refused
            .set(Some(matches!(result, Err(Error::RunLockHeld(_)))));
    }
}

#[test]
fn two_sessions_on_one_index_are_kept_apart_by_the_run_lock() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    fake_target(&root, "a", 16, 0);
    let state = TempDir::new().unwrap();
    let (first, second) = (session(state.path()), session(state.path()));
    let request = lossless(&[root]);
    let intruder = Intruder {
        other: &second,
        request: &request,
        refused: Cell::new(None),
    };
    let control = Control {
        observer: &intruder,
        ..Control::default()
    };

    first.apply(&request, &control).unwrap();

    assert_eq!(intruder.refused.get(), Some(true));
    // Once the first one is done, the second one runs.
    second.apply(&request, &Control::default()).unwrap();
}

#[test]
fn the_cli_exits_2_when_another_run_holds_the_lock() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    fake_target(&root, "a", 16, 0);
    let state = TempDir::new().unwrap();
    let index = state.path().join("hashes.bin");
    let held = Settings {
        index: index.clone(),
        ..Settings::default()
    }
    .run_lock();
    let lock = std::fs::File::create(&held).unwrap();
    lock.lock().unwrap();

    common::swarfr(tmp.path())
        .args(["run", "--index"])
        .arg(&index)
        .arg(&root)
        .assert()
        .code(2)
        .stderr(predicates::str::contains("another run of swarfr holds"));
}

/// Raises the stop flag once the first group is done.
struct StopAfterFirst<'a> {
    stop: &'a AtomicBool,
}

impl Observer for StopAfterFirst<'_> {
    fn report(&self, _group: &Path, _report: &Report) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[test]
fn a_stop_leaves_the_groups_not_yet_started_alone() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    // No repository: each target is a family of its own, so two groups.
    fake_target(&root, "a", 16, 0);
    fake_target(&root, "b", 16, 0);
    let state = TempDir::new().unwrap();
    let stop = AtomicBool::new(false);
    let observer = StopAfterFirst { stop: &stop };
    let control = Control {
        observer: &observer,
        stop: Some(&stop),
        ..Control::default()
    };

    let report = session(state.path())
        .apply(&lossless(std::slice::from_ref(&root)), &control)
        .unwrap();

    assert!(report.stopped);
    assert_eq!(report.groups.len(), 1, "{:?}", report.groups);
    assert!(report.groups[0].0.starts_with(root.join("a")));
}

#[test]
fn a_group_out_of_lock_budget_lets_go_and_is_visited_once_more() {
    // The action to stop in front of is a dedupe, which needs clones.
    if !common::filesystem_can(|caps| caps.clone, "share blocks") {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    // Equal artifacts in one group, so there is always an action to stop in front of.
    fake_target(&root, "a", 16, 0);
    fake_target(&root, "b", 16, 0);
    let state = TempDir::new().unwrap();
    let request = Request {
        across_families: true,
        ..lossless(&[root])
    };
    let control = Control {
        lock_budget: Some(Duration::ZERO),
        ..Control::default()
    };

    let report = session(state.path()).apply(&request, &control).unwrap();

    assert_eq!(report.groups.len(), 2, "the first visit and the return");
    for (_, group) in &report.groups {
        assert_eq!(group.interrupted, Some(Interrupted::OutOfBudget));
        assert!(group.passes.iter().all(|pass| pass.applied == 0));
    }
    assert!(report.left_busy, "what is left is said, like a busy build");
}
