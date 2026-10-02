# Architecture for many build systems and monorepos

A proposal. `DESIGN.md` stays the contract for what exists; this document says what the code
should turn into so that a second build system is an adapter and a monorepo — one repository,
many projects, several build systems, several worktrees — is the normal case rather than a
special one. Which build systems are worth it is in `docs/ecosystems.md`; the tasks are T28–T35.

Nothing here changes what the tool does for a cargo project today. That is the acceptance test
of the first step.

## What breaks today when the repository is a monorepo

Read off the code, not guessed:

1. **One build system.** `model::is_cargo_target` was the only way a dir became work (now
   `eco::discover` over a registry of one).
2. **Family comes from where the build dir is**, not from what it was built from:
   `inventory::git_link` walks up from the target looking for `.git`. A build dir outside the
   checkout — cargo's `build.build-dir`, an out-of-tree CMake dir, Xcode's DerivedData, a Bazel
   output base — has no family, so it gets no dedupe partner, no `seed` source and no `orphans`.
   (T38.1: a cargo target dir moved out gets its owner from its dep-info; a `build-dir` not yet.)
3. **`seed` knows one position.** `seed::choose` already has the right idea — "the same place
   inside the sibling checkout" — but for one dir with the hardcoded name `target`. A monorepo
   checkout has dozens of build dirs to seed.
4. **`orphans` knew one kind of owner** (T38 added the second, *project gone*): a worktree whose git record is gone. In a monorepo the
   common orphan is smaller: a project deleted, renamed, or absent on this branch, whose `obj/`
   or build dir stays behind in a checkout that is very much alive.
5. **Discovery is one walk per question.** One walk with one marker is fine; five adapters doing
   five walks over a monorepo's source tree is not.
6. **Nothing stops a second adapter from claiming a dir inside a first one's.** Cargo targets
   contain CMake build dirs (`target/*/build/*/out/build/CMakeCache.txt`, from build scripts).
   Those belong to cargo's build and sit under cargo's lock.
7. **`status` prints one line per target.** Three hundred `bin/` dirs is not a report.
8. **Config scopes by repository only** (`[family."…"] skip`). In a monorepo the repository is
   everything.

What already fits and stays: the inode model, the engine's re-check + temp + `rename`, the hash
index keyed by `(dev, ino, size, mtime)`, `evict` by unit rather than by repository (a monorepo
is always active, most of its build dirs are idle), locks taken in sorted path order, and
`engine::Locks::{PerDir, Shared}` — which was a guard abstraction waiting for a third variant,
and is now `eco::Guard`.

## Vocabulary

| Term | Meaning | Today |
| --- | --- | --- |
| Family | a repository and its worktrees, keyed by git common dir | same |
| Checkout | one working tree of a family | implicit |
| Project | the manifest a build dir was built from: a cargo workspace root, a `.csproj`, a CMake source dir, a `Package.swift` | implicit: the target's parent |
| Build dir | a dir one adapter claimed by its marker | `Target` |
| Unit | the smallest dir guarded by one guard and scanned as one `Profile` | a profile dir |
| Store | a per-user cache with no project: the cargo home, `GOCACHE`, `~/.cabal/store` | `cargo_home` |
| Position | the project's path relative to its checkout root | inside `seed::choose` |
| Owner | project + checkout: what must exist for the build dir to have a reason | the dir above `target` |

The key move is **owner before location**: family, position and orphan status are all computed
from the owner the adapter names, never from where the build dir happens to sit. For cargo with
default settings the two coincide, which is why nothing changes there.

`(family, position, ecosystem)` names "the same build dir" across worktrees. It is what `seed`
copies between, what the report groups by, and the first place dedupe finds a twin.

## The adapter

One trait, one implementation per build system, registered in a fixed order. A sketch, not an
API freeze:

