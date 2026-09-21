//! Xcode DerivedData: fixtures for what the adapter claims, and, where `xcodebuild` is installed,
//! a real Swift package built into a DerivedData dir inside a temp dir. Nothing here reads or
//! writes `~/Library`: every build names its own `-derivedDataPath`.
#![cfg(unix)]

use std::cell::RefCell;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use dunnage::compress::Compress;
use dunnage::dedupe::Dedupe;
use dunnage::eco::xcode::{INFO, XCODE};
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

/// A DerivedData entry built from `workspace`, as far as the adapter reads one.
fn derived_data(dir: &Path, workspace: &Path) -> PathBuf {
    fs::create_dir_all(dir.join("Build/Products/Debug")).unwrap();
    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n\
         \t<key>LastAccessedDate</key>\n\t<date>2026-01-01T00:00:00Z</date>\n\
         \t<key>WorkspacePath</key>\n\t<string>{}</string>\n</dict>\n</plist>\n",
        workspace.display()
    );
    fs::write(dir.join(INFO), plist).unwrap();
    dir.to_path_buf()
}

#[test]
fn an_entry_is_claimed_and_owned_by_the_workspace_its_plist_names() {
    let (_tmp, root) = root();
    let package = root.join("src/app");
    fs::create_dir_all(&package).unwrap();
    fs::write(
        package.join("Package.swift"),
        "// swift-tools-version:5.9\n",
    )
    .unwrap();
    let project = root.join("src/game/Game.xcodeproj");
    fs::create_dir_all(&project).unwrap();
    let dd = root.join("DerivedData");
    let app = derived_data(&dd.join("app-abc"), &package);
    let game = derived_data(&dd.join("Game-def"), &project);

    let mut found: Vec<(PathBuf, &str)> = eco::discover(std::slice::from_ref(&dd))
        .into_iter()
        .map(|(dir, eco)| (dir, eco.name()))
        .collect();
    found.sort();
    assert_eq!(found, [(game.clone(), "xcode"), (app.clone(), "xcode")]);
    assert_eq!(XCODE.owner(&app).unwrap().project, package);
    assert_eq!(
        XCODE.manifest(&package),
        Some(package.join("Package.swift"))
    );
    assert_eq!(XCODE.manifest(&project), Some(project.clone()));
    assert_eq!(XCODE.units(&app).unwrap(), std::slice::from_ref(&app));
    assert_eq!(XCODE.guard(&app), Guard::Quiet);
    assert!(!XCODE.policy().share.links(true));

    let gone = |dir: &Path| {
        inventory::inventory(std::slice::from_ref(&dd))
            .unwrap()
            .targets
            .into_iter()
            .find(|target| target.root == dir)
            .unwrap()
            .project_gone
    };
    assert!(!gone(&app) && !gone(&game));
    fs::remove_file(package.join("Package.swift")).unwrap();
    fs::remove_dir_all(&project).unwrap();
    assert!(gone(&app), "the package has no manifest");
    assert!(gone(&game), "the project bundle is gone");
}

#[test]
fn a_plist_alone_or_a_workspace_inside_is_not_an_entry() {
    let (_tmp, root) = root();
    // No Build/ or Logs/: some other dir with an info.plist.
    let bare = root.join("bare");
    fs::create_dir_all(&bare).unwrap();
    fs::write(
        bare.join(INFO),
        "<dict><key>WorkspacePath</key><string>/elsewhere</string></dict>",
    )
    .unwrap();
    // The workspace inside the dir: removing the unit would remove it too.
    let inside = root.join("inside");
    derived_data(&inside, &inside.join("App.xcodeproj"));
    // No WorkspacePath at all.
    let unnamed = root.join("unnamed");
    fs::create_dir_all(unnamed.join("Logs")).unwrap();
    fs::write(unnamed.join(INFO), "<dict></dict>").unwrap();

    assert!(eco::discover(std::slice::from_ref(&root)).is_empty());
    for dir in [&bare, &inside, &unnamed] {
        assert!(XCODE.units(dir).is_err(), "{}", dir.display());
    }
}

/// A tool holding a file of a quiet unit open makes the unit busy, wherever its current dir is.
/// `tail -f`, run through a link named after a tool the adapter looks for, keeps the file open;
/// a copy of the system binary would not start.
#[cfg(target_os = "macos")]
#[test]
fn a_tool_holding_a_file_open_inside_makes_the_entry_busy() {
    let (_tmp, root) = root();
    let package = root.join("app");
    fs::create_dir_all(&package).unwrap();
    let dd = derived_data(&root.join("dd"), &package);
    let db = dd.join("Build/build.db");
    fs::write(&db, "x".repeat(64 * 1024)).unwrap();
    File::options()
        .write(true)
        .open(&db)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - TWO_DAYS)
        .unwrap();
    let tool = root.join("bin/SWBBuildService");
    fs::create_dir_all(tool.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/usr/bin/tail", &tool).unwrap();
    let mut held = Command::new(&tool)
        .current_dir("/")
        .arg("-f")
        .arg(&db)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // Until lsof sees the file open.
    let open = (0..100).any(|_| {
        std::thread::sleep(Duration::from_millis(50));
        Command::new("/usr/sbin/lsof")
            .arg("-Fn")
            .arg("-p")
            .arg(held.id().to_string())
            .output()
            .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("build.db"))
    });

    let index = RefCell::new(HashIndex::default());
    let compress = Compress::new(&index);
    let units = XCODE.units(&dd).unwrap();
    let busy = engine::run(&units, &[&compress], &Options::default(), &XCODE)
        .unwrap()
        .busy;
    held.kill().unwrap();
    held.wait().unwrap();
    assert!(open, "tail never opened the file");
    assert_eq!(busy, units);

    // And with the tool gone, the unit is worked on.
    let report = engine::run(&units, &[&compress], &Options::default(), &XCODE).unwrap();
    assert!(report.busy.is_empty(), "{report:?}");
}

