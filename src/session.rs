//! One run of the tool, whoever starts it. The CLI and the daemon are front ends that build a
//! [`Request`], call a [`Session`] and show what it returns; everything the tool *does* happens
//! here. The session never prints, never exits and never reads the environment or a config file
//! on its own: [`Settings`] carries the paths, and the helpers that resolve them from the
//! environment ([`default_index`], [`crate::config::default_path`], [`crate::eco::cargo::home::path`])
//! are functions a front end calls.
//!
//! Two sessions that change anything — `plan`, `apply` and `seed` — are kept apart by the run
//! lock, a file next to the hash index; the second one gets [`Error::RunLockHeld`]. That is the
//! whole coordination between a manual run and the daemon: no IPC.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant, SystemTime};

use crate::compress::{self, Compress};
use crate::config::Config;
use crate::dedupe::{self, Dedupe};
use crate::eco::cargo::advise::{self, Finding, Kind, Note};
use crate::eco::cargo::doc::{self, Doc, Docs};
use crate::eco::cargo::home::{self as cargo_home, Home};
use crate::eco::cargo::incremental::{self, Incremental};
use crate::eco::go::{self, MOD_CACHE};
use crate::eco::store::{self, STORE};
use crate::eco::{self, Ecosystem, cargo::CARGO};
use crate::engine::{self, Interrupt, Interrupted, Options, Pass, Report};
use crate::error::{Error, Result};
use crate::evict::{self, Evict, Limits};
use crate::index::HashIndex;
use crate::inventory::{self, Inventory, ProfileInfo, Target};
use crate::known;
use crate::orphans::{self, Orphan, Orphans};
use crate::seed::{self, Seeded};
use crate::sys;
use crate::tmpdir;

/// Relative to `$HOME`. The digit follows the index file format.
const DEFAULT_INDEX: &str = ".cache/dunnage/hashes-v1.bin";
/// The run lock's file name, in the hash index's dir.
pub const RUN_LOCK: &str = "run.lock";
/// What a report calls the one group `across_families` makes, in place of a family dir.
pub const ACROSS_FAMILIES: &str = "<across families>";
/// Every pass that deletes rebuildable data; `lossy` takes these names.
pub const LOSSY_PASSES: [&str; 5] = [
    orphans::NAME,
    evict::NAME,
    incremental::NAME,
    doc::NAME,
    tmpdir::NAME,
];
/// Every pass, in pipeline order; `passes` takes these names.
pub const PASSES: [&str; 7] = [
    orphans::NAME,
    evict::NAME,
    incremental::NAME,
    doc::NAME,
    compress::NAME,
    dedupe::NAME,
    tmpdir::NAME,
];

/// `$HOME/.cache/dunnage/hashes-v1.bin`; `None` without a `$HOME`.
pub fn default_index() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join(DEFAULT_INDEX))
}

/// How long an index entry no run has looked up is kept, unless the settings say otherwise.
pub const DEFAULT_INDEX_IDLE: Duration = Duration::from_secs(30 * SECS_PER_DAY);
const SECS_PER_DAY: u64 = 24 * 60 * 60;

/// Where a session keeps its state. The default names no file, which is enough for `inventory`
/// and `advise`: they keep no state. `plan`, `apply` and `seed` refuse it.
#[derive(Clone, Debug)]
pub struct Settings {
    /// The content-hash cache; the run lock sits next to it.
    pub index: PathBuf,
    /// Entries of the index no run has looked up for this long are dropped on save.
    pub index_idle: Duration,
    /// How long `plan` and `apply` use the build dirs of the last walk ([`known`]); zero walks
    /// every time.
    pub rediscover_every: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            index: PathBuf::new(),
            index_idle: DEFAULT_INDEX_IDLE,
            rediscover_every: known::DEFAULT_EVERY,
        }
    }
}

impl Settings {
    /// `index`, with what the config file says about keeping it.
    pub fn from_config(index: PathBuf, config: &Config) -> Self {
        let idle = config.index.idle_days;
        Self {
            index,
            index_idle: idle.map_or(DEFAULT_INDEX_IDLE, |days| {
                Duration::from_secs(days.saturating_mul(SECS_PER_DAY))
            }),
            rediscover_every: config
                .discovery
                .every_secs
                .map_or(known::DEFAULT_EVERY, Duration::from_secs),
        }
    }

    pub fn run_lock(&self) -> PathBuf {
        self.index.with_file_name(RUN_LOCK)
    }

    /// The build dirs of the last walk, next to the index; none without an index.
    pub fn known_dirs(&self) -> Option<PathBuf> {
        (!self.index.as_os_str().is_empty()).then(|| self.index.with_file_name(known::FILE))
    }
}

