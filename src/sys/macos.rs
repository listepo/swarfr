//! macOS on APFS: `clonefile` for copies that share their blocks, and transparent compression
//! through applesauce, which is what the tool was measured on.

use std::cell::RefCell;
use std::fs::{self, Metadata, Permissions};
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::Caps;
use applesauce::FileCompressor;
use applesauce::compressor::Kind;
use applesauce::progress::{Progress, SkipReason, Task};

/// `UF_COMPRESSED` from `<sys/stat.h>`: the file is transparently compressed.
pub const COMPRESSED: u32 = 0x20;

/// APFS charges the compressed size in `st_blocks`, so every figure the reports print — `du`'s
/// number, the engine's `freed_bytes` — shows the compression win by itself.
pub const ALLOCATED_SHOWS_COMPRESSION: bool = true;

/// `st_blocks` counts 512-byte units whatever the filesystem block size is.
const ST_BLOCK_BYTES: u64 = 512;
const PERMISSION_BITS: u32 = 0o7777;
/// LZFSE: the best ratio of the three codecs at a read speed T2 could not tell from plain.
const KIND: Kind = Kind::Lzfse;
/// The backend's own defaults.
const LEVEL: u32 = 5;
/// A copy that does not get below this share of its size stays uncompressed.
const MIN_RATIO: f64 = 0.95;
/// The backend reads the compressed copy back and compares it before it lets it stand.
const VERIFY: bool = true;

/// Device and inode: the pair that says "the same file", whatever path it was reached by. The
/// path is what platforms without an inode number need; here it is not read.
pub fn file_id(_path: &Path, meta: &Metadata) -> (u64, u64) {
    (meta.dev(), meta.ino())
}

pub fn nlink(meta: &Metadata) -> u64 {
    meta.nlink()
}

pub fn allocated(meta: &Metadata) -> u64 {
    meta.blocks() * ST_BLOCK_BYTES
}

/// BSD file flags (`st_flags`): `COMPRESSED`, and the ones that are not ours to drop. They sit
/// in the stat the caller already has, so the path is not read.
pub fn flags(_path: &Path, meta: &Metadata) -> u32 {
    std::os::macos::fs::MetadataExt::st_flags(meta)
}

/// APFS clones and compresses, and it is what every measurement in `docs/bench.md` was taken
/// on; the probe only asks whether the directory can be written to at all, because a capability
/// nothing can be tried in is not one we may claim.
///
// ponytail: an HFS+ or SMB volume on a Mac would get `true` here and lose the dedupe pass its
// gain (`fs::copy` falls back to a byte copy silently). Call `fclonefileat` on a probe file the
// way `unix.rs` calls `FICLONE` if such a volume ever turns up in a benchmark.
pub fn caps(dir: &Path) -> Caps {
    super::probing_in(dir, || {
        let probe = super::probe_path(dir);
        if fs::write(&probe, b"swarfr").is_err() {
            return Caps::NONE;
        }
        let _ = fs::remove_file(&probe);
        Caps {
            clone: true,
            compress: true,
        }
    })
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

/// A copy that shares its blocks with the source until one of them is written.
// ponytail: `fs::copy` clones through `fclonefileat` on APFS and quietly falls back to a byte
// copy elsewhere (correct, just saves nothing). Call `fclonefileat` via rustix if a silent
// fallback ever needs to be an error.
pub fn clone_file(source: &Path, destination: &Path) -> io::Result<()> {
    fs::copy(source, destination).map(|_| ())
}

/// The applesauce backend, kept alive between batches, with what it refused and why.
#[derive(Default)]
pub struct Compressor {
    backend: RefCell<Option<FileCompressor>>,
    notes: Notes,
}

impl Compressor {
    pub fn new() -> Self {
        Self::default()
    }

    /// The engine hands private copies only, so a refusal costs nothing but the note.
    // ponytail: a copy the backend refuses is tried again on every run; remember refusals in
    // the index if the benchmarks show them.
    pub fn compress(&self, copies: &[PathBuf]) {
        let mut backend = self.backend.borrow_mut();
        backend
            .get_or_insert_with(FileCompressor::new)
            .recursive_compress(
                copies.iter().map(PathBuf::as_path),
                KIND,
                MIN_RATIO,
                LEVEL,
                &self.notes,
                VERIFY,
            );
    }

    /// One line per copy the backend did not compress. The paths are the engine's temp copies:
    /// the dir tells where, the engine's report tells which.
    pub fn notes(&self) -> Vec<String> {
        self.notes.0.lock().map(|n| n.clone()).unwrap_or_default()
    }
}

#[derive(Clone, Default)]
struct Notes(Arc<Mutex<Vec<String>>>);

impl Notes {
    fn push(&self, path: &Path, message: &str) {
        if let Ok(mut notes) = self.0.lock() {
            notes.push(format!("{}: {message}", path.display()));
        }
    }
}

impl Progress for Notes {
    type Task = FileNotes;

    fn error(&self, path: &Path, message: &str) {
        self.push(path, message);
    }

    fn file_skipped(&self, path: &Path, why: SkipReason) {
        self.push(path, &why.to_string());
    }

    fn file_task(&self, path: &Path, _size: u64) -> FileNotes {
        FileNotes {
            notes: self.clone(),
            path: path.to_path_buf(),
        }
    }
}

struct FileNotes {
    notes: Notes,
    path: PathBuf,
}

impl Task for FileNotes {
    fn increment(&self, _amt: u64) {}

    fn error(&self, message: &str) {
        self.notes.push(&self.path, message);
    }

    fn not_compressible_enough(&self, path: &Path) {
        self.notes.push(path, "not compressible enough");
    }
}

/// The current dirs of the running processes called one of `tools`: `ps` for the names, `lsof`
/// for the dirs. `None` when either cannot run.
pub fn tool_cwds(tools: &[&str]) -> Option<Vec<PathBuf>> {
    let ps = std::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,comm="])
        .output()
        .ok()?;
    if !ps.status.success() {
        return None;
    }
    let pids: Vec<&str> = std::str::from_utf8(&ps.stdout)
        .ok()?
        .lines()
        .filter_map(|line| line.trim_start().split_once(' '))
        .filter(|(_, comm)| {
            let name = Path::new(comm.trim()).file_name();
            tools
                .iter()
                .any(|tool| name == Some(std::ffi::OsStr::new(tool)))
        })
        .map(|(pid, _)| pid)
        .collect();
    if pids.is_empty() {
        return Some(Vec::new());
    }
    // `-a`: both conditions; `-Fn`: one `n<path>` line per current dir.
    let lsof = std::process::Command::new("/usr/sbin/lsof")
        .args(["-a", "-d", "cwd", "-Fn", "-p", &pids.join(",")])
        .output()
        .ok()?;
    // lsof fails when one of the processes is gone or not ours, and still lists the rest.
    if !lsof.status.success() && lsof.stdout.is_empty() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&lsof.stdout)
            .lines()
            .filter_map(|line| line.strip_prefix('n'))
            .map(PathBuf::from)
            .collect(),
    )
}

/// `DARWIN_USER_TEMP_DIR`: the per-user temp dir, as `getconf` reads it from `confstr`.
pub fn user_temp_dir() -> Option<PathBuf> {
    let out = std::process::Command::new("/usr/bin/getconf")
        .arg("DARWIN_USER_TEMP_DIR")
        .output()
        .ok()?;
    let dir = String::from_utf8(out.stdout).ok()?;
    let dir = dir.trim();
    (out.status.success() && !dir.is_empty()).then(|| PathBuf::from(dir))
}