// A real package, when `xcodebuild` is installed.

fn xcodebuild_available() -> bool {
    let found = Command::new("xcodebuild")
        .arg("-version")
        .output()
        .is_ok_and(|out| out.status.success());
    if !found {
        eprintln!("skipped: no `xcodebuild` on this machine");
    }
    found
}

/// What one `xcodebuild build` of the package into `dd` compiled or linked, by the step lines
/// it prints.
fn build(package: &Path, dd: &Path) -> Vec<String> {
    let built = Command::new("xcodebuild")
        .current_dir(package)
        .args(["-scheme", "Hello", "-destination", "platform=macOS"])
        .arg("-derivedDataPath")
        .arg(dd)
        .arg("build")
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&built.stdout);
    assert!(built.status.success(), "{said}");
    assert!(said.contains("** BUILD SUCCEEDED **"), "{said}");
    said.lines()
        .filter(|line| {
            ["SwiftCompile ", "SwiftEmitModule ", "Ld ", "CompileC "]
                .iter()
                .any(|step| line.starts_with(step))
        })
        .map(str::to_owned)
        .collect()
}

/// An executable package with sources worth compiling, built into `dd`, then every file of `dd`
/// moved back two days, past the quiet floor. The build database records the mtimes of what it
/// wrote, so a few more builds relink until one does nothing; that is the state a run meets.
fn package(root: &Path) -> (PathBuf, PathBuf) {
    let package = root.join("app");
    let sources = package.join("Sources/Hello");
    fs::create_dir_all(&sources).unwrap();
    fs::write(
        package.join("Package.swift"),
        "// swift-tools-version:5.9\nimport PackageDescription\n\
         let package = Package(name: \"Hello\", targets: [.executableTarget(name: \"Hello\")])\n",
    )
    .unwrap();
    let numbers: String = (1..=300).map(|i| format!("{i},")).collect();
    for i in 1..=20 {
        fs::write(
            sources.join(format!("f{i}.swift")),
            format!("struct S{i} {{ var a = [{numbers}] }}\nfunc f{i}() -> Int {{ S{i}().a.reduce(0, +) }}\n"),
        )
        .unwrap();
    }
    fs::write(sources.join("main.swift"), "print(f7())\n").unwrap();
    let dd = root.join("dd");
    assert!(!build(&package, &dd).is_empty());
    for entry in WalkDir::new(&dd) {
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
    assert!(
        (0..5).any(|_| build(&package, &dd).is_empty()),
        "the build never settled after the mtimes moved"
    );
    (package, dd)
}

#[test]
fn after_compress_and_dedupe_xcodebuild_builds_nothing_and_the_binary_runs() {
    if !xcodebuild_available() {
        return;
    }
    let (_tmp, root) = root();
    let (package, dd) = package(&root);
    assert!(XCODE.claim(&dd));
    // xcodebuild records the path it was given, here the temp dir's link under /var.
    let owner = XCODE.owner(&dd).unwrap().project;
    assert_eq!(owner.canonicalize().unwrap(), package);
    let before = allocated_bytes(&dd);

    let index = RefCell::new(HashIndex::default());
    let compress = Compress::new(&index);
    let dedupe = Dedupe::new(&index);
    let units = XCODE.units(&dd).unwrap();
    let report = engine::run(&units, &[&compress, &dedupe], &Options::default(), &XCODE).unwrap();

    assert!(report.busy.is_empty(), "{report:?}");
    assert_eq!(report.quiet, units);
    let caps = dunnage::sys::caps(&dd);
    if caps.compress {
        assert!(report.passes[0].applied > 0, "{report:?}");
        if dunnage::sys::ALLOCATED_SHOWS_COMPRESSION {
            assert!(allocated_bytes(&dd) < before, "{before}");
        }
    }
    // The oracle: nothing to compile or link, and the binary still works.
    let steps = build(&package, &dd);
    assert!(steps.is_empty(), "rebuilt after the passes: {steps:?}");
    let ran = Command::new(dd.join("Build/Products/Debug/Hello"))
        .output()
        .unwrap();
    assert!(ran.status.success(), "{ran:?}");
    assert_eq!(String::from_utf8_lossy(&ran.stdout), "45150\n");
}
