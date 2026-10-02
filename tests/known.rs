//! The build dirs of the last walk (`build-dirs-v1.json`): when a run uses them instead of
//! walking the roots, and when it walks again.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use swarfr::known;
use swarfr::session::{Control, Request, RunReport, Session, Settings};
use tempfile::TempDir;

mod common;
use common::fake_target;

struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    state: PathBuf,
}

/// A root with one target at `sub/a`; `sub` is there from the start, so a target added under
/// it later leaves the root's own mtime alone.
fn fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let root = base.join("root");
    fake_target(&root.join("sub"), "a", 16, 0);
    Fixture {
        state: base.join("state"),
        _tmp: tmp,
        root,
    }
}

impl Fixture {
    fn session(&self, every: Duration) -> Session {
        Session::open(Settings {
            index: self.state.join("hashes.bin"),
            rediscover_every: every,
            ..Settings::default()
        })
    }

    /// A dry run of `roots`: one group per target, since none of them has a repository.
    fn plan(&self, roots: &[PathBuf], every: Duration, rediscover: bool) -> RunReport {
        let request = Request {
            roots: roots.to_vec(),
            rediscover,
            ..Request::default()
        };
        self.session(every)
            .plan(&request, &Control::default())
            .unwrap()
    }
}

fn groups(report: &RunReport) -> Vec<&Path> {
    let mut groups: Vec<&Path> = report
        .groups
        .iter()
        .map(|(group, _)| group.as_path())
        .collect();
    groups.sort();
    groups.dedup();
    groups
}

const HOUR: Duration = Duration::from_secs(60 * 60);

#[test]
fn a_new_build_dir_waits_for_the_next_walk_and_a_removed_one_drops_out_at_once() {
    let fx = fixture();
    let roots = [fx.root.clone()];
    let a = fx.root.join("sub/a/target");
    let b = fx.root.join("sub/b/target");

    let first = fx.plan(&roots, HOUR, false);
    assert!(first.walked);
    assert!(fx.state.join(known::FILE).exists());

    fake_target(&fx.root.join("sub"), "b", 16, 0);
    let second = fx.plan(&roots, HOUR, false);
    assert!(!second.walked, "the list still holds");
    assert_eq!(groups(&second), [a.as_path()]);

    let walked = fx.plan(&roots, HOUR, true);
    assert!(walked.walked);
    assert_eq!(groups(&walked), [a.as_path(), b.as_path()]);

    // Gone: its marker says so, no walk needed.
    fs::remove_dir_all(&a).unwrap();
    let after = fx.plan(&roots, HOUR, false);
    assert!(!after.walked);
    assert_eq!(groups(&after), [b.as_path()]);
}

#[test]
fn other_roots_a_moved_root_or_no_cadence_walk_again() {
    let fx = fixture();
    let roots = [fx.root.clone()];
    fx.plan(&roots, HOUR, false);

    assert!(fx.plan(&roots, Duration::ZERO, false).walked, "cadence 0");
    assert!(
        fx.plan(&[fx.root.join("sub")], HOUR, false).walked,
        "other roots"
    );
    // The line above wrote a list for `sub`; this one is for the root again.
    assert!(fx.plan(&roots, HOUR, false).walked, "other roots again");
    assert!(!fx.plan(&roots, HOUR, false).walked);

    // A new entry in the root itself moves its mtime.
    fake_target(&fx.root, "c", 16, 0);
    let moved = fx.plan(&roots, HOUR, false);
    assert!(moved.walked);
    assert_eq!(groups(&moved).len(), 2);
}

#[test]
fn the_list_holds_for_the_cadence_and_no_longer() {
    let fx = fixture();
    let roots = [fx.root.clone()];
    let file = fx.state.join(known::FILE);
    let minute = Duration::from_secs(60);

    let (found, walked) = known::discover(Some(&file), &roots, minute, false, 1000);
    assert!(walked);
    assert_eq!(found.len(), 1);
    assert!(!known::discover(Some(&file), &roots, minute, false, 1059).1);
    assert!(known::discover(Some(&file), &roots, minute, false, 1060).1);
    // Without a file there is nothing to keep: every call walks.
    assert!(known::discover(None, &roots, minute, false, 1060).1);
    // Nor without roots, and that list is not written over this one.
    assert!(known::discover(Some(&file), &[], minute, false, 1070).1);
    assert!(!known::discover(Some(&file), &roots, minute, false, 1070).1);
}