/// What a run is asked to do, already merged from flags and config by the front end.
#[derive(Clone, Debug, Default)]
pub struct Request {
    /// Dirs to search; a target dir itself works too.
    pub roots: Vec<PathBuf>,
    /// Only these passes; empty is every pass `lossy` does not gate.
    pub passes: Vec<String>,
    /// Lossy passes to enable.
    pub lossy: Vec<String>,
    pub evict: Limits,
    /// Remove a target dir itself once `evict` took every profile dir of it.
    pub evict_whole_target: bool,
    pub incremental_idle_days: Option<u64>,
    /// With `orphans`: also remove build dirs whose project is gone, once idle this many days.
    pub orphans_project_idle_days: Option<u64>,
    /// Leave younger files alone; `None` keeps each pass's default.
    pub min_age: Option<Duration>,
    /// Leave smaller files alone; `None` keeps each pass's default.
    pub min_size: Option<u64>,
    /// Also compress this cargo home's unpacked sources, under its own lock.
    pub cargo_home: Option<PathBuf>,
    /// Also compress these content-addressed stores, each a group of its own without a lock.
    pub stores: Vec<PathBuf>,
    /// Also compress this Go module cache, lifting the write bit of its read-only dirs.
    pub go_modcache: Option<PathBuf>,
    /// One group for every target instead of one per family.
    pub across_families: bool,
    /// Where the filesystem cannot share blocks, share build artifacts as hardlinks.
    pub link_artifacts: bool,
    /// Families to leave alone, by their dir.
    pub skip_families: Vec<PathBuf>,
    /// Per family, positions inside a checkout to leave alone, as prefixes.
    pub skip_paths: BTreeMap<PathBuf, Vec<PathBuf>>,
    /// Per family, the only adapters whose build dirs are worked on.
    pub family_ecosystems: BTreeMap<PathBuf, Vec<String>>,
    /// Repeat the passes on a group while a round still applies anything, under the same
    /// locks, so one visit leaves nothing for the next.
    pub until_settled: bool,
    /// Walk the roots even when the build dirs of the last walk still hold.
    pub rediscover: bool,
    /// The temp dir the `tmpdir` pass cleans. The front end names it: the session reads no
    /// environment.
    pub tmpdir: Option<PathBuf>,
    /// With `tmpdir`: remove top-level entries with nothing inside modified for this many days.
    pub tmpdir_idle_days: Option<u64>,
}

impl Request {
    /// What the config file asks for when no flag says otherwise.
    pub fn from_config(config: &Config) -> Self {
        Self {
            roots: config.roots.clone(),
            lossy: config.lossy.clone(),
            evict: Limits {
                idle_days: config.evict.idle_days,
                max_total_bytes: config.evict.max_total_gib.map(gib_to_bytes),
            },
            evict_whole_target: config.evict.whole_target,
            incremental_idle_days: config.incremental.idle_days,
            orphans_project_idle_days: config.orphans.project_idle_days,
            tmpdir_idle_days: config.tmpdir.idle_days,
            min_age: config.min_age.map(Duration::from_secs),
            min_size: config.min_size,
            across_families: config.across_families,
            stores: config.stores.clone(),
            skip_families: config
                .family
                .iter()
                .filter(|(_, family)| family.skip)
                .map(|(dir, _)| dir.clone())
                .collect(),
            skip_paths: config
                .family
                .iter()
                .filter(|(_, family)| !family.skip_paths.is_empty())
                .map(|(dir, family)| (dir.clone(), family.skip_paths.clone()))
                .collect(),
            family_ecosystems: config
                .family
                .iter()
                .filter_map(|(dir, family)| Some((dir.clone(), family.ecosystems.clone()?)))
                .collect(),
            ..Self::default()
        }
    }

    /// Whether this request can run at all: known pass names, and every lossy pass with the
    /// threshold it needs.
    pub fn check(&self) -> Result<()> {
        for name in &self.lossy {
            ensure(LOSSY_PASSES.contains(&name.as_str()), || {
                format!("unknown lossy pass `{name}`")
            })?;
        }
        for name in &self.passes {
            ensure(PASSES.contains(&name.as_str()), || {
                format!("unknown pass `{name}`")
            })?;
        }
        for (family, names) in &self.family_ecosystems {
            for name in names {
                ensure(eco::named(name).is_some(), || {
                    format!("unknown ecosystem `{name}` for family {}", family.display())
                })?;
            }
        }
        ensure(
            self.enables(evict::NAME) != (self.evict == Limits::default()),
            || {
                "`--lossy evict` and a limit (--evict-idle-days, --evict-max-total-gib) need each other"
                    .into()
            },
        )?;
        ensure(
            self.enables(incremental::NAME) == self.incremental_idle_days.is_some(),
            || "`--lossy incremental` and `--incremental-idle-days` need each other".into(),
        )?;
        ensure(
            self.orphans_project_idle_days.is_none() || self.enables(orphans::NAME),
            || "`--orphans-project-idle-days` needs `--lossy orphans`".into(),
        )?;
        ensure(
            self.enables(tmpdir::NAME) == self.tmpdir_idle_days.is_some(),
            || "`--lossy tmpdir` and `--tmpdir-idle-days` need each other".into(),
        )?;
        ensure(!self.enables(tmpdir::NAME) || self.tmpdir.is_some(), || {
            "`--lossy tmpdir` needs the temp dir named".into()
        })
    }

