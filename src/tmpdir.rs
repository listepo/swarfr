//! The lossy `tmpdir` pass: whatever other programs left in the per-user temp dir. A top-level
//! entry goes when nothing anywhere inside it was modified for the days the caller asks for, and
//! no process holds a path in it. Everything old counts, not only what build tools leave: the
//! temp dir is by its name a place nobody keeps anything in.
//!
//! An entry stays, whatever its age, when:
//!
//! - any process of the user has a current dir, an open file or a mapped file inside it;
//! - it holds a socket — Linux names a listening socket by inode only, so the open-files check
//!   cannot see a server that still listens there;
//! - its walk meets another filesystem, a file it may not read, an mtime in the future, or a
//!   flag that forbids removal (macOS guards its own services' temp dirs with `sunlnk`);
//! - it holds a path the caller keeps (the hash index, if someone put it there).
//!
//! When nobody can say which paths processes hold, nothing is removed at all. An old entry is
//! walked again right before it goes, so one written into meanwhile stays.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use walkdir::WalkDir;

use crate::sys;

pub const NAME: &str = "tmpdir";
const SECS_PER_DAY: u64 = 24 * 60 * 60;
/// The owner's write bit, lifted on a dir whose contents a removal could not unlink.
const OWNER_WRITE: u32 = 0o200;

/// An entry that went, or on a dry run would.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub path: PathBuf,
    /// The newest mtime inside, as seconds since the epoch.
    pub newest_unix: u64,
    pub allocated_bytes: u64,
}

/// Why an entry older than the limit stayed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kept {
    /// A process holds a path inside it.
    InUse,
    Socket,
    OtherFilesystem,
    /// Something in it carries a flag that forbids removal (macOS's `sunlnk`, `uchg`, …).
    Protected,
    /// Its walk could not read something, or met an mtime in the future.
    Unreadable,
    /// It holds a path the caller keeps.
    Kept,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
    pub dir: PathBuf,
    pub idle_days: u64,
    pub removed: Vec<Entry>,
    /// Old entries that stayed, and why.
    pub kept: Vec<(PathBuf, Kept)>,
    /// Entries with something inside modified within the limit.
    pub young: usize,
    /// Entries a removal failed on, part of them possibly gone.
    pub failed: Vec<(PathBuf, String)>,
    /// Nobody could say which paths processes hold, so nothing was removed.
    pub unsure: bool,
}

impl Report {
    pub fn freed_bytes(&self) -> u64 {
        self.removed.iter().map(|entry| entry.allocated_bytes).sum()
    }
}

/// Why `dir` cannot be a temp dir to clean; `None` when it can.
pub fn check(dir: &Path) -> Option<&'static str> {
    if dir.parent().is_none() {
        Some("a filesystem root is not a temp dir")
    } else if !dir.is_dir() {
        Some("not a dir")
    } else {
        None
    }
}

