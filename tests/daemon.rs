//! `swarfr daemon`, through the binary, on fixture targets. `daemon install` is only ever
//! printed here: written, it would start a real agent on the machine running the tests.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;
use swarfr::session::LOSSY_PASSES;
use tempfile::TempDir;

mod common;
use common::{fake_target, filesystem_can, swarfr};

const KIB: usize = 64;
const DAYS: u64 = 2;

struct Fixture {
    _tmp: TempDir,
    home: PathBuf,
    root: PathBuf,
    config: PathBuf,
    index: PathBuf,
}

/// Two targets with an equal artifact, built two days ago, and a config naming their root.
fn fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let (home, root) = (base.join("home"), base.join("src"));
    fs::create_dir_all(&home).unwrap();
    for name in ["a", "b"] {
        let profile = fake_target(&root, name, KIB, DAYS);
        // The artifact too, not only the profile's top level: older than min-age.
        common::set_mtime(
            &profile.join("deps/libx.rlib"),
            SystemTime::now() - Duration::from_secs(DAYS * 24 * 3600),
        );
    }
    let config = base.join("config.toml");
    fs::write(
        &config,
        format!(
            "roots = [\"{}\"]\nacross-families = true\n",
            common::toml_basic(&root)
        ),
    )
    .unwrap();
    Fixture {
        index: base.join("state/hashes.bin"),
        _tmp: tmp,
        home,
        root,
        config,
    }
}

impl Fixture {
    /// `daemon run --once`; its stderr.
    fn once(&self) -> String {
        let out = swarfr(&self.home)
            .env("HOME", &self.home)
            .args(["daemon", "run", "--once", "--config"])
            .arg(&self.config)
            .arg("--index")
            .arg(&self.index)
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "{said}");
        said
    }

    fn state(&self) -> Value {
        let text = fs::read_to_string(self.index.with_file_name("daemon.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    fn profile(&self, name: &str) -> PathBuf {
        self.root.join(name).join("target/debug")
    }
}

fn unit<'a>(state: &'a Value, dir: &Path) -> &'a Value {
    state["units"]
        .as_array()
        .unwrap()
        .iter()
        .find(|unit| unit["dir"] == dir.to_str().unwrap())
        .unwrap_or_else(|| panic!("{} not in {state}", dir.display()))
}

