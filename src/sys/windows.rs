//! Windows: NTFS compression and ReFS block cloning.
//!
//! The two never meet on one volume. NTFS compresses a file with `FSCTL_SET_COMPRESSION`
//! (LZNT1) and has no copy-on-write; ReFS clones extents with `FSCTL_DUPLICATE_EXTENTS_TO_FILE`
//! and has no per-file compression. [`caps`] finds out which by trying, the same way Linux does,
//! and a pass whose half is missing plans nothing.
//!
//! Identity is `FILE_ID_INFO`: the volume serial plus a 128-bit file id. The 64-bit index from
//! `GetFileInformationByHandle` is not unique on ReFS, so [`file_id`] does not use it. Link count
//! is `FILE_STANDARD_INFO` (`number_of_links` on `Metadata` is still unstable). Size on disk is
//! `GetCompressedFileSizeW`, which is what shows an NTFS compression win.
//!
//! Process current directories are not read here. Finding one means reading another process's
//! memory, and the quiet tier stays unsure.

use std::collections::HashMap;
use std::fs::{self, File, Metadata};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Foundation::{
    ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED, GetLastError, NO_ERROR,
};
use windows_sys::Win32::Storage::FileSystem::{
    COMPRESSION_FORMAT_LZNT1, COMPRESSION_FORMAT_NONE, FILE_ID_INFO, FILE_STANDARD_INFO,
    FileIdInfo, FileStandardInfo, GetCompressedFileSizeW, GetDiskFreeSpaceW,
    GetFileInformationByHandleEx, INVALID_FILE_SIZE,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DUPLICATE_EXTENTS_DATA, FSCTL_DUPLICATE_EXTENTS_TO_FILE, FSCTL_SET_COMPRESSION,
};

use super::Caps;

/// `FILE_ATTRIBUTE_COMPRESSED`: NTFS is holding this file compressed.
pub const COMPRESSED: u32 = 0x800;

/// `GetCompressedFileSizeW` reports the bytes the file occupies, so a compression win shows up
/// in `allocated` the way `st_blocks` does on APFS.
pub const ALLOCATED_SHOWS_COMPRESSION: bool = true;

/// Attributes that mean "not ours to rewrite", plus `COMPRESSED` itself. Everything else NTFS
/// reports — `ARCHIVE` on very nearly every file, `NOT_CONTENT_INDEXED`, `TEMPORARY` — says
/// nothing about whether a file may be replaced, and must not turn into a blanket skip.
const READONLY: u32 = 0x1;
const HIDDEN: u32 = 0x2;
const SYSTEM: u32 = 0x4;
const REPARSE_POINT: u32 = 0x400;
const KNOWN: u32 = COMPRESSED | READONLY | HIDDEN | SYSTEM | REPARSE_POINT;

/// Opening a directory. Without it `CreateFile` on a directory is access denied, and the probe
/// cannot put the directory's mtime back.
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

/// One call of `FSCTL_DUPLICATE_EXTENTS_TO_FILE` copies at most this many bytes.
const GIB: u64 = 1024 * 1024 * 1024;
const MAX_DUPLICATE_BYTES: u64 = 4 * GIB;

/// Big enough that a filesystem has something to share or compress, small enough to cost nothing.
const PROBE_BYTES: usize = 64 * 1024;

/// Keep the compressed form only when it occupies less than this percentage of the logical size.
/// LZNT1 that barely shrinks a file still fragments it.
const MIN_COMPRESSED_PERCENT: u128 = 95;
const PERCENT: u128 = 100;

/// Volume serial and the 128-bit file id. Two hardlinks are one id; a clone is a new one.
/// The path is only a fallback for a file that cannot be opened, and it is not unique across
/// links — a scan that cannot open a file already cannot group it.
pub fn file_id(path: &Path, _meta: &Metadata) -> (u64, u128) {
    if let Ok(id) = read_file_id(path) {
        return id;
    }
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    (0, u128::from(hasher.finish()))
}

/// Link count from `FILE_STANDARD_INFO`. std's `number_of_links` is still unstable.
pub fn nlink(path: &Path, _meta: &Metadata) -> u64 {
    link_count(path).unwrap_or(1)
}

/// Bytes on disk. A compressed or sparse file occupies less than its logical length.
pub fn allocated(path: &Path, meta: &Metadata) -> u64 {
    compressed_size(path).unwrap_or_else(|_| meta.file_size())
}

