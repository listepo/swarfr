//! `swarfr seed`: give a fresh checkout a build dir cloned from a sibling's. Where the
//! filesystem shares blocks (APFS, btrfs, XFS) the copy costs nothing until one side is
//! rewritten, and the new worktree starts with a warm target for free. Where it does not, the
//! copy is a real one: it still saves the build, it no longer saves the disk.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::eco::{self, Ecosystem, Guard};
use crate::engine::ProfileLock;
use crate::index::HashIndex;
use crate::inventory;
use crate::model::{Stamp, TMP_PREFIX};
use crate::sys;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Seeded {
    pub source: PathBuf,
    pub files: usize,
    pub symlinks: usize,
    /// Whether the copies share their blocks with the source. False on a filesystem without
    /// copy-on-write, where seeding still saves the build and no longer saves the disk — worth
    /// saying, because `bytes` then means bytes actually spent.
    pub shared_blocks: bool,
    /// Allocated bytes of what was copied, as `du` reports it. Where the copies are clones
    /// they share these blocks with the source and the volume loses nothing; this is what the
    /// new target will appear to weigh either way.
    pub bytes: u64,
    /// Profile dirs of the source that a build held; nothing under them was copied.
    pub busy: Vec<PathBuf>,
}

/// The best build dir to seed from: in the sibling checkouts of the same repository, at the same
/// place inside them as `checkout` is inside its own, the one built most recently. `None` when
/// the checkout has no repository, no siblings, or none of them has a build dir there.
pub fn choose(checkout: &Path, eco: &dyn Ecosystem) -> Option<PathBuf> {
    let root = inventory::checkout_root(checkout)?;
    // A workspace can sit anywhere inside a checkout; its sibling sits in the same place.
    let relative = checkout.strip_prefix(root).ok()?;
    let common = inventory::family(checkout)?;
    let mut best: Option<(u64, PathBuf)> = None;
    for sibling in inventory::checkouts(&common) {
        let sibling = sibling.canonicalize().unwrap_or(sibling);
        if sys::plain(&sibling) == sys::plain(root) {
            continue;
        }
        let Some(target) = eco.build_dir(&sibling.join(relative)) else {
            continue;
        };
        let Ok(profiles) = eco.units(&target) else {
            continue;
        };
        let built = profiles.iter().filter_map(|dir| eco.last_used(dir)).max();
        // A target nobody ever built is still better than nothing, hence `unwrap_or(0)`.
        let built = built.unwrap_or(0);
        if best.as_ref().is_none_or(|(seen, _)| built > *seen) {
            best = Some((built, target));
        }
    }
    best.map(|(_, target)| target)
}

/// One place in a checkout that a sibling checkout can fill: the project here, its adapter, and
/// the sibling's build dir for the same project.
pub struct Position {
    pub project: PathBuf,
    pub eco: &'static dyn Ecosystem,
    pub source: PathBuf,
}

/// Every position of `checkout` (a checkout root) that a sibling checkout of the same repository
/// can seed, sorted by project. A position counts when a sibling has a build dir at its adapter's
/// default place for a project, the same project dir exists in `checkout`, and it has no build
/// dir yet; a project absent on this branch is left out. Each position comes from the sibling
/// that built *it* most recently — no single checkout is the newest everywhere.
pub fn positions(checkout: &Path) -> Vec<Position> {
    let Some(common) = inventory::family(checkout) else {
        return Vec::new();
    };
    let mut best: Vec<(u64, Position)> = Vec::new();
    for sibling in inventory::checkouts(&common) {
        let sibling = sibling.canonicalize().unwrap_or(sibling);
        if sys::plain(&sibling) == sys::plain(checkout) {
            continue;
        }
        for (build_dir, eco) in eco::discover(std::slice::from_ref(&sibling)) {
            let Some(owner) = eco.owner(&build_dir) else {
                continue;
            };
            // A worktree nested inside this sibling is a sibling of its own.
            if inventory::checkout_root(&owner.project) != Some(sibling.as_path())
                || eco.build_dir(&owner.project).as_ref() != Some(&build_dir)
            {
                continue;
            }
            let Ok(relative) = owner.project.strip_prefix(&sibling) else {
                continue;
            };
            let project = checkout.join(relative);
            if !project.is_dir() || eco.build_dir(&project).is_none_or(|dir| dir.exists()) {
                continue;
            }
            let built = eco
                .units(&build_dir)
                .into_iter()
                .flatten()
                .filter_map(|unit| eco.last_used(&unit))
                .max()
                // A build dir nobody ever built is still better than nothing.
                .unwrap_or(0);
            let seen = best
                .iter_mut()
                .find(|(_, seen)| seen.project == project && seen.eco.name() == eco.name());
            match seen {
                Some((when, _)) if *when >= built => {}
                Some(slot) => {
                    *slot = (
                        built,
                        Position {
                            project,
                            eco,
                            source: build_dir,
                        },
                    )
                }
                None => best.push((
                    built,
                    Position {
                        project,
                        eco,
                        source: build_dir,
                    },
                )),
            }
        }
    }
    let mut positions: Vec<Position> = best.into_iter().map(|(_, position)| position).collect();
    positions.sort_by(|a, b| a.project.cmp(&b.project));
    positions
}

