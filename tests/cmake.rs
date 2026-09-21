//! CMake build dirs: fixtures for what the adapter claims, and, where `cmake` and a C compiler
//! are installed, a real project in a temp dir.
#![cfg(unix)]

use std::cell::RefCell;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use dunnage::compress::Compress;
use dunnage::dedupe::Dedupe;
use dunnage::eco::cmake::CMAKE;
use dunnage::eco::{self, Ecosystem, Guard};
use dunnage::engine::{self, Options};
use dunnage::index::HashIndex;
use dunnage::inventory;
use tempfile::TempDir;
use walkdir::WalkDir;

mod common;
use common::allocated_bytes;

/// Past the quiet tier's one-day floor.
const TWO_DAYS: Duration = Duration::from_secs(2 * 24 * 60 * 60);

fn root() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    (tmp, root)
}

/// A build dir configured from `source`, as far as the adapter reads one.
fn build_dir(dir: &Path, source: &Path) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let cache = format!(
        "# This is the CMakeCache file.\nCMAKE_BUILD_TYPE:STRING=Debug\n\
         CMAKE_HOME_DIRECTORY:INTERNAL={}\n",
        source.display()
    );
    fs::write(dir.join("CMakeCache.txt"), cache).unwrap();
    dir.to_path_buf()
}

#[test]
fn a_build_dir_is_claimed_and_owned_by_the_source_dir_its_cache_names() {
    let (_tmp, root) = root();
    let source = root.join("src/app");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("CMakeLists.txt"), "project(app C)\n").unwrap();
    // Anywhere at all: here, a sibling of the source tree.
    let build = build_dir(&root.join("build/app-debug"), &source);

    let found: Vec<(PathBuf, &str)> = eco::discover(std::slice::from_ref(&root))
        .into_iter()
        .map(|(dir, eco)| (dir, eco.name()))
        .collect();
    assert_eq!(found, [(build.clone(), "cmake")]);
    assert_eq!(CMAKE.owner(&build).unwrap().project, source);
    assert_eq!(CMAKE.units(&build).unwrap(), std::slice::from_ref(&build));
    assert_eq!(CMAKE.guard(&build), Guard::Quiet);
    assert!(!CMAKE.policy().share.links(true));

    let inventory = inventory::inventory(std::slice::from_ref(&root)).unwrap();
    assert!(!inventory.targets[0].project_gone);
    fs::remove_dir_all(&source).unwrap();
    let inventory = inventory::inventory(std::slice::from_ref(&root)).unwrap();
    assert!(inventory.targets[0].project_gone, "the source tree is gone");
}

#[test]
fn an_in_source_build_is_not_a_build_dir() {
    let (_tmp, root) = root();
    let source = root.join("app");
    build_dir(&source, &source);
    // A build dir holding the source tree is no better.
    let outer = root.join("outer");
    build_dir(&outer, &outer.join("src"));

    assert!(eco::discover(std::slice::from_ref(&root)).is_empty());
    assert!(CMAKE.units(&source).is_err());
}

// A real project, when `cmake` and a C compiler are installed.

fn cmake(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("cmake")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap()
}

fn available(tool: &str) -> bool {
    let found = Command::new(tool)
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    if !found {
        eprintln!("skipped: no `{tool}` on this machine");
    }
    found
}

/// Two executables from the same sources with debuginfo, configured into `build/` with
/// `generator` and built, then every file moved back two days, sources and outputs alike, past
/// the quiet floor.
fn project(root: &Path, generator: &str) -> PathBuf {
    let source = root.join("app");
    fs::create_dir_all(&source).unwrap();
    fs::write(
        source.join("CMakeLists.txt"),
        "cmake_minimum_required(VERSION 3.10)\nproject(app C)\n\
         set(CMAKE_BUILD_TYPE Debug)\n\
         add_executable(app main.c words.c)\nadd_executable(twin main.c words.c)\n",
    )
    .unwrap();
    fs::write(
        source.join("main.c"),
        "#include <stdio.h>\nconst char *word(int);\n\
         int main(void) { puts(word(7)); return 0; }\n",
    )
    .unwrap();
    let words: String = (0..400)
        .map(|i| format!("static const char w{i}[] = \"the same words again {i}\";\n"))
        .collect();
    let table: String = (0..400).map(|i| format!("w{i},")).collect();
    fs::write(
        source.join("words.c"),
        format!("{words}static const char *all[] = {{{table}}};\nconst char *word(int i) {{ return all[i]; }}\n"),
    )
    .unwrap();
    let build = root.join("build");
    let configured = cmake(root, &["-S", "app", "-B", "build", "-G", generator]);
    assert!(configured.status.success(), "{configured:?}");
    let built = cmake(root, &["--build", "build"]);
    assert!(built.status.success(), "{built:?}");
    for entry in WalkDir::new(root) {
        let entry = entry.unwrap();
        if entry.file_type().is_file() {
            let modified = entry.metadata().unwrap().modified().unwrap();
            File::options()
                .write(true)
                .open(entry.path())
                .unwrap()
                .set_modified(modified - TWO_DAYS)
                .unwrap();
        }
    }
    build
}

