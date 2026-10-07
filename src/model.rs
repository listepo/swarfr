//! Inode model of a unit (a cargo profile dir): every regular file, grouped by inode.

use std::collections::HashMap;
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use walkdir::WalkDir;

use crate::eco::Ecosystem;
use crate::sys;

/// Prefix of our temp files. Leftovers of a crashed run are removed by the next one.
pub const TMP_PREFIX: &str = ".swarfr-tmp-";

/// Identity and version of a file. Any rewrite by the build changes it.
///
/// `dev` and `ino` are whatever [`sys::file_id`] means by identity on this platform: a real
/// device and inode where the filesystem has them. `ino` is 128-bit because a ReFS file id is;
/// on every other platform the high half is zero.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub dev: u64,
    pub ino: u128,
    pub size: u64,
    pub mtime: SystemTime,
}

impl Stamp {
    /// `path` is the one the metadata was read through. On Windows the file id is read from
    /// it; everywhere else the metadata already holds the inode.
    pub fn of(path: &Path, meta: &Metadata) -> io::Result<Self> {
        let (dev, ino) = sys::file_id(path, meta);
        Ok(Self {
            dev,
            ino,
            size: meta.len(),
            mtime: meta.modified()?,
        })
    }

    /// Never follows a symlink: a path swapped for a link reads as changed.
    pub fn read(path: &Path) -> io::Result<Self> {
        Self::of(path, &fs::symlink_metadata(path)?)
    }
}

/// One inode with every path to it found inside the profile dir.
#[derive(Clone, Debug)]
pub struct Inode {
    pub stamp: Stamp,
    pub mode: u32,
    /// Filesystem flags, as [`sys::flags`] reads them: the `COMPRESSED` bit and the ones that
    /// mean the file is not ours to rewrite. 0 where the platform reports none.
    pub flags: u32,
    /// Link count from the filesystem. More than `paths.len()` means links we did not find.
    pub nlink: u64,
    /// Allocated bytes, not logical length.
    pub allocated: u64,
    pub paths: Vec<PathBuf>,
}

impl Inode {
    fn of(path: &Path, meta: &Metadata) -> io::Result<Self> {
        Ok(Self {
            stamp: Stamp::of(path, meta)?,
            mode: sys::mode(meta),
            flags: sys::flags(path, meta),
            nlink: sys::nlink(path, meta),
            allocated: sys::allocated(path, meta),
            paths: Vec::new(),
        })
    }

    /// The inode at `path` as it is now, knowing only this one path. Never follows a symlink.
    pub fn read(path: &Path) -> io::Result<Self> {
        let mut inode = Self::of(path, &fs::symlink_metadata(path)?)?;
        inode.paths.push(path.to_path_buf());
        Ok(inode)
    }
}

#[derive(Debug)]
pub struct Profile {
    pub dir: PathBuf,
    pub inodes: Vec<Inode>,
    pub stale_temps: Vec<PathBuf>,
    /// When a build last worked here, as the adapter tells it, read at scan time: under the
    /// unit's guard when the engine scans.
    pub last_used: Option<u64>,
}

/// Walks one unit. Symlinks are not followed, other devices are not entered, and the build's
/// private files and dirs are left out.
pub fn scan(dir: &Path, eco: &dyn Ecosystem) -> io::Result<Profile> {
    let mut by_inode: HashMap<(u64, u128), Inode> = HashMap::new();
    let mut stale_temps = Vec::new();
    let walk = WalkDir::new(dir)
        .follow_links(false)
        .same_file_system(true)
        .into_iter()
        .filter_entry(|entry| entry.depth() == 0 || !eco.private(entry.file_name()));
    for entry in walk {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.file_name().to_string_lossy().starts_with(TMP_PREFIX) {
            stale_temps.push(entry.into_path());
            continue;
        }
        let inode = Inode::of(entry.path(), &entry.metadata()?)?;
        by_inode
            .entry((inode.stamp.dev, inode.stamp.ino))
            .or_insert(inode)
            .paths
            .push(entry.into_path());
    }
    let mut inodes: Vec<Inode> = by_inode.into_values().collect();
    for inode in &mut inodes {
        inode.paths.sort();
    }
    inodes.sort_by(|a, b| a.paths.cmp(&b.paths));
    Ok(Profile {
        dir: dir.to_path_buf(),
        inodes,
        stale_temps,
        last_used: eco.last_used(dir),
    })
}
