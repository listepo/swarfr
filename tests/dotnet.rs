//! .NET `bin/` and `obj/`: fixtures for what the adapter claims, and, where a .NET 9 SDK is
//! installed, real projects in temp dirs, built offline with a NuGet cache of their own.
#![cfg(unix)]

use std::cell::RefCell;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use swarfr::compress::Compress;
use swarfr::dedupe::Dedupe;
use swarfr::eco::dotnet::DOTNET;
use swarfr::eco::{self, Ecosystem, Guard};
use swarfr::engine::{self, Options};
use swarfr::index::HashIndex;
use swarfr::inventory;
use tempfile::TempDir;
use walkdir::WalkDir;

mod common;
use common::allocated_bytes;

/// Past the quiet tier's one-day floor.
const TWO_DAYS: Duration = Duration::from_secs(2 * 24 * 60 * 60);
/// Any 9.0 SDK: 10.0 needs workloads a broken install lacks, and the fixture needs none.
const GLOBAL_JSON: &str = r#"{"sdk":{"version":"9.0.100","rollForward":"latestFeature"}}"#;
const APP: &str = "App";

fn root() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    (tmp, root)
}

/// A restored and built project, as far as the adapter looks at one.
fn project(root: &Path, name: &str) -> PathBuf {
    let project = root.join(name);
    for (file, content) in [
        (
            format!("{name}.csproj"),
            "<Project Sdk=\"Microsoft.NET.Sdk\" />",
        ),
        ("obj/project.assets.json".into(), "{}"),
        (format!("obj/{name}.csproj.nuget.dgspec.json"), "{}"),
        ("bin/Debug/net9.0/Lib.dll".into(), "an assembly"),
    ] {
        let path = project.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    project
}

#[test]
fn bin_and_obj_of_a_restored_project_are_claimed_each_as_one_unit() {
    let (_tmp, root) = root();
    let app = project(&root, APP);
    // Not restored: a `bin/` and an `obj/` of anybody's.
    fs::create_dir_all(root.join("other/bin")).unwrap();
    fs::create_dir_all(root.join("other/obj")).unwrap();

    let found: Vec<(PathBuf, &str)> = eco::discover(std::slice::from_ref(&root))
        .into_iter()
        .map(|(dir, eco)| (dir, eco.name()))
        .collect();
    assert_eq!(
        found,
        [(app.join("bin"), "dotnet"), (app.join("obj"), "dotnet")]
    );
    for dir in [app.join("bin"), app.join("obj")] {
        assert_eq!(DOTNET.units(&dir).unwrap(), std::slice::from_ref(&dir));
        assert_eq!(DOTNET.owner(&dir).unwrap().project, app);
        assert_eq!(DOTNET.guard(&dir), Guard::Quiet);
    }
    assert!(DOTNET.units(&root.join("other/bin")).is_err());
}

#[test]
fn the_manifest_is_the_project_file_restore_recorded() {
    let (_tmp, root) = root();
    let app = project(&root, APP);
    // A second project file in the same dir, which this build is not of.
    fs::write(app.join("Tests.csproj"), "").unwrap();

    assert_eq!(DOTNET.manifest(&app), Some(app.join("App.csproj")));

    fs::remove_file(app.join("App.csproj")).unwrap();
    let inventory = inventory::inventory(std::slice::from_ref(&root)).unwrap();
    assert!(inventory.targets.iter().all(|target| target.project_gone));
}

#[test]
fn equal_files_are_never_hardlinked_even_when_asked() {
    assert!(!DOTNET.policy().share.links(true));
}

// Real projects, when a .NET 9 SDK is installed.

fn dotnet(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new("dotnet")
        .current_dir(root)
        .args(args)
        .env("NUGET_PACKAGES", root.join(".nuget"))
        .env("DOTNET_CLI_TELEMETRY_OPTOUT", "1")
        .env("DOTNET_NOLOGO", "1")
        .env("DOTNET_SKIP_FIRST_TIME_EXPERIENCE", "1")
        // No worker node or compiler server outlives a build: one would make the units busy.
        .env("MSBUILDDISABLENODEREUSE", "1")
        .env("DOTNET_CLI_USE_MSBUILD_SERVER", "0")
        .output()
        .unwrap()
}

/// The SDK `global.json` picks under `root`, if there is one.
fn sdk_available(root: &Path) -> bool {
    fs::write(root.join("global.json"), GLOBAL_JSON).unwrap();
    let found = Command::new("dotnet")
        .current_dir(root)
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    if !found {
        eprintln!("skipped: no .NET 9 SDK on this machine");
    }
    found
}

/// A library big enough for both passes, and two apps that each copy it into their `bin/`: the
/// shape of every NuGet assembly in every project's output. Built once, then every file moved
/// back two days, sources and outputs alike, so their order stays and the quiet floor is passed.
fn solution(root: &Path) {
    for (template, name) in [("classlib", "Lib"), ("console", APP), ("console", "Other")] {
        let made = dotnet(root, &["new", template, "-f", "net9.0", "-o", name]);
        assert!(made.status.success(), "{made:?}");
    }
    let constants: String = (0..600)
        .map(|i| format!("    public const string Words{i} = \"the same words again {i}\";\n"))
        .collect();
    fs::write(
        root.join("Lib/Words.cs"),
        format!("namespace Lib;\npublic static class Words\n{{\n{constants}}}\n"),
    )
    .unwrap();
    for app in [APP, "Other"] {
        let added = dotnet(root, &["add", app, "reference", "Lib/Lib.csproj"]);
        assert!(added.status.success(), "{added:?}");
        let built = dotnet(root, &["build", app, "--disable-build-servers"]);
        assert!(built.status.success(), "{built:?}");
    }
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
}

/// Whether building `App` compiles or copies anything, by what MSBuild says at normal
/// verbosity: every `CoreCompile` it reaches is skipped, and no `Copy` task copies a file.
fn builds_anything(root: &Path) -> bool {
    let built = dotnet(root, &["build", APP, "-v:n", "--disable-build-servers"]);
    let said = String::from_utf8_lossy(&built.stdout);
    assert!(built.status.success(), "{said}");
    let reached = said.matches("CoreCompile:").count();
    let skipped = said
        .matches("Skipping target \"CoreCompile\" because all output files are up-to-date")
        .count();
    assert!(reached > 0, "{said}");
    skipped < reached || said.contains("Copying file from")
}

#[test]
fn after_dedupe_and_compress_msbuild_builds_nothing_and_the_app_runs() {
    let (_tmp, root) = root();
    if !sdk_available(&root) {
        return;
    }
    solution(&root);
    assert!(
        !builds_anything(&root),
        "the control: a second build is a no-op"
    );
    let units: Vec<PathBuf> = eco::discover(std::slice::from_ref(&root))
        .into_iter()
        .filter(|(_, eco)| eco.name() == DOTNET.name())
        .map(|(dir, _)| dir)
        .collect();
    assert_eq!(units.len(), 6, "bin and obj of three projects: {units:?}");
    let before = allocated_bytes(&root);

    let index = RefCell::new(HashIndex::default());
    let compress = Compress::new(&index);
    let dedupe = Dedupe::new(&index);
    let report = engine::run(&units, &[&compress, &dedupe], &Options::default(), &DOTNET).unwrap();

    assert!(report.busy.is_empty(), "{report:?}");
    assert_eq!(report.quiet, units);
    let caps = swarfr::sys::caps(&root);
    if caps.clone {
        // `Lib.dll` in `Lib/obj`, `Lib/bin` and both apps' `bin/`: one file, four paths.
        assert!(report.passes[1].applied >= 3, "{report:?}");
    }
    if caps.compress {
        assert!(report.passes[0].applied > 0, "{report:?}");
    }
    if swarfr::sys::ALLOCATED_SHOWS_COMPRESSION || caps.clone {
        assert!(allocated_bytes(&root) < before, "{before}");
    }
    // The oracle: MSBuild sees nothing to do, and what it built still runs.
    assert!(!builds_anything(&root), "rebuilt after the passes");
    let dll = root.join(APP).join("bin/Debug/net9.0/App.dll");
    let ran = dotnet(&root, &[dll.to_str().unwrap()]);
    assert!(ran.status.success(), "{ran:?}");
    assert!(String::from_utf8_lossy(&ran.stdout).contains("Hello, World!"));
    // And the oracle can say no: a new mtime on a source is enough for a build.
    File::options()
        .write(true)
        .open(root.join("Lib/Words.cs"))
        .unwrap()
        .set_modified(SystemTime::now())
        .unwrap();
    assert!(builds_anything(&root));
}

/// `obj/` keeps the restore record of a project renamed since. The project file that still exists
/// is the manifest, whatever the names' order, so the project does not read as gone.
#[test]
fn a_renamed_project_file_is_found_past_the_old_record() {
    let (_tmp, root) = root();
    let app = project(&root, APP);
    fs::write(app.join("obj/Aaa.csproj.nuget.dgspec.json"), "{}").unwrap();
    assert_eq!(DOTNET.manifest(&app), Some(app.join("App.csproj")));

    fs::remove_file(app.join("App.csproj")).unwrap();
    let gone = DOTNET.manifest(&app).unwrap();
    assert!(
        !gone.exists(),
        "every recorded project file is gone: {gone:?}"
    );
}
