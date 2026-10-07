//! Which workspace a target dir was built for, from the dep-info files cargo and rustc leave in it.
//!
//! Cargo writes `<profile>/<name>.d` for every artifact it uplifts, naming each source by its
//! absolute path. Rustc writes `<profile>/deps/<name>-<hash>.d` naming the same sources as cargo
//! passed them: relative to the workspace root for every path package under that root, since
//! cargo runs rustc there, and absolute for everything else. An absolute path that ends in a
//! relative one, minus that tail, is the workspace root. Nothing is guessed from where a
//! `Cargo.toml` happens to be, so a workspace that is gone is still named.
//!
//! A relative path alone is ambiguous — `src/lib.rs` ends every package's absolute path — so the
//! root must explain one relative path of every rustc file that pairs at all, and no rustc file
//! may name an absolute source under it: an absolute source under a candidate (a path dependency
//! outside the workspace, `/r/vendor/v`) shows the candidate is not the root. Generated sources
//! under the build dir itself do not count.
//!
//! A target only ever checked (`cargo check` uplifts nothing) or built with
//! `build.dep-info-basedir` (which makes cargo's paths relative too) names no root. Neither does
//! a `build.build-dir`: cargo's own dep-info goes to the target dir, not there.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

const DEP_INFO_EXT: &str = "d";
const DEPS_DIR: &str = "deps";

/// The workspace roots the profile dirs `units` of `build_dir` were built for, sorted. One per
/// profile dir at most; more than one when profile dirs were built from different checkouts.
pub fn workspace_roots(build_dir: &Path, units: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();
    for unit in units {
        let cargo: Vec<PathBuf> = dep_info(unit)
            .into_iter()
            .flatten()
            .filter(|path| path.is_absolute())
            .collect();
        if cargo.is_empty() {
            continue;
        }
        let mut candidates: Option<BTreeSet<PathBuf>> = None;
        let mut foreign = Vec::new();
        for file in dep_info(&unit.join(DEPS_DIR)) {
            let (relative, absolute): (Vec<PathBuf>, Vec<PathBuf>) =
                file.into_iter().partition(|path| path.is_relative());
            foreign.extend(
                absolute.into_iter().filter(|path| {
                    !crate::sys::plain(path).starts_with(crate::sys::plain(build_dir))
                }),
            );
            let explained: BTreeSet<PathBuf> = relative
                .iter()
                .flat_map(|tail| cargo.iter().filter_map(|path| strip_tail(path, tail)))
                .collect();
            if explained.is_empty() {
                continue;
            }
            candidates = Some(match candidates {
                None => explained,
                Some(kept) => kept.intersection(&explained).cloned().collect(),
            });
        }
        roots.extend(
            candidates
                .into_iter()
                .flatten()
                .filter(|root| !foreign.iter().any(|path| path.starts_with(root))),
        );
    }
    roots.into_iter().collect()
}

/// `path` without `tail`, when `path` ends in `tail`'s components.
fn strip_tail(path: &Path, tail: &Path) -> Option<PathBuf> {
    if !path.ends_with(tail) {
        return None;
    }
    let depth = tail.components().count();
    path.ancestors().nth(depth).map(Path::to_path_buf)
}

/// The prerequisites of each `.d` file directly in `dir`. Unreadable files name nothing.
fn dep_info(dir: &Path) -> Vec<Vec<PathBuf>> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(OsStr::new(DEP_INFO_EXT)))
        .filter_map(|path| fs::read_to_string(path).ok())
        .map(|text| prerequisites(&text))
        .collect()
}

