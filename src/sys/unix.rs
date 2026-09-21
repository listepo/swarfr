//! Unix that is not macOS, which in practice is Linux.
//!
//! Sharing and compression are filesystem questions here, not platform ones: btrfs does both,
//! XFS shares blocks when it was made with `reflink=1`, ext4 does neither, and btrfs mounted
//! `nodatacow` does neither either. [`caps`] therefore asks the filesystem rather than its name,
//! by trying the two calls on a probe file and cleaning up after itself.
//!
//! Compression is `FS_COMPR_FL`, which btrfs applies to new writes only, so the flag alone
//! shrinks nothing: the compressor sets it on the engine's private copy and then rewrites the
//! copy through itself. That is exactly what the engine already hands it — a clone nobody else
//! can see — so the rewrite costs a copy that was going to be made anyway.
//!
//! The two halves of the probe are answered differently, because only one of the two calls
//! tells the truth. `FICLONE` does: it fails on a filesystem that cannot share blocks, which is
//! exactly the question. `FS_IOC_SETFLAGS` does not: ext4 accepts `FS_COMPR_FL`, keeps it where
//! `lsattr` shows it, and stores the bytes unchanged — measured in the Linux VM, 200 MiB
//! written with the flag set took 200 MiB on disk. Measuring the file afterwards does not
//! rescue it either, because btrfs reports the *uncompressed* size in `st_blocks`: the same
//! 200 MiB really did take 6 MiB there, and `stat` said 200. So compression is decided by the
//! one thing that is not a lie, the filesystem's own magic number — `FS_COMPR_FL` is a btrfs
//! feature, and ext4 and XFS ignore it by design.

use std::collections::HashMap;
use std::fs::{self, File, Metadata, Permissions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use rustix::fs::{IFlags, ioctl_ficlone, ioctl_getflags, ioctl_setflags};

use super::Caps;

/// `FS_COMPR_FL` from `<linux/fs.h>`: new writes to this file are compressed. A unit test holds
/// it to `IFlags::COMPRESSED`, which is where the number comes from.
pub const COMPRESSED: u32 = 0x4;

/// `st_blocks` counts 512-byte units whatever the filesystem block size is.
const ST_BLOCK_BYTES: u64 = 512;
const PERMISSION_BITS: u32 = 0o7777;
/// Flags that say "not ours to rewrite", plus `COMPRESSED` itself. `NOCOW` is among them: a file
/// that is not copy-on-write cannot be reflinked, so a plan that includes it is a plan that
/// would fail. Everything else Linux keeps here (`NOATIME`, `NODUMP`, `SYNC`, …) says nothing
/// about whether a file may be replaced, and a flag we do not understand must not become a skip.
/// Flags under which a file cannot be removed or changed.
pub const PROTECTED: u32 = IFlags::IMMUTABLE.union(IFlags::APPEND).bits();
const KNOWN: IFlags = IFlags::COMPRESSED
    .union(IFlags::IMMUTABLE)
    .union(IFlags::APPEND)
    .union(IFlags::NOCOW);
/// Big enough that a filesystem has something to share, small enough to cost nothing: `FICLONE`
/// works on whole files, and an empty one would tell us nothing.
const PROBE_BYTES: usize = 64 * 1024;
/// btrfs reports the *uncompressed* size in `st_blocks` — 200 MiB written with `FS_COMPR_FL`
/// set took 6 MiB on the volume and still stat-ed as 200 MiB. So the compression win is real
/// and none of the per-file figures show it: `freed_bytes` reads 0 for every file compressed
/// here, and so does `du`. `df` on the volume is where it shows, `compsize` per file.
pub const ALLOCATED_SHOWS_COMPRESSION: bool = false;

/// `BTRFS_SUPER_MAGIC`. The one filesystem that acts on `FS_COMPR_FL`.
const BTRFS_MAGIC: i64 = 0x9123_683e;

pub fn file_id(_path: &Path, meta: &Metadata) -> (u64, u64) {
    (meta.dev(), meta.ino())
}

pub fn nlink(meta: &Metadata) -> u64 {
    meta.nlink()
}

pub fn allocated(meta: &Metadata) -> u64 {
    meta.blocks() * ST_BLOCK_BYTES
}

/// Linux keeps per-file flags behind an ioctl, not in the stat the caller already has, so
/// reading them costs an `open` per file. That is paid only where a flag could be set by us in
/// the first place: on a filesystem without compression there is nothing to find, and the scan
/// of a large target dir should not open 70 000 files to be told so.
pub fn flags(path: &Path, meta: &Metadata) -> u32 {
    if !caps_of(meta.dev(), path).compress {
        return 0;
    }
    read_flags(path).map_or(0, |flags| (flags & KNOWN).bits())
}

pub fn mode(meta: &Metadata) -> u32 {
    meta.mode() & PERMISSION_BITS
}

pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    fs::set_permissions(path, Permissions::from_mode(mode))
}

