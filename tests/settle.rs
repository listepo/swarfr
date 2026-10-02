//! `run` repeats its passes until a round finds nothing, so a second `run` has nothing to do.

use std::path::Path;

use tempfile::TempDir;

mod common;
use common::{Fixture, swarfr};

/// `run --json` with every file old enough, over the fixture's workspace.
///
/// Two targets of one workspace, compared across families so dedupe has pairs to clone. The
/// fixture is too small to reproduce the 46 actions a second run found on the benchmark
/// workspace (`docs/bench.md`); `tests/engine.rs` shows the rounds themselves, this that a whole
/// run ends settled and fresh.
fn run(fx: &Fixture, index: &Path, config_home: &Path, extra: &[&str]) -> serde_json::Value {
    let out = swarfr(config_home)
        .args(["run", "--json", "--min-age", "0", "--index"])
        .arg(index)
        .args(extra)
        .arg(&fx.root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// Every pass of every group, as `(name, planned)`.
fn planned(report: &serde_json::Value) -> Vec<(String, u64)> {
    let mut planned = Vec::new();
    for group in report["groups"].as_array().unwrap() {
        for pass in group["passes"].as_array().unwrap() {
            let name = pass["name"].as_str().unwrap().to_string();
            planned.push((name, pass["planned"].as_u64().unwrap()));
        }
    }
    planned
}

#[test]
fn one_run_leaves_nothing_for_the_next() {
    let fx = Fixture::new();
    let (target, second) = (fx.target(), fx.root.join("ws/target-b"));
    fx.build(&target);
    fx.build(&second);
    let state = TempDir::new().unwrap();
    let index = state.path().join("hashes.bin");

    run(&fx, &index, state.path(), &["--across-families"]);
    let again = run(
        &fx,
        &index,
        state.path(),
        &["--across-families", "--dry-run"],
    );

    let left: Vec<_> = planned(&again)
        .into_iter()
        .filter(|(_, planned)| *planned > 0)
        .collect();
    assert_eq!(left, [] as [(String, u64); 0], "{again:#}");
    fx.assert_fresh(&target);
    fx.assert_fresh(&second);
}