/// Removes, or on a dry run lists, every top-level entry of `dir` that is old and unused.
/// `dir` is canonical; `in_use` is every path a process of the user holds, `None` when that is
/// unknown; `keep` are canonical paths whose entries stay.
pub fn run(
    dir: &Path,
    idle_days: u64,
    in_use: Option<&[PathBuf]>,
    keep: &[PathBuf],
    now: SystemTime,
    dry_run: bool,
) -> io::Result<Report> {
    let mut report = Report {
        dir: dir.to_path_buf(),
        idle_days,
        ..Report::default()
    };
    let Some(in_use) = in_use else {
        report.unsure = true;
        return Ok(report);
    };
    // The top-level names held, by whatever path inside them.
    let held: HashSet<OsString> = in_use
        .iter()
        .filter_map(|path| path.strip_prefix(dir).ok()?.components().next())
        .map(|name| name.as_os_str().to_owned())
        .collect();
    let cutoff = now
        .checked_sub(Duration::from_secs(idle_days.saturating_mul(SECS_PER_DAY)))
        .unwrap_or(UNIX_EPOCH);
    let mut names: Vec<OsString> = fs::read_dir(dir)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<_>>()?;
    names.sort();
    for name in names {
        let path = dir.join(&name);
        if held.contains(&name) {
            report.kept.push((path, Kept::InUse));
            continue;
        }
        if keep.iter().any(|kept| kept.starts_with(&path)) {
            report.kept.push((path, Kept::Kept));
            continue;
        }
        let mut scanned = scan(&path, cutoff, now);
        if let Scan::Old { .. } = scanned
            && !dry_run
        {
            // Walked again right before it goes: a program may have come back to it.
            scanned = scan(&path, cutoff, now);
        }
        match scanned {
            Scan::Young => report.young += 1,
            Scan::Kept(why) => report.kept.push((path, why)),
            Scan::Old { newest, bytes } => {
                let entry = Entry {
                    path,
                    newest_unix: newest
                        .duration_since(UNIX_EPOCH)
                        .map_or(0, |since| since.as_secs()),
                    allocated_bytes: bytes,
                };
                match if dry_run { Ok(()) } else { remove(&entry.path) } {
                    Ok(()) => report.removed.push(entry),
                    Err(err) => report.failed.push((entry.path, err.to_string())),
                }
            }
        }
    }
    Ok(report)
}

enum Scan {
    Young,
    Kept(Kept),
    Old { newest: SystemTime, bytes: u64 },
}

/// One walk of an entry, symlinks not followed, stopping at the first thing that keeps it.
fn scan(path: &Path, cutoff: SystemTime, now: SystemTime) -> Scan {
    let Ok(top) = fs::symlink_metadata(path) else {
        return Scan::Kept(Kept::Unreadable);
    };
    let device = sys::file_id(path, &top).0;
    let (mut newest, mut bytes) = (UNIX_EPOCH, 0);
    for entry in WalkDir::new(path).follow_root_links(false) {
        let Some((at, meta)) = entry
            .ok()
            .and_then(|entry| Some((entry.path().to_path_buf(), entry.metadata().ok()?)))
        else {
            return Scan::Kept(Kept::Unreadable);
        };
        let Ok(modified) = meta.modified() else {
            return Scan::Kept(Kept::Unreadable);
        };
        if modified > cutoff {
            // Written within the limit, or dated in the future: either way not idle.
            return if modified > now {
                Scan::Kept(Kept::Unreadable)
            } else {
                Scan::Young
            };
        }
        if is_socket(&meta) {
            return Scan::Kept(Kept::Socket);
        }
        if sys::flags(&at, &meta) & sys::PROTECTED != 0 {
            return Scan::Kept(Kept::Protected);
        }
        if sys::file_id(path, &meta).0 != device {
            return Scan::Kept(Kept::OtherFilesystem);
        }
        newest = newest.max(modified);
        bytes += sys::allocated(&meta);
    }
    Scan::Old { newest, bytes }
}

#[cfg(unix)]
fn is_socket(meta: &Metadata) -> bool {
    std::os::unix::fs::FileTypeExt::is_socket(&meta.file_type())
}

#[cfg(not(unix))]
fn is_socket(_meta: &Metadata) -> bool {
    false
}

/// A file or a link goes by itself; a dir with everything in it, never following a link out.
/// A dir its owner made read-only (a Go module cache does) gets the write bit back and one more
/// try.
fn remove(path: &Path) -> io::Result<()> {
    if !fs::symlink_metadata(path)?.is_dir() {
        return fs::remove_file(path);
    }
    match fs::remove_dir_all(path) {
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            for entry in WalkDir::new(path)
                .follow_root_links(false)
                .into_iter()
                .flatten()
            {
                if entry.file_type().is_dir()
                    && let Ok(meta) = entry.metadata()
                {
                    let _ = sys::set_mode(entry.path(), sys::mode(&meta) | OWNER_WRITE);
                }
            }
            fs::remove_dir_all(path)
        }
        done => done,
    }
}