fn pass_names(state: &Value) -> Vec<String> {
    state["last_run"]["passes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pass| pass["name"].as_str().unwrap().to_owned())
        .collect()
}

/// A file written into `profile`, as a build would, `age` ago.
fn build(profile: &Path, age: Duration) {
    let path = profile.join("deps/new.rlib");
    fs::write(&path, vec![2; KIB * 1024]).unwrap();
    for path in [path, profile.join("deps")] {
        common::set_mtime(&path, SystemTime::now() - age);
    }
}

#[test]
fn cold_build_dirs_are_visited_once_per_build() {
    let fx = fixture();

    let said = fx.once();
    assert!(said.contains("2 build dirs have gone cold"), "{said}");
    let state = fx.state();
    for name in ["a", "b"] {
        let unit = unit(&state, &fx.profile(name));
        assert_eq!(
            unit["visited_build_unix"], unit["last_built_unix"],
            "{state}"
        );
    }
    if filesystem_can(|caps| caps.clone, "dedupe") {
        let dedupe = &state["last_run"]["passes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|pass| pass["name"] == "dedupe")
            .unwrap()["applied"];
        assert_eq!(dedupe, 1, "{state}");
    }
    let first_run = state["last_run"]["started_unix"].clone();

    // Nothing built since: nothing to do.
    let said = fx.once();
    assert!(!said.contains("gone cold"), "{said}");
    assert_eq!(fx.state()["last_run"]["started_unix"], first_run);

    // A build just now: pending, but warm until min-age has passed.
    build(&fx.profile("a"), Duration::ZERO);
    let said = fx.once();
    assert!(!said.contains("gone cold"), "{said}");
    let state = fx.state();
    let a = unit(&state, &fx.profile("a"));
    let (built, due) = (
        a["last_built_unix"].as_u64().unwrap(),
        a["due_unix"].as_u64().unwrap(),
    );
    assert_eq!(due, built + 3600, "{state}");
    assert!(state["next_wake_unix"].as_u64().unwrap() <= due);

    // The same build, gone cold: one more visit, of that dir alone.
    build(&fx.profile("a"), Duration::from_secs(2 * 3600));
    let said = fx.once();
    assert!(said.contains("1 build dirs have gone cold"), "{said}");
    let state = fx.state();
    let a = unit(&state, &fx.profile("a"));
    assert_eq!(a["visited_build_unix"], a["last_built_unix"]);
}

#[test]
fn a_build_holding_its_lock_is_not_waited_for_and_stays_pending() {
    let fx = fixture();
    let lock = File::open(fx.profile("a").join(".cargo-lock")).unwrap();
    lock.try_lock().unwrap();

    let started = Instant::now();
    fx.once();
    let took = started.elapsed();

    assert!(took < Duration::from_secs(30), "{took:?}");
    let state = fx.state();
    let a = unit(&state, &fx.profile("a"));
    assert_eq!(a["visited_build_unix"], Value::Null, "{state}");
    assert!(state["last_run"]["left_busy"].as_bool().unwrap());
    let busy = &state["last_run"]["busy"];
    assert!(
        busy.as_array()
            .unwrap()
            .contains(&Value::from(fx.profile("a").to_str().unwrap())),
        "{state}"
    );
    let b = unit(&state, &fx.profile("b"));
    assert_eq!(b["visited_build_unix"], b["last_built_unix"]);

    // The build is over: the next look takes it.
    drop(lock);
    let said = fx.once();
    assert!(said.contains("1 build dirs have gone cold"), "{said}");
}

#[test]
fn no_lossy_pass_runs_unless_the_config_enables_it() {
    let fx = fixture();
    fx.once();
    let names = pass_names(&fx.state());
    assert!(!names.is_empty());
    assert!(
        names
            .iter()
            .all(|name| !LOSSY_PASSES.contains(&name.as_str())),
        "{names:?}"
    );
}

#[test]
fn a_config_without_roots_is_refused() {
    let fx = fixture();
    fs::write(&fx.config, "").unwrap();
    swarfr(&fx.home)
        .env("HOME", &fx.home)
        .args(["daemon", "run", "--once", "--config"])
        .arg(&fx.config)
        .arg("--index")
        .arg(&fx.index)
        .assert()
        .failure()
        .stderr(predicates::str::contains("names none"));
}

#[test]
fn status_reads_the_state_file() {
    let fx = fixture();
    let status = || {
        swarfr(&fx.home)
            .env("HOME", &fx.home)
            .args(["daemon", "status", "--index"])
            .arg(&fx.index)
            .assert()
            .success()
    };
    status().stdout(predicates::str::contains("the daemon has not run"));
    fx.once();
    status()
        .stdout(predicates::str::contains(
            "2 build dirs known, 0 built since",
        ))
        .stdout(predicates::str::contains("last run"));
}

#[test]
fn install_prints_a_unit_that_runs_this_binary() {
    let fx = fixture();
    let out = swarfr(&fx.home)
        .env("HOME", &fx.home)
        .args(["daemon", "install", "--print", "--config"])
        .arg(&fx.config)
        .output()
        .unwrap();
    if cfg!(not(any(target_os = "macos", target_os = "linux"))) {
        assert!(!out.status.success());
        return;
    }
    let unit = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{unit}");
    assert!(unit.contains(env!("CARGO_BIN_EXE_swarfr")), "{unit}");
    assert!(unit.contains(fx.config.to_str().unwrap()), "{unit}");
    // Printed only: nothing written under HOME.
    assert_eq!(fs::read_dir(&fx.home).unwrap().count(), 0);
}