pub fn flags(_path: &Path, meta: &Metadata) -> u32 {
    meta.file_attributes() & KNOWN
}

/// What the volume under `dir` can do, found out by doing it and cached per volume serial.
/// A directory we cannot stat or identify is not one we may claim a capability for. A file is
/// answered from its parent: the probe has to create a file beside it.
pub fn caps(dir: &Path) -> Caps {
    let Ok(meta) = fs::metadata(dir) else {
        return Caps::NONE;
    };
    let dev = match read_file_id(dir) {
        Ok((dev, _)) => dev,
        Err(_) => file_id(dir, &meta).0,
    };
    if dev == 0 {
        return Caps::NONE;
    }
    caps_of(dev, dir)
}

pub fn mode(_meta: &Metadata) -> u32 {
    0
}

/// Windows permissions live in the ACL, not in a mode. Nothing here is restored; a copy keeps
/// the attributes `fs::copy` already carried.
pub fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

pub fn symlink(original: &Path, link: &Path) -> io::Result<()> {
    if original.is_dir() {
        std::os::windows::fs::symlink_dir(original, link)
    } else {
        std::os::windows::fs::symlink_file(original, link)
    }
}

/// Block cloning. A volume that cannot share extents — NTFS, among others — gets
/// [`io::ErrorKind::Unsupported`] and never a second copy of the bytes. The callers that want
/// a copy either way fall back on their own.
pub fn clone_file(source: &Path, destination: &Path) -> io::Result<()> {
    let result = clone_extents(source, destination);
    if result.is_err() {
        // A failed call may have sized the destination already. What is left would be a
        // file of the right length full of zeros — a copy that shares nothing.
        let _ = fs::remove_file(destination);
    }
    result
}

fn clone_extents(source: &Path, destination: &Path) -> io::Result<()> {
    let src = open_info(source)?;
    let len = src.metadata()?.len();
    if len == 0 {
        // A zero-length call is rejected by both filesystems, and an empty file has no
        // extents to share. Succeed only where a non-empty clone would.
        let parent = source.parent().unwrap_or(source);
        return if caps(parent).clone {
            File::create(destination)?;
            Ok(())
        } else {
            Err(unsupported())
        };
    }
    let dst = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(destination)?;
    // The destination has to be sized first. The control code does not extend it.
    dst.set_len(len)?;
    // One call may copy up to 4 GiB, and a range that ends at EOF need not be cluster-aligned.
    // Only a file larger than that has to be split on cluster boundaries.
    if len <= MAX_DUPLICATE_BYTES {
        return duplicate(&src, &dst, 0, len);
    }
    let cluster = cluster_bytes(source)?;
    if cluster == 0 {
        return Err(io::Error::other("the volume reported a zero cluster size"));
    }
    let aligned_max = (MAX_DUPLICATE_BYTES / cluster) * cluster;
    if aligned_max == 0 {
        return Err(io::Error::other(
            "the cluster size is larger than one clone call can move",
        ));
    }
    let mut offset = 0u64;
    while offset < len {
        let remaining = len - offset;
        // Every range but the one that ends at EOF has to be cluster-aligned.
        let count = if remaining <= aligned_max {
            remaining
        } else {
            aligned_max
        };
        duplicate(&src, &dst, offset, count)?;
        offset += count;
    }
    Ok(())
}

/// NTFS LZNT1, applied to the engine's private copies.
#[derive(Default)]
pub struct Compressor {
    notes: Mutex<Vec<String>>,
}

impl Compressor {
    pub fn new() -> Self {
        Self::default()
    }

    /// The engine hands private copies only, so compressing one disturbs nothing else.
    pub fn compress(&self, copies: &[PathBuf]) {
        for copy in copies {
            if let Err(error) = compress_file(copy)
                && let Ok(mut notes) = self.notes.lock()
            {
                notes.push(format!("{}: {error}", copy.display()));
            }
        }
    }

    pub fn notes(&self) -> Vec<String> {
        self.notes.lock().map(|n| n.clone()).unwrap_or_default()
    }
}

pub fn tool_cwds(_tools: &[&str]) -> Option<Vec<PathBuf>> {
    None
}

