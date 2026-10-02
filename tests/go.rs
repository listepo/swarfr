//! Go's module cache: `compress` on read-only dirs whose write bit it lifts for a moment, on
//! throwaway dirs only. `GOCACHE` alone is `tests/store.rs`.
#![cfg(unix)]

use std::cell::RefCell;
use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

use swarfr::compress::Compress;
use swarfr::eco::Ecosystem;
use swarfr::eco::go::{self, MOD_CACHE};
use swarfr::eco::store::STORE;
use swarfr::engine::{self, Options, Skip};
use swarfr::index::HashIndex;
use tempfile::TempDir;
use walkdir::WalkDir;

mod common;
use common::{allocated_bytes, swarfr};

/// Old enough for every floor: the immutable hour and compress's own default.
const TWO_DAYS: Duration = Duration::from_secs(2 * 24 * 60 * 60);

/// A path, its mode, and for a file its content and mtime.
type Entry = (PathBuf, u32, Option<(Vec<u8>, SystemTime)>);

/// Every entry under `dir`.
fn entries(dir: &Path) -> Vec<Entry> {
    let mut entries: Vec<_> = WalkDir::new(dir)
        .into_iter()
        .map(Result::unwrap)
        .map(|entry| {
            let meta = entry.metadata().unwrap();
            let file = entry
                .file_type()
                .is_file()
                .then(|| (fs::read(entry.path()).unwrap(), meta.modified().unwrap()));
            (entry.path().to_path_buf(), meta.permissions().mode(), file)
        })
        .collect();
    entries.sort();
    entries
}

/// Makes every dir under its path writable again when dropped, so the temp dir can go even
/// after a failed assertion.
struct Writable(PathBuf);

impl Drop for Writable {
    fn drop(&mut self) {
        for entry in WalkDir::new(&self.0).into_iter().flatten() {
            if entry.file_type().is_dir() {
                let _ = fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o755));
            }
        }
    }
}

/// Moves every file under `dir` back by `age`, then makes files and dirs read-only as `go` does.
fn age_and_seal(dir: &Path, age: Duration) {
    let then = SystemTime::now() - age;
    for entry in WalkDir::new(dir).into_iter().map(Result::unwrap) {
        let path = entry.path();
        if entry.file_type().is_file() {
            fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
            let file = File::options().write(true).open(path).unwrap();
            file.set_modified(then).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o444)).unwrap();
        }
    }
    // Deepest first: a dir sealed before its children could not be walked into for them.
    for entry in WalkDir::new(dir).contents_first(true) {
        let entry = entry.unwrap();
        if entry.file_type().is_dir() {
            fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o555)).unwrap();
        }
    }
}

/// A module cache the way `go` leaves one: `cache/download`, and `count` unpacked modules of
/// compressible sources, read-only and two days old.
fn mod_cache(count: usize) -> (TempDir, PathBuf, Writable) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap().join("gomod");
    fs::create_dir_all(root.join(go::DOWNLOADS)).unwrap();
    for i in 0..count {
        let module = root.join(format!("example.com/m{i}@v1.0.0"));
        fs::create_dir_all(module.join("sub")).unwrap();
        fs::write(module.join("go.mod"), format!("module example.com/m{i}\n")).unwrap();
        let source = format!("package m\n\n// {}\n", "the same words again ".repeat(2000));
        fs::write(module.join("m.go"), &source).unwrap();
        fs::write(module.join("sub/s.go"), &source).unwrap();
        age_and_seal(&module, TWO_DAYS);
    }
    let writable = Writable(root.clone());
    (tmp, root, writable)
}

fn compress(dir: &Path, eco: &dyn Ecosystem) -> engine::Report {
    let index = RefCell::new(HashIndex::default());
    let compress = Compress::new(&index);
    let units = MOD_CACHE.units(dir).unwrap();
    engine::run(&units, &[&compress], &Options::default(), eco).unwrap()
}

#[test]
fn compress_lifts_each_read_only_dir_for_a_moment_and_changes_nothing_else() {
    if !common::filesystem_can(|caps| caps.compress, "compress") {
        return;
    }
    let (_tmp, root, _writable) = mod_cache(3);
    let before = entries(&root);
    let bytes_before = allocated_bytes(&root);

    let report = compress(&root, &MOD_CACHE);

    assert_eq!(report.passes[0].applied, 6, "{report:?}");
    // Modes of every file and dir, bytes and mtimes of every file: as they were. No temp copy
    // is left, since the listing would have it.
    assert_eq!(entries(&root), before);
    if swarfr::sys::ALLOCATED_SHOWS_COMPRESSION {
        assert!(allocated_bytes(&root) < bytes_before / 2);
    }
}

#[test]
fn an_adapter_that_does_not_lift_leaves_read_only_dirs_alone() {
    if !common::filesystem_can(|caps| caps.compress, "compress") {
        return;
    }
    let (_tmp, root, _writable) = mod_cache(1);
    let before = entries(&root);

    let report = compress(&root, &STORE);

    assert_eq!(report.passes[0].applied, 0);
    assert!(
        report.passes[0]
            .skipped
            .iter()
            .all(|(_, skip)| *skip == Skip::Failed(std::io::ErrorKind::PermissionDenied)),
        "{report:?}"
    );
    assert_eq!(entries(&root), before);
}