    /// Whether the `tmpdir` pass runs: enabled, and not left out by `passes`.
    fn cleans_tmpdir(&self) -> bool {
        self.enables(tmpdir::NAME)
            && (self.passes.is_empty() || self.passes.iter().any(|name| name == tmpdir::NAME))
    }

    fn enables(&self, lossy: &str) -> bool {
        self.lossy.iter().any(|name| name == lossy)
    }

    /// Whether `target` is worked on at all: its family is not skipped, its position is not,
    /// and its adapter is one the family allows. A skipped build dir is left out before the lossy
    /// passes choose, so it is neither touched nor counted.
    pub fn keeps(&self, target: &Target) -> bool {
        let family = target.family.as_ref().unwrap_or(&target.root);
        if self.skip_families.contains(family) {
            return false;
        }
        let skipped = self.skip_paths.get(family).is_some_and(|prefixes| {
            target
                .position
                .as_ref()
                .is_some_and(|position| prefixes.iter().any(|prefix| position.starts_with(prefix)))
        });
        let allowed = self
            .family_ecosystems
            .get(family)
            .is_none_or(|names| names.iter().any(|name| name == target.ecosystem));
        !skipped && allowed
    }
}

pub fn gib_to_bytes(gib: u64) -> u64 {
    gib.saturating_mul(1 << 30)
}

fn ensure(ok: bool, message: impl FnOnce() -> String) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(message()))
    }
}

/// What a front end hears while a run goes on. Every method has a default that does nothing.
pub trait Observer {
    /// A stage of the run starts that is not a group: the walk of the roots, reading the build
    /// dirs found, the temp dir.
    fn step(&self, _what: &str) {}
    /// How many groups the run is about to visit; a group let go early is visited once more.
    fn groups(&self, _total: usize) {}
    /// A group of targets is about to be worked on: a family, [`ACROSS_FAMILIES`], or a cargo
    /// home.
    fn group(&self, _group: &Path) {}
    /// The group is done, or let go early (`report.interrupted`).
    fn report(&self, _group: &Path, _report: &Report) {}
}

/// An observer that hears nothing.
pub struct Quiet;

impl Observer for Quiet {}

/// How a run is steered from outside while it goes on.
pub struct Control<'a> {
    pub observer: &'a dyn Observer,
    /// Raised, the run finishes the action it is on and returns; nothing new is started.
    pub stop: Option<&'a AtomicBool>,
    /// How long a group's build locks may be held. A group that runs out lets go, so a build
    /// waiting on a lock gets it, and is visited once more after the other groups; what is left
    /// then waits for the next run. `None` holds them until the group is done.
    pub lock_budget: Option<Duration>,
}

impl Default for Control<'_> {
    fn default() -> Self {
        Self {
            observer: &Quiet,
            stop: None,
            lock_budget: None,
        }
    }
}

impl Control<'_> {
    fn interrupt(&self) -> Interrupt<'_> {
        Interrupt {
            stop: self.stop,
            deadline: self.lock_budget.map(|budget| Instant::now() + budget),
        }
    }

    fn stopped(&self) -> bool {
        self.stop
            .is_some_and(|stop| stop.load(std::sync::atomic::Ordering::Relaxed))
    }
}

/// What a whole run did, group by group.
#[derive(Debug, Default)]
pub struct RunReport {
    pub dry_run: bool,
    /// In the order they ran. A group that ran out of lock budget appears twice.
    pub groups: Vec<(PathBuf, Report)>,
    pub compress_notes: Vec<String>,
    pub files_hashed: usize,
    /// Something was left for a later run: a build held a lock, or the budget ran out twice.
    pub left_busy: bool,
    /// The stop flag ended the run.
    pub stopped: bool,
    /// The build dirs came from a walk of the roots, not from the last walk's list.
    pub walked: bool,
    /// What the `tmpdir` pass did, when it ran.
    pub tmpdir: Option<tmpdir::Report>,
}