```rust
pub trait Ecosystem: Sync {
    fn name(&self) -> &'static str;                       // "cargo", "dotnet", "cmake", …

    /// Asked once per directory of the shared walk, with the names already read.
    /// Must decide from names and at most one small file; no walking of its own.
    fn claim(&self, dir: &Path, names: &DirNames) -> Option<Claim>;

    /// Per-user places that no root contains: DerivedData, a Bazel output user root.
    fn well_known(&self) -> Vec<PathBuf> { Vec::new() }

    fn owner(&self, build_dir: &Path) -> Option<Owner>;   // project manifest + source dir
    fn units(&self, build_dir: &Path) -> io::Result<Vec<Unit>>;
    fn guard(&self, unit: &Unit) -> Guard;
    fn volatile(&self, unit: &Unit, relative: &Path) -> bool;  // never scanned, never seeded
    fn last_used(&self, unit: &Unit) -> Option<SystemTime>;
    fn policy(&self, unit: &Unit) -> Policy;
    fn passes(&self) -> Vec<Box<dyn Pass>> { Vec::new() } // cargo: `incremental`, `doc`
}

pub struct Claim { pub build_dirs: Vec<PathBuf> }         // .NET claims `obj/` and its `bin/`

pub enum Guard {
    Held,               // reserved: an embedding caller already holds the build's lock
    Lock(PathBuf),      // the build holds it for the whole build: cargo, SwiftPM
    Shared(PathBuf),    // one lock for many dirs: cargo home's `.package-cache`
    Quiet,              // no lock exists: T29's tier — min-age, busy files, process check
    Immutable,          // content-addressed store: a name never gets other bytes (T33)
}

pub struct Policy {
    pub share: Sharing,     // ClonesOnly | LinkOptIn | LinkSafe
    pub seed: bool,         // does a build dir survive a change of absolute path?
    pub lossy: bool,        // may whole units be removed at all?
}
```

How the adapters of `docs/ecosystems.md` answer:

| | claim marker | unit | guard | share | seed |
| --- | --- | --- | --- | --- | --- |
| cargo target | `CACHEDIR.TAG` with cargo's sentence | profile dir | `Lock(.cargo-lock)` | `LinkOptIn` | yes |
| cargo home | `--cargo-home` | `registry/src`, `git/checkouts` | `Shared(.package-cache)` | `LinkSafe` | — |
| SwiftPM | `.build/` next to `Package.swift` | `.build/` | `Lock` (spike) | `ClonesOnly` | no |
| Xcode | `info.plist` with `WorkspacePath`, via `well_known` | the DerivedData entry | `Quiet` | `ClonesOnly` | no |
| .NET | `obj/project.assets.json` | `obj/`, `bin/` | `Quiet` | `ClonesOnly`, link refused | no |
| CMake / Meson / Ninja | `CMakeCache.txt`, `meson-info/`, `.ninja_log` | the build dir | `Quiet` | `ClonesOnly` | no |
| immutable store, Go | named by the user, `go env` | the store | `Immutable` | — | — |

Rules the engine enforces, so that no adapter can get them wrong:

- A lossy pass needs `Guard::Lock` / `Shared` held, or `Quiet` with every check clear. Never
  `Immutable`: the tool that owns a store evicts from it.
- A dedupe group whose members have different `Sharing` uses the strictest one.
- `--link-artifacts` is ignored with a printed reason where the policy is `ClonesOnly`.
- The report names the guard tier of every unit. `Quiet` is a weaker promise than `DESIGN.md`'s
  invariant 1 and is never printed as if it were the same.

## Discovery: one walk, outermost claim wins

```
for each root (and each enabled adapter's well_known dirs):
    walk, never following symlinks, never entering `.git`
    for each dir: read its names once, ask the enabled adapters in registry order
        first Some(claim) wins → record it → do not descend
```

- **Outermost wins** settles nesting without a rule per pair: the CMake dir inside a cargo target
  is never seen, because the walk stopped at the target. The same holds for whatever a build
  orchestrator (Bazel, Nx, Gradle) keeps under its own output dir.
- **One walk** however many adapters are on. The cost of a monorepo is its source tree, and that
  is paid once.
- **A claim is cheap by contract** — names, plus at most one small read to tell cargo's
  `CACHEDIR.TAG` from Gradle's. An adapter that needs more does it in `units`, after the claim.
- **Adapters are opt-in.** `ecosystems = ["cargo"]` is the default, so an upgrade never starts
  touching dirs the user did not ask about. `--ecosystem <name>` adds one for a run.
- If the walk itself is measured to hurt on a large monorepo, git can name the candidates: build
  dirs are ignored dirs, and `git ls-files --others --ignored --exclude-standard --directory`
  lists those without visiting tracked trees. Measure first; not part of the first step.
