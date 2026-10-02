//! `swarfr advise`: read the files that decide how big a target grows and name what to
//! change. Reports only — nothing here writes, and every finding says which file and which key
//! it is about. The numbers it quotes about the cost of each change are the measured ones in
//! `docs/research.md`.

use serde::Serialize;
use std::path::{Path, PathBuf};
use toml::{Table, Value};

use crate::inventory::{self, Target};

/// One thing to change, anchored at the file and the key it is about. The key may be missing
/// from the file: then it is the key to add.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub file: PathBuf,
    pub key: String,
    pub note: String,
}

/// What a file is. Cargo reads `[profile.*]` from both, but only a config carries `[unstable]`
/// and only the cargo home's config carries the cache settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Manifest,
    Config,
    /// `$CARGO_HOME/config.toml`.
    Home,
}

/// Profiles whose artifacts are rebuilt often and read by a debugger, so debuginfo is the thing
/// to look at; `dev` is also the one cargo gives full debuginfo by default.
const DEV_PROFILES: [&str; 2] = ["dev", "test"];

pub fn review(file: &Path, kind: Kind, doc: &Table, nightly: bool) -> Vec<Finding> {
    let mut found = Vec::new();
    let mut add = |key: &str, note: String| {
        found.push(Finding {
            file: file.to_path_buf(),
            key: key.to_string(),
            note,
        });
    };
    let profiles = doc.get("profile").and_then(Value::as_table);
    let profile = |name: &str| profiles.and_then(|table| table.get(name)?.as_table());
    if kind == Kind::Manifest {
        for name in DEV_PROFILES {
            let debug = profile(name).and_then(|table| table.get("debug"));
            // Cargo gives `dev` full debuginfo unless the manifest says otherwise, so a missing
            // key is the same finding as `debug = true`.
            let full = match debug {
                None => name == "dev",
                Some(value) => full_debuginfo(value),
            };
            if full {
                add(
                    &format!("profile.{name}.debug"),
                    "full debuginfo is the largest single thing in a dev target; \
                     `debug = \"line-tables-only\"` keeps backtraces with file and line"
                        .to_string(),
                );
            }
        }
        let deps = profile("dev")
            .and_then(|table| table.get("package")?.as_table()?.get("*")?.as_table())
            .and_then(|table| table.get("debug"));
        if deps.is_none_or(full_debuginfo) {
            add(
                "profile.dev.package.\"*\".debug",
                "dependencies are not stepped into: `[profile.dev.package.\"*\"] debug = false` \
                 drops their debuginfo and leaves your own crates untouched"
                    .to_string(),
            );
        }
        if profile("release").is_none_or(|table| !table.contains_key("strip")) {
            add(
                "profile.release.strip",
                "`strip = \"debuginfo\"` for a profile nobody debugs".to_string(),
            );
        }
        for name in DEV_PROFILES {
            // macOS defaults to `unpacked`, which leaves the debuginfo in the object files the
            // target already holds; `packed` adds a `.dSYM` bundle per binary on top.
            let packed = Some(&Value::from("packed"));
            if profile(name).and_then(|table| table.get("split-debuginfo")) == packed {
                add(
                    &format!("profile.{name}.split-debuginfo"),
                    "`packed` writes a `.dSYM` bundle per binary into the target; macOS defaults \
                     to `unpacked`, which needs no bundle"
                        .to_string(),
                );
            }
            if profile(name).and_then(|table| table.get("codegen-units")) == Some(&Value::from(1)) {
                add(
                    &format!("profile.{name}.codegen-units"),
                    "`codegen-units = 1` in a dev profile buys a little size for a much slower \
                     rebuild; it belongs in release"
                        .to_string(),
                );
            }
        }
    }
    if let Some(unstable) = doc.get("unstable").and_then(Value::as_table)
        && !nightly
    {
        let keys: Vec<&str> = unstable.keys().map(String::as_str).collect();
        add(
            "unstable",
            format!(
                "a stable toolchain ignores every key here without a word: {}",
                keys.join(", ")
            ),
        );
    }
    let incremental = doc
        .get("build")
        .and_then(|build| build.as_table()?.get("incremental"));
    if incremental == Some(&Value::from(true)) {
        add(
            "build.incremental",
            "on by default for dev profiles anyway; `swarfr run --lossy incremental` drops \
             the idle caches instead, at one non-incremental rebuild each"
                .to_string(),
        );
    }
    if kind == Kind::Home
        && doc
            .get("cache")
            .and_then(|cache| cache.as_table()?.get("auto-clean-frequency"))
            .is_none()
    {
        add(
            "cache.auto-clean-frequency",
            "cargo cleans the downloaded sources in the cargo home itself since 1.88; this tool \
             never touches them"
                .to_string(),
        );
    }
    found
}