/// What `advise` found.
#[derive(Debug, Default)]
pub struct Advice {
    pub findings: Vec<Finding>,
    pub notes: Vec<Note>,
    /// Files that are there and could not be read as TOML, with the parser's word for it.
    pub warnings: Vec<String>,
}

/// What `seed` did, and to which target.
#[derive(Debug)]
pub struct Seeding {
    /// The target dir that was filled.
    pub target: PathBuf,
    pub seeded: Seeded,
}

/// What `worktree_add` did.
#[derive(Debug)]
pub struct WorktreeAdded {
    /// The new worktree, as git lists it.
    pub worktree: PathBuf,
    /// One per position filled; empty when no other checkout of the repository has a build
    /// dir this one lacks. An error here leaves the worktree in place; only the seeding failed.
    pub seeding: Result<Vec<Seeding>>,
}

pub struct Session {
    settings: Settings,
}

impl Session {
    pub fn open(settings: Settings) -> Self {
        Self { settings }
    }

    /// Every cargo target dir under `roots`, and the cargo home's sources when one is named.
    /// Read-only; needs no run lock.
    pub fn inventory(&self, roots: &[PathBuf], cargo_home: Option<&Path>) -> Result<Inventory> {
        let mut inventory = read_inventory(roots)?;
        if let Some(home) = cargo_home {
            inventory.cargo_home = Some(cargo_home::inspect(home)?);
        }
        Ok(inventory)
    }

    /// What the manifests and cargo configs under `roots` and the cargo home's config make
    /// bigger than it needs to be. Reads text only; needs no run lock.
    pub fn advise(&self, roots: &[PathBuf], cargo_home: Option<&Path>) -> Result<Advice> {
        let mut inventory = read_inventory(roots)?;
        // Manifests, configs and notes here are all cargo's.
        inventory
            .targets
            .retain(|target| target.ecosystem == CARGO.name());
        let mut advice = Advice::default();
        let mut read = Vec::new();
        for target in &inventory.targets {
            let Some(project) = target.project.as_deref() else {
                continue;
            };
            let files = [
                (project.join("Cargo.toml"), Kind::Manifest),
                (project.join(".cargo/config.toml"), Kind::Config),
            ];
            for (file, kind) in files {
                if read.contains(&file) {
                    continue;
                }
                review_file(&file, kind, project, &mut advice);
                read.push(file);
            }
        }
        // The cargo home is advised on even when it has no config file at all: the keys it is
        // missing are the point.
        if let Some(home) = cargo_home {
            review_file(&home.join("config.toml"), Kind::Home, home, &mut advice);
        }
        advice.notes = advise::notes(&inventory.targets);
        Ok(advice)
    }

    /// What [`apply`](Self::apply) would do, changing nothing but the hash cache.
    pub fn plan(&self, request: &Request, control: &Control) -> Result<RunReport> {
        self.run(request, control, true)
    }

    /// Plan and apply the passes, one group of targets at a time.
    pub fn apply(&self, request: &Request, control: &Control) -> Result<RunReport> {
        self.run(request, control, false)
    }

    /// Fill the empty target of `checkout` from `from` (a checkout or a target dir), or from the
    /// family's most recently built target. A checkout root with no `from` gets every position
    /// a sibling can fill, each from the sibling that built it last ([`seed::positions`]).
    pub fn seed(
        &self,
        checkout: &Path,
        from: Option<&Path>,
        dry_run: bool,
    ) -> Result<Vec<Seeding>> {
        let checkout = canonical(checkout)?;
        if from.is_none() && checkout.join(".git").exists() {
            let done = self.seed_positions(&checkout, dry_run)?;
            ensure(!done.is_empty(), || {
                "no other checkout of this repository has a build dir this one lacks; name one with --from"
                    .into()
            })?;
            return Ok(done);
        }
        // Seeding one dir names no adapter; cargo is the one whose build dir `--from` means.
        let eco: &dyn Ecosystem = &CARGO;
        let source = match from {
            Some(path) => {
                let path = canonical(path)?;
                // A checkout or its target dir; both are what somebody means by "from there".
                match eco.build_dir(&path) {
                    Some(target) if target.is_dir() => target,
                    _ => path,
                }
            }
            None => seed::choose(&checkout, eco).ok_or_else(|| {
                Error::Invalid(
                    "no other checkout of this repository has a target dir; name one with --from"
                        .into(),
                )
            })?,
        };
        let _lock = self.lock()?;
        let mut hashes = HashIndex::load(&self.settings.index);
        let seeded =
            seed::seed(&checkout, &source, eco, &mut hashes, dry_run).map_err(Error::at(
                format_args!("seeding {} from {}", checkout.display(), source.display()),
            ))?;
        if !dry_run {
            self.save(&mut hashes)?;
        }
        Ok(vec![Seeding {
            target: eco.build_dir(&checkout).unwrap_or(checkout),
            seeded,
        }])
    }