pub fn symlink(original: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(original, link)
}

/// `FICLONE`, not `fs::copy`: a filesystem that cannot share blocks must fail here rather than
/// write a second copy of them quietly. The callers that want a copy either way — seeding a new
/// worktree — fall back on their own.
pub fn clone_file(source: &Path, destination: &Path) -> io::Result<()> {
    let source = File::open(source)?;
    let destination = File::create(destination)?;
    Ok(ioctl_ficlone(&destination, &source)?)
}

/// btrfs compression, applied by rewriting the file with `FS_COMPR_FL` set.
#[derive(Default)]
pub struct Compressor {
    notes: Mutex<Vec<String>>,
}

impl Compressor {
    pub fn new() -> Self {
        Self::default()
    }

    /// The engine hands private copies only, so rewriting one in place disturbs nothing.
    pub fn compress(&self, copies: &[PathBuf]) {
        for copy in copies {
            if let Err(e) = compress_file(copy)
                && let Ok(mut notes) = self.notes.lock()
            {
                notes.push(format!("{}: {e}", copy.display()));
            }
        }
    }

    pub fn notes(&self) -> Vec<String> {
        self.notes.lock().map(|n| n.clone()).unwrap_or_default()
    }
}

/// What the filesystem under `dir` can do, found out by doing it. Cached per device: a run over
/// forty target dirs on one disk probes once.
pub fn caps(dir: &Path) -> Caps {
    match fs::metadata(dir) {
        Ok(meta) => caps_of(meta.dev(), dir),
        // A directory we cannot even stat is not one we may claim a capability for.
        Err(_) => Caps::NONE,
    }
}

fn cache() -> &'static Mutex<HashMap<u64, Caps>> {
    static CACHE: OnceLock<Mutex<HashMap<u64, Caps>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The cached answer for a device, probed inside `near` on the first ask. `near` is a path on
/// that device: a directory to probe in, or a file whose parent is one.
fn caps_of(dev: u64, near: &Path) -> Caps {
    if let Ok(cache) = cache().lock()
        && let Some(caps) = cache.get(&dev)
    {
        return *caps;
    }
    let dir = if near.is_dir() {
        near
    } else {
        near.parent().unwrap_or(near)
    };
    let caps = probe(dir);
    if let Ok(mut cache) = cache().lock() {
        cache.insert(dev, caps);
    }
    caps
}

/// Two temp files, one `FICLONE` and one `FS_IOC_SETFLAGS`, both removed again. A probe that
/// cannot be written at all answers [`Caps::NONE`]: a read-only or full filesystem is not one
/// the passes could write to either.
fn probe(dir: &Path) -> Caps {
    super::probing_in(dir, || {
        let source = super::probe_path(dir);
        let copy = super::probe_path(dir);
        let caps = probe_with(&source, &copy).unwrap_or(Caps::NONE);
        let _ = fs::remove_file(&source);
        let _ = fs::remove_file(&copy);
        caps
    })
}

fn probe_with(source: &Path, copy: &Path) -> io::Result<Caps> {
    fs::write(source, vec![0; PROBE_BYTES])?;
    Ok(Caps {
        // A real attempt: it fails on ext4 and on a btrfs mounted `nodatacow`, which is exactly
        // the question being asked.
        clone: clone_file(source, copy).is_ok(),
        // Not an attempt, because the attempt lies — see the module header. The flag still has
        // to be settable: a read-only mount takes neither pass anywhere.
        compress: is_btrfs(source).unwrap_or(false) && set_compress_flag(source).is_ok(),
    })
}

fn is_btrfs(path: &Path) -> io::Result<bool> {
    Ok(rustix::fs::statfs(path)?.f_type as i64 == BTRFS_MAGIC)
}

