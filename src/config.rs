//! `~/.config/swarfr/config.toml`: what `run` does when no flags say otherwise. Every key is
//! optional, unknown keys are an error (a typo that silently does nothing is worse than a stop),
//! and a flag always wins over the file.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};

/// Under `$XDG_CONFIG_HOME`, or `$HOME/.config` when that is not set.
const RELATIVE: &str = "swarfr/config.toml";

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    /// What `run` searches when the command line names no root.
    #[serde(default)]
    pub roots: Vec<PathBuf>,
    /// Lossy passes to enable, as `--lossy` would. They still need their thresholds.
    #[serde(default)]
    pub lossy: Vec<String>,
    /// Leave files younger than this alone, in seconds; both lossless passes.
    pub min_age: Option<u64>,
    /// Leave files smaller than this alone; both lossless passes.
    pub min_size: Option<u64>,
    /// Compare targets of different repositories too, as `--across-families` does.
    #[serde(default)]
    pub across_families: bool,
    /// Content-addressed stores to compress on every run, as `--store` does.
    #[serde(default)]
    pub stores: Vec<PathBuf>,
    #[serde(default)]
    pub evict: Evict,
    #[serde(default)]
    pub incremental: Incremental,
    #[serde(default)]
    pub index: Index,
    #[serde(default)]
    pub orphans: OrphansConfig,
    #[serde(default)]
    pub daemon: Daemon,
    #[serde(default)]
    pub discovery: Discovery,
    /// Per-family overrides, keyed by the family's dir: the git common dir of the repository and
    /// its worktrees, or the target dir itself when there is no repository.
    #[serde(default)]
    pub family: BTreeMap<PathBuf, Family>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Evict {
    pub idle_days: Option<u64>,
    pub max_total_gib: Option<u64>,
    /// Take a target dir whole once every profile dir of it is evicted.
    #[serde(default)]
    pub whole_target: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Incremental {
    pub idle_days: Option<u64>,
}

/// `[orphans]`: the pass that removes build dirs whose reason is gone.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct OrphansConfig {
    /// Also remove build dirs whose project manifest is gone, once idle this many days.
    pub project_idle_days: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Index {
    /// Drop hash-index entries no run has looked up for this many days.
    pub idle_days: Option<u64>,
}

/// `[discovery]`: how long the build dirs a walk found are used instead of walking again.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Discovery {
    /// Walk the roots again when the last walk is this old; 0 walks on every run.
    pub every_secs: Option<u64>,
}

/// `[daemon]`: how often `swarfr daemon run` looks, and how long it may keep a build waiting.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Daemon {
    /// The longest sleep between two looks at the known build dirs.
    pub interval_secs: Option<u64>,
    /// How often the roots are walked again for new build dirs.
    pub rediscover_secs: Option<u64>,
    /// How long a group may hold a build's locks before it lets go.
    pub lock_budget_secs: Option<u64>,
}

/// Only what is decided per family. The `evict` cap and the idle rules are global, because the
/// passes choose over everything under the roots at once.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Family {
    /// Leave this family alone entirely.
    #[serde(default)]
    pub skip: bool,
    /// Leave alone the build dirs at these positions, relative to their checkout: a position
    /// that starts with one of them, whole components, is skipped in every checkout.
    #[serde(default)]
    pub skip_paths: Vec<PathBuf>,
    /// Only these adapters' build dirs, by name; every adapter when not set.
    pub ecosystems: Option<Vec<String>>,
}

impl Config {
    /// The file, or the defaults when it is not there. A file that is there and unreadable or
    /// invalid is an error: a run that silently ignores its configuration is worse than no run.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(Error::at(format_args!("reading {}", path.display()))(error)),
        };
        toml::from_str(&text).map_err(|source| Error::Config {
            path: path.to_path_buf(),
            source,
        })
    }
}

/// `$XDG_CONFIG_HOME/swarfr/config.toml`, else `$HOME/.config/swarfr/config.toml`.
pub fn default_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(xdg) if !xdg.is_empty() => PathBuf::from(xdg),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join(RELATIVE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_the_default_and_a_broken_one_is_an_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");

        assert_eq!(Config::load(&path).unwrap(), Config::default());

        fs::write(&path, "roots = 3\n").unwrap();
        let error = format!("{:#}", Config::load(&path).unwrap_err());
        assert!(error.contains("config.toml"), "{error}");
        assert!(error.contains("roots"), "{error}");
    }

    #[test]
    fn an_unknown_key_names_itself() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(&path, "min-aeg = 60\n").unwrap();

        let error = format!("{:#}", Config::load(&path).unwrap_err());

        assert!(error.contains("min-aeg"), "{error}");
    }

    #[test]
    fn the_keys_are_kebab_case_and_families_are_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        fs::write(
            &path,
            "roots = [\"/a\", \"/b\"]\n\
             lossy = [\"evict\"]\n\
             min-age = 60\n\
             min-size = 4096\n\
             [evict]\n\
             idle-days = 30\n\
             max-total-gib = 50\n\
             [incremental]\n\
             idle-days = 7\n\
             [index]\n\
             idle-days = 90\n\
             [family.\"/a/repo\"]\n\
             skip = true\n\
             [family.\"/a/mono\"]\n\
             skip-paths = [\"vendor\"]\n\
             ecosystems = [\"cargo\"]\n",
        )
        .unwrap();

        let config = Config::load(&path).unwrap();

        assert_eq!(config.roots, [PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(config.lossy, ["evict"]);
        assert_eq!((config.min_age, config.min_size), (Some(60), Some(4096)));
        assert_eq!(config.evict.idle_days, Some(30));
        assert_eq!(config.evict.max_total_gib, Some(50));
        assert_eq!(config.incremental.idle_days, Some(7));
        assert_eq!(config.index.idle_days, Some(90));
        assert!(
            config
                .family
                .get(Path::new("/a/repo"))
                .is_some_and(|family| family.skip)
        );
        assert!(
            !config
                .family
                .get(Path::new("/a/other"))
                .is_some_and(|family| family.skip)
        );
        let mono = &config.family[Path::new("/a/mono")];
        assert_eq!(mono.skip_paths, [PathBuf::from("vendor")]);
        assert_eq!(mono.ecosystems.as_deref(), Some(&["cargo".to_owned()][..]));
    }
}
