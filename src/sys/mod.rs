//! Everything that differs between platforms, and nothing else. No `std::os` import belongs
//! anywhere above this module.
//!
//! [`caps`] decides what the lossless passes may plan: block sharing and transparent
//! compression, for the filesystem under one directory. A pass whose capability is false plans
//! nothing there at all — it does not copy, fail or apologise, it simply finds no work, because
//! a copy that is not a clone costs a second copy of the bytes.
//!
//! It is a question about a filesystem, not about a platform, so it is answered per directory
//! and cached per device. On Linux the answer is found by trying: btrfs and XFS with
//! `reflink=1` share blocks and ext4 does not, but so does btrfs mounted `nodatacow` and XFS
//! with `reflink=0`, which no table of filesystem names gets right. macOS answers `true` for
//! both without asking, because APFS is what the tool was measured on. Windows answers `false`
//! for both until `T21`.

#[cfg_attr(target_os = "macos", path = "macos.rs")]
#[cfg_attr(all(unix, not(target_os = "macos")), path = "unix.rs")]
#[cfg_attr(windows, path = "windows.rs")]
mod imp;

pub use imp::{
    ALLOCATED_SHOWS_COMPRESSION, COMPRESSED, Compressor, allocated, clone_file, file_id, flags,
    mode, nlink, set_mode, symlink,
};

use std::path::Path;

/// What the filesystem under one directory can do for us.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Caps {
    /// Copy-on-write copies: [`clone_file`] shares the blocks instead of writing them again.
    pub clone: bool,
    /// Transparent compression: [`Compressor`] has a backend here.
    pub compress: bool,
}

impl Caps {
    /// A filesystem neither pass can win anything on. Also what an unreadable directory gets:
    /// a probe that cannot run must not claim a capability.
    pub const NONE: Self = Self {
        clone: false,
        compress: false,
    };

    /// For the report: what `status` says about a root nothing can be done to.
    pub fn is_none(&self) -> bool {
        *self == Self::NONE
    }
}

/// What the filesystem under `dir` can do. Cached per device, so a run probes each filesystem
/// once however many profile dirs it holds.
pub fn caps(dir: &Path) -> Caps {
    imp::caps(dir)
}

/// A path a running build tool holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Held {
    /// Its current dir.
    Cwd(std::path::PathBuf),
    /// A file it has open or mapped. Only macOS reports these: the Xcode build service works in
    /// DerivedData from a current dir inside Xcode.app.
    Open(std::path::PathBuf),
}

/// Whether a process called one of `tools` works in `dir`: its current dir is `dir`, below it,
/// or a dir around it — `make` run from the project root builds into `build/` — or it holds a
/// file inside `dir` open. A process in a filesystem root says nothing about any dir. `None`
/// when it cannot be told: no tools named, no process table here, or the platform does not say.
/// Only processes this user may inspect are seen.
pub fn tool_running(dir: &Path, tools: &[&str]) -> Option<bool> {
    if tools.is_empty() {
        return None;
    }
    let held = imp::tool_paths(tools)?;
    Some(held.iter().any(|held| match held {
        Held::Cwd(cwd) => cwd.starts_with(dir) || (cwd.parent().is_some() && dir.starts_with(cwd)),
        Held::Open(file) => file.starts_with(dir),
    }))
}

/// The temp dir build tools put their lock files in: `TMPDIR`, else, on macOS, the per-user one
/// Foundation falls back to, where a tool started without `TMPDIR` looks too.
pub fn temp_dir() -> std::path::PathBuf {
    #[cfg(target_os = "macos")]
    if std::env::var_os("TMPDIR").is_none_or(|dir| dir.is_empty())
        && let Some(dir) = imp::user_temp_dir()
    {
        return dir;
    }
    std::env::temp_dir()
}

/// Runs a probe inside `dir` and puts the directory's modification time back afterwards.
///
/// Creating and removing a file changes the mtime of the directory it is in, and that mtime is
/// how `evict` and `incremental` tell a profile nobody has built for a week from one built this
/// morning. A probe that moved it would make every target look freshly built — found exactly
/// that way, by the incremental test on btrfs.
#[cfg(not(windows))]
fn probing_in<T>(dir: &Path, probe: impl FnOnce() -> T) -> T {
    let before = std::fs::metadata(dir).and_then(|meta| meta.modified());
    let found = probe();
    if let Ok(mtime) = before
        && let Ok(handle) = std::fs::File::open(dir)
    {
        let _ = handle.set_times(std::fs::FileTimes::new().set_modified(mtime));
    }
    found
}