    /// Every position of the checkout root `checkout` that a sibling can fill, under one run
    /// lock. Empty when there is none.
    fn seed_positions(&self, checkout: &Path, dry_run: bool) -> Result<Vec<Seeding>> {
        let positions = seed::positions(checkout);
        if positions.is_empty() {
            return Ok(Vec::new());
        }
        let _lock = self.lock()?;
        let mut hashes = HashIndex::load(&self.settings.index);
        let mut done = Vec::with_capacity(positions.len());
        for position in positions {
            let seed::Position {
                project,
                eco,
                source,
            } = position;
            let seeded =
                seed::seed(&project, &source, eco, &mut hashes, dry_run).map_err(Error::at(
                    format_args!("seeding {} from {}", project.display(), source.display()),
                ))?;
            done.push(Seeding {
                target: eco.build_dir(&project).unwrap_or(project),
                seeded,
            });
        }
        if !dry_run {
            self.save(&mut hashes)?;
        }
        Ok(done)
    }

    /// `git worktree add <git_args>`, run in `dir`, then `seed` into the new worktree at the
    /// place `dir` has inside its own checkout — so a workspace in a subdir is seeded where it
    /// is, and a run from the checkout root seeds every position. Nothing is seeded when git
    /// fails; `dry_run` goes to `seed` only.
    pub fn worktree_add(
        &self,
        dir: &Path,
        git_args: &[OsString],
        dry_run: bool,
    ) -> Result<WorktreeAdded> {
        let dir = canonical(dir)?;
        let top = git(&dir, &["rev-parse".into(), "--show-toplevel".into()])?;
        let top = canonical(Path::new(top.trim_end()))?;
        let inside = dir
            .strip_prefix(&top)
            .unwrap_or(Path::new(""))
            .to_path_buf();
        // The new one is whichever git lists afterwards and did not before: no argument of
        // `git worktree add` has to be understood here.
        let before = worktrees(&dir)?;
        let mut add: Vec<OsString> = vec!["worktree".into(), "add".into()];
        add.extend(git_args.iter().cloned());
        git(&dir, &add)?;
        let worktree = worktrees(&dir)?
            .into_iter()
            .find(|listed| !before.contains(listed))
            .ok_or_else(|| Error::Invalid("git worktree add added no worktree".into()))?;
        let checkout = worktree.join(&inside);
        // From the checkout root every position; from a subdir only the one it is.
        let seeding = if inside.as_os_str().is_empty() {
            self.seed_positions(&checkout, dry_run)
        } else {
            match seed::choose(&checkout, &CARGO) {
                Some(source) => self.seed(&checkout, Some(&source), dry_run),
                None => Ok(Vec::new()),
            }
        };
        Ok(WorktreeAdded { worktree, seeding })
    }

