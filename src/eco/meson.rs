//! Meson: a build dir holding `meson-private/coredata.dat`, built by Ninja. Meson's own lock,
//! `meson-private/meson.lock`, is held only while it configures; the build itself is Ninja's,
//! which takes none. So the whole dir is one [`Guard::Quiet`] unit, as for CMake.
//! `meson-info/meson-info.json` names the source dir, which makes the owner a JSON read away.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{Ecosystem, Guard, Owner, Policy, Sharing};

/// What every configured Meson build dir holds.
pub const COREDATA: &str = "meson-private/coredata.dat";
/// Meson's introspection summary, with the source dir.
pub const INFO: &str = "meson-info/meson-info.json";

#[derive(Deserialize)]
struct Info {
    directories: Directories,
}

#[derive(Deserialize)]
struct Directories {
    source: PathBuf,
}

/// Meson build dirs.
pub struct Meson;

pub static MESON: Meson = Meson;

impl Ecosystem for Meson {
    fn name(&self) -> &'static str {
        "meson"
    }

    /// Meson refuses an in-source build; one that claims to be is not taken either, since a
    /// lossy pass removing the unit would remove the sources with it.
    fn claim(&self, dir: &Path) -> bool {
        let real = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        dir.join(COREDATA).is_file()
            && self
                .owner(dir)
                .is_some_and(|owner| !real(&owner.project).starts_with(real(dir)))
    }

    /// The source dir `meson-info.json` names; a build dir sits anywhere.
    fn owner(&self, build_dir: &Path) -> Option<Owner> {
        let info: Info = serde_json::from_slice(&fs::read(build_dir.join(INFO)).ok()?).ok()?;
        Some(Owner {
            project: info.directories.source,
        })
    }

    fn manifest(&self, project: &Path) -> Option<PathBuf> {
        Some(project.join("meson.build"))
    }

    fn units(&self, build_dir: &Path) -> io::Result<Vec<PathBuf>> {
        if !self.claim(build_dir) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("no {COREDATA} or {INFO}: not a Meson build dir"),
            ));
        }
        Ok(vec![build_dir.to_path_buf()])
    }

    fn guard(&self, _unit: &Path) -> Guard {
        Guard::Quiet
    }

    fn last_used(&self, unit: &Path) -> Option<u64> {
        super::cargo::last_built(unit)
    }

    /// Meson runs Ninja, or samurai where it stands in for Ninja; `meson compile` and
    /// `meson test` run for the whole build.
    fn tools(&self) -> &'static [&'static str] {
        &["meson", "ninja", "samu"]
    }

    /// Objects and archives may be rewritten in place, as under CMake: clones only.
    fn policy(&self) -> Policy {
        Policy {
            share: Sharing::ClonesOnly,
        }
    }
}