- **Done in T40: measured, and the walk is skipped instead.** On a 320k-file synthetic monorepo
  the walk was about 40% of a settled re-run. `src/known.rs` keeps the last walk next to the
  index and reuses it while the roots, the cadence and the roots' own mtimes hold, claiming each
  entry again; `--rediscover` walks now. The git listing stays unneeded until a walk that is
  paid once an hour hurts.

## Monorepo behavior, pass by pass

**dedupe.** Scope stays the family, across positions and across adapters: the size-bucket
prefilter makes an unrelated pair cost nothing, and "across positions inside one checkout" is
exactly where .NET's duplicate NuGet assemblies are. Lock width grows with the family — every
`Lock` unit of it is held for the run — but the large families are `Quiet` ones, which hold
nothing. If a cargo monorepo with many workspaces shows builds waiting in a benchmark, the cure
is to run the family position by position first; not before it is measured.

**seed.** `seed` in a checkout root seeds every position: for each build dir of the sibling
checkouts whose adapter says `seed: true`, if the owner's project exists in the new checkout
and the position is empty there, clone it from the sibling where that position was built most
recently. The source is chosen per position, not per checkout — in a monorepo no single
worktree is the newest everywhere. Positions whose project is absent on this branch are skipped,
which is the owner check doing its job. `seed <dir>` keeps today's meaning: that one position.

**orphans.** Two reasons, one pass, both printed:

- *checkout gone* — today's rule, now covering every build dir whose owner is in that checkout,
  out-of-tree ones included;
- *project gone* — the owner's manifest no longer exists in a live checkout. A branch switch
  produces the same picture as a deletion, so this reason also requires the unit to be idle for
  `evict`'s `idle-days` (or an `orphans` threshold of its own); without a threshold it is only
  reported.

**evict.** Unchanged in kind: per unit, by `last_used`, one size cap over everything under the
roots whatever the adapter. The adapter supplies `last_used` because "newest top-level mtime" is
cargo's truth, not everyone's.

**compress.** Unchanged. It is the pass every adapter gets for free.

**status.** Family → checkout → per-ecosystem subtotal, then the largest build dirs up to a
limit, `--all` for the rest. `--json` stays flat and gains `ecosystem`, `checkout`, `position`
and `guard` per build dir; existing keys keep their names.

**config.** Two additions, both without a glob dependency:

```toml
ecosystems = ["cargo", "dotnet"]

[family."/Users/me/code/monorepo/.git"]
skip-paths = ["services/legacy", "third_party"]   # positions, as prefixes
ecosystems = ["cargo"]                            # narrower than the global list
```

## Process model: one library, three front ends

Decided by the creator: the tool runs as a **CLI** and as a **daemon**, the two share as much
code as can be shared, and it must stay possible to **embed it as a library** in a build system
later (not built now). An earlier draft of this section argued for a CLI alone; its arguments
survive below as constraints on how the daemon and an embedding behave, not as a reason to have
neither.

```
the library `swarfr` — no clap, no printing, no exit codes, no env reads
    Session: open(settings) → inventory() → plan(request) → apply(plan) → Report
    eco/ adapters · engine · model · index · passes · sys/

front ends, each a thin caller of Session
    CLI        flags → Request; Report → text / JSON / exit code
    daemon     triggers → the same Request; the same Report → log and state file
    embedding  a build system, for the one build dir whose lock it already holds (later)
```

**Where this stands.** Done in T36: the run moved out of `src/main.rs` into `src/session.rs`,
and the binary is parsing, printing and the exit code. What landed, in short (`DESIGN.md`,
"Session", has the rest):

```rust
pub struct Session { settings: Settings }             // the index path; the run lock next to it

impl Session {
    pub fn open(settings: Settings) -> Self;
    pub fn inventory(&self, roots: &[PathBuf], cargo_home: Option<&Path>) -> Result<Inventory>;
    pub fn advise(&self, roots: &[PathBuf], cargo_home: Option<&Path>) -> Result<Advice>;
    pub fn plan(&self, request: &Request, control: &Control) -> Result<RunReport>;  // dry run
    pub fn apply(&self, request: &Request, control: &Control) -> Result<RunReport>;
    pub fn seed(&self, checkout: &Path, from: Option<&Path>, dry_run: bool) -> Result<Seeding>;
}

pub struct Control<'a> { pub observer: &'a dyn Observer,     // each group, before and after
                         pub stop: Option<&'a AtomicBool>,    // checked between two actions
                         pub lock_budget: Option<Duration> }  // longest a group holds its locks
```

