//! Cargo: target and build dirs tagged by cargo, guarded per profile dir by `.cargo-lock` or
//! `.cargo-build-lock`, and the cargo home guarded as a whole by `.package-cache`. Also the
//! passes and the reports only cargo has.

pub mod advise;
pub mod depinfo;
pub mod doc;
pub mod home;
pub mod incremental;
pub mod toolchains;

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use walkdir::WalkDir;

use super::{Ecosystem, Guard, Owner, Policy, Sharing};

/// Cargo's per-profile lock file; its presence marks a profile dir.
pub const LOCK_FILE: &str = ".cargo-lock";
/// Cargo 1.97+ holds this exclusively for the length of a build. A 1.98 `target/debug`
/// still has [`LOCK_FILE`] too, but only as a shared lock.
pub const BUILD_LOCK_FILE: &str = ".cargo-build-lock";
/// Cargo 1.98's exclusive lock on the artifact directory. Not a unit marker.
pub const ARTIFACT_LOCK_FILE: &str = ".cargo-artifact-lock";
/// The target dir of a workspace, as cargo names it by default.
pub const TARGET: &str = "target";

const CACHEDIR_TAG: &str = "CACHEDIR.TAG";
/// Gradle, uv and others write the same tag file; only cargo writes this sentence.
const CARGO_TAG_MARK: &str = "created by cargo";
/// `<target>/<triple>/<profile>/.cargo-lock` is the deepest place a profile lock lives.
const PROFILE_LOCK_MAX_DEPTH: usize = 3;

fn is_profile_lock(name: &OsStr) -> bool {
    name == LOCK_FILE || name == BUILD_LOCK_FILE
}

fn is_cargo_lock(name: &OsStr) -> bool {
    is_profile_lock(name) || name == ARTIFACT_LOCK_FILE
}

/// The lock cargo holds exclusively in `unit`. 1.98 writes all three names; the build lock
/// is the write lock. Older cargo, and fixtures that only plant [`LOCK_FILE`], have that.
fn profile_lock(unit: &Path) -> PathBuf {
    let build = unit.join(BUILD_LOCK_FILE);
    if build.exists() {
        build
    } else {
        unit.join(LOCK_FILE)
    }
}

/// Cargo target dirs and `build-dir`s.
pub struct Cargo;

pub static CARGO: Cargo = Cargo;

impl Ecosystem for Cargo {
    fn name(&self) -> &'static str {
        "cargo"
    }

    fn claim(&self, dir: &Path) -> bool {
        is_target(dir)
    }

    /// The dir above the target when it holds a `Cargo.toml`: where cargo puts a target by
    /// default. Otherwise the workspace the dep-info names ([`depinfo`]): of several, one that
    /// still exists, so a dir shared by checkouts is an orphan only once all of them are gone,
    /// and none at all when they are in different repositories. No dep-info names one: the dir
    /// above, which for a target left behind by a gone project is its project.
    fn owner(&self, build_dir: &Path) -> Option<Owner> {
        let above = build_dir.parent()?;
        if self
            .manifest(above)
            .is_some_and(|manifest| manifest.exists())
        {
            return Some(Owner {
                project: above.to_path_buf(),
            });
        }
        let units = profile_dirs(build_dir).unwrap_or_default();
        let roots = depinfo::workspace_roots(build_dir, &units);
        let families: BTreeSet<Option<PathBuf>> = roots
            .iter()
            .filter(|root| root.exists())
            .map(|root| crate::inventory::family(root))
            .collect();
        if families.len() > 1 {
            return None;
        }
        let project = roots
            .iter()
            .find(|root| root.exists())
            .or(roots.first())
            .cloned()
            .unwrap_or_else(|| above.to_path_buf());
        Some(Owner { project })
    }

    fn manifest(&self, project: &Path) -> Option<PathBuf> {
        Some(project.join("Cargo.toml"))
    }

    fn build_dir(&self, project: &Path) -> Option<PathBuf> {
        Some(project.join(TARGET))
    }

    fn units(&self, build_dir: &Path) -> io::Result<Vec<PathBuf>> {
        profile_dirs(build_dir)
    }

    fn guard(&self, unit: &Path) -> Guard {
        Guard::Lock(profile_lock(unit))
    }

    fn private(&self, name: &OsStr) -> bool {
        is_cargo_lock(name)
    }

    /// `incremental/` is a cache of one checkout's build that cargo will not use in another.
    fn volatile(&self, name: &OsStr) -> bool {
        name == incremental::DIR
    }

    fn last_used(&self, unit: &Path) -> Option<u64> {
        last_built(unit)
    }

    /// Artifacts may be rewritten in place by a later build, so a hardlink between two of them
    /// is the user's call.
    fn policy(&self) -> Policy {
        Policy {
            share: Sharing::LinkOptIn,
        }
    }
}

/// True when cargo itself tagged `dir` as a target or build dir.
pub fn is_target(dir: &Path) -> bool {
    fs::read_to_string(dir.join(CACHEDIR_TAG)).is_ok_and(|tag| tag.contains(CARGO_TAG_MARK))
}

/// Profile dirs of a cargo target dir. Refuses a dir that cargo did not tag as its own.
pub fn profile_dirs(target: &Path) -> io::Result<Vec<PathBuf>> {
    let tag = fs::read_to_string(target.join(CACHEDIR_TAG))?;
    if !tag.contains(CARGO_TAG_MARK) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{CACHEDIR_TAG} was not written by cargo"),
        ));
    }
    let mut dirs = Vec::new();
    let walk = WalkDir::new(target)
        .follow_links(false)
        .same_file_system(true)
        .max_depth(PROFILE_LOCK_MAX_DEPTH);
    for entry in walk {
        let entry = entry?;
        if is_profile_lock(entry.file_name())
            && let Some(parent) = entry.path().parent()
        {
            dirs.push(parent.to_path_buf());
        }
    }
    dirs.sort();
    dirs.dedup();
    Ok(dirs)
}

/// When cargo last worked in a profile dir: the newest mtime among its top-level entries, as
/// unix seconds.
pub fn last_built(profile_dir: &Path) -> Option<u64> {
    fs::read_dir(profile_dir)
        .ok()?
        .filter_map(|entry| entry.ok()?.metadata().ok()?.modified().ok())
        .max()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|since_epoch| since_epoch.as_secs())
}