    fn run(&self, request: &Request, control: &Control, dry_run: bool) -> Result<RunReport> {
        request.check()?;
        let opts = Options {
            dry_run,
            lossy: request.lossy.clone(),
            until_settled: request.until_settled,
        };
        // Named stores, canonical so the report and the unit agree, and each one checked
        // before anything is touched.
        let stores = request
            .stores
            .iter()
            .map(|dir| {
                let dir = dir.canonicalize().map_err(Error::at(dir.display()))?;
                match store::check(&dir) {
                    Some(why) => Err(Error::Invalid(format!("--store {}: {why}", dir.display()))),
                    None => Ok(dir),
                }
            })
            .collect::<Result<Vec<_>>>()?;
        let go_modcache = request
            .go_modcache
            .as_ref()
            .map(|dir| {
                let dir = dir.canonicalize().map_err(Error::at(dir.display()))?;
                match go::check(&dir) {
                    Some(why) => Err(Error::Invalid(format!("{}: {why}", dir.display()))),
                    None => Ok(dir),
                }
            })
            .transpose()?;
        let temp = request
            .tmpdir
            .as_ref()
            .filter(|_| request.cleans_tmpdir())
            .map(|dir| {
                let dir = dir.canonicalize().map_err(Error::at(dir.display()))?;
                match tmpdir::check(&dir) {
                    Some(why) => Err(Error::Invalid(format!("temp dir {}: {why}", dir.display()))),
                    None => Ok(dir),
                }
            })
            .transpose()?;
        // The cargo home, the stores, the module cache and the temp dir are runs of their own:
        // they need no target and no root.
        let only_home = request.roots.is_empty()
            && (request.cargo_home.is_some()
                || !stores.is_empty()
                || go_modcache.is_some()
                || temp.is_some());
        ensure(!request.roots.is_empty() || only_home, || {
            "no roots: name them on the command line or set `roots` in the config file".into()
        })?;
        let mut report_walked = false;
        let mut inventory = if only_home {
            Inventory::default()
        } else {
            let roots = request
                .roots
                .iter()
                .map(|root| canonical(root))
                .collect::<Result<Vec<_>>>()?;
            control.observer.step("walking the roots");
            let (found, walked) = known::discover(
                self.settings.known_dirs().as_deref(),
                &roots,
                self.settings.rediscover_every,
                request.rediscover,
                now_unix(),
            );
            report_walked = walked;
            control.observer.step("reading the build dirs");
            inventory::inventory_of(found)?
        };
        ensure(!inventory.targets.is_empty() || only_home, || {
            "no cargo target dirs found".into()
        })?;
        // After the check: a run whose every build dir the config skips has nothing to do, and
        // that is not a mistake.
        inventory.targets.retain(|target| request.keeps(target));
        // Everything above only reads. From here on the index is loaded and saved, and the passes
        // change targets: one session at a time.
        let _lock = self.lock()?;
        let index = RefCell::new(HashIndex::load(&self.settings.index));
        let (mut compress, mut dedupe) = (Compress::new(&index), Dedupe::new(&index));
        // Artifacts are only linked when the user asks; the cargo home's unpacked sources are
        // always safe to link, because cargo replaces a source dir instead of rewriting its files.
        let mut home_dedupe = Dedupe::new(&index);
        dedupe.link_fallback = CARGO.policy().share.links(request.link_artifacts);
        home_dedupe.link_fallback = Home::POLICY.share.links(request.link_artifacts);
        if let Some(min_age) = request.min_age {
            (compress.min_age, dedupe.min_age) = (min_age, min_age);
            home_dedupe.min_age = min_age;
        }
        if let Some(bytes) = request.min_size {
            (compress.min_size, dedupe.min_size) = (bytes, bytes);
            home_dedupe.min_size = bytes;
        }
        // The size cap is global, so eviction is decided over everything under the roots at once.
        let profiles: Vec<ProfileInfo> = inventory
            .targets
            .iter()
            .flat_map(|target| target.profiles.iter().cloned())
            .collect();
        let chosen = evict::select(&profiles, now_unix(), request.evict);
        let mut evict = Evict::new(chosen.clone());
        if request.evict_whole_target {
            evict = evict.whole(evict::whole_targets(&inventory.targets, &chosen));
        }
        // Cargo keeps `incremental/` for workspace members only, so this costs one plain rebuild.
        let idle_days = request.incremental_idle_days.unwrap_or(u64::MAX);
        let incremental = Incremental::new(incremental::select(&profiles, now_unix(), idle_days));
        // Whole targets of checkouts git no longer registers, and of projects gone for as long
        // as the request allows; the sources next to them stay.
        let orphans = Orphans::new(
            inventory
                .targets
                .iter()
                .filter_map(|target| {
                    let project = target.project.clone()?;
                    let reason = if target.orphaned {
                        orphans::Reason::CheckoutGone
                    } else if target.project_gone {
                        orphans::Reason::ProjectGone {
                            manifest: eco::named(target.ecosystem)?.manifest(&project)?,
                            idle_days: request.orphans_project_idle_days?,
                        }
                    } else {
                        return None;
                    };
                    Some(Orphan {
                        target: target.root.clone(),
                        project,
                        allocated_bytes: target.allocated_bytes,
                        reason,
                    })
                })
                .collect(),
            now_unix(),
        );
        // `cargo doc` writes this dir again from scratch and no build reads it.
        let docs = Doc::new(
            inventory
                .targets
                .iter()
                .filter(|target| target.doc_bytes > 0)
                .map(|target| Docs {
                    target: target.root.clone(),
                    allocated_bytes: target.doc_bytes,
                })
                .collect(),
        );
        // Pipeline order (`DESIGN.md`): orphans, evict, incremental, doc, compress, dedupe.
        let all: [&dyn Pass; 6] = [&orphans, &evict, &incremental, &docs, &compress, &dedupe];
        let passes: Vec<&dyn Pass> = all
            .into_iter()
            .filter(|pass| {
                request.passes.is_empty() || request.passes.iter().any(|name| name == pass.name())
            })
            .collect();
        // One group per family keeps a run's locks inside the repository it is working on. Across
        // families every target is compared with every other — unrelated projects do share
        // artifacts — and the price is that the locks of all of them are held for the whole run.
        // A group is also one adapter's, whose guards the engine takes.
        type Group = (&'static dyn Ecosystem, Vec<PathBuf>);
        let mut groups: BTreeMap<(PathBuf, &'static str), Group> = BTreeMap::new();
        for target in inventory.targets {
            // Every inventoried dir was claimed by a registered adapter.
            let Some(eco) = eco::named(target.ecosystem) else {
                continue;
            };
            let family = target.family.unwrap_or_else(|| target.root.clone());
            // Not a path: the group is every family at once, and the report says so.
            let key = if request.across_families {
                PathBuf::from(ACROSS_FAMILIES)
            } else {
                family
            };
            let dirs = target.profiles.into_iter().map(|profile| profile.dir);
            groups
                .entry((key, eco.name()))
                .or_insert_with(|| (eco, Vec::new()))
                .1
                .extend(dirs);
        }

        let mut report = RunReport {
            dry_run,
            walked: report_walked,
            ..RunReport::default()
        };
        control.observer.groups(
            groups.len()
                + usize::from(request.cargo_home.is_some())
                + stores.len()
                + usize::from(go_modcache.is_some()),
        );
        let mut again = Vec::new();
        for ((group, _), (eco, profile_dirs)) in &groups {
            if control.stopped() {
                break;
            }
            let done = visit(
                group,
                profile_dirs,
                &passes,
                &opts,
                *eco,
                control,
                &mut report,
            )?;
            if done == Some(Interrupted::OutOfBudget) {
                again.push((group, *eco, profile_dirs));
            }
        }
        for (group, eco, profile_dirs) in again {
            if control.stopped() {
                break;
            }
            let done = visit(
                group,
                profile_dirs,
                &passes,
                &opts,
                eco,
                control,
                &mut report,
            )?;
            report.left_busy |= done == Some(Interrupted::OutOfBudget);
        }
        // One more group, guarded by cargo's own home lock instead of per-profile locks. Only
        // `compress` runs here: these are unpacked sources, not build output.
        if let Some(home) = request.cargo_home.as_deref().filter(|_| !control.stopped()) {
            let dirs = cargo_home::dirs(home);
            ensure(!dirs.is_empty(), || {
                format!(
                    "no {} or {} in {}",
                    cargo_home::DIRS[0],
                    cargo_home::DIRS[1],
                    home.display()
                )
            })?;
            let lock = home.join(cargo_home::LOCK_FILE);
            ensure(lock.is_file(), || {
                format!(
                    "no {} in {}: cargo has never used it as its home",
                    cargo_home::LOCK_FILE,
                    home.display()
                )
            })?;
            // Compression, and sharing with the home's own policy: on a filesystem without
            // copy-on-write these sources are the one place a hardlink is safe.
            let home_passes: Vec<&dyn Pass> = passes
                .iter()
                .copied()
                .filter(|pass| pass.name() == compress::NAME)
                .chain(
                    passes
                        .iter()
                        .any(|pass| pass.name() == dedupe::NAME)
                        .then_some(&home_dedupe as &dyn Pass),
                )
                .collect();
            let adapter = Home {
                home: home.to_path_buf(),
            };
            let done = visit(
                home,
                &dirs,
                &home_passes,
                &opts,
                &adapter,
                control,
                &mut report,
            )?;
            report.left_busy |= done == Some(Interrupted::OutOfBudget);
        }
        // A store gets `compress` only: dedupe finds nothing in it by construction.
        let store_passes: Vec<&dyn Pass> = passes
            .iter()
            .copied()
            .filter(|pass| pass.name() == compress::NAME)
            .collect();
        for dir in stores.iter().filter(|_| !control.stopped()) {
            let done = visit(
                dir,
                std::slice::from_ref(dir),
                &store_passes,
                &opts,
                &STORE,
                control,
                &mut report,
            )?;
            report.left_busy |= done == Some(Interrupted::OutOfBudget);
        }
        // Unpacked module sources, like the cargo home's, but immutable like a store: `compress`
        // only, no lock.
        if let Some(dir) = go_modcache.as_ref().filter(|_| !control.stopped()) {
            let units = MOD_CACHE.units(dir).map_err(Error::at(dir.display()))?;
            let done = visit(
                dir,
                &units,
                &store_passes,
                &opts,
                &MOD_CACHE,
                control,
                &mut report,
            )?;
            report.left_busy |= done == Some(Interrupted::OutOfBudget);
        }
        // Last: nothing above reads the temp dir, and the index dir is kept whatever its age.
        if let Some(dir) = temp.as_ref().filter(|_| !control.stopped()) {
            let keep: Vec<PathBuf> = self
                .settings
                .index
                .parent()
                .and_then(|parent| parent.canonicalize().ok())
                .into_iter()
                .collect();
            control.observer.step("the temp dir");
            let in_use = sys::paths_in_use(dir);
            let idle_days = request.tmpdir_idle_days.unwrap_or(u64::MAX);
            report.tmpdir = Some(
                tmpdir::run(
                    dir,
                    idle_days,
                    in_use.as_deref(),
                    &keep,
                    SystemTime::now(),
                    dry_run,
                )
                .map_err(Error::at(dir.display()))?,
            );
        }
        report.stopped = control.stopped();
        report.compress_notes = compress.notes();
        report.files_hashed = dedupe.hashed();
        // The index only caches hashes of files as they are, so it is worth keeping on a dry run too.
        self.save(&mut index.borrow_mut())?;
        Ok(report)
    }

    /// The run lock, held until dropped. Taken before the index is loaded, so no two sessions
    /// ever write the index over each other.
    fn lock(&self) -> Result<File> {
        ensure(!self.settings.index.as_os_str().is_empty(), || {
            "no hash index in the session's settings".into()
        })?;
        let path = self.settings.run_lock();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(Error::at(dir.display()))?;
        }
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(Error::at(path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(file),
            Err(TryLockError::WouldBlock) => Err(Error::RunLockHeld(path)),
            Err(TryLockError::Error(error)) => Err(Error::at(path.display())(error)),
        }
    }

    fn save(&self, hashes: &mut HashIndex) -> Result<()> {
        let path = &self.settings.index;
        hashes.expire(self.settings.index_idle);
        hashes
            .save(path)
            .map_err(Error::at(format_args!("saving {}", path.display())))
    }
}

/// `git <args>` in `dir`, its stdout on success. On failure git's own words are the error.
fn git(dir: &Path, args: &[OsString]) -> Result<String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(Error::at("running git"))?;
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stderr);
        let args: Vec<_> = args.iter().map(|arg| arg.to_string_lossy()).collect();
        return Err(Error::Invalid(format!(
            "git {} failed: {}",
            args.join(" "),
            said.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Every worktree of the repository `dir` is in, canonical.
fn worktrees(dir: &Path) -> Result<Vec<PathBuf>> {
    let listed = git(
        dir,
        &["worktree".into(), "list".into(), "--porcelain".into()],
    )?;
    Ok(listed
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(|path| {
            let path = PathBuf::from(path);
            path.canonicalize().unwrap_or(path)
        })
        .collect())
}

/// One engine run over one group, told to the observer on both sides.
fn visit(
    group: &Path,
    dirs: &[PathBuf],
    passes: &[&dyn Pass],
    opts: &Options,
    eco: &dyn Ecosystem,
    control: &Control,
    report: &mut RunReport,
) -> Result<Option<Interrupted>> {
    control.observer.group(group);
    let done = engine::run_with(dirs, passes, opts, eco, control.interrupt())?;
    control.observer.report(group, &done);
    report.left_busy |= !done.busy.is_empty();
    let interrupted = done.interrupted;
    report.groups.push((group.to_path_buf(), done));
    Ok(interrupted)
}

/// Canonical, so that families, scanned paths and locked dirs all compare equal.
fn read_inventory(roots: &[PathBuf]) -> Result<Inventory> {
    let roots = roots
        .iter()
        .map(|root| canonical(root))
        .collect::<Result<Vec<_>>>()?;
    Ok(inventory::inventory(&roots)?)
}

fn canonical(path: &Path) -> Result<PathBuf> {
    path.canonicalize().map_err(Error::at(path.display()))
}

/// One file's findings. A file that is not there is not a finding of its own — except for the
/// cargo home's config, whose absent keys `review` reports from an empty document.
fn review_file(file: &Path, kind: Kind, dir: &Path, advice: &mut Advice) {
    let text = match fs::read_to_string(file) {
        Ok(text) => text,
        Err(_) if kind == Kind::Home => String::new(),
        Err(_) => return,
    };
    let doc: toml::Table = match text.parse() {
        Ok(doc) => doc,
        Err(error) => {
            advice.warnings.push(format!("{}: {error}", file.display()));
            return;
        }
    };
    // Only asked when the answer matters, since it costs a process.
    let nightly = doc.get("unstable").is_some() && nightly_toolchain(dir);
    advice
        .findings
        .extend(advise::review(file, kind, &doc, nightly));
}

/// Whether the toolchain cargo would use in `dir` is a nightly one, which is the only one that
/// reads `[unstable]`. A rustc that cannot be run at all is treated as stable: the advice is
/// then about a key that does nothing, which is still the safer thing to say.
fn nightly_toolchain(dir: &Path) -> bool {
    std::process::Command::new("rustc")
        .arg("--version")
        .current_dir(dir)
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("nightly"))
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since_epoch| since_epoch.as_secs())
}
