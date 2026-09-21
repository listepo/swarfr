//! Xcode: an entry of DerivedData (`~/Library/Developer/Xcode/DerivedData/<name>-<hash>/`, or
//! whatever `-derivedDataPath` named), holding `info.plist` with the `WorkspacePath` it was built
//! from next to `Build/` or `Logs/`. Nothing is found by default: the library reads no home dir,
//! so DerivedData is a root the user names.
//!
//! No lock an outsider can take. `xcodebuild` starts its own build service, which holds the build
//! database and the compilation cache open, some of it mapped, from a current dir inside
//! Xcode.app; the Xcode app keeps one alive for as long as it runs. So the unit is
//! [`Guard::Quiet`], and a tool holding any file inside it open makes it busy (`sys::Held`).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{Ecosystem, Guard, Owner, Policy, Sharing};

pub const INFO: &str = "info.plist";
const WORKSPACE_KEY: &str = "<key>WorkspacePath</key>";

/// DerivedData entries.
pub struct Xcode;

pub static XCODE: Xcode = Xcode;

impl Ecosystem for Xcode {
    fn name(&self) -> &'static str {
        "xcode"
    }

    /// A DerivedData entry that does not hold what it was built from. `Build/` or `Logs/` beside
    /// the plist tells it from any other dir with an `info.plist`.
    fn claim(&self, dir: &Path) -> bool {
        let real = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        (dir.join("Build").is_dir() || dir.join("Logs").is_dir())
            && self
                .owner(dir)
                .is_some_and(|owner| !real(&owner.project).starts_with(real(dir)))
    }

    /// The workspace, project or package dir the plist names.
    fn owner(&self, build_dir: &Path) -> Option<Owner> {
        let path = build_dir.join(INFO);
        let bytes = fs::read(&path).ok()?;
        let xml = if bytes.starts_with(b"bplist") {
            // Xcode writes XML; a binary plist is read through Apple's own tool.
            let out = Command::new("/usr/bin/plutil")
                .args(["-convert", "xml1", "-o", "-"])
                .arg(&path)
                .output()
                .ok()?;
            String::from_utf8(out.stdout).ok()?
        } else {
            String::from_utf8(bytes).ok()?
        };
        Some(Owner {
            project: PathBuf::from(workspace_path(&xml)?),
        })
    }

    /// A workspace or project is a bundle that exists or not; a package dir has its manifest.
    fn manifest(&self, project: &Path) -> Option<PathBuf> {
        let bundle = project
            .extension()
            .is_some_and(|ext| ext == "xcodeproj" || ext == "xcworkspace" || ext == "playground");
        Some(if bundle {
            project.to_path_buf()
        } else {
            project.join("Package.swift")
        })
    }

    fn units(&self, build_dir: &Path) -> io::Result<Vec<PathBuf>> {
        if !self.claim(build_dir) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("no {INFO} with a WorkspacePath outside it: not a DerivedData entry"),
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

    /// `xcodebuild`, the build services it or the app starts, and the app itself, which writes
    /// the index while a project is open.
    fn tools(&self) -> &'static [&'static str] {
        &["xcodebuild", "SWBBuildService", "XCBBuildService", "Xcode"]
    }

    /// Objects and archives may be rewritten in place: clones only.
    fn policy(&self) -> Policy {
        Policy {
            share: Sharing::ClonesOnly,
        }
    }
}

/// The `<string>` after `<key>WorkspacePath</key>` in an XML plist, with XML's escapes undone.
fn workspace_path(xml: &str) -> Option<String> {
    let after = &xml[xml.find(WORKSPACE_KEY)? + WORKSPACE_KEY.len()..];
    let value = after.trim_start().strip_prefix("<string>")?;
    let value = &value[..value.find("</string>")?];
    Some(
        value
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_workspace_path_is_read_and_unescaped() {
        let xml = "<dict>\n\t<key>LastAccessedDate</key>\n\t<date>2026-01-01T00:00:00Z</date>\n\
                   \t<key>WorkspacePath</key>\n\t<string>/a/R&amp;D &lt;x&gt;/App.xcodeproj</string>\n</dict>";
        assert_eq!(
            workspace_path(xml).as_deref(),
            Some("/a/R&D <x>/App.xcodeproj")
        );
        assert_eq!(workspace_path("<dict></dict>"), None);
    }
}