/// The prerequisites of a Makefile-style dep-info file: what follows `target:` on each rule line.
/// A `\` before a space or another `\` is an escape. Any other `\` is kept: cargo on Windows
/// writes path separators unescaped (`C:\ws\src\lib.rs`).
fn prerequisites(text: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        let Some((_, deps)) = line.split_once(": ") else {
            continue;
        };
        let mut word = String::new();
        let mut chars = deps.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => match chars.next() {
                    Some(' ') => word.push(' '),
                    Some('\\') => word.push('\\'),
                    Some(other) => {
                        word.push('\\');
                        word.push(other);
                    }
                    None => word.push('\\'),
                },
                ' ' => {
                    if !word.is_empty() {
                        out.push(PathBuf::from(std::mem::take(&mut word)));
                    }
                }
                _ => word.push(c),
            }
        }
        if !word.is_empty() {
            out.push(PathBuf::from(word));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_lines_give_their_prerequisites_unescaped() {
        let text = "/t/debug/ws: /w\\ s/src/main.rs /ws/src/lib.rs\n\
                    # env-dep:CARGO_PKG_NAME=ws\n\
                    src/lib.rs:\n\
                    C:\\out\\lib.rlib: C:\\ws\\src\\lib.rs C:\\w\\ s\\src\\lib.rs\n";
        assert_eq!(
            prerequisites(text),
            [
                PathBuf::from("/w s/src/main.rs"),
                PathBuf::from("/ws/src/lib.rs"),
                PathBuf::from(r"C:\ws\src\lib.rs"),
                PathBuf::from(r"C:\w s\src\lib.rs"),
            ]
        );
    }

    #[test]
    fn a_tail_is_matched_by_whole_components() {
        let path = Path::new("/r/ws/crates/b/src/lib.rs");
        assert_eq!(
            strip_tail(path, Path::new("crates/b/src/lib.rs")),
            Some(PathBuf::from("/r/ws"))
        );
        assert_eq!(
            strip_tail(path, Path::new("b/src/lib.rs")),
            Some(PathBuf::from("/r/ws/crates"))
        );
        assert_eq!(strip_tail(path, Path::new("s/b/src/lib.rs")), None);
    }

    /// Absolute on this platform. `/r/ws` is absolute on Unix and relative on Windows, and a
    /// Windows path's `\` is escaped in dep-info the way the parser undoes it.
    fn rooted(tail: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!(r"C:\{}", tail.replace('/', r"\")))
        } else {
            PathBuf::from(format!("/{tail}"))
        }
    }

    fn escaped(path: &Path) -> String {
        path.display().to_string().replace('\\', "\\\\")
    }

    /// `src/lib.rs` alone would name the member `b` and the dependency `v` as roots too.
    #[test]
    fn roots_come_from_pairs_of_cargo_and_rustc_dep_info() {
        let tmp = tempfile::TempDir::new().unwrap();
        let unit = tmp.path().join("debug");
        fs::create_dir_all(unit.join(DEPS_DIR)).unwrap();
        let (member, own, vendor) = (
            rooted("r/ws/crates/b/src/lib.rs"),
            rooted("r/ws/src/lib.rs"),
            rooted("r/vendor/v/src/lib.rs"),
        );
        fs::write(
            unit.join("libws.d"),
            format!(
                "{}: {} {} {}\n",
                unit.display(),
                escaped(&member),
                escaped(&own),
                escaped(&vendor),
            ),
        )
        .unwrap();
        fs::write(unit.join("deps/ws-1.d"), "x.rmeta: src/lib.rs\n").unwrap();
        fs::write(unit.join("deps/b-2.d"), "x.rmeta: crates/b/src/lib.rs\n").unwrap();
        // A dependency outside the workspace: absolute in rustc's file too.
        fs::write(
            unit.join("deps/v-3.d"),
            format!("x.rmeta: {}\n", escaped(&vendor)),
        )
        .unwrap();

        assert_eq!(workspace_roots(tmp.path(), &[unit]), [rooted("r/ws")]);
    }

    #[test]
    fn a_single_crate_is_told_from_its_path_dependency() {
        let tmp = tempfile::TempDir::new().unwrap();
        let unit = tmp.path().join("debug");
        fs::create_dir_all(unit.join(DEPS_DIR)).unwrap();
        let (own, vendor) = (rooted("r/ws/src/lib.rs"), rooted("r/vendor/v/src/lib.rs"));
        fs::write(
            unit.join("libws.d"),
            format!("x.rlib: {} {}\n", escaped(&own), escaped(&vendor)),
        )
        .unwrap();
        fs::write(unit.join("deps/ws-1.d"), "x.rmeta: src/lib.rs\n").unwrap();
        fs::write(
            unit.join("deps/v-3.d"),
            format!("x.rmeta: {}\n", escaped(&vendor)),
        )
        .unwrap();

        assert_eq!(workspace_roots(tmp.path(), &[unit]), [rooted("r/ws")]);
    }

    #[test]
    fn a_target_only_checked_names_no_root() {
        let tmp = tempfile::TempDir::new().unwrap();
        let unit = tmp.path().join("debug");
        fs::create_dir_all(unit.join(DEPS_DIR)).unwrap();
        fs::write(unit.join("deps/ws-1.d"), "x.rmeta: src/lib.rs\n").unwrap();

        assert_eq!(workspace_roots(tmp.path(), &[unit]), [] as [PathBuf; 0]);
    }
}