`plan` is a dry run of the same pipeline rather than a `Plan` value handed to `apply`: each pass
plans on what the pass before it left behind, so a plan made up front would be wrong by the
second pass. `Request` holds what flags and config ask for today; `until_settled` joins it with
T25, and a `Scope` in place of `roots` with the adapters (T28).

Rules that keep all three front ends possible, each of them checkable:

- The library never prints, never exits, never reads the environment or a config file on its
  own; `Settings` carries the paths. Helpers that resolve `XDG_CONFIG_HOME` or `CARGO_HOME` stay,
  as functions a front end chooses to call.
- A typed `Error`, not `anyhow`; `anyhow` and `clap` belong to the binary. The proof is a
  build: `cargo check --lib --no-default-features` with `clap` and `anyhow` behind a default
  `cli` feature the `[[bin]]` requires. That is the whole cost of "embeddable later" today — no
  workspace, no second crate, until an embedding exists to ask for one.
- Everything a front end shows comes out of `Report`, `Inventory` and `Observer` events, which
  are already serializable. If the CLI needs a fact the daemon cannot get, the fact is in the
  wrong place.
- Synchronous API, `rayon` inside, no async runtime: a build system that embeds the library
  brings its own, or none.
- The run lock (one file next to the hash index, `try_lock`) is taken by `plan`, `apply` and
  `seed`. CLI
  and daemon are two processes on one machine and must not work on the same files at once;
  `Quiet` and `Immutable` units have no build lock that would keep them apart. There is **no
  IPC**: the two coordinate through that lock and through files — the hash index, the daemon's
  state file — so neither is the other's client and either works without the other.

**The daemon** (T34) is `swarfr daemon`: the same binary, a foreground process that the
service manager keeps alive — launchd, a systemd user unit, a Windows service later.
`swarfr daemon install | remove | status` writes and removes that unit and reads the state
file; the process never forks itself into the background. What is daemon-only is small:

- *Triggers.* A slow timer re-runs discovery; a fast one looks at known units. A filesystem
  watcher is one more trigger behind the same trait, watching only the top level of known units
  — a handful of dirs, not a monorepo's source tree, which is what `inotify` limits would not
  survive. The watcher crate (`notify`) is the creator's call when T34 is claimed; the timers
  work without it.
- *Per-unit due times.* The CLI asks "what is cold now"; the daemon knows when each unit was
  last written and schedules it for `last write + min-age`. That is the real gain over a cron
  line: work happens once per build, when its files have gone cold, and not at all otherwise.
- *A state file* — last report, next due times, what was busy — that `daemon status` and
  `status` print.

What the earlier arguments still demand of it:

- **A build must never wait for the daemon.** Cargo blocks on `.cargo-lock`; a user who started
  a CLI run knows why, a user with a daemon does not. Hence `lock_budget`: the engine stops
  taking new actions for a unit once the budget is spent, releases the lock and comes back in a
  later round. The session has the knob; the CLI leaves it unset, the daemon sets it low.
- **Lossy passes run from the daemon only when the config enables them**, exactly as from a
  scheduled CLI run. No trigger turns one on.
- **Low priority** is the service unit's job (`Nice`, `LowPriorityIO`, `IOSchedulingClass=idle`),
  not code.

**Embedding** (not built; R8 in `roadmap.md`). What keeps it possible is already listed: the
library rules above, plus one reserved guard — `Guard::Held`, "the caller holds this build's
lock" — so that a build system can run a pass on its own build dir at a moment it chooses,
without the engine trying to take a lock its caller already has. The reasons hooks make a poor
*default* stand (hot files, cross-dir passes, committed files), which is why embedding is an
option for a build system's authors and not something the tool installs into projects.

## Layout in the crate

One package, one binary, a `cli` feature (see "Process model"). Two new boundaries — the
session above the passes and `src/eco/` beside `src/sys/`:

```
src/main.rs       flags → Request, Report → text / JSON / exit code; `daemon` subcommand   (cli)
src/daemon/       triggers, due times, state file, service units                          (cli)
src/session.rs    Session, Request, Plan, Control, Observer, Error, the run lock          (lib)
src/eco/mod.rs    the trait, Claim / Unit / Guard / Policy, the registry, the shared walk
src/eco/cargo/    CACHEDIR.TAG, profile dirs, .cargo-lock, last_built, incremental, doc,
                  toolchains, advise, cargo_home, what seed leaves behind
src/eco/<name>.rs one file per later adapter
src/sys/          everything that differs between platforms                          (exists)
src/…             engine, model, index, inventory, dedupe, compress, evict, orphans, seed, config
```

The rule that goes with it, stated the way `sys` states its own: no build system's name, file
or directory appears outside `src/eco/`. `model::CARGO_LOCK_FILE` and the `TARGET` constant in
`seed.rs` were the first things to move.

**Where this stands.** Done in T28: `src/eco/mod.rs` has the trait, `Guard`, `Policy`, the
registry and the shared walk; `src/eco/cargo/` the cargo adapter, the cargo home adapter and
every cargo-only module. The engine, the inode model, `seed`, `evict` and `orphans` no longer
name a cargo file or dir. Differences from the sketch above: `claim` answers yes or no (a
`Claim` with several dirs waits for .NET), units are paths, `private` and `volatile` are two
questions (the lock file is never scanned, `incremental/` is scanned but never seeded), and
`Policy` carries only `share` until a task needs the rest. Still naming cargo outside
`src/eco/`: the session, which wires cargo's passes and reports, `seed`'s choice of adapter
(T37 seeds every position) and the inventory's cargo-shaped status fields. `tests/monorepo.rs`
is the monorepo fixture with the assertions that hold today: each build dir found once, the
nested ones nobody's, every position one family, `seed` choosing by position.

Done in T29: `Guard::Quiet` works, with the tier of `DESIGN.md`, "Safety tier without a build
lock"; `Ecosystem::tools` names the processes the check looks for. No adapter uses it yet.
Done in T33: `Guard::Immutable` and the `Store` adapter behind `--store`; `DESIGN.md`,
"Immutable stores".
Done in T35: `run --go`, with the `go modcache` adapter and `Ecosystem::lifts_read_only_dirs`;
`DESIGN.md`, "Go module cache".
Done in T37: `seed::positions` seeds every position of a checkout root, each from the sibling
that built it last, with the adapter the shared walk found; `seed <dir>` with one dir still
assumes cargo.

Done in T34: `src/daemon/` with timers only, per-unit due times, `daemon.json`, and
`daemon install | remove | status` for launchd and systemd (`DESIGN.md`, "Daemon"). The watcher
waits for the `notify` decision.

The daemon's loop lives in the binary's half because only a process has triggers; everything
it *does* is a `Session` call. How the tool is invoked without cargo is settled with the first
non-cargo adapter; since T42 the binary has a name of its own (`swarfr` since T46), with
`cargo swarfr` as an optional link, so someone with no cargo at all already has a name to call.

## Testing

- The freshness oracle becomes a small trait — build, list stale units — with one
  implementation per adapter, and the pass suite runs over every fixture the machine has the
  toolchain for, skipping the rest loudly. It is the move `tests/caps.rs` already made for
  filesystems: one assertion, every backend.
- A **monorepo fixture**: one repository, two cargo workspaces at different positions, a build
  script that leaves a CMake-looking dir inside a target, two worktrees, one project present on
  one branch only. It asserts that each build dir is found once, the nested dir is nobody's,
  `seed` fills exactly the positions whose project exists, *project gone* is reported and not
  removed without a threshold, and an out-of-tree `build-dir` lands in its owner's family.
- T29's race test (a writer during a pass ends with the newer bytes) runs for every `Quiet`
  adapter.

## Order of work

1. **T36** — the library boundary: `Session`, typed errors, the `cli` feature, the run lock,
   `stop` and `lock_budget`. No new behavior; the CLI snapshots prove it.
2. **T28** — `src/eco/`, the cargo adapter, the shared walk, owner-based family and position.
   No new behavior either; the monorepo fixture arrives here.
3. On the session: **T25**, **T26**, **T34** (the daemon). On the adapter boundary: **T29**
   (`Guard::Quiet`), **T33** (`Guard::Immutable`), the monorepo behaviors **T37**–**T39**, the
   persisted inventory **T40**.
4. Adapters in the order of `docs/ecosystems.md`: T30, T31, T32, and T35 last.
5. Embedding stays in `roadmap.md` (R8) until the creator asks for it.
