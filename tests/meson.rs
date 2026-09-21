//! Meson build dirs: fixtures for what the adapter claims, and, where `meson`, `ninja` and a C
//! compiler are installed, a real project in a temp dir.
#![cfg(unix)]

use std::cell::RefCell;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use dunnage::compress::Compress;
use dunnage::dedupe::Dedupe;
use dunnage::eco::meson::{COREDATA, INFO, MESON};
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
    fs::create_dir_all(dir.join("meson-private")).unwrap();
    fs::create_dir_all(dir.join("meson-info")).unwrap();
    fs::write(dir.join(COREDATA), b"\x80\x04pickled").unwrap();
    let info = serde_json::json!({
        "directories": {"source": source, "build": dir, "info": dir.join("meson-info")},
    });
    fs::write(dir.join(INFO), info.to_string()).unwrap();
    // Meson tags its build dirs as caches too; cargo must not take this one for a target.
    fs::write(
        dir.join("CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55\n\
         # This file is a cache directory tag created by meson.\n",
    )
    .unwrap();
    dir.to_path_buf()
}

#[test]
fn a_build_dir_is_claimed_and_owned_by_the_source_dir_its_info_names() {
    let (_tmp, root) = root();
    let source = root.join("src/app");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("meson.build"), "project('app', 'c')\n").unwrap();
    let build = build_dir(&root.join("builddir"), &source);

    let found: Vec<(PathBuf, &str)> = eco::discover(std::slice::from_ref(&root))
        .into_iter()
        .map(|(dir, eco)| (dir, eco.name()))
        .collect();
    assert_eq!(found, [(build.clone(), "meson")]);
    assert_eq!(MESON.owner(&build).unwrap().project, source);
    assert_eq!(MESON.units(&build).unwrap(), std::slice::from_ref(&build));
    assert_eq!(MESON.guard(&build), Guard::Quiet);
    assert!(!MESON.policy().share.links(true));

    let inventory = inventory::inventory(std::slice::from_ref(&root)).unwrap();
    assert!(!inventory.targets[0].project_gone);
    fs::remove_dir_all(&source).unwrap();
    let inventory = inventory::inventory(std::slice::from_ref(&root)).unwrap();
    assert!(inventory.targets[0].project_gone, "the source tree is gone");
}

#[test]
fn what_lacks_coredata_or_info_or_holds_its_source_is_not_a_build_dir() {
    let (_tmp, root) = root();
    let source = root.join("app");
    let no_info = build_dir(&root.join("a"), &source);
    fs::remove_file(no_info.join(INFO)).unwrap();
    let no_coredata = build_dir(&root.join("b"), &source);
    fs::remove_file(no_coredata.join(COREDATA)).unwrap();
    let outer = root.join("outer");
    build_dir(&outer, &outer.join("src"));

    assert!(eco::discover(std::slice::from_ref(&root)).is_empty());
    assert!(MESON.units(&outer).is_err());
}

// A real project, when `meson`, `ninja` and a C compiler are installed.

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

/// A static library and two executables with debuginfo, set up into `build/` and built, then
/// every file moved back two days, sources and outputs alike, past the quiet floor.
fn project(root: &Path) -> PathBuf {
    let source = root.join("app");
    fs::create_dir_all(&source).unwrap();
    fs::write(
        source.join("meson.build"),
        "project('app', 'c')\nwords = static_library('words', 'words.c')\n\
         executable('app', 'main.c', link_with: words)\n\
         executable('twin', 'main.c', link_with: words)\n",
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
    let set_up = Command::new("meson")
        .args(["setup", "--buildtype=debug", "build", "app"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(set_up.status.success(), "{set_up:?}");
    let built = Command::new("ninja").current_dir(&build).output().unwrap();
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

/// Whether `ninja -n` in `build` would run nothing.
fn ninja_plans_nothing(build: &Path) -> bool {
    let planned = Command::new("ninja")
        .args(["-n", "-d", "explain"])
        .current_dir(build)
        .output()
        .unwrap();
    assert!(planned.status.success(), "{planned:?}");
    String::from_utf8_lossy(&planned.stdout).contains("no work to do")
}

#[test]
fn after_compress_and_dedupe_ninja_plans_nothing_and_the_binaries_run() {
    if !available("meson") || !available("ninja") {
        return;
    }
    let (_tmp, root) = root();
    let build = project(&root);
    assert!(ninja_plans_nothing(&build), "the control");
    let before = allocated_bytes(&build);

    let index = RefCell::new(HashIndex::default());
    let compress = Compress::new(&index);
    let dedupe = Dedupe::new(&index);
    let units = MESON.units(&build).unwrap();
    let report = engine::run(&units, &[&compress, &dedupe], &Options::default(), &MESON).unwrap();

    assert!(report.busy.is_empty(), "{report:?}");
    assert_eq!(report.quiet, units);
    if dunnage::sys::caps(&build).compress {
        assert!(report.passes[0].applied > 0, "{report:?}");
        if dunnage::sys::ALLOCATED_SHOWS_COMPRESSION {
            assert!(allocated_bytes(&build) < before, "{before}");
        }
    }
    assert!(
        ninja_plans_nothing(&build),
        "ninja would build after the passes"
    );
    for binary in ["app", "twin"] {
        let ran = Command::new(build.join(binary)).output().unwrap();
        assert!(ran.status.success(), "{ran:?}");
        assert_eq!(
            String::from_utf8_lossy(&ran.stdout),
            "the same words again 7\n"
        );
    }
    // And the oracle can say no.
    File::options()
        .write(true)
        .open(root.join("app/words.c"))
        .unwrap()
        .set_modified(SystemTime::now())
        .unwrap();
    assert!(!ninja_plans_nothing(&build));
}
