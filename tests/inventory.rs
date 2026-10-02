//! Discovery, real sizes, families and orphans, on throwaway dirs only.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;

use swarfr::eco::cargo::LOCK_FILE;
use swarfr::inventory::{self, Target};
use tempfile::TempDir;

const CARGO_TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n\
    # This file is a cache directory tag created by cargo.\n";
const GRADLE_TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n\
    # This file is a cache directory tag created by Gradle.\n";
const MIB: usize = 1 << 20;
const BYTES_PER_KIB: u64 = 1024;
/// The done criterion of T4: within 1% of `du`.
const DU_TOLERANCE_PERCENT: u64 = 1;

fn root() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    (tmp, root)
}

fn make_target(target: &Path) {
    fs::create_dir_all(target.join("debug/deps")).unwrap();
    fs::write(target.join("CACHEDIR.TAG"), CARGO_TAG).unwrap();
    File::create(target.join("debug").join(LOCK_FILE)).unwrap();
}

fn find<'a>(targets: &'a [Target], root: &Path) -> &'a Target {
    targets.iter().find(|t| t.root == root).unwrap()
}

#[test]
fn allocated_size_matches_du_with_hardlinks_and_files_outside_profiles() {
    let (_tmp, root) = root();
    let target = root.join("target");
    make_target(&target);
    fs::write(target.join("debug/deps/libbig.rlib"), vec![1; 2 * MIB]).unwrap();
    fs::hard_link(
        target.join("debug/deps/libbig.rlib"),
        target.join("debug/libbig.rlib"),
    )
    .unwrap();
    fs::write(target.join("debug/deps/small.d"), b"x").unwrap();
    fs::create_dir(target.join("doc")).unwrap();
    fs::write(target.join("doc/index.html"), vec![2; MIB]).unwrap();

    let inventory = inventory::inventory(&[root]).unwrap();

    let found = &inventory.targets[0];
    let du = Command::new("du").arg("-sk").arg(&target).output().unwrap();
    let du_kib: u64 = String::from_utf8(du.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let ours_kib = found.allocated_bytes / BYTES_PER_KIB;
    assert!(
        ours_kib.abs_diff(du_kib) * 100 <= du_kib * DU_TOLERANCE_PERCENT,
        "ours {ours_kib} KiB, du {du_kib} KiB"
    );
    assert_eq!(found.paths, found.inodes + 1, "one inode has two paths");
    let [profile] = found.profiles.as_slice() else {
        panic!("{:?}", found.profiles)
    };
    assert_eq!(profile.dir, target.join("debug"));
    // Everything but `doc/` and the tag file sits in the one profile dir.
    assert!(profile.allocated_bytes >= 2 * MIB as u64);
    assert!(profile.allocated_bytes < found.allocated_bytes);
    assert_eq!(profile.last_built_unix, found.last_built_unix);
    assert!(found.last_built_unix.is_some());
    // The figure is what this filesystem could actually compress, so where it compresses
    // nothing the honest answer is zero — `tests/caps.rs` runs the pass on both sides.
    if swarfr::sys::caps(&target).compress {
        assert!(found.compressible_bytes >= 3 * MIB as u64);
    } else {
        assert_eq!(found.compressible_bytes, 0);
    }
}

#[test]
fn foreign_caches_are_ignored_and_a_found_target_is_not_entered() {
    let (_tmp, root) = root();
    let target = root.join("proj/target");
    make_target(&target);
    make_target(&target.join("debug/nested"));
    let gradle = root.join("proj/.gradle");
    fs::create_dir_all(&gradle).unwrap();
    fs::write(gradle.join("CACHEDIR.TAG"), GRADLE_TAG).unwrap();

    assert_eq!(inventory::discover(&[root.clone(), root]), [target]);
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

#[test]
fn worktrees_form_a_family_and_a_removed_record_reads_as_orphaned() {
    let (_tmp, root) = root();
    let (main, worktree, loose) = (root.join("main"), root.join("wt"), root.join("loose"));
    fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["commit", "-q", "--allow-empty", "-m", "init"]);
    git(&main, &["worktree", "add", "-q", "../wt"]);
    for project in [&main, &worktree, &loose] {
        make_target(&project.join("target"));
    }
    // The same size in both family members, a different one in the unrelated project.
    fs::write(main.join("target/debug/deps/libx.rlib"), vec![1; MIB]).unwrap();
    fs::write(worktree.join("target/debug/deps/libx.rlib"), vec![2; MIB]).unwrap();
    fs::write(loose.join("target/debug/deps/libx.rlib"), vec![3; MIB]).unwrap();

    let before = inventory::inventory(std::slice::from_ref(&root))
        .unwrap()
        .targets;

    let family = Some(main.join(".git"));
    let (in_main, in_worktree) = (
        find(&before, &main.join("target")),
        find(&before, &worktree.join("target")),
    );
    assert_eq!((&in_main.family, in_main.orphaned), (&family, false));
    assert_eq!(
        (&in_worktree.family, in_worktree.orphaned),
        (&family, false)
    );
    // An upper bound on what dedupe could share, which is nothing where blocks cannot be
    // shared at all.
    if swarfr::sys::caps(&in_main.root).clone {
        assert!(in_main.dedupe_candidate_bytes >= MIB as u64);
        assert!(in_worktree.dedupe_candidate_bytes >= MIB as u64);
    } else {
        assert_eq!(in_main.dedupe_candidate_bytes, 0);
        assert_eq!(in_worktree.dedupe_candidate_bytes, 0);
    }
    let in_loose = find(&before, &loose.join("target"));
    assert_eq!((&in_loose.family, in_loose.orphaned), (&None, false));
    assert_eq!(in_loose.dedupe_candidate_bytes, 0);

    // What `git worktree prune` or a deleted admin dir leaves behind: sources, a `.git` file,
    // a target, and no record.
    fs::remove_dir_all(main.join(".git/worktrees/wt")).unwrap();
    let after = inventory::inventory(&[root]).unwrap().targets;

    let orphan = find(&after, &worktree.join("target"));
    assert_eq!((&orphan.family, orphan.orphaned), (&family, true));
    assert!(!find(&after, &main.join("target")).orphaned);
}

#[test]
fn status_json_is_machine_readable() {
    let (_tmp, root) = root();
    make_target(&root.join("a/target"));
    make_target(&root.join("b/target"));

    let out = Command::new(env!("CARGO_BIN_EXE_swarfr"))
        .args(["status", "--json"])
        .arg(&root)
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let roots: Vec<&str> = json["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["root"].as_str().unwrap())
        .collect();
    assert_eq!(
        roots,
        [
            root.join("a/target").to_str().unwrap(),
            root.join("b/target").to_str().unwrap()
        ]
    );
}
