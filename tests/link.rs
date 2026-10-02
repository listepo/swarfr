//! Sharing where the filesystem cannot clone (`T22`).
//!
//! Two questions, and each is asked on whatever filesystem the test is standing on. Where blocks
//! can be shared nothing here changes — `dedupe` clones as it always did, and `--link-artifacts`
//! is a flag with nothing to do. Where they cannot, the cargo home's unpacked sources become one
//! inode without anyone asking, and build artifacts do so only when asked.
//!
//! Point `TMPDIR` at a mount to choose the side, as in `tests/caps.rs`:
//!
//! ```sh
//! TMPDIR=/mnt/btrfs cargo test --test link   # clones; the flag changes nothing
//! TMPDIR=/mnt/ext4  cargo test --test link   # hardlinks, and only where they are safe
//! ```

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use common::{Fixture, ino, swarfr};
use swarfr::sys;

mod common;

/// Above the dedupe pass's floor.
const TWIN: &[u8] = &[7; 64 * 1024];

/// A fake cargo home holding one crate in two versions whose `lib.rs` never changed — the
/// commonest duplicate there is. The machine's own `~/.cargo` is never the subject of a test.
fn twin_home() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().canonicalize().unwrap().join("cargo-home");
    let registry = home.join("registry/src/index.crates.io-1949cf8c6b5b557f");
    let mut twins = Vec::new();
    for version in ["serde-1.0.0", "serde-1.0.1"] {
        let crate_dir = registry.join(version);
        fs::create_dir_all(crate_dir.join("src")).unwrap();
        // What cargo writes last when it has finished extracting; its absence is what makes
        // cargo extract the crate again.
        fs::write(crate_dir.join(".cargo-ok"), "{\"v\":1}").unwrap();
        let file = crate_dir.join("src/lib.rs");
        fs::write(&file, TWIN).unwrap();
        twins.push(file);
    }
    File::create(home.join(".package-cache")).unwrap();
    (tmp, home)
}

fn twins_of(home: &Path) -> (PathBuf, PathBuf) {
    let registry = home.join("registry/src/index.crates.io-1949cf8c6b5b557f");
    (
        registry.join("serde-1.0.0/src/lib.rs"),
        registry.join("serde-1.0.1/src/lib.rs"),
    )
}

/// `run --pass dedupe` over a cargo home, with a config home of its own.
fn run_home(home: &Path, extra: &[&str]) -> serde_json::Value {
    let parent = home.parent().unwrap();
    let config_home = parent.join("config-home");
    fs::create_dir_all(&config_home).unwrap();
    let out = swarfr(&config_home)
        .args(["run", "--pass", "dedupe", "--cargo-home"])
        .arg(home)
        .args(extra)
        .args(["--min-age", "0", "--json", "--index"])
        .arg(parent.join("index.bin"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// `run --pass dedupe` over a target dir.
fn run_target(fixture: &Fixture, extra: &[&str]) {
    let config_home = fixture.root.join("config-home");
    fs::create_dir_all(&config_home).unwrap();
    let out = swarfr(&config_home)
        .args(["run", "--pass", "dedupe"])
        .args(extra)
        .args(["--min-age", "0", "--index"])
        .arg(fixture.root.join("index.bin"))
        .arg(&fixture.root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn planned(report: &serde_json::Value) -> u64 {
    report["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| group["passes"].as_array().unwrap())
        .filter(|pass| pass["name"] == "dedupe")
        .map(|pass| pass["planned"].as_u64().unwrap())
        .sum()
}

/// Two identical crate sources are shared with no flag at all, because cargo replaces a source
/// dir instead of rewriting the files in it. How they are shared is the filesystem's business:
/// a clone where that exists, one inode where it does not.
#[test]
fn cargo_home_sources_are_shared_without_a_flag() {
    let (_tmp, home) = twin_home();
    let (a, b) = twins_of(&home);
    let before = (ino(&a), ino(&b));

    let report = run_home(&home, &[]);

    assert!(planned(&report) >= 1, "{report}");
    assert_eq!(ino(&a), before.0, "the canonical is not touched");
    assert_ne!(ino(&b), before.1, "the twin was replaced");
    if !sys::caps(&home).clone {
        assert_eq!(ino(&a), ino(&b), "with no clone to make, one inode is left");
    }
    assert_eq!(fs::read(&a).unwrap(), TWIN);
    assert_eq!(fs::read(&b).unwrap(), TWIN);
    assert!(
        home.join("registry/src/index.crates.io-1949cf8c6b5b557f/serde-1.0.1/.cargo-ok")
            .is_file(),
        "cargo's own marker is none of our business"
    );
}

/// The A/B for `--link-artifacts`, and the flag is the only difference between the two runs.
///
/// On a filesystem that shares blocks the flag changes nothing — the twin is cloned either way,
/// which is better than a link and needs no permission. On one that does not, the control run
/// leaves the artifacts exactly as it found them and only the treatment shares them, because a
/// rebuild rewrites a linked artifact under every name it has.
#[test]
fn ab_artifacts_are_shared_only_when_the_flag_is_given() {
    let sharing = sys::caps(&std::env::temp_dir()).clone;
    let plant = |fixture: &Fixture| {
        let deps = fixture.target().join("debug/deps");
        let (a, b) = (deps.join("libtwin-a.rlib"), deps.join("libtwin-b.rlib"));
        fs::write(&a, TWIN).unwrap();
        fs::write(&b, TWIN).unwrap();
        (a, b)
    };

    let control = Fixture::new();
    control.build(&control.target());
    let (control_a, control_b) = plant(&control);
    let control_before = (ino(&control_a), ino(&control_b));

    let treatment = Fixture::new();
    treatment.build(&treatment.target());
    let (treatment_a, treatment_b) = plant(&treatment);
    let treatment_before = (ino(&treatment_a), ino(&treatment_b));

    run_target(&control, &[]);
    run_target(&treatment, &["--link-artifacts"]);

    assert_eq!(
        (ino(&treatment_a), fs::read(&treatment_a).unwrap()),
        (treatment_before.0, TWIN.to_vec()),
        "the canonical is not touched"
    );
    assert_ne!(ino(&treatment_b), treatment_before.1, "the twin was shared");
    assert_eq!(fs::read(&treatment_b).unwrap(), TWIN);

    if sharing {
        // Nothing to permit: a clone is what both runs get, flag or no flag.
        assert_ne!(ino(&control_b), control_before.1);
        assert_ne!(
            ino(&treatment_a),
            ino(&treatment_b),
            "a clone keeps its own inode"
        );
    } else {
        assert_eq!(
            (ino(&control_a), ino(&control_b)),
            control_before,
            "without the flag, artifacts are left alone where they cannot be cloned"
        );
        assert_eq!(
            ino(&treatment_a),
            ino(&treatment_b),
            "with the flag, the twin is one inode with its source"
        );
    }
    // The hazard the flag documents is a later build's; this run must not cost one.
    control.assert_fresh(&control.target());
    treatment.assert_fresh(&treatment.target());
}