/// A file or directory opened so its mtime can be changed. `FILE_WRITE_ATTRIBUTES` is what
/// `SetFileTime` requires, and `FILE_FLAG_BACKUP_SEMANTICS` is what lets the handle be a directory.
pub fn open_for_times(path: &Path) -> io::Result<File> {
    File::options()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

fn open_info(path: &Path) -> io::Result<File> {
    File::options()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

/// `\\?\C:\…`, `C:\…` and `C:/…` are one key. Git prints the drive form with forward slashes;
/// `canonicalize` prints the verbatim form. The drive letter is folded to upper case so the two
/// agree on `c:` and `C:` as well.
pub fn plain(path: &Path) -> PathBuf {
    let mut text = path.to_string_lossy().replace('/', "\\");
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        text = format!(r"\\{rest}");
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        text = rest.to_string();
    }
    let bytes = text.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        let mut owned = text.into_bytes();
        owned[0] = owned[0].to_ascii_uppercase();
        text = String::from_utf8(owned).expect("a drive letter is ASCII");
    }
    PathBuf::from(text)
}

fn cache() -> &'static Mutex<HashMap<u64, Caps>> {
    static CACHE: OnceLock<Mutex<HashMap<u64, Caps>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

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
    let clone = clone_file(source, copy).is_ok();
    // The attribute is the truth. A control code that is accepted and does not mark the file
    // compressed did not compress it.
    let compress = set_format(source, COMPRESSION_FORMAT_LZNT1).is_ok()
        && fs::metadata(source).is_ok_and(|meta| flags(source, &meta) & COMPRESSED != 0);
    Ok(Caps { clone, compress })
}

fn compress_file(path: &Path) -> io::Result<()> {
    set_format(path, COMPRESSION_FORMAT_LZNT1)?;
    let meta = fs::metadata(path)?;
    let logical = u128::from(meta.file_size());
    let on_disk = u128::from(allocated(path, &meta));
    if logical == 0 || on_disk * PERCENT >= logical * MIN_COMPRESSED_PERCENT {
        set_format(path, COMPRESSION_FORMAT_NONE)?;
        return Err(io::Error::other("not compressible enough"));
    }
    Ok(())
}

fn set_format(path: &Path, format: u16) -> io::Result<()> {
    let file = File::options().read(true).write(true).open(path)?;
    device_io_control(&file, FSCTL_SET_COMPRESSION, &format.to_le_bytes())
}

fn duplicate(src: &File, dst: &File, offset: u64, count: u64) -> io::Result<()> {
    let offset =
        i64::try_from(offset).map_err(|_| io::Error::other("extent offset does not fit"))?;
    let count = i64::try_from(count).map_err(|_| io::Error::other("extent length does not fit"))?;
    let data = DUPLICATE_EXTENTS_DATA {
        FileHandle: src.as_raw_handle(),
        SourceFileOffset: offset,
        TargetFileOffset: offset,
        ByteCount: count,
    };
    // SAFETY: `data` is a live `DUPLICATE_EXTENTS_DATA` for this synchronous call, and the
    // slice covers exactly that struct. `src` owns the handle stored in it and outlives the call.
    let input = unsafe {
        std::slice::from_raw_parts(
            (&data as *const DUPLICATE_EXTENTS_DATA).cast::<u8>(),
            std::mem::size_of::<DUPLICATE_EXTENTS_DATA>(),
        )
    };
    match device_io_control(dst, FSCTL_DUPLICATE_EXTENTS_TO_FILE, input) {
        Ok(()) => Ok(()),
        Err(error) if is_unsupported(&error) => Err(unsupported()),
        Err(error) => Err(error),
    }
}

fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "this volume has no block cloning",
    )
}

fn is_unsupported(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code)
            if code == ERROR_INVALID_FUNCTION as i32 || code == ERROR_NOT_SUPPORTED as i32
    )
}

/// `DeviceIoControl` with an input buffer and no output. The handle must already have the
/// access the control code asks for.
fn device_io_control(file: &File, code: u32, input: &[u8]) -> io::Result<()> {
    let mut returned = 0u32;
    // SAFETY: `file` owns the handle. `input` is readable for `input.len()` bytes for this
    // synchronous call (`lpoverlapped` is null). There is no output buffer.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            code,
            input.as_ptr().cast(),
            u32::try_from(input.len()).unwrap_or(u32::MAX),
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn read_file_id(path: &Path) -> io::Result<(u64, u128)> {
    let file = open_info(path)?;
    let mut info = FILE_ID_INFO::default();
    // SAFETY: `file` owns the handle. `info` is a `FILE_ID_INFO`, which is what `FileIdInfo`
    // writes, and the buffer length is the size of that struct. The call is synchronous.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            u32::try_from(std::mem::size_of::<FILE_ID_INFO>()).unwrap_or(u32::MAX),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        info.VolumeSerialNumber,
        u128::from_le_bytes(info.FileId.Identifier),
    ))
}

