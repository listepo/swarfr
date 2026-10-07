//! Read-only inventory: which build dirs exist under some roots, how big they really are,
//! which belong together, and what the passes could win. Takes no locks and changes nothing.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::compress::DEFAULT_MIN_SIZE as COMPRESS_MIN_SIZE;
use crate::dedupe::DEFAULT_MIN_SIZE as DEDUPE_MIN_SIZE;
use crate::eco::{self, Ecosystem};
use crate::model;
use crate::sys::COMPRESSED;

const GITDIR_KEY: &str = "gitdir:";
const WORKTREES_DIR: &str = "worktrees";

/// The unit the lossy `evict` pass removes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProfileInfo {
    pub dir: PathBuf,
    /// Inodes are counted for the profile dir that holds their first path.
    pub allocated_bytes: u64,
    /// Newest mtime among the dir's top-level entries, as unix seconds.
    pub last_built_unix: Option<u64>,
}

#[derive(Debug, Default, Serialize)]
pub struct Target {
    pub root: PathBuf,
    /// The adapter that claimed the dir, by name: see [`eco::named`].
    pub ecosystem: &'static str,
    /// The checkout the project is in: the nearest dir above it holding a `.git`.
    pub checkout: Option<PathBuf>,
    /// Where the build dir sits inside that checkout; `None` outside it.
    pub position: Option<PathBuf>,
    /// What keeps a build and the passes apart here: [`eco::Guard::name`] of its first unit.
    pub guard: &'static str,
    /// The project the dir was built from, as the adapter tells it. Family and orphan status
    /// are the project's.
    #[serde(skip)]
    pub project: Option<PathBuf>,
    pub profiles: Vec<ProfileInfo>,
    /// Targets with the same git common dir (a repository and its worktrees) form a family.
    pub family: Option<PathBuf>,
    /// The project is a git worktree whose record in the repository is gone.
    pub orphaned: bool,
    /// The project's manifest is gone: deleted, renamed, or absent on this branch. The two look
    /// the same, so this is reported and removed only when the build dir is idle long enough.
    pub project_gone: bool,
    pub inodes: usize,
    pub paths: usize,
    pub logical_bytes: u64,
    /// What `du` reports: allocated blocks, every hardlinked inode once.
    pub allocated_bytes: u64,
    pub compressed_bytes: u64,
    /// Allocated bytes of files the compress pass would still look at.
    pub compressible_bytes: u64,
    /// Allocated bytes of `doc/`, which `cargo doc` writes again from scratch.
    pub doc_bytes: u64,
    /// Allocated bytes of the profiles' `incremental/` dirs, which no build needs to keep.
    pub incremental_bytes: u64,
    /// Upper bound for dedupe: bytes of files whose size also occurs in a sibling target.
    pub dedupe_candidate_bytes: u64,
    /// The latest build of any profile, as unix seconds.
    pub last_built_unix: Option<u64>,
    /// Units per rustc over the profile dirs, the compiler cargo uses here first.
    /// What the filesystem under this target can do for the lossless passes. A pass whose
    /// capability is false finds no work here, and says so by planning none.
    pub caps: crate::sys::Caps,
    pub toolchains: Vec<crate::eco::cargo::toolchains::Built>,
    /// Units left behind by a compiler that is no longer the one in use.
    pub stale_units: usize,
    /// What those units cost, estimated: the profile dirs' bytes in the share of the units,
    /// because which files belong to which unit is not knowable without parsing hashed names.
    pub stale_bytes_estimate: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct Inventory {
    /// Sorted by family, then by allocated size, largest first.
    pub targets: Vec<Target>,
    /// Only when asked for: reading the cargo home costs another full walk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cargo_home: Option<crate::eco::cargo::home::Stats>,
}

/// Build dirs under `roots`, by the shared walk of [`eco::discover`]. A found one is not entered.
pub fn discover(roots: &[PathBuf]) -> Vec<PathBuf> {
    eco::discover(roots)
        .into_iter()
        .map(|(dir, _)| dir)
        .collect()
}

pub fn inventory(roots: &[PathBuf]) -> io::Result<Inventory> {
    inventory_of(eco::discover(roots))
}

/// The inventory of build dirs already found, by a walk or from [`crate::known`].
pub fn inventory_of(found: Vec<(PathBuf, &'static dyn Ecosystem)>) -> io::Result<Inventory> {
    let mut targets = Vec::new();
    let mut sizes = Vec::new();
    for (root, eco) in found {
        // A dir from the known list may have been removed since; it is simply no longer there.
        let (target, target_sizes) = match inspect(&root, eco) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            done => done?,
        };
        targets.push(target);
        sizes.push(target_sizes);
    }

    // How many targets of a family hold a file of a given size.
    let mut holders: HashMap<(&Path, u64), usize> = HashMap::new();
    for (target, target_sizes) in targets.iter().zip(&sizes) {
        if let Some(family) = &target.family {
            for &size in target_sizes.keys() {
                *holders.entry((family, size)).or_default() += 1;
            }
        }
    }
    let candidates: Vec<u64> = targets
        .iter()
        .zip(&sizes)
        .map(|(target, target_sizes)| {
            let Some(family) = &target.family else {
                return 0;
            };
            target_sizes
                .iter()
                .filter(|&(&size, _)| holders[&(family.as_path(), size)] > 1)
                .map(|(_, allocated)| allocated)
                .sum()
        })
        .collect();
    for (target, bytes) in targets.iter_mut().zip(candidates) {
        // An upper bound on a filesystem that shares no blocks is not an upper bound, it is a
        // promise the pass cannot keep. Zero is the honest figure there.
        target.dedupe_candidate_bytes = if target.caps.clone { bytes } else { 0 };
    }

    targets.sort_by(|a, b| {
        (&a.family, b.allocated_bytes, &a.root).cmp(&(&b.family, a.allocated_bytes, &b.root))
    });
    Ok(Inventory {
        targets,
        cargo_home: None,
    })
}

/// Totals of one target, plus allocated bytes per file size for the dedupe estimate.
fn inspect(root: &Path, eco: &'static dyn Ecosystem) -> io::Result<(Target, HashMap<u64, u64>)> {
    let profiles: Vec<ProfileInfo> = eco
        .units(root)?
        .into_iter()
        .map(|dir| ProfileInfo {
            last_built_unix: eco.last_used(&dir),
            allocated_bytes: 0,
            dir,
        })
        .collect();
    let project = eco.owner(root).map(|owner| owner.project);
    let checkout = checkout_root(project.as_deref().unwrap_or(root)).map(Path::to_path_buf);
    let position = checkout
        .as_deref()
        .and_then(|checkout| root.strip_prefix(checkout).ok())
        .map(Path::to_path_buf);
    let guard = eco
        .guard(profiles.first().map_or(root, |profile| &profile.dir))
        .name();
    let (family, orphaned) = project.as_deref().map_or((None, false), git_link);
    let project_gone = project
        .as_deref()
        .is_some_and(|project| is_project_gone(eco, project));
    let mut target = Target {
        root: root.to_path_buf(),
        ecosystem: eco.name(),
        checkout,
        position,
        guard,
        project,
        last_built_unix: profiles.iter().filter_map(|p| p.last_built_unix).max(),
        profiles,
        family,
        orphaned,
        project_gone,
        inodes: 0,
        paths: 0,
        logical_bytes: 0,
        allocated_bytes: 0,
        compressed_bytes: 0,
        compressible_bytes: 0,
        doc_bytes: 0,
        incremental_bytes: 0,
        dedupe_candidate_bytes: 0,
        caps: crate::sys::Caps::NONE,
        toolchains: Vec::new(),
        stale_units: 0,
        stale_bytes_estimate: 0,
    };
    let mut sizes: HashMap<u64, u64> = HashMap::new();
    // The whole target, not only the profile dirs: `doc/`, `package/` and `tmp/` weigh too.
    for inode in model::scan(root, eco)?.inodes {
        target.inodes += 1;
        target.paths += inode.paths.len();
        target.logical_bytes += inode.stamp.size;
        target.allocated_bytes += inode.allocated;
        if inode.paths[0].starts_with(root.join(crate::eco::cargo::doc::DIR)) {
            target.doc_bytes += inode.allocated;
        }
        let holder = target
            .profiles
            .iter_mut()
            .find(|profile| inode.paths[0].starts_with(&profile.dir));
        if let Some(profile) = holder {
            profile.allocated_bytes += inode.allocated;
            if inode.paths[0].starts_with(profile.dir.join(crate::eco::cargo::incremental::DIR)) {
                target.incremental_bytes += inode.allocated;
            }
        }
        if inode.flags & COMPRESSED != 0 {
            target.compressed_bytes += inode.allocated;
        } else if inode.stamp.size >= COMPRESS_MIN_SIZE {
            target.compressible_bytes += inode.allocated;
        }
        if inode.stamp.size >= DEDUPE_MIN_SIZE {
            *sizes.entry(inode.stamp.size).or_default() += inode.allocated;
        }
    }

    target.caps = crate::sys::caps(&target.root);
    if !target.caps.compress {
        // Same reasoning as `dedupe_candidate_bytes`: nothing here is compressible if the
        // filesystem does not compress, whatever the files' sizes say.
        target.compressible_bytes = 0;
    }
    let dirs: Vec<PathBuf> = target.profiles.iter().map(|p| p.dir.clone()).collect();
    target.toolchains = crate::eco::cargo::toolchains::scan(&dirs);
    target.stale_units = crate::eco::cargo::toolchains::stale(&target.toolchains);
    let units: usize = target.toolchains.iter().map(|built| built.units).sum();
    if target.stale_units > 0 {
        let profile_bytes: u64 = target.profiles.iter().map(|p| p.allocated_bytes).sum();
        target.stale_bytes_estimate = profile_bytes * target.stale_units as u64 / units as u64;
    }
    Ok((target, sizes))
}

/// The dir of the checkout `dir` belongs to: the nearest one above it holding a `.git`.
pub fn checkout_root(dir: &Path) -> Option<&Path> {
    dir.ancestors().find(|above| above.join(".git").exists())
}

/// Whether `project` is in a git worktree its repository no longer knows. Cheap enough to
/// repeat under the lock, which is what the `orphans` pass does.
pub fn is_orphaned(project: &Path) -> bool {
    git_link(project).1
}

/// Whether the manifest that makes `project` a project of `eco` is missing. An adapter with no
/// manifest never says so.
pub fn is_project_gone(eco: &dyn Ecosystem, project: &Path) -> bool {
    eco.manifest(project)
        .is_some_and(|manifest| fs::symlink_metadata(manifest).is_err())
}

/// The git common dir of the repository `project` is in, if it is in one.
pub fn family(project: &Path) -> Option<PathBuf> {
    git_link(project).0
}

/// Every checkout git registers under this common dir: the repository itself and each worktree.
/// Read from git's files directly, as everything else here is.
pub fn checkouts(common: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = common.parent().map(Path::to_path_buf).into_iter().collect();
    let entries = fs::read_dir(common.join(WORKTREES_DIR))
        .into_iter()
        .flatten();
    for entry in entries.flatten() {
        // `<common>/worktrees/<name>/gitdir` holds the path of the worktree's own `.git` file.
        let Ok(text) = fs::read_to_string(entry.path().join("gitdir")) else {
            continue;
        };
        if let Some(checkout) = Path::new(text.trim()).parent() {
            out.push(checkout.to_path_buf());
        }
    }
    out
}

/// The git common dir of the repository `project` is in, and whether `project` is in a worktree
/// that its repository no longer knows. Reads git's files directly: a worktree whose record is
/// gone is exactly the case where `git` itself refuses to answer.
///
/// A project that no longer exists at all is orphaned too when what is left above it is in no
/// checkout: its checkout went with it (`git worktree remove`, a deleted clone). Only an owner
/// outside the build dir's own tree can be missing while the build dir is there — a target dir
/// moved out of its checkout. Missing inside a live checkout, it is a gone project instead.
fn git_link(project: &Path) -> (Option<PathBuf>, bool) {
    for dir in project.ancestors() {
        match fs::symlink_metadata(dir) {
            Ok(_) if dir == project => break,
            Ok(_) if checkout_root(dir).is_none() => return (None, true),
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            // Unreadable is not gone.
            Err(_) => break,
        }
    }
    for dir in project.ancestors() {
        let dot_git = dir.join(".git");
        let Ok(meta) = fs::symlink_metadata(&dot_git) else {
            continue;
        };
        if meta.is_dir() {
            return (Some(dot_git), false);
        }
        // A worktree or submodule: `.git` is a file holding `gitdir: <path>`.
        let Some(gitdir) = fs::read_to_string(&dot_git).ok().and_then(|text| {
            let path = text
                .lines()
                .find_map(|line| line.strip_prefix(GITDIR_KEY))?;
            Some(dir.join(path.trim()))
        }) else {
            return (None, false);
        };
        if !gitdir.exists() {
            // `<repo>/.git/worktrees/<name>` is gone; the path still names the repository.
            let family = gitdir
                .parent()
                .filter(|worktrees| worktrees.file_name().is_some_and(|n| n == WORKTREES_DIR))
                .and_then(Path::parent)
                .map(|path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf()));
            return (family, true);
        }
        let common = match fs::read_to_string(gitdir.join("commondir")) {
            Ok(relative) => gitdir.join(relative.trim()),
            Err(_) => gitdir,
        };
        return (Some(common.canonicalize().unwrap_or(common)), false);
    }
    (None, false)
}
