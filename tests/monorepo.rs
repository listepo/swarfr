//! One repository with several workspaces at different positions, and a worktree of it: what
//! the adapter boundary already gets right. Fake targets in temp dirs only.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use swarfr::config::Config;
use swarfr::eco::cargo::CARGO;
use swarfr::inventory;
use swarfr::seed;
use swarfr::session::{Control, Request, Session, Settings};
use tempfile::TempDir;

mod common;
use common::{fake_target, filesystem_can, git, swarfr};

/// `repo` with workspaces `services/api` and `tools/cli`, both built, and a worktree `wt` with
/// only `services/api` built. A build script of `api` left a CMake build dir and a whole nested
/// cargo target inside `api`'s target.
struct Mono {
    _tmp: TempDir,
    repo: PathBuf,
    wt: PathBuf,
}

impl Mono {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let (repo, wt) = (base.join("repo"), base.join("wt"));
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("README"), "mono\n").unwrap();
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "mono"]);
        git(&repo, &["worktree", "add", wt.to_str().unwrap()]);

        fake_target(&repo, "services/api", 16, 0);
        fake_target(&repo, "tools/cli", 16, 0);
        fake_target(&wt, "services/api", 16, 0);
        let out = repo.join("services/api/target/debug/build/sys-1/out");
        fs::create_dir_all(out.join("build")).unwrap();
        fs::write(
            out.join("build/CMakeCache.txt"),
            "CMAKE_HOME_DIRECTORY:INTERNAL=x\n",
        )
        .unwrap();
        fake_target(&out, "vendored", 16, 0);
        fs::create_dir_all(wt.join("tools/cli")).unwrap();
        Self {
            _tmp: tmp,
            repo,
            wt,
        }
    }
}

/// The build dirs found, sorted by path.
fn roots(targets: &[inventory::Target]) -> Vec<&Path> {
    let mut roots: Vec<&Path> = targets.iter().map(|target| target.root.as_path()).collect();
    roots.sort();
    roots
}

#[test]
fn every_build_dir_is_found_once_and_a_nested_one_is_nobodys() {
    let mono = Mono::new();
    let base = mono.repo.parent().unwrap().to_path_buf();

    // Roots that overlap still find each dir once.
    let found = inventory::inventory(&[base.clone(), mono.repo.clone()]).unwrap();

    assert_eq!(
        roots(&found.targets),
        [
            mono.repo.join("services/api/target"),
            mono.repo.join("tools/cli/target"),
            mono.wt.join("services/api/target"),
        ]
        .iter()
        .map(PathBuf::as_path)
        .collect::<Vec<_>>()
    );
    for target in &found.targets {
        assert!(
            !target
                .root
                .starts_with(mono.repo.join("services/api/target/debug")),
            "the nested target is part of api's build, not a build dir of its own"
        );
    }
}

#[test]
fn every_position_of_every_checkout_is_one_family_and_owned_by_its_workspace() {
    let mono = Mono::new();
    let base = mono.repo.parent().unwrap().to_path_buf();

    let found = inventory::inventory(&[base]).unwrap();

    let common = mono.repo.join(".git");
    for target in &found.targets {
        assert_eq!(
            target.family.as_deref(),
            Some(common.as_path()),
            "{target:?}"
        );
        assert!(!target.orphaned);
        assert_eq!(target.project.as_deref(), target.root.parent());
    }
}

#[test]
fn seed_finds_the_same_position_in_a_sibling_checkout() {
    let mono = Mono::new();

    assert_eq!(
        seed::choose(&mono.wt.join("tools/cli"), &CARGO),
        Some(mono.repo.join("tools/cli/target"))
    );
    // `services/api` of the main checkout has the worktree's as its sibling.
    assert_eq!(
        seed::choose(&mono.repo.join("services/api"), &CARGO),
        Some(mono.wt.join("services/api/target"))
    );
}

