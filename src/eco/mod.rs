//! Build systems. Everything that knows one by name lives under this module, the way everything
//! that knows a platform lives under `sys`: the engine, the inode model, the index and the
//! lossless passes ask through [`Ecosystem`] and never name a build system's files or dirs.
//!
//! Cargo, SwiftPM, .NET and CMake so far. The shape is `docs/architecture.md`, "The adapter".

pub mod cargo;
pub mod cmake;
pub mod dotnet;
pub mod go;
pub mod meson;
pub mod store;
pub mod swiftpm;
pub mod xcode;

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

/// One build system: how its build dirs are found, what guards them, and what in them is
/// bound to one build. Methods a kind of dir has no answer for keep their defaults.
pub trait Ecosystem: Sync {
    /// As a report would name it.
    fn name(&self) -> &'static str;

    /// Whether `dir` is a build dir of this build system. Decided from `dir` itself and at most
    /// one small file in it: the shared walk asks every adapter about every dir.
    fn claim(&self, _dir: &Path) -> bool {
        false
    }

    /// What must exist for `build_dir` to have a reason. Family and orphan status are computed
    /// from it, never from where the build dir happens to sit.
    fn owner(&self, _build_dir: &Path) -> Option<Owner> {
        None
    }

    /// The file that makes `project` a project of this build system. Missing: the build dir's
    /// reason is gone — the project was deleted, renamed, or is absent on this branch.
    fn manifest(&self, _project: &Path) -> Option<PathBuf> {
        None
    }

    /// Where a build of `project` goes when nothing says otherwise: what `seed` fills. `None`
    /// where a build dir cannot be seeded.
    fn build_dir(&self, _project: &Path) -> Option<PathBuf> {
        None
    }

    /// The units of a claimed build dir: the dirs one guard protects, each scanned on its own.
    /// Refuses a dir the adapter would not claim.
    fn units(&self, build_dir: &Path) -> io::Result<Vec<PathBuf>>;

    /// What keeps a build and this tool from working on `unit` at the same time.
    fn guard(&self, unit: &Path) -> Guard;

    /// The build's own bookkeeping, by file or dir name: never scanned, never copied, never
    /// touched.
    fn private(&self, _name: &OsStr) -> bool {
        false
    }

    /// Dirs or files, by name, that belong to one checkout's build and are left behind when a
    /// build dir is seeded into another. [`Self::private`] ones are left behind too.
    fn volatile(&self, _name: &OsStr) -> bool {
        false
    }

    /// When a build last worked in `unit`, as unix seconds.
    fn last_used(&self, _unit: &Path) -> Option<u64> {
        None
    }

    /// For [`Guard::Quiet`]: the build tool's process names. One of them running in a unit, or
    /// in a dir around it, makes the unit busy. Empty: nothing can be checked, so every quiet
    /// unit is unsure and lossy passes leave it alone.
    fn tools(&self) -> &'static [&'static str] {
        &[]
    }

    /// Whether its tool keeps dirs read-only on purpose and `compress` may lift a dir's owner
    /// write bit for the length of one batch, putting the mode back after it: Go's module cache.
    /// A file's bytes, mode and mtime never change either way.
    fn lifts_read_only_dirs(&self) -> bool {
        false
    }

    fn policy(&self) -> Policy;
}

/// The project a build dir was built from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owner {
    /// The dir of the project's manifest: a cargo workspace root.
    pub project: PathBuf,
}

/// How a unit is kept apart from the build that writes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Guard {
    /// Reserved: a caller embedding the library already holds the build's lock (R8). Not
    /// implemented; the engine refuses it.
    Held,
    /// The build holds this file locked for as long as it writes the unit, and so does this
    /// tool: cargo's `.cargo-lock`.
    Lock(PathBuf),
    /// One lock file for many units at once: cargo's `.package-cache` for its home. Held: every
    /// unit under it is worked on. Busy: none of them is.
    Shared(PathBuf),
    /// No lock exists: Make, Ninja, MSBuild, Xcode. The weaker tier of `DESIGN.md`, "Safety
    /// tier without a build lock": young files are left out, a running build tool makes the
    /// unit busy, and lossy passes run only where nothing says "maybe".
    Quiet,
    /// A content-addressed store whose names never get other bytes: no lock, files younger than
    /// an hour left out, and no lossy pass. `DESIGN.md`, "Immutable stores".
    Immutable,
}

impl Guard {
    /// The tier, as a report names it.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Held => "held",
            Self::Lock(_) => "lock",
            Self::Shared(_) => "shared",
            Self::Quiet => "quiet",
            Self::Immutable => "immutable",
        }
    }
}

/// What may be done to the files of a unit beyond rewriting them in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub share: Sharing,
}

/// How dedupe may make two equal files one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sharing {
    /// Only by a clone: the files stay separate inodes.
    ClonesOnly,
    /// A hardlink where no clone is possible, when the user asks for it: artifacts a build may
    /// rewrite in place.
    LinkOptIn,
    /// A hardlink where no clone is possible, always: the build replaces files and never
    /// rewrites them.
    LinkSafe,
}

impl Sharing {
    /// Whether dedupe falls back to a hardlink, given whether the user asked for one.
    pub fn links(self, asked: bool) -> bool {
        match self {
            Self::ClonesOnly => false,
            Self::LinkOptIn => asked,
            Self::LinkSafe => true,
        }
    }
}

/// Every adapter, in the order the shared walk asks them.
pub static REGISTRY: [&dyn Ecosystem; 6] = [
    &cargo::CARGO,
    &swiftpm::SWIFTPM,
    &dotnet::DOTNET,
    &cmake::CMAKE,
    &meson::MESON,
    &xcode::XCODE,
];

/// The registered adapter called `name`.
pub fn named(name: &str) -> Option<&'static dyn Ecosystem> {
    REGISTRY.iter().copied().find(|eco| eco.name() == name)
}

/// Build dirs under `roots` with the adapter that claimed each, sorted by path. One walk however
/// many adapters there are; the first claim in registry order wins and a claimed dir is not
/// entered, so a build dir nested in another one — a CMake dir a build script left in a cargo
/// target — is nobody's. Symlinks are not followed and `.git` is not entered.
pub fn discover(roots: &[PathBuf]) -> Vec<(PathBuf, &'static dyn Ecosystem)> {
    let mut found: Vec<(PathBuf, &'static dyn Ecosystem)> = Vec::new();
    for root in roots {
        let mut walk = WalkDir::new(root).follow_links(false).into_iter();
        while let Some(entry) = walk.next() {
            // A dir we may not read cannot hold a build dir we could work on.
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_dir() {
                continue;
            }
            if entry.file_name() == ".git" {
                walk.skip_current_dir();
                continue;
            }
            if let Some(eco) = REGISTRY.iter().find(|eco| eco.claim(entry.path())) {
                found.push((entry.path().to_path_buf(), *eco));
                walk.skip_current_dir();
            }
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found.dedup_by(|a, b| a.0 == b.0);
    found
}