#[test]
fn check_refuses_what_is_not_a_module_cache() {
    let (tmp, root, _writable) = mod_cache(0);

    assert_eq!(go::check(&root), None);
    assert!(go::check(&tmp.path().join("missing")).is_some());
    assert!(go::check(tmp.path()).is_some(), "no cache/download");
}

/// `go` with every cache, path and setting of its own under `root`, and modules from the proxy
/// dir `root/proxy` only: nothing is downloaded.
fn go_at(root: &Path, dir: &Path, args: &[&str]) -> Output {
    Command::new("go")
        .args(args)
        .current_dir(dir)
        .envs(go_env(root))
        .output()
        .unwrap()
}

fn go_env(root: &Path) -> Vec<(&'static str, String)> {
    let at = |name: &str| root.join(name).to_str().unwrap().to_owned();
    vec![
        ("GOCACHE", at("gocache")),
        ("GOMODCACHE", at("gomod")),
        ("GOPATH", at("gopath")),
        ("GOPROXY", format!("file://{}", at("proxy"))),
        ("GOSUMDB", "off".into()),
        ("GOENV", "off".into()),
        ("GOTOOLCHAIN", "local".into()),
        ("GOFLAGS", String::new()),
    ]
}

/// `example.com/lib v1.0.0` in the proxy dir, zipped the way a module proxy serves it.
fn publish_module(root: &Path) {
    let stage = root.join("stage");
    let module = stage.join("example.com/lib@v1.0.0");
    fs::create_dir_all(&module).unwrap();
    fs::write(module.join("go.mod"), "module example.com/lib\n").unwrap();
    fs::write(
        module.join("lib.go"),
        format!(
            "package lib\n\n// Text is compressible on purpose.\nconst Text = \"{}\"\n",
            "the same words again ".repeat(2000)
        ),
    )
    .unwrap();
    let versions = root.join("proxy/example.com/lib/@v");
    fs::create_dir_all(&versions).unwrap();
    fs::write(versions.join("list"), "v1.0.0\n").unwrap();
    fs::write(versions.join("v1.0.0.mod"), "module example.com/lib\n").unwrap();
    fs::write(
        versions.join("v1.0.0.info"),
        "{\"Version\":\"v1.0.0\",\"Time\":\"2020-01-01T00:00:00Z\"}\n",
    )
    .unwrap();
    let zipped = Command::new("zip")
        .args(["-qrD"])
        .arg(versions.join("v1.0.0.zip"))
        .arg("example.com/lib@v1.0.0")
        .current_dir(&stage)
        .status()
        .unwrap();
    assert!(zipped.success());
}

/// The owning tool's oracle, through `run --go`: `go mod verify` still matches every module to
/// the hash of its download, and a build afterwards compiles nothing.
#[test]
fn run_go_compresses_the_module_cache_and_go_still_verifies_it() {
    let have = |tool: &str, arg: &str| Command::new(tool).arg(arg).output().is_ok();
    if !have("go", "version")
        || !have("zip", "-v")
        || !common::filesystem_can(|caps| caps.compress, "compress")
    {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let _writable = Writable(root.join("gomod"));
    publish_module(&root);
    let app = root.join("app");
    fs::create_dir_all(&app).unwrap();
    fs::write(
        app.join("go.mod"),
        "module app\n\ngo 1.21\n\nrequire example.com/lib v1.0.0\n",
    )
    .unwrap();
    fs::write(
        app.join("main.go"),
        "package main\n\nimport (\n\t\"fmt\"\n\n\t\"example.com/lib\"\n)\n\n\
         func main() { fmt.Println(len(lib.Text)) }\n",
    )
    .unwrap();
    let tidy = go_at(&root, &app, &["mod", "tidy"]);
    assert!(tidy.status.success(), "{tidy:?}");
    let built = go_at(&root, &app, &["build", "-o", "first", "."]);
    assert!(built.status.success(), "{built:?}");
    let module = root.join("gomod/example.com/lib@v1.0.0");
    // `go` sealed it; only the age is the test's.
    for dir in [module.as_path(), module.parent().unwrap()] {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
    }
    age_and_seal(&module, TWO_DAYS);
    let before = entries(&module);

    let out = swarfr(&root.join("config-home"))
        .args(["run", "--json", "--go", "--index"])
        .arg(root.join("index.bin"))
        .envs(go_env(&root))
        .assert()
        .success();

    let json: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    let group = json["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|group| group["family"] == root.join("gomod").to_str().unwrap())
        .expect("a group for the module cache");
    assert_eq!(group["passes"][0]["name"], "compress");
    assert_eq!(group["passes"][0]["applied"], 1, "{group}");
    assert_eq!(entries(&module), before);
    let verified = go_at(&root, &app, &["mod", "verify"]);
    assert!(verified.status.success(), "{verified:?}");
    let again = go_at(&root, &app, &["build", "-x", "-o", "second", "."]);
    assert!(again.status.success(), "{again:?}");
    let log = String::from_utf8_lossy(&again.stderr);
    assert!(
        !log.contains("/compile "),
        "a package was compiled again:\n{log}"
    );
}