/// A name for a probe file inside `dir`, unique per process and call. It carries the engine's
/// temp prefix, so one left behind by a kill is swept by the next run like any other temp.
/// Windows answers [`Caps::NONE`] without probing, so nothing there calls this.
#[cfg(not(windows))]
fn probe_path(dir: &Path) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    dir.join(format!(
        "{}probe-{}-{}",
        crate::model::TMP_PREFIX,
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::time::{Duration, SystemTime};

    /// The same facts on every platform, so that the port is what is tested and not macOS.
    #[test]
    fn a_file_has_an_identity_of_its_own_and_a_size_on_disk() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        fs::write(&a, vec![7; 64 * 1024]).unwrap();
        fs::write(&b, vec![7; 64 * 1024]).unwrap();

        let (meta_a, meta_b) = (fs::metadata(&a).unwrap(), fs::metadata(&b).unwrap());
        assert_ne!(
            file_id(&a, &meta_a),
            file_id(&b, &meta_b),
            "two files are two inodes"
        );
        assert_eq!(
            file_id(&a, &fs::metadata(&a).unwrap()),
            file_id(&a, &meta_a)
        );
        assert_eq!(nlink(&meta_a), 1);
        assert!(allocated(&meta_a) >= 64 * 1024);
        assert_eq!(flags(&a, &meta_a) & !COMPRESSED, 0, "a plain file is ours");
    }

    /// A clone holds the bytes of its source — and where the filesystem cannot share blocks, it
    /// must fail instead of writing them again. A silent byte copy is the one outcome that
    /// would make the dedupe pass cost disk rather than save it.
    #[test]
    fn a_clone_holds_the_bytes_of_its_source_or_refuses_to_pretend() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (source, copy) = (tmp.path().join("source"), tmp.path().join("copy"));
        let bytes = vec![3; 128 * 1024];
        fs::write(&source, &bytes).unwrap();

        let cloned = clone_file(&source, &copy);

        if !caps(tmp.path()).clone {
            assert!(cloned.is_err(), "a copy that shares nothing is not a clone");
            return;
        }
        cloned.unwrap();
        assert_eq!(fs::read(&copy).unwrap(), bytes);
        assert_ne!(
            file_id(&source, &fs::metadata(&source).unwrap()),
            file_id(&copy, &fs::metadata(&copy).unwrap()),
            "a clone shares blocks, not identity"
        );
    }

    /// An empty batch is what a pass hands over where it can plan nothing, and it must be free.
    #[test]
    fn the_compressor_survives_having_nothing_to_do() {
        let compressor = Compressor::new();

        compressor.compress(&[]);

        assert!(compressor.notes().is_empty());
    }

    /// Whatever the answer is on this machine, asking twice must give it twice: the cache is
    /// keyed by device, and a wrong key would show up here as two different answers.
    #[test]
    fn the_probe_answers_the_same_thing_twice() {
        let tmp = tempfile::TempDir::new().unwrap();
        let nested = tmp.path().join("deep/profile");
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(
            caps(tmp.path()),
            caps(&nested),
            "one filesystem, one answer"
        );
        assert_eq!(caps(tmp.path()), caps(tmp.path()));
    }

    /// A probe must leave nothing behind, whatever it found out — and "nothing" includes the
    /// directory's own modification time, which is what `evict` and `incremental` read to tell
    /// an idle profile from a busy one.
    #[test]
    fn the_probe_cleans_up_after_itself() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("profile");
        fs::create_dir(&dir).unwrap();
        let a_week_ago = SystemTime::now() - Duration::from_secs(7 * 24 * 60 * 60);
        fs::File::open(&dir)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(a_week_ago))
            .unwrap();

        caps(&dir);

        let left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(left.is_empty(), "{left:?}");
        assert_eq!(
            fs::metadata(&dir).unwrap().modified().unwrap(),
            a_week_ago,
            "a probe that moves the mtime makes an idle profile look freshly built"
        );
    }

    /// A directory that does not exist cannot be probed, and a probe that cannot run must not
    /// claim anything.
    #[test]
    fn an_unreadable_directory_gets_no_capability() {
        let tmp = tempfile::TempDir::new().unwrap();

        assert_eq!(caps(&tmp.path().join("nothing-here")), Caps::NONE);
    }
}
