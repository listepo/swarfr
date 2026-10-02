//! `--store`: compress on a content-addressed store without a lock (`Guard::Immutable`), on
//! throwaway dirs only.
#![cfg(unix)]

use std::cell::RefCell;
use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};
use swarfr::compress::Compress;
use swarfr::eco::store::{self, STORE};
use swarfr::engine::{self, Action, Options, Pass, Skip};
use swarfr::index::HashIndex;
use swarfr::model::Profile;
use tempfile::TempDir;
use walkdir::WalkDir;

mod common;
use common::{allocated_bytes, fake_target, swarfr};

/// Old enough for every floor: the store's hour and compress's own default.
const TWO_DAYS: Duration = Duration::from_secs(2 * 24 * 60 * 60);

/// Every regular file under `dir`: path, content, mtime and mode.
fn files(dir: &Path) -> Vec<(PathBuf, Vec<u8>, SystemTime, u32)> {
    let mut files: Vec<_> = WalkDir::new(dir)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            let meta = entry.metadata().unwrap();
            (
                entry.path().to_path_buf(),
                fs::read(entry.path()).unwrap(),
                meta.modified().unwrap(),
                meta.permissions().mode(),
            )
        })
        .collect();
    files.sort();
    files
}

/// Moves every file under `dir` back by `age`.
fn age_all(dir: &Path, age: Duration) {
    let then = SystemTime::now() - age;
    for (path, ..) in files(dir) {
        let mode = fs::metadata(&path).unwrap().permissions();
        // A read-only entry still takes new times from its owner.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(then)
            .unwrap();
        fs::set_permissions(&path, mode).unwrap();
    }
}