/// Copies `source` (a build dir) to where `eco` puts the build dir of `checkout`. The destination
/// must not exist yet: this seeds a fresh checkout and never merges into a build dir somebody is
/// already using.
///
/// The index gets the copies of every source file it already knows, marked shared on both
/// sides, so the next dedupe run leaves the pair alone.
pub fn seed(
    checkout: &Path,
    source: &Path,
    eco: &dyn Ecosystem,
    index: &mut HashIndex,
    dry_run: bool,
) -> io::Result<Seeded> {
    let target = eco.build_dir(checkout).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            format!("a {} build dir cannot be seeded", eco.name()),
        )
    })?;
    if target.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already has a target dir", checkout.display()),
        ));
    }
    // The build's lock per source unit, held for the whole walk: a build writing into the
    // source would otherwise be copied half way.
    let mut locks = Vec::new();
    let mut seeded = Seeded {
        source: source.to_path_buf(),
        // The destination decides: a clone cannot cross a device, and it is the new target that
        // has to be written.
        shared_blocks: crate::sys::caps(checkout).clone,
        ..Seeded::default()
    };
    // A guard shared by several units is tried once, as the engine does: a second try from this
    // process would find it held by the first.
    let mut shared: Vec<(Guard, bool)> = Vec::new();
    for dir in eco.units(source)? {
        let guard = eco.guard(&dir);
        let held = match shared.iter().find(|(seen, _)| *seen == guard) {
            Some(&(_, held)) => held,
            None => {
                let lock = ProfileLock::try_guard(&guard)?;
                let held = lock.is_some();
                locks.extend(lock);
                if matches!(guard, Guard::Shared(_)) {
                    shared.push((guard, held));
                }
                held
            }
        };
        if !held {
            seeded.busy.push(dir);
        }
    }

    let skipped = |entry: &walkdir::DirEntry| {
        let name = entry.file_name();
        let busy = seeded.busy.iter().any(|dir| entry.path().starts_with(dir));
        busy || eco.volatile(name)
            || eco.private(name)
            || name.to_string_lossy().starts_with(TMP_PREFIX)
    };
    let walk = WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !skipped(entry));
    for entry in walk {
        let entry = entry?;
        let Ok(relative) = entry.path().strip_prefix(source) else {
            continue;
        };
        let to = target.join(relative);
        let kind = entry.file_type();
        if kind.is_dir() {
            if !dry_run {
                fs::create_dir_all(&to)?;
            }
        } else if kind.is_symlink() {
            seeded.symlinks += 1;
            if !dry_run {
                sys::symlink(&fs::read_link(entry.path())?, &to)?;
            }
        } else {
            let metadata = entry.metadata()?;
            seeded.files += 1;
            seeded.bytes += sys::allocated(entry.path(), &metadata);
            if !dry_run {
                // A clone where the filesystem has them: the copy shares the blocks until
                // one side is written. Where it does not, a real copy — the point of seeding is
                // the build it saves, and that holds either way.
                if seeded.shared_blocks {
                    sys::clone_file(entry.path(), &to)?;
                } else {
                    fs::copy(entry.path(), &to)?;
                }
                if seeded.shared_blocks {
                    register(index, entry.path(), &metadata, &to);
                }
            }
        }
    }
    Ok(seeded)
}

/// Both sides of a clone hold the same bytes, so the copy inherits the source's hash and both
/// are marked shared. A source the index has never hashed stays unknown: seeding must not read
/// gigabytes to fill an index that the next `run` fills anyway.
fn register(index: &mut HashIndex, source_path: &Path, source: &fs::Metadata, copy: &Path) {
    let Ok(from) = Stamp::of(source_path, source) else {
        return;
    };
    let Some(hash) = index.get(&from).map(|entry| entry.hash) else {
        return;
    };
    index.mark_shared(&from);
    if let Ok(stamp) = Stamp::read(copy) {
        index.put(&stamp, hash, true);
    }
}
