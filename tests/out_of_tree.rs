//! Target dirs moved out of their checkout by `CARGO_TARGET_DIR`: the owner comes from the dep-info
//! real cargo builds leave in them. Everything lives in temp dirs.

use std::fs;
use std::path::{Path, PathBuf};

use predicates::str::contains;
use swarfr::inventory::{self, Target};
use tempfile::TempDir;

mod common;
use common::{cargo_at, git, swarfr};

/// A workspace with a member in a subdir and a path dependency outside it, the files whose paths
/// rustc gets relative and absolute.
const FILES: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[package]\nname = \"ws\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n\
         [dependencies]\nb = { path = \"crates/b\" }\nv = { path = \"../vendor/v\" }\n\n\
         [workspace]\nmembers = [\"crates/b\"]\n",
    ),
    ("src/lib.rs", "pub fn f() -> u32 { b::g() + v::h() }\n"),
    (
        "crates/b/Cargo.toml",
        "[package]\nname = \"b\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    ),
    ("crates/b/src/lib.rs", "pub fn g() -> u32 { 1 }\n"),
];

const VENDOR: &[(&str, &str)] = &[
    (
        "vendor/v/Cargo.toml",
        "[package]\nname = \"v\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    ),
    ("vendor/v/src/lib.rs", "pub fn h() -> u32 { 2 }\n"),
];

fn root() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    (tmp, root)
}

fn write(dir: &Path, files: &[(&str, &str)]) {
    for (path, content) in files {
        let path = dir.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
}

/// A repository at `root/repo` holding the workspace, with `vendor/` next to it outside git.
fn repository(root: &Path) -> PathBuf {
    write(root, VENDOR);
    let repo = root.join("repo");
    write(&repo, FILES);
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "init"]);
    repo
}

/// `cargo build` of the workspace at `ws` into `target`.
fn build(ws: &Path, target: &Path) {
    assert!(cargo_at(ws, target, &["build"]).status().unwrap().success());
}

fn find<'a>(inventory: &'a inventory::Inventory, root: &Path) -> &'a Target {
    inventory
        .targets
        .iter()
        .find(|target| target.root == root)
        .unwrap_or_else(|| panic!("{} not found", root.display()))
}

#[test]
fn an_out_of_tree_target_lands_in_its_owners_family_and_checkout() {
    let (_tmp, root) = root();
    let repo = repository(&root);
    let outside = root.join("out/ws");
    build(&repo, &outside);
    build(&repo, &repo.join("target"));

    let inventory = inventory::inventory(std::slice::from_ref(&root)).unwrap();

    let moved = find(&inventory, &outside);
    let family = Some(repo.join(".git"));
    assert_eq!(moved.family, family);
    assert_eq!(moved.checkout.as_deref(), Some(repo.as_path()));
    assert_eq!(moved.position, None, "it is not inside its checkout");
    assert!(!moved.orphaned && !moved.project_gone, "{moved:?}");
    // The same family as the checkout's own target: a dedupe partner for it.
    assert_eq!(find(&inventory, &repo.join("target")).family, family);
}

#[test]
fn a_removed_worktree_loses_its_out_of_tree_target_and_nothing_else() {
    let (_tmp, root) = root();
    let repo = repository(&root);
    git(&repo, &["worktree", "add", "../wt"]);
    let (in_repo, in_wt) = (root.join("out/repo"), root.join("out/wt"));
    build(&repo, &in_repo);
    build(&root.join("wt"), &in_wt);
    let inventory = inventory::inventory(std::slice::from_ref(&root)).unwrap();
    assert_eq!(
        find(&inventory, &in_wt).checkout.as_deref(),
        Some(root.join("wt").as_path())
    );

    // `--force`: the build left an untracked `Cargo.lock` behind.
    git(&repo, &["worktree", "remove", "--force", "../wt"]);
    let inventory = inventory::inventory(std::slice::from_ref(&root)).unwrap();
    assert!(find(&inventory, &in_wt).orphaned);
    assert!(!find(&inventory, &in_repo).orphaned);

    swarfr(&root)
        .args(["run", "--pass", "orphans", "--lossy", "orphans", "--index"])
        .arg(root.join("index.bin"))
        .arg(&root)
        .assert()
        .success()
        .stdout(contains("the checkout is gone"))
        .stdout(contains("orphans: planned 1"));
    assert!(!in_wt.exists());
    assert!(in_repo.join("debug").exists());
    assert!(root.join("vendor/v/src/lib.rs").exists());
}

#[test]
fn a_workspace_gone_inside_a_live_checkout_is_a_gone_project_not_a_gone_checkout() {
    let (_tmp, root) = root();
    let repo = repository(&root);
    let outside = root.join("out/ws");
    build(&repo, &outside);
    // Moved into a subdir on another branch, say: the checkout is there, the workspace is not.
    let sub = repo.join("sub");
    fs::create_dir(&sub).unwrap();
    for name in ["Cargo.toml", "src", "crates"] {
        fs::rename(repo.join(name), sub.join(name)).unwrap();
    }

    let inventory = inventory::inventory(std::slice::from_ref(&root)).unwrap();

    let moved = find(&inventory, &outside);
    assert!(!moved.orphaned, "{moved:?}");
    assert!(moved.project_gone, "{moved:?}");
    assert_eq!(moved.family, Some(repo.join(".git")));
}