/// Whether a `debug` value means the full debuginfo cargo puts in a dev profile by default.
fn full_debuginfo(value: &Value) -> bool {
    match value {
        Value::Boolean(on) => *on,
        Value::Integer(level) => *level == 2,
        Value::String(level) => level == "full",
        _ => false,
    }
}

/// What no single file explains: it takes the inventory to see it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Note {
    pub about: String,
    pub note: String,
}

const GIB: u64 = 1 << 30;

pub fn notes(targets: &[Target]) -> Vec<Note> {
    let mut notes = Vec::new();
    let mut add = |about: String, note: String| notes.push(Note { about, note });
    let incremental: u64 = targets.iter().map(|target| target.incremental_bytes).sum();
    if incremental > 0 {
        add(
            "incremental caches".to_string(),
            format!(
                "{:.1} GiB under the roots. `--lossy incremental` drops the idle ones and costs \
                 one non-incremental rebuild each; `incremental = false` saves more and makes \
                 every local rebuild 1.4-5x slower (docs/research.md)",
                incremental as f64 / GIB as f64
            ),
        );
    }
    for (family, targets) in families(targets) {
        if targets.len() > 1 {
            add(
                family.display().to_string(),
                format!(
                    "{} targets in one repository: a shared `[build] build-dir` (stable since \
                     1.91) builds their third-party dependencies once, at the price of \
                     serializing parallel builds on one lock",
                    targets.len()
                ),
            );
        }
        // A checkout of the same repository that has never been built: `seed` gives it a target
        // that shares its blocks with a sibling's and so costs no space.
        for checkout in inventory::checkouts(&family) {
            let built = targets
                .iter()
                .any(|target| target.root.starts_with(&checkout));
            if !built && !targets.is_empty() {
                add(
                    checkout.display().to_string(),
                    "a checkout with no target dir: `swarfr seed` clones a sibling's".to_string(),
                );
            }
        }
    }
    // What a toolchain upgrade leaves behind: cargo compiles the units again under new hashes
    // and never looks at the old ones. Nothing here removes them, so this is advice, not a pass.
    for target in targets.iter().filter(|target| target.stale_units > 0) {
        let units: usize = target.toolchains.iter().map(|built| built.units).sum();
        add(
            target.root.display().to_string(),
            format!(
                "{} of {units} units were built by a rustc that is no longer the one cargo uses                  here, about {:.1} GiB: nothing reclaims them short of `cargo clean`, because                  finding a unit's files means parsing hashed file names, which this tool does                  not do",
                target.stale_units,
                target.stale_bytes_estimate as f64 / GIB as f64
            ),
        );
    }
    let orphaned: Vec<&Target> = targets.iter().filter(|target| target.orphaned).collect();
    if !orphaned.is_empty() {
        let bytes: u64 = orphaned.iter().map(|target| target.allocated_bytes).sum();
        add(
            "orphaned worktrees".to_string(),
            format!(
                "{} targets, {:.1} GiB, of checkouts git no longer registers: `--lossy orphans`",
                orphaned.len(),
                bytes as f64 / GIB as f64
            ),
        );
    }
    let gone: Vec<&Target> = targets
        .iter()
        .filter(|target| target.project_gone && !target.orphaned)
        .collect();
    if !gone.is_empty() {
        let bytes: u64 = gone.iter().map(|target| target.allocated_bytes).sum();
        add(
            "projects gone".to_string(),
            format!(
                "{} targets, {:.1} GiB, whose `Cargo.toml` is gone (a deleted crate, or one only \
                 another branch has): `--lossy orphans --orphans-project-idle-days N`",
                gone.len(),
                bytes as f64 / GIB as f64
            ),
        );
    }
    notes
}