fn compressed_size(path: &Path) -> io::Result<u64> {
    let wide = wide_path(path);
    let mut high = 0u32;
    // SAFETY: `wide` is NUL-terminated UTF-16 and lives for the call. `high` is a writable u32.
    let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &mut high) };
    if low == INVALID_FILE_SIZE {
        // SAFETY: nothing has run since the failing call, so `GetLastError` is that failure.
        let error = unsafe { GetLastError() };
        if error != NO_ERROR {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
    }
    Ok((u64::from(high) << 32) | u64::from(low))
}

fn link_count(path: &Path) -> io::Result<u64> {
    let file = open_info(path)?;
    let mut info = FILE_STANDARD_INFO::default();
    // SAFETY: `file` owns the handle. `info` is a `FILE_STANDARD_INFO`, which is what
    // `FileStandardInfo` writes, and the buffer length is the size of that struct.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileStandardInfo,
            (&mut info as *mut FILE_STANDARD_INFO).cast(),
            u32::try_from(std::mem::size_of::<FILE_STANDARD_INFO>()).unwrap_or(u32::MAX),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(u64::from(info.NumberOfLinks))
}

fn cluster_bytes(path: &Path) -> io::Result<u64> {
    let wide = wide_path(&volume_root(path));
    let (mut sectors, mut bytes_per_sector, mut free_clusters, mut total_clusters) = (0, 0, 0, 0);
    // SAFETY: `wide` is NUL-terminated UTF-16 and lives for the call. The four out-params are
    // writable u32s.
    let ok = unsafe {
        GetDiskFreeSpaceW(
            wide.as_ptr(),
            &mut sectors,
            &mut bytes_per_sector,
            &mut free_clusters,
            &mut total_clusters,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(u64::from(sectors) * u64::from(bytes_per_sector))
}

/// `GetDiskFreeSpaceW` wants a volume root (`C:\`, `\\?\C:\`, `\\server\share\`).
fn volume_root(path: &Path) -> PathBuf {
    let text = plain(path).to_string_lossy().into_owned();
    if let Some(rest) = text.strip_prefix(r"\\") {
        let mut parts = rest.split('\\');
        let server = parts.next().unwrap_or("");
        let share = parts.next().unwrap_or("");
        return PathBuf::from(format!(r"\\?\UNC\{server}\{share}\"));
    }
    if text.len() >= 2 && text.as_bytes()[1] == b':' {
        return PathBuf::from(format!(r"\\?\{}\", &text[..2]));
    }
    path.parent().unwrap_or(path).to_path_buf()
}

/// The `\\?\` form, so a path past `MAX_PATH` reaches `GetCompressedFileSizeW` and
/// `GetDiskFreeSpaceW`. std adds the prefix for its own calls; these two do not.
fn wide_path(path: &Path) -> Vec<u16> {
    let text = plain(path).to_string_lossy().into_owned();
    let verbatim = if let Some(rest) = text.strip_prefix(r"\\") {
        format!(r"\\?\UNC\{rest}")
    } else if text.len() >= 2 && text.as_bytes()[1] == b':' {
        format!(r"\\?\{text}")
    } else {
        text
    };
    Path::new(&verbatim)
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hardlink_is_the_same_file_and_a_second_name_counts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        fs::write(&a, b"same").unwrap();
        fs::hard_link(&a, &b).unwrap();

        let (meta_a, meta_b) = (fs::metadata(&a).unwrap(), fs::metadata(&b).unwrap());
        assert_eq!(file_id(&a, &meta_a), file_id(&b, &meta_b));
        assert_eq!(nlink(&a, &meta_a), 2);
        assert_eq!(nlink(&b, &meta_b), 2);
        assert_ne!(file_id(&a, &meta_a).1, 0);
    }

    #[test]
    fn cloning_follows_the_volume_and_never_pretends() {
        let tmp = tempfile::TempDir::new().unwrap();
        let source = tmp.path().join("source");
        let dest = tmp.path().join("dest");
        let bytes = vec![3u8; PROBE_BYTES * 2];
        fs::write(&source, &bytes).unwrap();
        let found = caps(tmp.path());

        let cloned = clone_file(&source, &dest);

        if found.clone {
            cloned.unwrap();
            assert_eq!(fs::read(&dest).unwrap(), bytes);
            assert_ne!(
                file_id(&source, &fs::metadata(&source).unwrap()),
                file_id(&dest, &fs::metadata(&dest).unwrap()),
                "a clone shares blocks, not identity"
            );
            assert!(
                !found.compress,
                "a volume that clones (ReFS) has no per-file compression"
            );
        } else {
            let error = cloned.unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Unsupported, "{error}");
            assert!(!dest.exists(), "a refused clone must not leave a copy");
        }
    }

    #[test]
    fn compression_shrinks_a_repetitive_file_where_the_volume_can() {
        let tmp = tempfile::TempDir::new().unwrap();
        if !caps(tmp.path()).compress {
            eprintln!("this volume does not compress; point TEMP at an NTFS volume");
            return;
        }
        let path = tmp.path().join("repetitive.bin");
        let bytes = vec![9u8; PROBE_BYTES * 4];
        fs::write(&path, &bytes).unwrap();
        let before = allocated(&path, &fs::metadata(&path).unwrap());

        Compressor::new().compress(std::slice::from_ref(&path));

        let meta = fs::metadata(&path).unwrap();
        assert_ne!(
            flags(&path, &meta) & COMPRESSED,
            0,
            "LZNT1 sets the attribute"
        );
        let after = allocated(&path, &meta);
        assert!(after < before / 2, "{before} -> {after}");
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "compression is transparent"
        );
        eprintln!("lznt1 fixture: {before} -> {after}");
    }

    #[test]
    fn an_incompressible_file_is_put_back() {
        let tmp = tempfile::TempDir::new().unwrap();
        if !caps(tmp.path()).compress {
            eprintln!("this volume does not compress; point TEMP at an NTFS volume");
            return;
        }
        let path = tmp.path().join("noise.bin");
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut bytes = Vec::with_capacity(PROBE_BYTES * 2);
        while bytes.len() < PROBE_BYTES * 2 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            bytes.extend_from_slice(&state.to_le_bytes());
        }
        bytes.truncate(PROBE_BYTES * 2);
        fs::write(&path, &bytes).unwrap();

        let compressor = Compressor::new();
        compressor.compress(std::slice::from_ref(&path));

        let meta = fs::metadata(&path).unwrap();
        assert_eq!(flags(&path, &meta) & COMPRESSED, 0);
        assert!(
            compressor
                .notes()
                .iter()
                .any(|note| note.contains("not compressible enough")),
            "{:?}",
            compressor.notes()
        );
    }

    #[test]
    fn a_verbatim_path_a_drive_path_and_slashes_are_one_key() {
        let tmp = tempfile::TempDir::new().unwrap();
        let canon = tmp.path().canonicalize().unwrap();
        let text = canon.to_string_lossy();
        assert!(
            text.starts_with(r"\\?\"),
            "canonicalize returns the verbatim form, got {text}"
        );
        let drive = text.trim_start_matches(r"\\?\").replace('\\', "/");
        let lower = {
            let mut chars: Vec<u8> = drive.clone().into_bytes();
            chars[0] = chars[0].to_ascii_lowercase();
            String::from_utf8(chars).unwrap()
        };
        assert_eq!(plain(&canon), plain(Path::new(&drive)));
        assert_eq!(plain(&canon), plain(Path::new(&lower)));
        assert_eq!(plain(&canon), plain(&plain(&canon)));
    }

    #[test]
    fn a_path_past_max_path_still_has_an_identity_and_a_size() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut dir = tmp.path().to_path_buf();
        // `MAX_PATH` is 260. One component stays under the 255-character limit.
        while dir.as_os_str().len() < 300 {
            dir.push("nested");
        }
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("artifact.bin");
        fs::write(&file, vec![1u8; PROBE_BYTES]).unwrap();

        let meta = fs::metadata(&file).unwrap();
        let id = file_id(&file, &meta);
        assert_ne!(id.1, 0);
        assert_eq!(file_id(&file, &fs::metadata(&file).unwrap()), id);
        assert!(allocated(&file, &meta) >= PROBE_BYTES as u64);
    }
}