/// A store under a fresh temp dir with `count` compressible entries named by their content, the
/// way content-addressed stores name them, some of them read-only.
fn store(count: usize) -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let store = tmp.path().canonicalize().unwrap().join("store");
    for i in 0..count {
        let content = format!("entry {i}: {}\n", "the same words again ".repeat(4000));
        let name = hex(&Sha256::digest(content.as_bytes()));
        let path = store.join(&name[..2]).join(format!("{name}-d"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        if i % 3 == 0 {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        }
    }
    age_all(&store, TWO_DAYS);
    (tmp, store)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn compress_store(dir: &Path) -> engine::Report {
    let index = RefCell::new(HashIndex::default());
    let compress = Compress::new(&index);
    engine::run(
        std::slice::from_ref(&dir.to_path_buf()),
        &[&compress],
        &Options::default(),
        &STORE,
    )
    .unwrap()
}

#[test]
fn compress_leaves_mtime_mode_and_content_of_every_entry_as_they_were() {
    if !common::filesystem_can(|caps| caps.compress, "compress") {
        return;
    }
    let (_tmp, store) = store(12);
    let before = files(&store);
    let bytes_before = allocated_bytes(&store);

    let report = compress_store(&store);

    assert_eq!(report.quiet, std::slice::from_ref(&store));
    assert!(report.busy.is_empty());
    assert_eq!(report.passes[0].applied, 12, "{report:?}");
    assert_eq!(files(&store), before);
    // btrfs reports the uncompressed size in `st_blocks`.
    if swarfr::sys::ALLOCATED_SHOWS_COMPRESSION {
        assert!(allocated_bytes(&store) < bytes_before / 2);
    }
}

#[test]
fn an_entry_younger_than_an_hour_is_left_alone() {
    if !common::filesystem_can(|caps| caps.compress, "compress") {
        return;
    }
    let (_tmp, store) = store(3);
    let young = store.join("young-d");
    fs::write(&young, "still being written ".repeat(4000)).unwrap();
    let young_bytes = allocated_bytes(&young);

    let report = compress_store(&store);

    assert_eq!(report.passes[0].applied, 3, "{report:?}");
    assert_eq!(allocated_bytes(&young), young_bytes);
}

/// A lossy pass that removes every unit it sees.
struct RemoveAll;

impl Pass for RemoveAll {
    fn name(&self) -> &'static str {
        "remove-all"
    }
    fn lossy(&self) -> bool {
        true
    }
    fn plan(&self, profiles: &[Profile]) -> Vec<Action> {
        profiles
            .iter()
            .map(|profile| Action::Remove {
                dir: profile.dir.clone(),
                reason: "test".into(),
            })
            .collect()
    }
}

#[test]
fn a_store_never_gets_a_lossy_pass() {
    let (_tmp, store) = store(1);
    let opts = Options {
        lossy: vec!["remove-all".into()],
        ..Options::default()
    };

    let report = engine::run(std::slice::from_ref(&store), &[&RemoveAll], &opts, &STORE).unwrap();

    assert_eq!(report.passes[0].skipped, [(store, Skip::Unsure)]);
    assert!(report.quiet[0].exists());
}

#[test]
fn check_refuses_what_is_not_an_immutable_store() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let dir = |name: &str| {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        dir
    };

    assert_eq!(store::check(&dir("plain")), None);
    assert!(store::check(&root.join("missing")).is_some());
    assert!(store::check(&dir("ccache")).is_some());
    let conf = dir("cc1");
    fs::write(conf.join("ccache.conf"), "").unwrap();
    assert!(store::check(&conf).is_some());
    let tagged = dir("cc2");
    fs::write(
        tagged.join("CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55\n# created by ccache\n",
    )
    .unwrap();
    assert!(store::check(&tagged).is_some());
    let target = fake_target(&root, "proj", 1, 0);
    assert!(store::check(&target.join("deps")).is_some());
    let home = dir("cargo-home");
    File::create(home.join(".package-cache")).unwrap();
    assert!(store::check(&dir("cargo-home/registry/src")).is_some());
}

/// `go` with every cache, path and setting of its own under `root`.
fn go(root: &Path, module: &Path, args: &[&str]) -> std::process::Output {
    Command::new("go")
        .args(args)
        .current_dir(module)
        .env("GOCACHE", root.join("gocache"))
        .env("GOPATH", root.join("gopath"))
        .env("GOENV", "off")
        .env("GOTOOLCHAIN", "local")
        .env("GOFLAGS", "")
        .output()
        .unwrap()
}

/// The owning tool's oracle: every data entry of `GOCACHE` is still named by the SHA-256 of its
/// bytes, and a build after the pass compiles nothing and runs.
#[test]
fn a_compressed_gocache_still_hits_and_still_matches_its_names() {
    if Command::new("go").arg("version").output().is_err()
        || !common::filesystem_can(|caps| caps.compress, "compress")
    {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let module = root.join("hello");
    fs::create_dir_all(&module).unwrap();
    fs::write(module.join("go.mod"), "module hello\n\ngo 1.21\n").unwrap();
    fs::write(
        module.join("main.go"),
        "package main\n\nimport \"fmt\"\n\nfunc main() { fmt.Println(\"hello\") }\n",
    )
    .unwrap();
    let built = go(&root, &module, &["build", "-o", "first", "."]);
    assert!(built.status.success(), "{built:?}");
    let cache = root.join("gocache");
    age_all(&cache, TWO_DAYS);
    let bytes_before = allocated_bytes(&cache);

    let report = compress_store(&cache);

    assert!(report.passes[0].applied > 0, "{report:?}");
    if swarfr::sys::ALLOCATED_SHOWS_COMPRESSION {
        assert!(allocated_bytes(&cache) < bytes_before);
    }
    let mut entries = 0;
    for (path, content, ..) in files(&cache) {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if let Some(hash) = name.strip_suffix("-d") {
            assert_eq!(hash, hex(&Sha256::digest(&content)), "{}", path.display());
            entries += 1;
        }
    }
    assert!(entries > 0);
    let again = go(&root, &module, &["build", "-x", "-o", "second", "."]);
    assert!(again.status.success(), "{again:?}");
    let log = String::from_utf8_lossy(&again.stderr);
    assert!(
        !log.contains("/compile "),
        "a package was compiled again:\n{log}"
    );
    let ran = Command::new(module.join("second")).output().unwrap();
    assert_eq!(ran.stdout, b"hello\n");
}

#[test]
fn run_store_needs_no_root_and_reports_the_store_as_unlocked() {
    let (tmp, store) = store(2);
    let config_home = tmp.path().join("config-home");
    fs::create_dir_all(&config_home).unwrap();

    let out = swarfr(&config_home)
        .args(["run", "--json", "--store"])
        .arg(&store)
        .arg("--index")
        .arg(tmp.path().join("index.bin"))
        .assert()
        .success();

    let json: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    let group = &json["groups"][0];
    assert_eq!(group["family"], store.to_str().unwrap());
    assert_eq!(group["quiet"][0], store.to_str().unwrap());
    assert_eq!(group["passes"][0]["name"], "compress");
}

#[test]
fn run_refuses_a_ccache_dir_before_touching_anything() {
    let tmp = TempDir::new().unwrap();
    let ccache = tmp.path().join("ccache");
    fs::create_dir_all(&ccache).unwrap();

    swarfr(tmp.path())
        .args(["run", "--store"])
        .arg(&ccache)
        .arg("--index")
        .arg(tmp.path().join("index.bin"))
        .assert()
        .failure()
        .stderr(predicates::str::contains("compresses its own entries"));
    assert!(!tmp.path().join("index.bin").exists());
}