/// The targets grouped by repository, in a stable order. A target without a repository is its
/// own family, as everywhere else in this tool.
fn families(targets: &[Target]) -> Vec<(PathBuf, Vec<&Target>)> {
    let mut families: Vec<(PathBuf, Vec<&Target>)> = Vec::new();
    for target in targets {
        let key = target.family.clone().unwrap_or_else(|| target.root.clone());
        match families.iter_mut().find(|(seen, _)| *seen == key) {
            Some((_, group)) => group.push(target),
            None => families.push((key, vec![target])),
        }
    }
    families
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review_str(kind: Kind, text: &str, nightly: bool) -> Vec<String> {
        let doc: Table = text.parse().unwrap();
        review(Path::new("f.toml"), kind, &doc, nightly)
            .into_iter()
            .map(|finding| finding.key)
            .collect()
    }

    #[test]
    fn a_manifest_that_says_nothing_still_gets_the_default_advice() {
        let keys = review_str(Kind::Manifest, "[package]\nname = \"x\"\n", false);

        assert_eq!(
            keys,
            [
                "profile.dev.debug",
                "profile.dev.package.\"*\".debug",
                "profile.release.strip"
            ]
        );
    }

    #[test]
    fn a_manifest_that_did_the_work_gets_nothing() {
        let text = "[profile.dev]\ndebug = \"line-tables-only\"\n\
                    [profile.dev.package.\"*\"]\ndebug = false\n\
                    [profile.release]\nstrip = \"debuginfo\"\n";

        assert!(review_str(Kind::Manifest, text, false).is_empty());
    }

    #[test]
    fn full_debuginfo_is_recognised_however_it_is_spelled() {
        for value in ["true", "2", "\"full\""] {
            let text = format!(
                "[profile.test]\ndebug = {value}\n\
                 [profile.dev]\ndebug = false\n\
                 [profile.dev.package.\"*\"]\ndebug = false\n\
                 [profile.release]\nstrip = true\n"
            );

            assert_eq!(
                review_str(Kind::Manifest, &text, false),
                ["profile.test.debug"]
            );
        }
    }

    #[test]
    fn unstable_keys_are_a_finding_only_on_a_stable_toolchain() {
        let text = "[unstable]\nno-embed-metadata = true\n";

        let finding = &review(
            Path::new("f.toml"),
            Kind::Config,
            &text.parse().unwrap(),
            false,
        )[0];
        assert_eq!(finding.key, "unstable");
        assert!(finding.note.contains("no-embed-metadata"), "{finding:?}");
        assert!(review_str(Kind::Config, text, true).is_empty());
    }

    #[test]
    fn the_cache_key_is_asked_for_in_the_cargo_home_only() {
        assert_eq!(
            review_str(Kind::Home, "", false),
            ["cache.auto-clean-frequency"]
        );
        assert!(review_str(Kind::Config, "", false).is_empty());
        let set = "[cache]\nauto-clean-frequency = \"1 day\"\n";
        assert!(review_str(Kind::Home, set, false).is_empty());
    }

    #[test]
    fn profile_keys_in_a_config_are_left_to_the_manifest() {
        let text = "[profile.dev]\ndebug = true\n[build]\nincremental = true\n";

        assert_eq!(review_str(Kind::Config, text, false), ["build.incremental"]);
    }
}