/// Whether `cmake --build` compiles or links anything, by what the Makefiles generator prints.
fn builds_anything(root: &Path) -> bool {
    let built = cmake(root, &["--build", "build"]);
    let said = String::from_utf8_lossy(&built.stdout);
    assert!(built.status.success(), "{said}");
    assert!(said.contains("Built target app"), "{said}");
    said.contains("Building C object") || said.contains("Linking C executable")
}

#[test]
fn after_compress_and_dedupe_cmake_builds_nothing_and_the_binaries_run() {
    if !available("cmake") {
        return;
    }
    let (_tmp, root) = root();
    let build = project(&root, "Unix Makefiles");
    assert!(
        !builds_anything(&root),
        "the control: a second build is a no-op"
    );
    let before = allocated_bytes(&build);

    run_passes(&build, before);
    // The oracle: nothing to compile or link, and both binaries still work.
    assert!(!builds_anything(&root), "rebuilt after the passes");
    binaries_run(&build);
    // And the oracle can say no: a new mtime on a source is enough for a build.
    File::options()
        .write(true)
        .open(root.join("app/words.c"))
        .unwrap()
        .set_modified(SystemTime::now())
        .unwrap();
    assert!(builds_anything(&root));
}

/// Compress and dedupe on the build dir, which must shrink where compression is possible.
fn run_passes(build: &Path, before: u64) {
    let index = RefCell::new(HashIndex::default());
    let compress = Compress::new(&index);
    let dedupe = Dedupe::new(&index);
    let units = CMAKE.units(build).unwrap();
    let report = engine::run(&units, &[&compress, &dedupe], &Options::default(), &CMAKE).unwrap();

    assert!(report.busy.is_empty(), "{report:?}");
    assert_eq!(report.quiet, units);
    if dunnage::sys::caps(build).compress {
        assert!(report.passes[0].applied > 0, "{report:?}");
        if dunnage::sys::ALLOCATED_SHOWS_COMPRESSION {
            assert!(allocated_bytes(build) < before, "{before}");
        }
    }
}

fn binaries_run(build: &Path) {
    for binary in ["app", "twin"] {
        let ran = Command::new(build.join(binary)).output().unwrap();
        assert!(ran.status.success(), "{ran:?}");
        assert_eq!(
            String::from_utf8_lossy(&ran.stdout),
            "the same words again 7\n"
        );
    }
}

/// What `ninja -n` would run in `build`: nothing is `ninja: no work to do.`
fn ninja_plans_nothing(build: &Path) -> bool {
    let planned = Command::new("ninja")
        .args(["-n", "-d", "explain"])
        .current_dir(build)
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&planned.stdout);
    assert!(planned.status.success(), "{planned:?}");
    said.contains("no work to do")
}

#[test]
fn after_compress_and_dedupe_ninja_plans_nothing_and_the_binaries_run() {
    if !available("cmake") || !available("ninja") {
        return;
    }
    let (_tmp, root) = root();
    let build = project(&root, "Ninja");
    assert!(ninja_plans_nothing(&build), "the control");
    let before = allocated_bytes(&build);

    run_passes(&build, before);

    assert!(
        ninja_plans_nothing(&build),
        "ninja would build after the passes"
    );
    binaries_run(&build);
    // And the oracle can say no.
    File::options()
        .write(true)
        .open(root.join("app/words.c"))
        .unwrap()
        .set_modified(SystemTime::now())
        .unwrap();
    assert!(!ninja_plans_nothing(&build));
}