/// Moves the entries of every profile dir of `target` back by `days`: what `last_used` reads.
fn built_days_ago(target: &Path, days: u64) {
    let then = std::time::SystemTime::now() - std::time::Duration::from_secs(days * 24 * 60 * 60);
    for profile in swarfr::eco::cargo::profile_dirs(target).unwrap() {
        for entry in fs::read_dir(&profile).unwrap() {
            common::set_mtime(&entry.unwrap().path(), then);
        }
    }
}

#[test]
fn seed_fills_every_position_from_the_sibling_that_built_it_last() {
    let mono = Mono::new();
    // `api` was built last in the worktree, `cli` only ever in the main checkout, and `legacy`
    // exists in the main checkout only: absent on the new branch.
    built_days_ago(&mono.repo.join("services/api/target"), 5);
    fake_target(&mono.repo, "tools/legacy", 16, 0);
    let fresh = mono.repo.parent().unwrap().join("fresh");
    git(&mono.repo, &["worktree", "add", fresh.to_str().unwrap()]);
    for project in ["services/api", "tools/cli"] {
        fs::create_dir_all(fresh.join(project)).unwrap();
    }

    let chosen: Vec<(PathBuf, PathBuf)> = seed::positions(&fresh)
        .into_iter()
        .map(|position| (position.project, position.source))
        .collect();
    assert_eq!(
        chosen,
        [
            (
                fresh.join("services/api"),
                mono.wt.join("services/api/target")
            ),
            (fresh.join("tools/cli"), mono.repo.join("tools/cli/target")),
        ]
    );

    let state = mono.repo.parent().unwrap().join("state");
    let session = Session::open(Settings {
        index: state.join("hashes.bin"),
        ..Settings::default()
    });
    let done = session.seed(&fresh, None, false).unwrap();

    assert_eq!(done.len(), 2);
    for project in ["services/api", "tools/cli"] {
        assert!(
            fresh
                .join(project)
                .join("target/debug/deps/libx.rlib")
                .is_file()
        );
    }
    assert!(!fresh.join("tools/legacy").exists());
    // Everything is filled now: a second seed has nothing to do and says so.
    assert!(session.seed(&fresh, None, false).is_err());
}

#[test]
fn every_build_dir_says_its_ecosystem_checkout_position_and_guard() {
    let mono = Mono::new();
    let found = inventory::inventory(std::slice::from_ref(&mono.repo)).unwrap();

    let api = found
        .targets
        .iter()
        .find(|target| target.root == mono.repo.join("services/api/target"))
        .unwrap();
    let json = serde_json::to_value(api).unwrap();
    assert_eq!(json["ecosystem"], "cargo");
    assert_eq!(json["checkout"], mono.repo.to_str().unwrap());
    assert_eq!(
        json["position"],
        "services/api/target".replace('/', std::path::MAIN_SEPARATOR_STR)
    );
    assert_eq!(json["guard"], "lock");
    // The keys that were there before stay.
    assert!(json["allocated_bytes"].is_u64() && json["family"].is_string());
}

/// A request that works on `roots` with `family` configured as the config file would.
fn with_family(roots: &[PathBuf], config: &str) -> Request {
    let family = inventory::family(&roots[0]).unwrap();
    let text = format!("[family.\"{}\"]\n{config}", common::toml_basic(&family));
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("config.toml");
    fs::write(&path, text).unwrap();
    Request {
        roots: roots.to_vec(),
        min_age: Some(Duration::ZERO),
        ..Request::from_config(&Config::load(&path).unwrap())
    }
}

fn kept(request: &Request, targets: &[inventory::Target]) -> Vec<PathBuf> {
    let mut kept: Vec<PathBuf> = targets
        .iter()
        .filter(|target| request.keeps(target))
        .map(|target| target.root.clone())
        .collect();
    kept.sort();
    kept
}

