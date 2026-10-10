//! SwiftPM scratch dirs: fixtures for what the adapter claims and guards, and, where `swift` is
//! installed, real packages in temp dirs. Nothing here points at a real package.
#![cfg(unix)]

use std::cell::RefCell;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use swarfr::compress::Compress;
use swarfr::dedupe::Dedupe;
use swarfr::eco::swiftpm::{self, SWIFTPM};
use swarfr::eco::{self, Ecosystem, Guard};
use swarfr::engine::{self, Options};
use swarfr::index::HashIndex;
use swarfr::model;
use tempfile::TempDir;

mod common;
use common::allocated_bytes;

const KIB: usize = 1024;
/// How long a `swift build` that waits for a lock is given to prove it waits.
const WAIT: Duration = Duration::from_secs(5);

fn root() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    (tmp, root)
}

/// The lock file `swift build` takes for `scratch`, which a test must not leave behind.
struct Lock(PathBuf);

impl Lock {
    fn of(unit: &Path) -> Self {
        let Guard::Shared(file) = SWIFTPM.guard(unit) else {
            panic!("a SwiftPM unit is guarded by a shared lock");
        };
        Self(file)
    }

    /// The lock of the package [`package`] makes under `root`, named before `swift build` makes
    /// it, so a failing build leaves nothing behind either.
    fn before_build(root: &Path) -> Self {
        let scratch = root.join("app").join(swiftpm::SCRATCH);
        Self(swarfr::sys::temp_dir().join(swiftpm::lock_name(&scratch)))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// A scratch dir as both build systems leave it, plus what is not a build output.
fn scratch(root: &Path) -> PathBuf {
    let build = root.join("app/.build");
    fs::create_dir_all(&build).unwrap();
    fs::write(
        root.join("app/Package.swift"),
        "// swift-tools-version:5.9\n",
    )
    .unwrap();
    fs::write(build.join("workspace-state.json"), "{}").unwrap();
    for (file, kib) in [
        ("out/Products/Debug/app", 64),
        ("out/CompilationCache.noindex/generic/v1.1/data.v1", 64),
        ("arm64-apple-macosx/debug/app", 64),
        ("checkouts/dep/Sources/dep.swift", 16),
        ("repositories/dep-1234/objects/pack", 16),
        ("index-build/out/Products/Debug/app", 16),
    ] {
        let path = build.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "a build product ".repeat(kib * KIB / 16)).unwrap();
    }
    build
}

#[test]
fn a_scratch_dir_is_claimed_and_its_build_outputs_are_the_units() {
    let (_tmp, root) = root();
    let build = scratch(&root);
    // Without the state file SwiftPM writes, a `.build` is anybody's.
    fs::create_dir_all(root.join("other/.build/out")).unwrap();

    let found: Vec<(PathBuf, &str)> = eco::discover(std::slice::from_ref(&root))
        .into_iter()
        .map(|(dir, eco)| (dir, eco.name()))
        .collect();
    assert_eq!(found, [(build.clone(), "swiftpm")]);

    let units = SWIFTPM.units(&build).unwrap();
    assert_eq!(
        units,
        [build.join("arm64-apple-macosx"), build.join("out")],
        "checkouts, repositories and index-build are not build outputs of this scratch dir"
    );
    assert_eq!(
        SWIFTPM.owner(&build).unwrap().project,
        root.join("app"),
        "the package dir"
    );
    assert!(SWIFTPM.units(&root.join("other/.build")).is_err());
}

#[test]
fn the_compilation_cache_is_never_scanned() {
    let (_tmp, root) = root();
    let build = scratch(&root);

    let out = model::scan(&build.join("out"), &SWIFTPM).unwrap();

    let paths: Vec<&PathBuf> = out.inodes.iter().flat_map(|inode| &inode.paths).collect();
    assert_eq!(paths, [&build.join("out/Products/Debug/app")]);
}

#[test]
fn all_units_share_one_lock_named_after_the_scratch_dir_in_the_temp_dir() {
    let (_tmp, root) = root();
    let build = scratch(&root);
    let [triple, out] = &SWIFTPM.units(&build).unwrap()[..] else {
        panic!("two units");
    };

    assert_eq!(SWIFTPM.guard(triple), SWIFTPM.guard(out));
    let lock = Lock::of(out);
    assert_eq!(
        lock.0,
        swarfr::sys::temp_dir().join(swiftpm::lock_name(&build))
    );
}

#[test]
fn a_held_lock_makes_every_unit_busy_and_a_missing_one_is_created() {
    let (_tmp, root) = root();
    let build = scratch(&root);
    let units = SWIFTPM.units(&build).unwrap();
    let lock = Lock::of(&units[0]);
    let _ = fs::remove_file(&lock.0);
    let index = RefCell::new(HashIndex::default());
    let compress = Compress::new(&index);
    let dry = Options {
        dry_run: true,
        ..Options::default()
    };

    let free = engine::run(&units, &[&compress], &dry, &SWIFTPM).unwrap();
    assert!(free.busy.is_empty(), "{free:?}");
    assert!(lock.0.exists(), "created, as `swift build` creates it");

    // What `swift build` holds for the length of a build.
    let build_lock = File::options().write(true).open(&lock.0).unwrap();
    build_lock.lock().unwrap();
    let held = engine::run(&units, &[&compress], &dry, &SWIFTPM).unwrap();
    assert_eq!(held.busy, units);
}

// Real packages, when `swift` is installed.

fn swift_available() -> bool {
    let found = Command::new("swift")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !found {
        eprintln!("skipped: no `swift` on this machine");
    }
    found
}

fn swift(package: &Path, args: &[&str]) -> std::process::Output {
    Command::new("swift")
        .current_dir(package)
        .args(args)
        .output()
        .unwrap()
}

/// `swift package init` and one build, in a temp dir. Returns the package dir.
fn package(root: &Path) -> PathBuf {
    let package = root.join("app");
    fs::create_dir_all(&package).unwrap();
    let init = swift(
        &package,
        &["package", "init", "--type", "executable", "--name", "app"],
    );
    assert!(init.status.success(), "{init:?}");
    // Something compress and dedupe find: a source big enough to leave big objects behind.
    let big: String = (0..400)
        .map(|i| format!("let words{i} = \"the same words again\"\n"))
        .collect();
    fs::write(package.join("Sources/app/Words.swift"), big).unwrap();
    let built = swift(&package, &["build"]);
    assert!(built.status.success(), "{built:?}");
    package
}

#[test]
fn swift_build_waits_for_the_lock_the_adapter_names() {
    if !swift_available() {
        return;
    }
    let (_tmp, root) = root();
    let lock = Lock::before_build(&root);
    let package = package(&root);

    let held = File::options().write(true).open(&lock.0).unwrap();
    held.lock().unwrap();
    let mut waiting = Command::new("swift")
        .current_dir(&package)
        .arg("build")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while started.elapsed() < WAIT {
        assert!(
            waiting.try_wait().unwrap().is_none(),
            "`swift build` did not wait for {}",
            lock.0.display()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    held.unlock().unwrap();
    let done = waiting.wait_with_output().unwrap();
    assert!(done.status.success(), "{done:?}");
    // Not the wording of the wait message ("Another instance of SwiftPM is already running ..."
    // up to Swift 6.3, different or absent on 6.4): it is a diagnostic, and it changes between
    // releases. What the lock guards is the behaviour: the build stays blocked while the lock
    // is held (above), and runs to the end once it is released.
    assert!(package.join(".build/debug/app").exists(), "{done:?}");
}

#[test]
fn after_compress_and_dedupe_swift_builds_nothing_and_the_binary_runs() {
    if !swift_available() {
        return;
    }
    let (_tmp, root) = root();
    let lock = Lock::before_build(&root);
    let package = package(&root);
    let build = package.join(".build");
    let units = SWIFTPM.units(&build).unwrap();
    assert_eq!(Lock::of(&units[0]).0, lock.0);
    let before = allocated_bytes(&build);

    let index = RefCell::new(HashIndex::default());
    let mut compress = Compress::new(&index);
    compress.min_age = Duration::ZERO;
    let mut dedupe = Dedupe::new(&index);
    dedupe.min_age = Duration::ZERO;
    let report = engine::run(&units, &[&compress, &dedupe], &Options::default(), &SWIFTPM).unwrap();

    assert!(report.busy.is_empty(), "{report:?}");
    if swarfr::sys::caps(&build).compress {
        assert!(report.passes[0].applied > 0, "{report:?}");
        if swarfr::sys::ALLOCATED_SHOWS_COMPRESSION {
            assert!(allocated_bytes(&build) < before, "{before}");
        }
    }
    // The oracle: the build system sees nothing to do, and what it built still works.
    assert!(!compiles(&package), "rebuilt after the passes");
    let ran = swift(&package, &["run", "--skip-build", "app"]);
    assert!(ran.status.success(), "{ran:?}");
    assert!(String::from_utf8_lossy(&ran.stdout).contains("Hello, world!"));
    // And the oracle can say no: an edited source is compiled.
    let words = package.join("Sources/app/Words.swift");
    let mut source = fs::read_to_string(&words).unwrap();
    source.push_str("let edited = \"a new word\"\n");
    fs::write(&words, source).unwrap();
    assert!(compiles(&package));
}

/// What a compile of a source writes: its dependency and diagnostics files, even when the object
/// comes out byte-identical and is left alone (measured on Swift 6.4); a build with nothing to do
/// writes none of them.
const COMPILE_OUTPUTS: [&str; 4] = ["o", "d", "dia", "swiftdeps"];

/// Whether `swift build` compiles anything: a compile output it writes or rewrites. Not the
/// build's output — which lines name a compile differs between Swift releases, and on the CI
/// images no line of `swift build -v` said "Compil" even for an edited source.
fn compiles(package: &Path) -> bool {
    let outputs = || -> Vec<(PathBuf, std::time::SystemTime)> {
        walkdir::WalkDir::new(package.join(".build"))
            .sort_by_file_name()
            .into_iter()
            .map(Result::unwrap)
            .filter(|entry| {
                let ext = entry.path().extension().and_then(|ext| ext.to_str());
                ext.is_some_and(|ext| COMPILE_OUTPUTS.contains(&ext))
            })
            .map(|entry| {
                let mtime = entry.metadata().unwrap().modified().unwrap();
                (entry.into_path(), mtime)
            })
            .collect()
    };
    let before = outputs();
    assert!(
        !before.is_empty(),
        "no compile outputs under .build to compare"
    );
    let build = swift(package, &["build"]);
    assert!(build.status.success(), "{build:?}");
    outputs() != before
}