fn read_flags(path: &Path) -> io::Result<IFlags> {
    Ok(ioctl_getflags(&File::open(path)?)?)
}

fn set_compress_flag(path: &Path) -> io::Result<()> {
    let file = File::options().read(true).write(true).open(path)?;
    let flags = ioctl_getflags(&file)?;
    Ok(ioctl_setflags(&file, flags | IFlags::COMPRESSED)?)
}

/// btrfs compresses what is written after the flag is set, so the flag alone changes nothing on
/// a file that already holds its bytes. Rewriting it through itself is what makes the extents
/// compressed ones — the same thing `btrfs filesystem defragment -c` does, for one file.
///
/// The win is real and invisible to `stat`: btrfs reports the uncompressed size in `st_blocks`,
/// so the engine's `freed_bytes` reads 0 for every file compressed here. `df` on the volume is
/// where it shows, and `compsize` is where it shows per file. `docs/bench.md` says so too.
///
// ponytail: the whole copy is held in memory, which is fine for artifacts (the largest in the
// measured targets is ~200 MiB) and wrong for a hypothetical huge one. Rewrite in chunks
// through a second temp if a benchmark ever meets one.
fn compress_file(path: &Path) -> io::Result<()> {
    let mut file = File::options().read(true).write(true).open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    set_compress_flag(path)?;
    // Truncate before writing, so the extents that hold the uncompressed bytes are released
    // instead of being overwritten in place.
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&bytes)?;
    file.sync_all()
}

/// The current dirs of the running processes called one of `tools`, from `/proc`; open files
/// are not looked at. `None` where
/// there is no `/proc`. A name longer than the kernel keeps (15 bytes) matches on its prefix.
pub fn tool_paths(tools: &[&str]) -> Option<Vec<super::Held>> {
    const COMM_LEN: usize = 15;
    let mut cwds = Vec::new();
    for entry in fs::read_dir("/proc").ok()? {
        let Ok(entry) = entry else { continue };
        if !entry
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|b| b.is_ascii_digit())
        {
            continue;
        }
        // A process can end between the listing and the read.
        let Ok(comm) = fs::read_to_string(entry.path().join("comm")) else {
            continue;
        };
        let comm = comm.trim_end_matches('\n');
        if !tools
            .iter()
            .any(|tool| tool.get(..tool.len().min(COMM_LEN)) == Some(comm))
        {
            continue;
        }
        if let Ok(cwd) = fs::read_link(entry.path().join("cwd")) {
            cwds.push(super::Held::Cwd(cwd));
        }
    }
    Some(cwds)
}

/// What every process of `dir`'s owner holds, from `/proc`: the current dir, what each descriptor
/// names and every mapped file. A socket or a pipe names no path, and a file deleted while open
/// is named with ` (deleted)` after it, which matches nothing. `None` where there is no `/proc`.
pub fn user_paths(dir: &Path) -> Option<Vec<PathBuf>> {
    let uid = fs::metadata(dir).ok()?.uid();
    let mut paths = Vec::new();
    for entry in fs::read_dir("/proc").ok()? {
        let Ok(entry) = entry else { continue };
        let proc = entry.path();
        let is_pid = entry
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|b| b.is_ascii_digit());
        // A process can end between the listing and the reads; its paths go with it.
        if !is_pid || fs::metadata(&proc).map(|meta| meta.uid()).ok() != Some(uid) {
            continue;
        }
        paths.extend(fs::read_link(proc.join("cwd")));
        if let Ok(fds) = fs::read_dir(proc.join("fd")) {
            for fd in fds.flatten() {
                paths.extend(
                    fs::read_link(fd.path())
                        .into_iter()
                        .filter(|p| p.is_absolute()),
                );
            }
        }
        if let Ok(maps) = fs::read_to_string(proc.join("maps")) {
            // `address perms offset dev inode path`, the path from the sixth field on.
            paths.extend(maps.lines().filter_map(|line| {
                let path = line.splitn(6, ' ').nth(5)?.trim_start();
                path.starts_with('/').then(|| PathBuf::from(path))
            }));
        }
    }
    Some(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The constant the engine masks with must be the flag the kernel means, and nothing here
    /// would notice if it drifted.
    #[test]
    fn the_compressed_bit_is_the_kernels_own() {
        assert_eq!(COMPRESSED, IFlags::COMPRESSED.bits());
    }
}