#[test]
fn a_skipped_position_is_skipped_in_every_checkout_and_never_worked_on() {
    let mono = Mono::new();
    let roots = [mono.repo.clone(), mono.wt.clone()];
    let found = inventory::inventory(&roots).unwrap();

    let request = with_family(&roots, "skip-paths = [\"services\"]\n");
    request.check().unwrap();
    assert_eq!(
        kept(&request, &found.targets),
        [mono.repo.join("tools/cli/target")]
    );
    // Whole components: `service` is no prefix of `services`.
    let request = with_family(&roots, "skip-paths = [\"service\"]\n");
    assert_eq!(kept(&request, &found.targets).len(), 3);

    // Through a whole dry run: compress plans the artifact of the one dir left.
    if filesystem_can(|caps| caps.compress, "compress") {
        let state = TempDir::new().unwrap();
        let session = Session::open(Settings {
            index: state.path().join("hashes.bin"),
            ..Settings::default()
        });
        let planned = |request: &Request| -> usize {
            let report = session.plan(request, &Control::default()).unwrap();
            report
                .groups
                .iter()
                .flat_map(|(_, group)| &group.passes)
                .filter(|pass| pass.name == "compress")
                .map(|pass| pass.planned)
                .sum()
        };
        let all = planned(&with_family(&roots, ""));
        let skipped = planned(&with_family(&roots, "skip-paths = [\"services\"]\n"));
        assert!(all > skipped && skipped > 0, "{all} {skipped}");
    }
}

#[test]
fn a_family_can_narrow_the_ecosystems_it_is_worked_on_by() {
    let mono = Mono::new();
    let roots = [mono.repo.clone()];
    let found = inventory::inventory(&roots).unwrap();

    let request = with_family(&roots, "ecosystems = [\"cmake\"]\n");
    request.check().unwrap();
    assert!(kept(&request, &found.targets).is_empty());
    let request = with_family(&roots, "ecosystems = [\"cargo\"]\n");
    assert_eq!(kept(&request, &found.targets).len(), 2);

    let error = with_family(&roots, "ecosystems = [\"carg\"]\n")
        .check()
        .unwrap_err();
    assert!(
        error.to_string().contains("unknown ecosystem `carg`"),
        "{error}"
    );
}

#[test]
fn status_groups_by_family_checkout_and_ecosystem_and_lists_the_largest() {
    let mono = Mono::new();
    for name in 1..=6 {
        fake_target(&mono.repo, &format!("crates/c{name}"), 16, 0);
    }
    let base = mono.repo.parent().unwrap();
    let status = |all: bool| -> String {
        let tmp = TempDir::new().unwrap();
        let mut cmd = swarfr(tmp.path());
        cmd.arg("status").arg(base);
        if all {
            cmd.arg("--all");
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success());
        let text = String::from_utf8_lossy(&out.stdout).replace(base.to_str().unwrap(), "[BASE]");
        // Up to the totals, and without the lines a filesystem without clones or compression adds.
        text.lines()
            .take_while(|line| !line.ends_with("logical)"))
            .filter(|line| !line.contains("this filesystem"))
            .map(|line| format!("{line}\n"))
            .collect()
    };
    let row = |place: &str| format!("         0.00 GiB  built 0d ago      {place}\n");

    let mut expected = String::from(
        "family [BASE]/repo/.git\n  checkout [BASE]/repo\n    cargo: 8 build dirs, 0.00 GiB\n",
    );
    // Largest first: `api`'s target holds a nested one. Then by path.
    let rows: Vec<String> = ["services/api/target"]
        .into_iter()
        .map(String::from)
        .chain((1..=6).map(|name| format!("crates/c{name}/target")))
        .chain(["tools/cli/target".to_owned()])
        .map(|place| row(&place))
        .collect();
    let tail = "  checkout [BASE]/wt\n    cargo: 1 build dirs, 0.00 GiB\n".to_owned()
        + &row("services/api/target");
    let limited = expected.clone()
        + &rows[..5].concat()
        + "      3 more, 0.00 GiB: --all lists them\n"
        + &tail;
    let native = |text: String| text.replace('/', std::path::MAIN_SEPARATOR_STR);
    assert_eq!(status(false), native(limited));
    expected += &rows.concat();
    assert_eq!(status(true), native(expected + &tail));
}
