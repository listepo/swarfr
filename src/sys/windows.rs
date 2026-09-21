//! Windows, with the standard library only.
//!
//! This is the floor: the crate builds, walks a target dir and reports what is there. What it
//! cannot do yet is anything that needs a handle — NTFS compression (`FSCTL_SET_COMPRESSION`),
//! the size on disk (`GetCompressedFileSize`), ReFS block cloning
//! (`FSCTL_DUPLICATE_EXTENTS_TO_FILE`) and the real file identity plus link count
//! (`GetFileInformationByHandle`). All of that is `T21`, where it can be written with a machine
//! to test it on; writing untestable `unsafe` FFI ahead of that machine is how it goes wrong.
//! Until then [`caps`] finds nothing, so no pass plans a single action here.

use super::Caps;

use std::fs::{self, Metadata};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// `FILE_ATTRIBUTE_COMPRESSED`: NTFS is holding this file compressed.
pub const COMPRESSED: u32 = 0x800;

/// `allocated` is the logical length here until `GetCompressedFileSize` lands (`T21`), so it
/// shows nothing about compression — and nothing compresses yet either.
pub const ALLOCATED_SHOWS_COMPRESSION: bool = false;

/// Attributes that mean "not ours to rewrite", plus `COMPRESSED` itself. Everything else NTFS
/// reports — `ARCHIVE` on very nearly every file, `NOT_CONTENT_INDEXED`, `TEMPORARY` — says
/// nothing about whether a file may be replaced, and must not turn into a blanket skip.
const READONLY: u32 = 0x1;
const HIDDEN: u32 = 0x2;
const SYSTEM: u32 = 0x4;
const REPARSE_POINT: u32 = 0x400;
const KNOWN: u32 = COMPRESSED | READONLY | HIDDEN | SYSTEM | REPARSE_POINT;

/// No inode number is reachable without a handle, so identity is the path: distinct paths are
/// distinct files. Two hardlinks to one file therefore look like two files — which is only ever
/// a count in a report while [`caps`] finds nothing here, because nothing plans work from it.
pub fn file_id(path: &Path, _meta: &Metadata) -> (u64, u64) {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    (0, hasher.finish())
}

/// See [`file_id`]: links are invisible from a path, and one is the count that keeps the
/// engine's "every link is one we found" check honest about what is known here.
pub fn nlink(_meta: &Metadata) -> u64 {
    1
}

/// The logical length. A compressed or sparse file occupies less; `GetCompressedFileSize` is
/// what reports that, and it is `T21`.
pub fn allocated(meta: &Metadata) -> u64 {
    meta.file_size()
}

pub fn flags(_path: &Path, meta: &Metadata) -> u32 {
    meta.file_attributes() & KNOWN
}

/// Nothing is claimed here yet. NTFS compresses and ReFS clones, both through an `FSCTL` on an
/// open handle, and both are `T21`; until then no probe can find a capability this module does
/// not have, so none is run — not even a write test, because a capability of `NONE` is the
/// answer either way.
/// Not told here: finding a process's current dir takes reading its memory. Every quiet unit
/// stays unsure.
pub fn tool_paths(_tools: &[&str]) -> Option<Vec<super::Held>> {
    None
}

pub fn caps(_dir: &Path) -> Caps {
    Caps::NONE
}

/// Windows permissions live in the ACL, not in a mode. Nothing is replaced here, so nothing
/// needs restoring; `fs::copy` carries the attributes across on its own.
pub fn mode(_meta: &Metadata) -> u32 {
    0
}

pub fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// Needs Developer Mode or `SeCreateSymbolicLinkPrivilege`; without it the caller gets the
/// error, which is the truth and better than a copy pretending to be a link.
pub fn symlink(original: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(original, link)
}

/// A plain copy: NTFS shares no blocks, and ReFS needs `FSCTL_DUPLICATE_EXTENTS_TO_FILE`
/// (`T21`). Seeding a target still saves the build it would otherwise cost.
pub fn clone_file(source: &Path, destination: &Path) -> io::Result<()> {
    fs::copy(source, destination).map(|_| ())
}

/// Nothing to drive until `FSCTL_SET_COMPRESSION` is wired up; the compress pass plans no work
/// at all ([`caps`]).
#[derive(Default)]
pub struct Compressor;

impl Compressor {
    pub fn new() -> Self {
        Self
    }

    pub fn compress(&self, _copies: &[PathBuf]) {}

    pub fn notes(&self) -> Vec<String> {
        Vec::new()
    }
}
