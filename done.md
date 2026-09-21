# Done

### T16. Lossy pass: `doc`

`target/doc` is fully regenerable by `cargo doc` and is usually tens to hundreds of MB.
`cargo clean --doc` does exactly this, and `kondo` / `cargo-clean-all` get it only by deleting
the whole target. A one-directory pass: `--lossy doc`, remove `<target>/doc` whole, reported
with its size like every other removal. Smallest task in the list and pure profit for anyone who
ever ran `cargo doc` once. Done: a test builds docs in the fixture, the pass removes them, the
build oracle stays green (docs are not part of the build graph).

Outcome: `src/doc.rs` plans one removal per target that has a `doc/` dir, gated by `--lossy doc`
and placed after `incremental` in the pipeline. Because `doc/` lies beside the profile dirs
rather than inside one, `Action::RemoveTarget` now names the guarding `target` and the `dir` to
remove separately; `orphans` and whole-target eviction pass the same path for both, and the
engine's guard is unchanged in what it allows them. The size comes from a new
`Target::doc_bytes`, summed in the scan the inventory already does.

Tests: `tests/doc.rs` on the real fixture with `cargo doc --no-deps` run into it — the A/B
`ab_only_the_named_run_removes_the_docs` (same built fixture twice, `--lossy doc` the only
difference; both sides then pass the freshness oracle, so docs really are outside the build
graph), a dry run that reports the dir with its measured size in JSON and removes nothing, and a
target with a held `.cargo-lock` that keeps its docs and exits 2. Docs: DESIGN.md "Doc pass" plus
the pipeline table, README.

### T12. `advise` and automation recipes

`cargo tare advise` reads the configs that decide how big a target grows and says what to change.
The checklist, from the comparison in `docs/research.md` — every line is something a competitor
either recommends or works around:

- profile keys that bloat a target: `debug` (`line-tables-only` instead of `true`), `debug = false`
  for `[profile.dev.package."*"]`, `split-debuginfo` (macOS leaves `.dSYM` trees otherwise),
  `strip` for release, `codegen-units`;
- `incremental`: what turning it off would save here, and that T13 is the cheaper answer;
- `[unstable]` keys that a stable toolchain silently ignores (this machine had some), including
  `-Zembed-metadata=no` and `-Ztrim-paths`, with the roadmap item that will use them;
- `cache.auto-clean-frequency` for the cargo home (stable since 1.88) — the cargo-cache /
  cargo-trim niche, which cargo now covers itself;
- families that could share a `build-dir` (stable since 1.91) and the lock contention that makes
  it a bad trade for parallel agents;
- `cargo-hakari` for workspaces that rebuild too often, and `sccache` for machines that rebuild
  from scratch a lot — neither shrinks a live target, and both compose with this tool;
- worktrees never seeded (needs T8).

Plus documented `just` and launchd examples for running the lossless passes after builds. Done:
advice reproduces the findings in `docs/research.md` on the measured machine, and each item
prints the file and key it is about.

Outcome: `cargo tare advise [--json] [ROOT]...` reads the manifests and cargo configs of the
projects the inventory finds, plus `$CARGO_HOME/config.toml`, and prints two lists. Findings come
from files through the pure `advise::review(file, kind, doc, nightly)`: `profile.<p>.debug` (all
three spellings of full debuginfo, and cargo's own default for `dev`), the `"*"` dependency
override, `profile.release.strip`, `split-debuginfo = "packed"`, `codegen-units = 1` in a dev
profile, `build.incremental`, an `[unstable]` table on a stable toolchain, and
`cache.auto-clean-frequency` in the cargo home. Notes come from the inventory: what `incremental/`
weighs under the roots (a new `Target::incremental_bytes`, summed in the existing scan), families
whose targets could share a `build-dir`, checkouts `seed` would fill, and orphaned worktrees. The
toolchain channel is only asked for when a config has an `[unstable]` table.

Checked against the machine the research was done on: `advise` over `packages/` reproduced
`docs/research.md`'s finding that `[unstable] no-embed-metadata` in `~/.cargo/config.toml` is
ignored on stable, and found the 6-target family and a checkout with no target dir.

Out of scope, deliberately: `cargo-hakari` and `sccache` help with rebuild time rather than with
the size of a live target, and no file says whether a workspace wants them; they stay in
`docs/research.md`. The `just` and launchd recipes are in README instead of the output.

Tests: 6 unit tests over literal TOML (including a manifest that has taken every piece of advice
and gets nothing) and `tests/advise.rs` — the A/B `ab_a_tuned_manifest_gets_no_profile_advice`
(same tree twice, the manifest the only difference), a finding-names-file-and-key test, a
snapshot test proving the command writes nothing, the JSON shape, and a broken TOML warning that
does not end the run. Docs: DESIGN.md "Advise command", README `advise` paragraph and a "Running
it automatically" section.

### T17. Whole-target eviction

`evict` today selects profile dirs. `cargo-clean-all` and `kondo` work at target granularity, so
they also take `doc/`, `package/`, `tmp/` and `CACHEDIR.TAG` — everything a target holds outside
its profile dirs. Add `--evict-whole-target`: when every profile dir of a target is selected,
remove the target dir itself rather than its profiles one by one. Needs the "remove a dir that
contains only locked profile dirs" guard that T9.1 introduces for orphans, so it is that task's
machinery applied to a second selector. Done: a test where a target with two profiles and a
`doc/` dir leaves nothing behind, and one where a busy profile keeps the whole target.

Outcome: `evict::whole_targets(targets, chosen)` (pure) returns every target whose profile dirs
the selection took whole, each carrying its profiles and its `du` bytes; `Evict::whole(...)` turns
the upgrade on, and `plan` emits one `Action::RemoveTarget` for such a target and drops the
per-profile `Remove` actions inside it. The re-check under the lock covers every profile of the
target, so a busy or freshly built profile keeps the target dir while the free profiles are still
evicted one by one. Wired as `--evict-whole-target` and `[evict] whole-target`.

Tests: `a_target_goes_whole_only_when_every_profile_of_it_is_chosen` (unit, over a partly idle
target and one with no profiles at all) and `tests/evict_whole.rs` — the A/B
`ab_only_the_whole_target_run_takes_what_is_outside_the_profiles` (same tree twice, the flag the
only difference: without it `doc/` and `CACHEDIR.TAG` survive, with it the target dir is gone and
the project around it stays), the busy-profile fallback, a fresh third profile keeping its target,
and the config-file route. Docs: DESIGN.md "Evict pass" and the CLI/config surface, README.

### T8. `seed`: clone-seed a new worktree's target

`cargo tare seed [--from <dir>] [<dir>]` with automatic source choice inside the family; excludes
`incremental/` and lock files. Done: in a fresh worktree of the fixture, the first build compiles
workspace members only and the seeded target adds ~0 allocated bytes. Register the seeded inodes
in the hash index as shared (`src/index.rs`), otherwise the first dedupe run clones them again.

Execution plan: `src/seed.rs` — `choose(dest)` picks the source inside the family: the git common
dir of the checkout (`inventory::family`), its registered checkouts (`inventory::checkouts`,
reading `<common>/worktrees/*/gitdir`), and of those the target with the newest build. `--from`
names one instead. `seed` walks the source target with `walkdir`, recreates dirs and symlinks and
`fs::copy`s every file — `clonefile` on APFS, so the copy shares blocks and costs no space —
skipping `incremental/`, `.cargo-lock` and leftover `.tare-tmp-` files. It holds the source's
profile locks (`ProfileLock::try_acquire`) for the walk and refuses if the destination target
already exists, so it can never merge into a live target. Index: for every copied file whose
source stamp the index knows, the destination stamp is stored with the same hash and `shared`,
and the source is marked shared, so the next dedupe leaves both alone; a file the index has not
seen stays unknown and costs one needless clone on the next dedupe — a known limit, not a hash
pass over gigabytes at seed time. Tests (`tests/seed.rs`, real `git worktree` + the cargo
fixture): what the first build in a seeded worktree actually rebuilds (measured, not assumed),
the excluded files, `--from`, refusal on an existing target, a busy source profile, the index
entries, and an A/B pair of identical worktrees where only one is seeded. Verify: `just check`.

Outcome: `src/seed.rs` + `cargo tare seed [--from <DIR>] [--dry-run] [--index <FILE>] [<DIR>]`.
`choose` takes the git common dir of the destination (`inventory::family`, now public), asks the
new `inventory::checkouts` for every checkout registered under it (the repository plus each
`worktrees/<name>/gitdir`) and looks in each at the *same relative path* the destination has
inside its own checkout — a workspace can sit anywhere in a repository, which the fixture proved
by having its workspace in `ws/` — then takes the target built most recently. The copy walks the
source with `walkdir`, recreates dirs and symlinks, `fs::copy`s files (`clonefile` on APFS, so
the new target shares every block and the volume loses nothing) and leaves behind
`incremental/`, `.cargo-lock` and `.tare-tmp-` leftovers. Every source profile dir is locked with
`ProfileLock::try_acquire` for the walk; one a build holds is reported and skipped whole (exit
code 2, as in `run`). A destination that already has a target is refused, never merged into.
Index: a copy whose source stamp the index knows is stored with the same hash and both sides are
marked shared, so the next dedupe leaves the pair alone.

Measured, not assumed: a seeded worktree rebuilds strictly less than an empty one, but not
nothing. Units whose absolute path is part of their fingerprint — the workspace members and the
path dependency — are compiled again in the new checkout. The fixture builds `--offline` from
path dependencies only, and registry dependencies are exactly the units that keep their paths
across worktrees, so the measured win is the floor of the real one. The card's "compiles
workspace members only" turned out to be optimistic and the A/B test states what actually
happens instead.

`tests/seed.rs`, 7 tests on a real `git worktree` of the cargo fixture: the copy is byte-identical
and its size is what the report claims; the cache and the lock files are left behind; a dry run
copies nothing and a second seed is refused; a busy source profile is reported and its dir not
copied; the source is chosen inside the family and both sides end up shared in the index; an A/B
pair where seeding is the only difference; and the CLI end to end. `tests/common/mod.rs` gained
`cargo_at` / `stale_units_at` so the oracle can run in any checkout. `just check` green.

Known limits: the seeded target weighs what `du` reports even though it shares every block —
only free space shows the truth (`docs/bench.md`); `--from` is not checked for belonging to the
same family; a file the index has never hashed costs one needless clone on the next dedupe.

### T10. Configuration and reporting

`~/.config/cargo-tare/config.toml` (roots, `min-age`, `min-size`, per-pass switches and thresholds,
family overrides), flag overrides, table and JSON reports, meaningful exit codes. Done: documented
in `README.md`, invalid config fails with a precise message. `run` without arguments takes the
configured roots (today it requires a `<ROOT>`).

Execution plan: `src/config.rs` — a serde `Config` read from
`$XDG_CONFIG_HOME/cargo-tare/config.toml` (else `$HOME/.config/...`), `deny_unknown_fields` and
kebab-case keys so a typo stops the run instead of silently doing nothing; keys `roots`,
`lossy`, `min-age`, `min-size`, `[evict] idle-days / max-total-gib`, `[incremental] idle-days`,
and `[family."<dir>"] skip` for leaving one repository alone. Only `skip` is per family: the
evict cap and the idle rules are decided over everything under the roots at once, so they stay
global — documented, not silently dropped. `--config <FILE>` points at another file (also what
the tests use); every flag wins over the file; `<ROOT>` becomes optional and falls back to
`roots`. Reporting: `run --json` prints one document (groups, busy dirs, per-pass counts,
removals with reasons, skips) built by a `#[derive(Serialize)]` view in `main.rs`, so the engine
types stay plain. Exit codes: 0 done, 1 error, 2 something was left busy — what a cron job needs
to tell the difference. Tests: unit tests on parsing and precedence, integration tests for a
config-driven run, a broken config naming its key, `--json` parsed back with serde_json, and
exit code 2 on a busy profile. Docs: `README.md` config section with a full example file,
`DESIGN.md` CLI surface. Verify: `just check`.

Outcome: `src/config.rs` — `Config::load` reads
`$XDG_CONFIG_HOME/cargo-tare/config.toml` (else `$HOME/.config/...`) with `deny_unknown_fields`
and kebab-case keys, so a typo names itself and stops the run instead of being ignored; a
missing file is the defaults, an unreadable or invalid one is an error naming the file. Keys:
`roots`, `lossy`, `min-age`, `min-size`, `[evict] idle-days / max-total-gib`,
`[incremental] idle-days`, `[family."<dir>"] skip`. `--config <FILE>` reads another file and
fails if it is not there. Every flag wins over the file (`Option::or` at each threshold, a
non-empty `--lossy` replaces the list). `<ROOT>` is now optional for `run` and falls back to
`roots`, with a precise error when both are empty; `status` takes the same `roots`, keeping `.`
as its fallback.

`skip` is the only per-family key, and the card's "family overrides" stop there on purpose: the
`evict` cap and both idle rules are decided over everything under the roots at once, so a
per-family threshold would be a lie. `indicatif` and `owo-colors` were approved for this task
and not used — nothing here needs a progress bar or colour yet.

Reporting: `run --json` prints one document (dry-run flag, groups with family, busy dirs, temps
removed, per-pass counts, every removal with its reason, every skip) from a `Serialize` view in
`main.rs`, so the engine types stay plain; the table print moved into `print_report`. Exit codes:
`0` done, `1` failed, `2` a profile dir was left alone because a build held its lock — `main`
now returns `ExitCode`.

Tests: 3 unit tests in `src/config.rs` (defaults, an unknown key naming itself, every key
parsed) and `tests/config.rs` with 7 more — the file supplying roots and the lossy pass, a flag
beating the file, an A/B pair where `skip = true` is the only difference between two identical
trees, a broken file naming itself and the key, a `--config` file that must exist, the JSON
report parsed back with `serde_json`, and exit code 2 on a busy profile. Every integration test
now runs the binary through `common::tare(config_home)`, which points `XDG_CONFIG_HOME` at a
temp dir: a test must never read the machine's configuration. `tests/cmd/run-needs-a-root.trycmd`
was dropped for the same reason (its subject is now our own error, and its outcome would depend
on the machine's config file); the case lives in `tests/cli.rs`. `just check` green.

Known limits: `~` in a config path is not expanded (a shell does it, a file does not); `roots`
are not de-duplicated; no `[compress]` / `[dedupe]` tables yet, `min-age` / `min-size` set both
passes at once as the flags do.

### T13. Lossy pass: `incremental`

Drop `<profile>/incremental/` in profile dirs nobody has built in for N days. Nothing in the
field does this: `cargo-clean-all` and `kondo` drop whole targets, and `CARGO_INCREMENTAL=0`
avoids the directory at the price of every rebuild everywhere. `docs/research.md` measured
`incremental = false` at −5.8 GB for one target (−40% of it) with local rebuilds 1.4–5× slower.
Keeping the cache for what you are working on and dropping it everywhere else takes the size
without the slowdown, and it is the largest single win still unclaimed after compress and dedupe.

Only workspace members are compiled incrementally, so the cost of dropping it is one
non-incremental rebuild of the workspace crates; third-party deps are untouched. Reuses the
removal machinery of `evict` (whole dir, under cargo's lock, re-checked after locking), with
`--lossy incremental --incremental-idle-days <N>`. Done: on the fixture the dir is gone and the
oracle shows the rebuild is limited to workspace members; a busy profile is never touched; the
reason is in the report on a dry run too.

Outcome: `src/incremental.rs` — pure `select(profiles, now, idle_days)` returns the profile dirs
that have an `incremental/` and whose last build is at least N days old (unknown last build is
never chosen, as in `evict`); the lossy `Incremental` pass re-checks under the lock that the dir
is still there and `inventory::last_built` still equals the inventory's reading. CLI:
`--lossy incremental --incremental-idle-days <N>`, which need each other; third in the pipeline,
after `evict`.

Engine: instead of a third removal variant, `Action::RemoveProfile` became `Action::Remove`,
which accepts a locked profile dir **or a dir inside one**, and its byte accounting sums the
inodes whose paths all lie under the removed dir (a link from outside frees nothing). `evict`
plans the same action unchanged; the profile's lock stays valid when only a subdir goes.

Measured, and better than the card assumed: dropping a real cache rebuilds **nothing**. The
cache is not part of cargo's fingerprint, so `Fixture::assert_fresh` passes right after the pass
(zero stale units, tests and binary still run). The price is one non-incremental rebuild of the
workspace members on the next edit — which is why the pass is meant for profiles you are not
working in.

`tests/incremental.rs`, 8 tests: idle cache goes while the artifacts, the lock file and a fresh
profile's cache stay; a profile without a cache is never planned; not run unless named, dry run
only lists; a running build is untouched; a build after the inventory keeps the cache; an A/B
pair of identical trees where the freed bytes equal exactly the control's cache; the real
fixture above; and the CLI end to end (both halves of the flag pair, dry run, removal). Two unit
tests cover `select`. `just check` green.

Known limits, as documented in `DESIGN.md`: the whole cache of a profile goes or none of it
(cargo's per-crate session dirs are not read); a busy profile is skipped; the pass's own yield
across a machine is not benchmarked yet — `docs/research.md` only has the −5.8 GB from building
one target with `incremental = false`.

### T9.1. Lossy pass: `orphans`

P0: on the measured machine 113.9 GB of ~158 GB sat in 30 worktree checkouts that git no longer
registered (sources and a `.git` file still present, last build a week old). Whole-dir removal
only, and only of `target/` — sources in such checkouts may hold uncommitted work that git can no
longer report. An orphan is a target whose project's `.git` file points at a missing worktree
record (the inventory already reports it). Off by default, always listed in `--dry-run` output
with the reason. Done: never touches a dir whose lock is held; a test covers a removed worktree.

Execution plan: the inventory already sets `Target.orphaned`; make the check reusable as
`inventory::is_orphaned(target)` so the pass can repeat it after locking, the way `evict`
re-reads the last build. `src/orphans.rs`: the `Orphans` pass (lossy) takes the orphaned targets
the inventory found, with their allocated bytes, and plans one `Action::RemoveTarget` per target
that is still orphaned. Engine: `RemoveTarget` removes the whole target dir — `doc/`, `package/`,
`CACHEDIR.TAG` and all — but only when at least one of the dirs we hold a lock for is inside it
and none of the dirs reported busy is, so a target with a running build is never touched. That
guard is what T17 will reuse. CLI: `--lossy orphans`, no threshold, reason in the report on a
dry run too. Tests (`tests/orphans.rs`, real `git worktree` on throwaway dirs): a removed
worktree record makes the target go whole, a live worktree's target stays, a busy profile keeps
the whole target, a record restored between inventory and lock keeps it, the pass does nothing
unless named, and a dry run lists without removing. Verify: `just check`.

Outcome: `src/orphans.rs` — the lossy `Orphans` pass takes the orphaned targets the inventory
found (with their allocated bytes) and plans one `Action::RemoveTarget` per target that still
holds a locked profile dir and that `inventory::is_orphaned` (new, the old private check made
reusable) still calls an orphan. `src/engine.rs` gained `Action::RemoveTarget { dir, reason,
bytes }` and a shared `remove()` helper: the target goes whole (`doc/`, `package/`,
`CACHEDIR.TAG` and all) only when a lock we hold is inside it and no busy dir is under it,
otherwise `Skip::Unlocked`; removed dirs leave the locked set, so later passes do not scan them.
CLI: `--lossy orphans` / `--pass orphans`, no threshold, reasons printed on a dry run too.

`tests/orphans.rs`, 7 tests on real `git worktree` checkouts in temp dirs: the whole target of
an orphan goes and the checkout (including uncommitted work) stays; a live worktree is
untouched; a running build keeps the whole target; a record restored between the inventory and
the lock keeps it; nothing happens unless the pass is named, and a dry run only lists; an A/B
pair of identical trees where the run without the pass keeps exactly the bytes the run with it
frees; and the CLI end to end. `just check` green (fmt, clippy `-D warnings`, 37 tests).

Known limits, as documented in `DESIGN.md`: a target with one busy profile is kept entirely,
even when its other profiles are free; removal is not atomic, so an interrupted run leaves a
half-removed target for the next run to finish; a checkout whose common dir merely sits on an
unmounted volume reads as an orphan, because the `.git` file points at a path that does not
exist and nothing else distinguishes the two. Measured only on fixtures — the 113.9 GB of real
orphans that motivated the P0 have not been touched.

### T11. Benchmarks: size and build time

On a real mid-size workspace, `hyperfine`, clean and incremental builds: baseline vs compress vs
dedupe vs fused vs seeded worktree vs shared `build-dir` vs sccache. Done: `docs/bench.md` with
numbers and the defaults (`min-age`, `min-size`) justified by them. Needs free disk space.

Execution plan: benchmark a **copy** of a private 587-crate workspace (creator's choice) — `git archive`/`rsync`
of the sources into a temp dir, a fresh `target/` built there; the tool never sees a real target.
Enabling flags first, because the defaults make a benchmark impossible: everything just built is
younger than `min-age`, so `run` would skip it, and the passes cannot be told apart. Add to
`run`: `--pass <NAME>...` (which lossless passes to run, default all), `--min-age <SECS>` and
`--min-size <BYTES>`. T10 plans the same switches in the config, so this is that surface, not a
throwaway. `scripts/bench.sh` then drives `hyperfine` over the variants: baseline, compress,
dedupe, both fused, sccache; per variant a clean build, an incremental build after touching one
workspace file, and `du -sk` of the target before and after the tool; plus the tool's own runtime
and a freshness check (`cargo build --message-format=json` must report no unit rebuilt). The
`min-age` / `min-size` sweep reuses the same script. Not measurable yet and to be written down as
such: seeded worktree (T8), shared `build-dir` (nightly-only `-Z build-dir`; this machine is on
stable 1.98). Results and the raw hyperfine JSON go to `docs/bench.md`, which also justifies the
defaults. Verify: `just check`, and the bench script run end to end on the copy.

Outcome: `scripts/bench.sh` (also `just bench <workspace>`) measures a **copy** of a workspace —
a shallow clone plus one git worktree of that clone, so the two targets form a family; the real
target dir is never touched. Per stage it records the tool's wall clock, `du` and free space
before and after, how many units cargo rebuilds, and hyperfine means for incremental builds.
Enabling flags, planned for the config in T10 and added here because the defaults make a
benchmark impossible: `--pass <NAME>` (repeatable), `--min-age <SECS>`, `--min-size <BYTES>`;
`main.rs` now carries a `RunArgs` struct, and `compress::NAME` / `dedupe::NAME` exist beside
`evict::NAME`. Tests: `tests/cli.rs` covers an unknown `--pass` name, a run limited to one pass,
and both floors; the fake-target helper moved to `tests/common/mod.rs`. `just check` green.

Numbers (`docs/bench.md`, that workspace, 587 crates, two checkouts, two full runs): compress
takes the two targets from 3.63 GiB to 1.32 GiB in 81.7 s; dedupe frees another 407 MiB in
12.4 s — together a 74% cut of freshly built targets no age-based cleaner would touch. After
each pass cargo reported **0** units out of date. Incremental builds: baseline 3.70 s mean
(2.39–5.10), after compress 4.96 s (2.78–10.17, the first build after the rewrite is the
outlier), after both 3.53 s (2.77–5.14) — no measurable slowdown. sccache, for comparison: cold
clean build 83.5 s, warm 32.3 s, 304 MiB cache; complementary, not a substitute.

Learned and written down: `du` cannot see copy-on-write sharing, so dedupe is only visible in
free space; one pipeline run does not converge (46 actions left right after a full run, because
dedupe's clones are files compress never saw); `hyperfine` needs three warmups after a clean
build or the numbers are dominated by the machine settling. Not measured, with the reason in the
doc: seeded worktree (needs T8) and shared `build-dir` (nightly-only on this toolchain).

### T9.2. Lossy pass: `evict`

Profile dirs idle for N days, then least-recently-built first until the total is under
`max-total`. Whole profile dirs only. Off by default, always listed in `--dry-run` output with
the reason. Done: never touches a dir whose lock is held; tests cover the idle rule and a size cap.

Execution plan: the inventory reports every profile dir with its allocated bytes and last build
(`Target.profiles` becomes a list of `ProfileInfo`). `src/evict.rs`: `select` — a pure function
over all profiles under the roots: idle ones first, then least recently built until the total
fits `max-total`; each choice carries its reason. The `Evict` pass (lossy) plans
`Action::RemoveProfile` only for selected dirs whose lock the engine holds and whose last build
still equals the inventory's — a build that slipped in between keeps its profile. Engine: the
dir must be one of the locked profile dirs; planned removals and reasons go to the report, dry
run included; a removed dir leaves the set that later passes scan. CLI until T10 brings the
config: `--lossy evict` with `--evict-idle-days <N>` and / or `--evict-max-total-gib <N>`, and
an error when neither is given. Tests: unit tests of `select`; `tests/evict.rs` on fake profile
dirs with aged mtimes — idle rule, size cap order, dry run, busy dir untouched, profile built
after the inventory kept, not run unless named; one CLI run end to end. Verify: `just check`.

Outcome: `src/evict.rs` — `select` (pure, global, reasons attached) and the lossy `Evict` pass;
the inventory reports `ProfileInfo` per profile dir; the engine got `Action::RemoveProfile`
(only an exactly locked profile dir, removed whole like `cargo clean`, dropped from later
passes) and `PassReport::removals`, filled on dry runs too. CLI: `--lossy evict` with
`--evict-idle-days` and / or `--evict-max-total-gib`; either without the other is an error before
anything is touched. Tests: 4 unit tests of `select`; `tests/evict.rs` — idle rule, cap order,
not run unless named, dry run lists only, profile with a held lock untouched, profile built after
the inventory kept, CLI end to end. fmt, clippy `-D warnings` and the suite green three runs in
a row. Documented in `DESIGN.md` "Evict pass" and `README.md`. Known limits: a busy profile is
skipped, so a run may end above the cap; the cap uses inventory sizes taken before compress and
dedupe; a profile dir vanishing between inventory and lock fails the run; thresholds move to the
config in T10; tested on fake profile dirs only, never on a real target.

### T6. Compress pass

Transparent APFS compression through the `applesauce` library (chosen in T2); skip compressed,
small and hot (`min-age`) inodes; whole hardlink groups only. Find out why T2 saw a single 119 MB
file left uncompressed — real targets hold 130 MB rlibs. Done: oracle green, fixture target
shrinks, a large file compresses or its limit is documented, second run is a no-op. Runs before
dedupe and compresses only canonicals and unique files (dedupe already prefers a compressed
canonical); compressing in place must keep or refresh the inode's entry in the hash index.

Execution plan: the library skips files with more than one link and replaces the inode, so the
engine never lets it near a live file. New `Action::Compress(Inode)`: the engine checks the group
like for a replacement, clones its first path into a sibling temp, hands a batch of temps to
`Pass::compress`, and swaps in — through `rename`, the other paths through `hard_link` — only
temps that came back with the compressed flag, after checking the group's stamps again; mtime and
mode are restored by the engine. `Pass::rewritten(old, new)` goes to every pass so dedupe moves
its index entry to the new inode. `src/compress.rs`: `Compress` plans unflagged inodes of at
least 8 KB, older than `min-age`, not marked shared in the index (compressing a clone un-shares
it); backend `applesauce` (LZFSE, level 5, ratio 0.95, verify on), its skip reasons and errors
are kept for the report. The hash index becomes a `RefCell` owned by the caller and borrowed by
both passes. Tests in `tests/compress.rs`: oracle green and target smaller on the fixture, a
hardlink group stays one inode with mtime and mode kept, second run plans nothing, small / hot /
shared / flagged files left alone, a 130 MiB file, dedupe after compress hashes nothing twice and
keeps clones compressed. Verify: `just check`.

Outcome: built as planned with `applesauce` 0.8.8 (added to the shared `rust.md` inventory).
7 tests in `tests/compress.rs`: the fixture profile shrinks by exactly the reported
`freed_bytes`, the oracle stays green and a second run applies nothing; a hardlink group stays
one inode with mtime and mode kept; small, hot, shared and flagged files are not planned; an
incompressible file keeps its inode, reads `NotCompressed` and the backend's reason is reported;
a 130 MiB file compresses; dedupe after compress leaves compressed clones and an idle second
run; an index entry follows its file to the new inode. 30 rounds of the file in a row and the
whole suite three times green, `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`
clean. The 119 MB file of T2 was not reproduced: size is not the limit, the backend skipped that
blob for a reason the spike did not capture; the pass now captures such reasons. Differs from the
card: every eligible duplicate is compressed before dedupe folds it (CPU, not space) instead of
canonicals only — that needs content hashes before compression and is left to the benchmarks.
Found on the way: the fork race assumed in T5 is real (3 of 20 rounds failed until
`run_unbusy` summed its attempts); it needs test threads that spawn processes and cannot happen
in the tool. Not verified: decompression cost at link time (T11), files of 4 GiB and more,
behaviour on a real target dir.

### T3. Test harness and freshness oracle

A small fixture workspace (third-party deps, a proc-macro, a build script, a test binary) built in
a temp dir, plus helpers: allocated-bytes measurement (`st_blocks`) and the oracle from
`DESIGN.md` — after a pass, `cargo build --message-format=json` reports every unit fresh and
`cargo test` passes. Done: the oracle fails when a deliberately broken pass changes an mtime.

Execution plan: `tests/common/mod.rs` — `Fixture` (a workspace with a bin + lib package that has
a build script writing into `OUT_DIR`, unit and integration tests, a proc-macro member, and a
path dependency outside the workspace standing in for a third-party crate: hermetic, builds
`--offline`; registry crates differ only in how their sources are fingerprinted, which the tool
never touches), `stale_units` / `assert_fresh` (JSON messages of `cargo build` and
`cargo test --no-run` parsed with `serde_json`, then `cargo test`), `allocated_bytes`
(`model::scan`), `run_unbusy` (the retry both test files duplicate today). `tests/engine.rs` and
`tests/dedupe.rs` move to the fixture and the oracle; `tests/harness.rs` proves the oracle: green
on an untouched build, reports stale units after mtimes of `.rlib` files are bumped. CLI tests
per `rust.md`: `trycmd` for full output (`tests/cmd/*.trycmd`), `assert_cmd` + `predicates` for
exit codes. Crates (dev): `trycmd`, `assert_cmd`, `predicates`. Verify: `just check`.

Outcome: built as planned; dev crates `trycmd` 1.2.1, `assert_cmd` 2.2.2, `predicates` 3.1.4.
`tests/harness.rs`: the oracle is green on an untouched build and reports stale units once the
mtimes of the `.rlib` files are bumped, then is green again after cargo's rebuild. The real-cargo
tests of `tests/engine.rs` and `tests/dedupe.rs` now use the fixture and `assert_fresh`, and the
duplicated busy-retry and `ino` helpers are gone. `tests/cli.rs`: three `trycmd` cases (version,
`status --help`, `run` without a root exits 2) and four exit-code tests. The whole suite green
three runs in a row, `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` clean.
Differs from the card: the third-party dependency is a path crate outside the workspace, not a
registry crate, so the suite needs no network. The "broken pass" is a direct mtime bump in the
test: the engine has no action that skips restoring the mtime, so such a pass cannot be written
against it. Not covered: `[patch]`, registry and git dependencies, custom profiles, a
`--target <triple>` layout.

### T4. Inventory, discovery and `status`

Walk configured roots, detect cargo target / build dirs by cargo's own `CACHEDIR.TAG` text, find
profile dirs, build the inode model (hardlink groups, size, mtime, compressed flag), group targets
into families by git common dir. `cargo tare status` prints the inventory and estimated savings,
with `--json`. Read-only. Done: matches `du` within 1% on the fixture and ignores non-cargo caches.
Also: `run` without arguments takes every discovered target, one family per engine run, so the
dedupe pass (which works across whatever profile dirs one run gets) searches inside a family.

Execution plan: `src/inventory.rs` — `discover(roots)` (walk, stop at every dir cargo tagged),
`git_link` (nearest `.git`: a dir is the common dir; a file is followed through `gitdir:` and
`commondir`; a missing record marks the target orphaned and the family is taken from the record's
path), per-target totals from `model::scan` of the whole target dir (inodes, paths, logical,
allocated, already compressed, compressible, last built), per-family upper bound for dedupe (bytes
of files whose size also occurs in a sibling target). `cargo tare status [--json] [ROOT]…` and
`cargo tare run` with roots instead of target dirs: one engine run per family. Crates: `serde`,
`serde_json`. Tests in `tests/inventory.rs`: `du` within 1% (hardlinks included), foreign
`CACHEDIR.TAG` ignored, nested targets not entered, main repo + worktree form one family, a removed
worktree record reads as orphaned, JSON parses. Verify: `just check`.

Outcome: built as planned; `serde` 1.0.229 and `serde_json` 1.0.151 added. 4 tests in
`tests/inventory.rs` (size within 1% of `du -sk` with a hardlink group and files outside profile
dirs; a Gradle-tagged dir ignored and a nested tagged dir not reported; repository + `git worktree`
form one family, an unrelated project has none, a deleted worktree record reads as orphaned with
the family kept, the dedupe estimate is non-zero only inside the family; `status --json` parses).
The whole suite green three runs in a row, `cargo fmt --check` and
`cargo clippy --all-targets -- -D warnings` clean. Differs from the card: `run` requires at least
one `<ROOT>` instead of defaulting to every discovered target — there are no configured roots
before T10, and a mutating command should not default to the current dir. Not verified: the tool
was never pointed at a real target dir, so `status` has not been compared with the measurements
in `docs/research.md`; a git submodule has no `commondir` file, so it reads as its own family —
untested.

### T7. Hash index and fused dedupe pass

Persistent hash cache keyed by `(device, inode, size, mtime)`, size-bucket prefilter, family-first
candidate search, `clonefile` replacement, fusion with compress (compress the canonical inode,
clone it over the group; per-group fallback if clones of compressed files do not share). Done:
oracle green on two sibling fixture targets, re-run after a small rebuild hashes only new inodes.

Taken ahead of T3 / T4 / T6 at the creator's request. Scope here: the index and the dedupe pass
over all profile dirs given to one run. Left to their own tasks: family-first narrowing (T4),
compressing the canonical before cloning (T6 — this pass already prefers a compressed canonical,
so T6 only has to compress canonicals and unique files first), the oracle on a fixture with real
dependencies (T3; here two sibling builds of a dependency-free fixture).

Execution plan: `src/index.rs` — `(dev, ino) → (size, mtime, sha256, shared)` in one flat binary
file, loaded at start, saved through temp + `rename`; `shared` marks inodes this tool cloned or
cloned from, so a second run does not clone them again. `src/dedupe.rs` — filter (`min-size`,
`min-age`, replaceable), bucket by `(dev, size)`, hash only buckets with two or more inodes
(`sha2`, `rayon`, index first), group by hash, canonical = shared, then compressed, then oldest;
members = unshared inodes. Engine: `Pass::replaced` hook so the pass can record the new inode.
CLI: `run --index <FILE>`, default `~/.cache/cargo-tare/hashes-v1.bin`. Tests in
`tests/dedupe.rs`: duplicates across two profiles, second run is a no-op and hashes nothing, a
rewritten file is rehashed, hot and small files are left alone, index survives a corrupt file,
two sibling cargo builds stay fresh. Verify: `just check`.

Outcome: built as planned with `sha2` 0.11 and `rayon` 1.12; 5 tests in `tests/dedupe.rs` and one
unit test for the canonical order, the whole suite green three runs in a row, `cargo fmt --check`
and `cargo clippy --all-targets -- -D warnings` clean. Two real builds of one source into two
target dirs did share at least one artifact, and both targets reported every unit fresh
afterwards. Documented in `DESIGN.md` ("Dedupe pass and hash index"), `README.md`,
`toolchain.md`. Not done here, moved into the cards of T4 (family-first via one engine run per
family), T6 (compress canonicals first, keep the index entry) and T8 (register seeded inodes as
shared). Not verified: the compressed-canonical preference is unit-tested on the ordering only,
no compressed file existed in the tests; real disk savings are not measured (clones are checked
by identity and content, block sharing needs `df` on a scratch volume — T11); never run on a real
target yet.

### T5. Engine: plan / apply pipeline and safety core

Pass trait producing actions against the shared inventory; ordered pipeline; `--dry-run`; cargo
lock acquisition (`try_lock`, sorted order, skip busy dirs); atomic group replacement; pre-apply
re-check of size and mtime; cleanup of leftover temp files; per-pass byte report. Done: every
safety invariant in `DESIGN.md` has a test, including a build running concurrently.

Taken ahead of T3 / T4 at the creator's request, so it carries the two pieces it cannot work
without: the per-profile inode model (`src/model.rs`; T4 keeps discovery, families, `status`) and
a dependency-free cargo fixture for the concurrent-build test (T3 keeps the full fixture and the
reusable oracle).

Execution plan: `src/lib.rs` + `src/model.rs` (scan one profile dir into inodes with all their
paths; no symlink following, one device) + `src/engine.rs` (`Pass` trait, `Action::Replace`,
`ProfileLock` over `.cargo-lock` with `File::try_lock`, sorted lock order, stale temp cleanup,
re-check, clone → restore mtime / mode → `rename`, hardlink the rest of the group, per-pass
report). `cargo tare run [--dry-run] [--lossy <pass>] <target-dir>…` wired with zero passes.
Crates: `walkdir`, `anyhow`, dev `tempfile`; clone through `std::fs::copy` (uses `fclonefileat`
on APFS). Tests in `tests/engine.rs`, one per invariant, plus a real `cargo build` holding the
lock and a freshness check afterwards. Verify: `just check`.

Outcome: 12 tests in `tests/engine.rs`, green three runs in a row, with `cargo fmt --check` and
`cargo clippy --all-targets -- -D warnings` clean. Covered: group replacement keeping one inode,
mtime and mode; busy profile untouched; stale temps; `--dry-run`; member changed after the scan;
links outside the profile; flagged inode; paths outside locked dirs; lossy gating; no symlink
following; foreign `CACHEDIR.TAG`; and a real `cargo build` that reads as busy while it runs and
reports every unit fresh after its final binary's hardlink group was replaced. Documented in
`DESIGN.md` ("Engine"), `README.md`, `toolchain.md`. Not covered: the device-boundary rule is
enforced (`same_file_system`, `CrossDevice`) but has no test, it needs a second volume; whether
the clone really shares blocks is measured in T7 / T11, the engine test only checks identity and
content. Gotcha for later tests: one test that runs the engine twice on the same dir failed once
with the second run seeing nothing to do. Most likely cause (not proven): a lock fd open while
another test thread spawns a process stays held by that child until it execs, so the dir briefly
reads as busy. The `run` helper in `tests/engine.rs` retries while busy; green since.

### T1. Scaffold the project

Cargo binary crate `cargo-tare` (latest stable Rust pinned via mise and `rust-toolchain.toml`),
git repository, lint / format / test commands, CI-free for now. Done: `cargo tare --version`
runs, `toolchain.md` lists what is actually used.

Execution plan: `Cargo.toml` (edition 2024, `publish = false`, `clap` with `derive` added via
`cargo add` to get the latest version), `rust-toolchain.toml` + `mise.toml` pinned to 1.98,
`src/main.rs` with the `cargo tare` subcommand wrapper and nothing else, one integration test for
`--version` on std only, `Justfile` with `check` (fmt, clippy `-D warnings`, test), `.gitignore`,
`git init` without committing. Verify: `cargo run -- tare --version` and `just check`.

Outcome: builds on rustc 1.98.1 with `clap` 4.6.7; `cargo run -- tare --version` prints
`cargo-tare 0.1.0`; `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and
`cargo test` (1 integration test) are green. The three `just check` steps were run one by one, not
through `just`. Repository initialised on `main`, nothing committed. Gotcha: a fresh `mise.toml`
is untrusted, so every `cargo` call fails until `mise trust` is run once in the project.

### T2. Spike: APFS primitives and backend choice

Answer on a scratch fixture, with a throwaway binary or script, before any real code:
(a) does `clonefile` + restored mtime keep cargo units fresh; (b) how to replace an inode for a
whole hardlink group atomically; (c) compression backend — `applesauce` as a library vs own
decmpfs writer: hardlink handling, real LZFSE ratio including small files, CPU cost; (d) do clones
of a compressed file share blocks; (e) does a recursive clone of a target make registry deps fresh
in another worktree; (f) hashing: `blake3` vs `sha2` throughput. Done: answers and numbers
recorded in `DESIGN.md`, dependency list sent to the creator for approval.

Execution plan: one bash script (`docs/spike/t2-spike.sh`) that runs entirely inside a throwaway
APFS sparse image, so `df` deltas are exact and no real target is touched. Fixture: a two-member
workspace (serde derive, serde_json, memchr, libc) built offline, plus git worktrees of it.
Sections: E1 seed by recursive clone (with / without `-p`) vs cold build; E2 content match between
independently built worktrees, clone replacement with restored mtime, negative control without
mtime; E3 hardlink-group replacement; E4 compression via `ditto` (freshness, relink, clone sharing
of compressed files, write-to-clone); E5 hash throughput; E6 cargo lock interop via `flock`;
E7 `applesauce` CLI on a target with hardlinks (ratio, links, mtimes). Verify with the freshness
oracle (`cargo build --message-format=json`). Raw output goes to `docs/spike/`, conclusions to
`DESIGN.md`. `blake3` and `applesauce` were approved by the creator.

Outcome: (a), (b), (d), (e) confirmed; (c) `applesauce` chosen (−65% on the fixture, hardlinks,
mtimes and modes intact); (f) `sha2` first (hardware SHA-256 1484 MB/s), `blake3` approved but not
needed yet. The `ditto` section compressed nothing and was replaced by `t2-spike-e8.sh`. Left
open: `applesauce` did not compress a single 119 MB blob (goes to T6), decompression cost at link
time (goes to T11). Results table: `DESIGN.md`, "T2 spike results". Raw output: `docs/spike/`.

### T14. Compress and report the cargo home

`~/.cargo/registry/src` holds every dependency's unpacked sources — plain text, the most
compressible bytes on the machine — and `git/checkouts` the same for git dependencies. No
competitor compresses them: `cargo-cache` and `cargo-trim` only delete, and cargo's own
`cache.auto-clean-frequency` (stable since 1.88) only evicts by age. Compression is lossless
here in the strongest sense: the files are a cache of immutable, re-downloadable sources.

Scope: `status` reports the cargo home's size next to the targets; `run` compresses
`registry/src` and `git/checkouts` when `--cargo-home` is given. Must take cargo's own lock
(`$CARGO_HOME/.package-cache`) for the length of the pass, the way the engine takes
`.cargo-lock` per profile, and must leave `registry/cache` (already compressed `.crate` files)
alone. Done: measured ratio in `docs/bench.md`, a `cargo build` after the pass does not
re-extract anything, and the pass refuses to run while another cargo holds the package lock.

Plan: `engine::run` gains a `Locks` argument — `PerDir` (today's behaviour, one `.cargo-lock` per
dir) or `Shared(path)`, one lock file for every dir at once, which is what
`$CARGO_HOME/.package-cache` is. `src/cargo_home.rs` finds the home (`CARGO_HOME`, else
`~/.cargo`), lists the dirs worth compressing (`registry/src`, `git/checkouts`, never
`registry/cache`) and inspects them for `status`. `run --cargo-home` then runs the existing
`compress` pass over those dirs as one more group in the report. Verify: `tests/cargo_home.rs`
over a fake home in a temp dir — an A/B with `--cargo-home` the only difference, `registry/cache`
untouched, every file byte-identical with its mtime after the pass (which is what decides whether
cargo re-extracts), and a held `.package-cache` leaving everything alone with exit 2. Bench on a
clone of a subset of the real home, never on the home itself.

Outcome: done. `src/cargo_home.rs` finds the home (`--cargo-home [DIR]` → `CARGO_HOME` →
`$HOME/.cargo`), lists the dirs worth compressing (`registry/src`, `git/checkouts`) and inspects
them for `status --cargo-home`, which stays opt-in because it costs a second full walk.
`engine::run` gained a `Locks` argument: `PerDir` is what every target group uses, `Shared(path)`
takes one lock for the whole group — `<home>/.package-cache`, the file cargo itself holds while
it fetches or extracts, since it writes no `.cargo-lock` there. A home cargo has never used (no
`.package-cache`) is refused rather than locked into existence, and `run --cargo-home` with no
roots at all is a valid run.

Only `compress` runs on the home: there is nothing to dedupe against and nothing stale to evict.
`registry/cache` (the `.crate` archives) and `registry/index` are left alone.

Measured (`docs/bench.md`, `scripts/bench-cargo-home.sh` / `just bench-home`, on an APFS clone of
the real home — never on the home itself): 1.52 GiB of registry sources → 469 MiB, −69.2%, in
44.5 s; `registry/cache` unchanged to the byte. The re-extraction criterion is answered twice:
the benchmark rebuilds a crate from the clone after the pass and cargo reports 0 units not fresh
with `.cargo-ok` unchanged in inode, mtime and size, and `tests/cargo_home.rs` asserts content
and mtime over every file in a fake home. `git/checkouts` was empty on this machine — that half
rests on the fixture test alone.

Tests (`tests/cargo_home.rs`, 6): the A/B with `--cargo-home` as the only difference, the packed
crates left alone, a held `.package-cache` giving exit 2 with nothing touched, a dry run
reporting the home as its own group with only `compress` in it, `status` measuring the home only
when asked, and a home without `.package-cache` refused.

### T15. Lossy pass: `orphan-toolchain` report

After a toolchain upgrade the artifacts built by the old rustc stay in the same profile dir
forever; cargo never revisits them. `cargo-sweep` covers this with `--installed` /
`--toolchains`, and does it by parsing hashed file names, which this project will not do.

The layout-independent source of truth is cargo's own fingerprint data: every
`.fingerprint/<unit>/*.json` records the rustc it was built with. First step is a report, not a
deletion: group the fingerprints by rustc, attribute bytes to each group, and show in `status`
how much of a target belongs to a rustc that is no longer the current one. Deleting those units
needs the unit-to-file mapping that only build-dir layout v2 gives (roadmap `R1`), so this task
ends at the number and an `advise` line. Done: the report is right on a fixture built with two
toolchains, and says nothing when there is only one.

Plan: `src/toolchains.rs` reads cargo's own fingerprints —
`<profile>/.fingerprint/<unit>/*.json`, whose `rustc` field is the hash of the compiler that
built that unit — and groups the units by it, newest first. No file name is parsed: the unit is
the directory, and the hash is a number cargo wrote. The inventory gets the counts per profile
(`ProfileInfo::toolchains`) and the target gets the totals plus an estimate of the bytes, which
is the profile's own size split by the share of units, because mapping a unit to its files needs
the layout only `R1` gives. `status` prints one line per target when more than one rustc appears,
`advise` adds the note, and both say nothing when there is only one.

Verify: `tests/toolchains.rs` over the real build fixture — build it, rewrite the `rustc` field
in half the fingerprints (which is what a toolchain upgrade leaves behind) and check the report
finds exactly those units, plus a pristine build reporting nothing at all. No A/B test: this task
adds no pass and changes nothing on disk.

Outcome: done as a report, which is where the card ended on purpose. `src/toolchains.rs` reads
`<profile>/.fingerprint/<unit>/*.json` and groups the units by the `rustc` hash cargo wrote
there, newest fingerprint first, so the head is the compiler in use and everything after it is
what an upgrade left behind. No file name is parsed anywhere: the unit is a directory, the
compiler is a number. `Target` gained `toolchains`, `stale_units` and `stale_bytes_estimate`;
`status` prints one line per target when a second compiler appears, `advise` adds a note that
points at `cargo clean`, and a target built by one rustc — the ordinary case — says nothing.

Honest about the number: the bytes are an estimate, and the field name says so. The profile
dirs' size is split by the share of the units, because a fingerprint names no artifact and the
exact map needs cargo's newer build-dir layout (roadmap `R1`). That is also why nothing here
deletes: `cargo-sweep --installed` can only do it by parsing hashed file names.

Tests: three unit tests over a handmade fingerprint tree (one compiler is no finding, the older
compiler's units are the stale ones, an unbuilt profile says nothing) and three integration
tests over the real build fixture, where the second toolchain is simulated the only honest way —
by rewriting the `rustc` hash in half the fingerprints and dating them back a month, which is
exactly the state an upgrade leaves. No A/B test: this task adds no pass and changes nothing on
disk.

### T18. Dedupe across families

Dedupe compares targets inside a family (a repository and its worktrees), because that is where
the duplicates are. `fclones` and `jdupes` compare everything, and unrelated projects do share
bytes: the same version of the same crate built with the same features is byte-identical, and the
hash index already holds the hashes needed to find that out. What is missing is not the
comparison but the locking: cloning across families means holding two families' locks at once,
and the benchmark run showed a family's own pass takes ~80 s, so a wider lock is a real cost.

Do it as an opt-in (`--across-families`), keep the sorted lock order that makes deadlock
impossible, and measure the extra yield on the benchmark workspace before making it a default.
Done: `docs/bench.md` gains the number, and a test proves two unrelated fixtures share a file
without either build going stale.

Plan: the engine already compares whatever profile dirs one run is given and already sorts its
locks, so the whole change is in the grouping: `--across-families` (config `across-families`)
puts every target under the roots into one group instead of one group per family. The report
names that group `<across families>` rather than a family dir. Per-family `skip` still applies,
because it is decided before the grouping. Verify: `tests/across_families.rs` — two fixtures in
temp dirs of their own, each with the same file planted in its target the way an identical
third-party artifact looks, run as an A/B where the flag is the only difference: with it the two
files share an inode, without it they do not, and both builds are still fresh either way. Then
`scripts/bench.sh` gets the second measurement and `docs/bench.md` the number.

Outcome: done, and smaller than the card feared. The engine already compares whatever profile
dirs one run is given and already takes its locks in sorted order, so `--across-families`
(config `across-families`) changes only the grouping: one group for every target under the
roots, named `<across families>` in the report. Per-family `skip` still applies, because it is
decided before the grouping.

Measured (`scripts/bench.sh`, `WITH_ACROSS=1`, which adds a second **independent clone** of the
repository — its own `.git`, so its own family): a freshly built 351.8 MiB target, already
deduped inside its own family, gave up another **172.6 MiB** in 1.9 s once it was compared with
the other family, and neither checkout had a single unit go stale. About half of a new target
was already on the disk in a project that has nothing to do with it.

Honest about the benchmark: this one ran on `cargo-tare`'s own repository, not on the bigger workspace,
because the machine had 10 GiB free and three checkouts of it do not fit under the script's
free-space guard. The ratio is what the number is good for; the absolute sizes are an order of
magnitude smaller than the other benchmarks in `docs/bench.md`.

It stays opt-in, and the reason is in the same numbers: the run holds every target's build locks
for its whole length, which on the 587-crate workspace is over a minute of no builds anywhere.

Tests (`tests/across_families.rs`): the A/B with two real fixtures in temp dirs of their own —
neither has a repository, so each is its own family — each holding the same planted artifact,
with the flag as the only difference. With it the second copy's inode is replaced (a clone is a
new inode sharing the old one's blocks, which is what an inode check has to assert) while its
bytes and its mtime are not; without it nothing moves; and all four builds are still fresh
afterwards. A second test checks the report names one group instead of two.

### T19. Platform layer: build and run on Linux and Windows

The tool was macOS-only, and not by design: `model.rs`, `engine.rs`, `seed.rs` and the two
lossless passes reached for `std::os::unix::fs::MetadataExt` (`dev`, `ino`, `nlink`, `blocks`,
`st_flags`) and for macOS's `clonefile` and `UF_COMPRESSED` directly. Windows has none of those
names, so the crate did not compile there at all.

Blocker for T20 and T21: `src/sys/` now owns every platform primitive — `file_id`, `nlink`,
`allocated`, `flags`, `mode`/`set_mode`, `symlink`, `clone_file`, a `Compressor` — plus the two
capability constants `CAN_CLONE` and `CAN_COMPRESS`, with one file per platform picked by
`#[cfg_attr(..., path = ...)]`. macOS kept exactly what it had; `applesauce` became a macOS-only
dependency. `Dedupe::plan` and `Compress::plan` return an empty plan when their capability is
false, before reading a single file: a clone the filesystem cannot share is a second copy of the
bytes, and a compress pass with no backend would clone every candidate only to throw the copy
away.

Outcome: `cargo check` is green for `x86_64-unknown-linux-gnu` and `x86_64-pc-windows-msvc`
(`just check-cross`), the macOS suite is unchanged and green, and no `std::os` import is left in
`src/` outside `src/sys/`. Three unit tests in `src/sys/mod.rs` state the facts that must hold on
every platform (a file has an identity of its own and a size on disk, a clone holds the bytes of
its source, an empty batch costs nothing), so they are what a port has to satisfy; the
integration suite still runs only where the machine is.

Honest about what the other two platforms do today: nothing but report. Both capabilities are
false on Linux and Windows, because a clone there depends on the filesystem under the root
(btrfs and XFS reflink, ext4 does not; ReFS clones, NTFS does not) and that is a runtime probe,
which is T20 and T21. Windows takes file identity from the path, so hardlinks read as separate
files and the link count always reads 1 — consistent within the model and inert while nothing is
planned, replaced by `GetFileInformationByHandle` in T21; sizes there are logical, not on-disk,
until `GetCompressedFileSize`. `seed` works on all three: where blocks are shared the copy is
free, where they are not it costs the disk and still saves the build. The test suite stayed
macOS-only, so the tests in `src/sys/` are a contract for the ports rather than proof they run.

### T20. Linux: reflink dedupe and filesystem compression

With T19 in place, fill in the Linux half. Dedupe: `FICLONE` (btrfs, XFS with reflink=1, bcachefs)
is the exact equivalent of `clonefile`; `FIDEDUPERANGE` is the safer variant that verifies the
bytes in the kernel and works even when the target is shared already. `rustix` is already a
dependency and covers both, so no new crate should be needed. Compression: btrfs takes
`chattr +c` (`FS_COMPR_FL`) per file, and only new writes are compressed, so a file has to be
rewritten to shrink — which is what the pass does anyway. ext4 has neither, so both passes must
report "not supported here" rather than pretend.

Done: the pass suite runs on a btrfs loopback image in CI, `ext4` falls back to T22 instead of
failing, and `docs/bench.md` gains a Linux row.

Plan: the capability stops being a constant. `sys::caps(dir) -> Caps { clone, compress }`, cached
per `st_dev`, answers what the filesystem under a profile dir can actually do, and both lossless
passes filter their profiles by it — which is also the answer to "report, do not pretend". On
Linux the probe is empirical rather than a filesystem-name table: two temp files and one
`FICLONE`, one `FS_IOC_SETFLAGS` with `FS_COMPR_FL`, both cleaned up. That gets XFS with
`reflink=0`, btrfs mounted `nodatacow` and a bind-mounted ext4 right, which a name table does
not. macOS keeps `true/true` (APFS is what it was measured on), Windows stays `false/false`
until T21.

`clone_file` on Linux becomes `FICLONE` through `rustix` instead of `fs::copy`, so a filesystem
that cannot share blocks fails loudly here instead of silently copying them. The compressor sets
`FS_COMPR_FL` on the engine's private copy and rewrites it through itself, because btrfs
compresses new writes only; `flags` starts reading `FS_IOC_GETFLAGS` so the engine's
"came back compressed" check works, and `st_blocks` then shows the win.

`FIDEDUPERANGE` is deliberately not used: the engine already replaces whole inode groups
atomically, re-checks every stamp under cargo's lock and restores mode and mtime, so an
in-place dedupe would be a second apply path with the same invariants to maintain and nothing
the first one does not already give. Say so if that call is wrong.

Verify: `just check` and `just check-cross` here, then a Linux VM (lima) with a btrfs loopback
image for the real suite, per the creator's choice of "compile first, then a VM". Nothing is
claimed to work on btrfs before that VM has run it.

Outcome: done, and two of the plan's own claims above turned out wrong — both found by running
it rather than reading it.

`sys::caps(dir) -> Caps { clone, compress }` landed as planned, cached per `st_dev`, and both
lossless passes filter their profiles by it before reading a file. `status` prints what the
filesystem cannot do under each target, and the inventory no longer counts savings it cannot
deliver: `compressible_bytes` and `dedupe_candidate_bytes` are zero where the capability is
missing, in the JSON as well as the text.

What the plan got wrong:

1. **The compression probe cannot be an attempt.** ext4 accepts `FS_IOC_SETFLAGS` with
   `FS_COMPR_FL`, keeps the flag where `lsattr` shows it, and compresses nothing — 200 MiB
   written with it set took 200 MiB. Measuring the file afterwards does not rescue the probe
   either, because btrfs reports the *uncompressed* size in `st_blocks`. Compression is now
   decided by `statfs().f_type == BTRFS_SUPER_MAGIC`; cloning stays a real `FICLONE`, which does
   tell the truth.
2. **`st_blocks` does not show the win on btrfs.** The plan said it would. A measured run
   compressed 1553 files and printed `applied 1553 (0 bytes)` while the volume gained 818 MiB —
   the pass worked, the platform cannot report it. `sys::ALLOCATED_SHOWS_COMPRESSION` says which
   platform is which, the A/B tests branch on it, and `docs/bench.md` has the Linux table with
   the free-space column marked as the only one to read there.

One real bug came out of the VM that no amount of macOS testing would have found: the capability
probe created and removed temp files inside the directory it probed, which moved that directory's
mtime — the same mtime `evict` and `incremental` read to tell an idle profile from a busy one.
Every target looked freshly built and `incremental` quietly planned nothing. The probe now puts
the mtime back, and `sys::tests::the_probe_cleans_up_after_itself` fails if it ever stops.

Verified: `just check` and `just check-cross` on macOS, the full suite green on macOS, and in a
lima VM (Ubuntu 24.04) on two loopback images — btrfs: 21 suites green; ext4: 21 suites green,
with `caps` finding neither capability and both passes planning nothing. Tests that can only be
observed where blocks are shared use `common::filesystem_can`, which returns early with a line on
stderr; the other side of each is asserted in `tests/caps.rs`, which runs the same fixture and
the same passes on both filesystems. One flake seen once under full parallel load on the ext4
image (`harness::oracle_is_green_on_an_untouched_build_and_sees_a_changed_mtime`, green alone and
green on the next full run) — fixture build timing in a 4-core VM, not a pass.

Not done, and deliberately: the suite runs in a local VM, not in CI — the creator chose
"compile first, then a local VM" over GitHub Actions. `ideas.md` carries the CI job as an idea.
`FIDEDUPERANGE` stays unused for the reason the card gives; nothing measured here changed that.
ext4 still wins nothing, which is T22.

### T22. Link fallback where the filesystem cannot clone

ext4 and NTFS have no copy-on-write, so `dedupe` has nothing to plan there. The fallback is a
hardlink, and the reason it is not simply the default is a real hazard: rustc opens its output
files with truncate, so a rebuild rewrites the inode in place and would rewrite every other name
pointing at it. Cargo's own hardlinks (`target/debug/fx` to `deps/fx-<hash>`) are safe because
cargo replaces the name rather than the inode; ours would not be.

So the fallback is split by what the file is, not by what the filesystem allows:

- **Cargo home sources** (`registry/src`, `git/checkouts`): safe to hardlink. Cargo extracts a
  crate into a fresh directory and writes `.cargo-ok` last; it never rewrites an extracted file
  in place. This is where the bytes are anyway — 1.52 GiB of them on the machine measured in
  `docs/bench.md`.
- **Build artifacts in a target dir**: only behind an explicit flag (`--link-artifacts`), with
  the truncate hazard in `--help`, in the README and in the dry-run output. Nothing enables it
  for the user.

Compression is unaffected and keeps its own answer per platform: APFS and btrfs and NTFS have
it, ext4 and XFS do not, and a pass that cannot run says so instead of failing. Done: a fixture
on a filesystem without reflinks shares the cargo home's sources and leaves target artifacts
alone unless the flag is given, `status` says which of the two the filesystem under each root
can do, and the hazard is written down where a user meets it.

Plan followed: `Replace` gained a `how: Share` field — `Share::Clone` or `Share::Link` — so the
engine is told how to share rather than guessing, and `Dedupe::share(profile)` answers it per
profile: a clone wherever `caps.clone` is true, a hardlink where it is not *and* the pass was
given `link_fallback`. `run --cargo-home` now runs `dedupe` beside `compress` with that fallback
on; target groups get it only from `--link-artifacts`.

Outcome: done. Two rules turned out to belong in the engine rather than the pass, because both
are about the mechanism and not about the policy:

- **Modes must already agree.** One inode holds one mode, so linking files whose permissions
  differ would quietly change the other name's. That is `Skip::ModeMismatch`, and nothing is
  touched when it fires.
- **The shared inode keeps the later of the two modification times.** A hardlink hands the
  member the source's mtime, and a file that suddenly reads older than what it was built from is
  a file cargo rebuilds — the pass would then cost a build instead of saving space. The source's
  own mtime moves forward with it, which is the safe direction.

`status` says the whole truth now: on a filesystem with neither capability the line reads
*compress finds nothing here, dedupe only links cargo home sources* rather than claiming both
passes are idle.

Tests: `tests/link.rs` is the A/B, and the flag is the only difference between its two runs. Both
filesystem outcomes are stated in each test rather than skipped, as in `tests/caps.rs` — where
blocks can be shared the twin is cloned and the flag changes nothing, where they cannot the
control run leaves the artifacts exactly as it found them and only the treatment shares them.
The cargo home's sources are shared with no flag on either side. Both runs end with the freshness
oracle, because linking moves mtimes and a pass that costs a rebuild is not a saving.
`tests/engine.rs` covers the mechanism itself on every platform (one inode for the whole group,
the later mtime kept, a mode mismatch refused), which is what keeps the link path under test on
macOS, where it is never taken by policy.

Verified: `just check`, `just check-cross` and the full suite on macOS (22 suites), and in the
lima VM on btrfs and ext4 loopback images, all green — on ext4 the link path is the one actually
taken. Two flakes were seen there under full parallel load, each once, each green alone and on
the next full run (`harness::oracle_is_green_…` during T20, `doc::ab_only_the_named_run_removes_
the_docs` here): fixture builds racing on 4 cores, not a pass.

Not done: nothing from the card. `docs/bench.md` gained no row for this — the VM has no real
cargo home to measure and pointing the tool at the machine's own is not something a test or a
benchmark here may do.

### T23. User guide, ecosystem study and T21 readiness analysis

Asked for directly by the creator, so it went from request to done without a stop in
`roadmap.md`. Documentation only; no code, manifest or dependency changed.

- `docs/usage.md` — install, the first five minutes, which passes are lossless and which delete,
  every command and option, exit codes, recipes, configuration, troubleshooting. The option
  tables were written from the binary's own `--help` output.
- `docs/ecosystems.md` — a desk study of whether the passes fit C / C++, .NET, Go, Swift / Xcode,
  content-addressed stores, the JVM and Bazel: what in the codebase is cargo-specific, the six
  questions an adapter has to answer, a verdict per pass per ecosystem, the existing tools and
  the gap they leave. Unverified claims are marked; nothing was measured. The follow-up is in
  `ideas.md`, not approved.
- `README.md` — the stale "Planned: `cargo tare advise`" block is gone (it has worked since
  T12), the status line says what works where and points at both documents.
- `plan.md` — T21's card gained a readiness analysis: eleven open points, the first of which
  (where Windows tests run) is the creator's decision and the task's real blocker.

Verified: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`
(127 passed on macOS) and both `check-cross` targets were green on macOS before the documents were
written; the documents change nothing those commands read.

### T42. Rename the project: `cargo-tare` becomes `dunnage`

Asked for by the creator: a name that fits a tool no longer tied to cargo, and that nobody
holds. `tare` itself is taken on crates.io. `dunnage` — the loose packing stuffed around the
cargo in a hold: it takes up room and is not the goods — keeps the metaphor and was free on
crates.io, Homebrew (formula and cask), npm and PyPI when checked, with only zero-star
repositories of that name on GitHub. Runners-up that were also free: `unladen` (crates.io and
Homebrew only), `plimsoll` and `freeboard` (both taken on npm and PyPI; `freeboard` is a
6.5k-star project).

What changed: the package, the library crate (`dunnage`) and the binary (`dunnage`, invoked
directly instead of as `cargo tare`); the config and cache dirs (`~/.config/dunnage`,
`~/.cache/dunnage` — neither existed under the old name on the creator's machine, so nothing
was migrated); the temp prefix (`.dunnage-tmp-`), the hash index magic, the launchd label, the
bench scripts, every living document and `rust.md`. `cargo dunnage <args>` still works through
a `cargo-dunnage` link to the binary: cargo passes the subcommand name first and `main` drops
it, which a new test in `tests/cli.rs` holds. Left alone on purpose: `done.md` above this entry
and `docs/spike/`, which are records of what was run under the old name.

The creator renamed the GitHub repository to `listepo/dunnage` (`gh repo rename`, which also
moved `origin`); `plan.md` and `docs/usage.md` carry the new URL. The local directory name is
the creator's to change.

Verified on macOS: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
`cargo test` (128 passed, one of them new) and both `check-cross` targets.

### T36. Library boundary: one `Session` under every front end

The blocker of everything the creator decided about how the tool runs: CLI and daemon share
the code, and embedding in a build system stays possible. Today the library holds the passes
and the engine but the *run* lives in the binary — `src/main.rs` is 806 lines, and `fn run`
alone (468–714) selects passes, groups families, picks what `evict` and `orphans` take, runs the
cargo home, loads and saves the hash index and decides the exit code; `status`, `advise` and
`seed_into` assemble their results there too. A daemon could share none of that.

Move it behind `Session` as sketched in `docs/architecture.md`, "Process model": `open`,
`inventory`, `advise`, `plan`, `apply`, `seed`; a `Request` both front ends build; a `Control`
with an `Observer` for progress and notes, a `stop` flag checked between groups of actions, and
a `lock_budget` after which the engine releases a unit's build lock and returns to it later.
The session takes the tool's own run lock (one file next to the hash index, `try_lock`, exit
code 2 for the CLI when held). The library stops printing, exiting and reading the environment
or config files on its own — `Settings` carries the paths, the resolving helpers stay as
functions a front end calls — and returns a typed `Error`; `anyhow` and `clap` move behind a
default `cli` feature that the `[[bin]]` requires. No workspace and no second crate.

No new behavior. Done: `main.rs` is parsing, printing and the exit code; every `tests/cmd`
snapshot and `tests/cli.rs` case is unchanged; `cargo check --lib --no-default-features` is part
of `just check`; a test drives a whole run through `Session` with no binary involved; two
sessions applying at once are serialized by the run lock; a `stop` raised mid-run leaves every
file either old or new; `DESIGN.md` gains the session next to "Engine".

Result. `src/session.rs` holds the run: `Session::{open, inventory, advise, plan, apply, seed}`,
`Request` (with `from_config` and `check`), `Control` (`Observer`, `stop`, `lock_budget`),
`RunReport`, `Advice`, `Seeding`, and the run lock `run.lock` next to the hash index, taken by
`plan`, `apply` and `seed` after the read-only checks, so a mistyped flag touches no state.
`src/error.rs` is the typed `Error`; `config.rs` uses it. The engine gained `run_with` and
`Interrupt`: a stop flag and a deadline checked between actions and compress batches, reported
as `Report::interrupted`. A group out of lock budget lets go and is visited once more after the
others; what is left then counts as busy, and scheduling it later is the daemon's (T34).
`plan` is a dry run of the same pipeline, not a `Plan` value: each pass plans on what the one
before it left. `main.rs` went from 810 to 587 lines of parsing, printing and exit codes; the
CLI's one new behavior is the run lock (exit code 2, "another run of dunnage holds …").
`anyhow` and `clap` are behind the default `cli` feature, which the `[[bin]]` requires.

Verified on macOS: `just check` — fmt, clippy with `-D warnings`, `cargo check --lib
--no-default-features`, 134 tests (the 128 before, unchanged, every `tests/cmd` snapshot
included, plus six new: a whole run through `Session` with the freshness oracle, two sessions
kept apart by the run lock, the CLI's exit code 2, a stop between groups, a group out of lock
budget, and in `tests/engine.rs` a stop raised between two replacements that leaves one file new,
the other old and no temp file) — and `just check-cross` for Linux and Windows.

### T25. `run` until nothing is left to do

`docs/usage.md` has to tell users that a second run still finds work: clones made by `dedupe`
are files `compress` never saw (46 actions on the benchmark workspace, `docs/bench.md`).
`DESIGN.md` already names the cure — repeat until a round plans nothing. The locks are held
once for all rounds, the report sums the rounds, `--dry-run` stays one round because it changes
nothing a second round could see. Done: a test where one `run` leaves a second `run --dry-run`
with an empty plan, the oracle green, and the troubleshooting entry gone from `docs/usage.md`.

Lands on the session (T36) as `Request::until_settled`, so the daemon settles a unit in one
visit exactly as the CLI does.

Result. `engine::Options::until_settled`: after a round of every pass that applied anything,
the engine runs the passes again on the rescanned profiles under the locks it already holds,
until a round applies nothing — "applied", because a group skipped for good is planned again
every round — at most 8 rounds, and never on a dry run. The stop flag and the lock budget end
the rounds too. `Report::rounds` counts them; a later round adds what it applied and what it
skipped or removed for the first time, so planned still equals applied plus skipped.
`Request::until_settled` carries it through the session; the CLI always sets it. No flag.

The fixture does not reproduce the 46 actions the benchmark workspace left for a second run:
one round already settles it, with one target or two compared across families. So the rounds
are proven in `tests/engine.rs` with two passes where the second makes work for the first (three
rounds, counts summed; one round without `until_settled` and on a dry run), and
`tests/settle.rs` checks the whole: two targets, one `run`, then `run --dry-run` plans nothing,
and both targets stay fresh. `docs/bench.md` says the loop exists and was not re-measured there.
The troubleshooting entry is gone from `docs/usage.md`; `DESIGN.md` describes the rounds.

Verified on macOS: `just check` (fmt, clippy `-D warnings`, `--lib --no-default-features`,
137 tests) and `just check-cross`.

### T26. `dunnage worktree add`

The recipe in `docs/usage.md` is two commands — `git worktree add`, then `seed` — and the second
is the one people forget, which is exactly how a cold first build happens. One command that runs
`git worktree add` with the arguments it was given and seeds the new checkout from the family.
If git fails, nothing is seeded; if seeding fails, the worktree stays and the error says so.
Done: a test on the fixture repository creates a worktree whose first build reports third-party
units fresh; `--dry-run` passes through to `seed` only.

The git call and the seeding are one `Session` operation (T36); the CLI only parses and prints.

#### Execution plan

1. `Session::worktree_add(dir, git_args, dry_run)`: `git worktree add <args>` run in `dir` with
   its output captured; the new worktree is the one `git worktree list --porcelain` shows after
   and not before, so no git argument is ever parsed. The dir to seed is the new worktree plus
   where `dir` sits inside its own checkout, so a workspace in a subdir of a monorepo is seeded
   at the same place. Seeding is `Session::seed` with `seed::choose`; no sibling with a target
   is a note, not an error. Git fails: an error, nothing seeded. Seeding fails: the worktree
   stays and the error names it.
2. CLI: `dunnage worktree add [--dry-run] [--index FILE] <GIT ARGS>...`; `--dry-run` goes to
   `seed` only.
3. `tests/worktree.rs` on a small repository: the seeded worktree builds less than one added
   with plain git; `--dry-run` adds the worktree and copies nothing; a failing git call seeds
   nothing and exits 1.
4. `docs/usage.md` recipe and command; `DESIGN.md` seed section.

#### Result

`Session::worktree_add` and `dunnage worktree add [--dry-run] [--index FILE] <GIT ARGS>...` as
planned. The shared fixture could not show the win: it has only path dependencies, and those
are rebuilt in any new checkout because their mtimes are newer than the copied fingerprints. A
vendored dependency inside the repository is rebuilt too (`PathToSourceChanged`). So
`tests/worktree.rs` builds its own repository whose one dependency is a directory source outside
it, as registry crates are: in the seeded worktree that dependency is fresh and only `app` is
rebuilt; in a worktree added by plain git it is built again. Docs: `docs/usage.md` command and
recipe, `DESIGN.md` seed section and synopsis, `README.md` synopsis.

#### Verified

`just check` and `just check-cross` green; the four tests in `tests/worktree.rs` pass.

### T41. Expire the hash index

`src/index.rs` says it itself: entries of deleted targets are never expired. For a CLI run now
and then that is a slowly growing file; under a daemon that visits every unit after every build
it grows for as long as the machine lives. A last-seen field per entry, entries not seen for a
configurable time dropped on save, the file format version bumped with a silent rebuild from an
old file. Free to start. Done: a test ages entries and sees them go; an old-format index is
read as empty rather than as an error.

#### Execution plan

1. `src/index.rs`: a `seen` field per entry (seconds since the epoch), stamped by `get`, `put`
   and `mark_shared` with the time the index was loaded; `load_at` for tests; `expire(idle)`
   drops entries seen before `now - idle`. Magic `DUNIDX02`; an old file fails the magic check
   and reads as empty.
2. `Settings::index_idle` (default 30 days) and `Settings::from_config`; the session expires the
   index before every save. Config `[index] idle-days`; `run`, `seed` and `worktree add` read it.
3. Unit tests in `src/index.rs`: entries age and go; the old format reads as empty and is
   rewritten.
4. `DESIGN.md` index paragraph and config line; `docs/usage.md` and `README.md` config examples.

#### Result

As planned. The CLI's config loading moved into one `load_config` helper, so `seed` and
`worktree add`, which also save the index, keep it as long as `run` does.

#### Verified

`just check` and `just check-cross` green.

### T28. Adapter boundary: what is cargo and what is not

`docs/ecosystems.md`, first table: the engine, the inode model, the hash index, `src/sys/` and
the two lossless passes know nothing about cargo; discovery (`CACHEDIR.TAG`), the unit of work
(a profile dir), the lock (`.cargo-lock`), "last built", what `seed` leaves behind and the
cargo-only passes do. Put the second list behind one trait answering the six questions of that
document — discover, lock, freshness-relevant volatile paths, owner, last use, and the oracle in
tests — with cargo as its only implementation. No crate split until a second binary needs one,
no new flag, no behavior change: the existing tests and the `--help` snapshots are the
proof. The card of the first non-cargo adapter decides how an ecosystem is selected on the
command line; this one only makes room for it.

The shape is worked out in `docs/architecture.md`: the `Ecosystem` trait and its `Guard` /
`Policy` answers, `src/eco/` as the twin of `src/sys/`, one shared discovery walk where the
outermost claim wins (a CMake dir inside a cargo target is nobody else's), and family and
position computed from the build dir's *owner* rather than from where it sits — which is what
gives an out-of-tree `build-dir` a family at all. Part of done: the monorepo fixture described
there, with the assertions that already hold.

Sits on T36: discovery and the adapters are reached through the session, and `Guard::Held` is
reserved in the enum for an embedding caller (R8) without being implemented.

#### Execution plan

1. `src/eco/mod.rs`: the `Ecosystem` trait (`name`, `claim`, `owner`, `units`, `guard`,
   `volatile`, `last_used`, `build_dir`), `Guard` (`Held`, `Lock`, `Shared`, `Quiet`,
   `Immutable`), `Policy` / `Sharing`, `Owner`, the registry and the one shared walk (first
   claim wins, a claimed dir is not entered).
2. `src/eco/cargo/`: the `Cargo` adapter built from what is in `model.rs` (`CACHEDIR.TAG`,
   `.cargo-lock`, profile dirs), `inventory::last_built`, `seed.rs` (`target`, what is left
   behind), plus `CargoHome` with `Guard::Shared(.package-cache)`. `incremental`, `doc`,
   `toolchains`, `advise` and `cargo_home` move under it.
3. The engine takes an `&dyn Ecosystem` instead of `Locks` and locks what `guard` names;
   `model::scan` skips what `volatile` names; `seed` asks `build_dir` and `units`; inventory
   takes family and orphan status from the owner's project instead of from the build dir's
   parents. Same result for cargo, where the owner is the dir above the target.
4. Tests: every existing test and `--help` snapshot unchanged in meaning; a monorepo test (one
   repository, two workspaces at different positions, a CMake-looking dir inside a target, a
   worktree) asserting each build dir is found once, the nested one is nobody's, and both
   positions are one family.
5. `DESIGN.md` and `docs/architecture.md` "Where this stands".

#### Result

As planned, with these differences from the sketch in `docs/architecture.md` (recorded there):
`claim` is a yes or no, units are paths, `private` (never scanned or copied: `.cargo-lock`) and
`volatile` (never seeded: `incremental/`) are separate questions, and `Policy` has only `share`.
`engine::Locks` is gone: the engine takes the adapter and locks what `guard` names, a shared
guard once for all its units. `Profile::last_used` is read at scan time, under the lock, so
`evict` and `incremental` re-check without calling cargo code; `Orphan` carries its project.
The walk no longer enters `.git`. Still naming cargo outside `src/eco/`: the session (cargo's
passes and reports, the adapter `seed` uses) and the inventory's cargo-shaped status fields.
`tests/monorepo.rs` is the monorepo fixture; the git test helper moved to `tests/common`.

#### Verified

`just check` and `just check-cross` green; every existing test passes unchanged in meaning (only
call sites moved to the new API), and so does `tests/monorepo.rs`.

### T29. A safety tier for build systems without a build lock

Cargo holds one advisory lock for the whole build; Ninja, Make, MSBuild and Xcode hold nothing
an outsider can test. Without a lock the engine's re-check of `(size, mtime)` before each
`rename` narrows the race and does not close it. Build the weaker tier and name it as such in
every report: a larger default `min-age`, a sharing violation or a busy file counted as "busy"
(exit code 2) instead of a failure, a check for the build tool's running processes under the
dir, and a refusal of lossy passes when any of those says "maybe". Two runs of the tool itself —
the daemon and a manual one — are kept apart by the session's run lock (T36), which this tier
relies on. Done: a test that writes into a fixture dir while a pass runs and ends with the newer
bytes in place, never the older ones; `DESIGN.md` gains the tier next to "Safety invariants"
with exactly what it does not promise.

#### Execution plan

1. `Guard::Quiet` in the engine: no lock; `Ecosystem::tools` names the build tool's processes,
   and `sys::tool_running(dir, tools)` (macOS: `ps` then `lsof -d cwd`; Linux: `/proc`; Windows:
   unknown) decides. Running under the unit, or the unit under its cwd: busy, left out. Unknown:
   the unit is *unsure*.
2. After the scan, files of a quiet unit younger than `Options::quiet_min_age` (default one day,
   never lowered by `--min-age`) are dropped from the model; any dropped makes the unit unsure.
3. Lossy actions on an unsure unit are refused (`Skip::Unsure`).
4. A busy file (`ETXTBSY`, a Windows sharing or lock violation) is `Skip::Busy`, and its unit is
   reported busy (exit 2) instead of failed — for every guard.
5. `Report::quiet` lists the units worked on without a lock; the table and `--json` say so.
6. `tests/quiet.rs` with a test adapter: a writer rewriting a file during a dedupe run ends with
   its own bytes and the old equal files are still deduped; a running tool makes the unit busy;
   a lossy removal in an unsure unit is refused. `DESIGN.md` "Safety tier without a build lock"
   next to "Safety invariants".

#### Result

- `Guard::Quiet` works. `Ecosystem::tools` names the build tool's processes;
  `sys::tool_running` looks for them (`ps` + `lsof` on macOS, `/proc` on Linux, unknown on
  Windows). A tool in the unit, below it or in a dir around it makes the unit busy; a process in
  a filesystem root counts for nothing.
- `engine::QUIET_MIN_AGE` (one day) is a constant rather than an option: no pass setting and no
  flag lowers it. Young files are dropped from the model at every scan, and their unit becomes
  unsure; an adapter with no tools, or a platform with no check, makes every quiet unit unsure.
- `Skip::Unsure` refuses `Remove` in an unsure unit and `RemoveTarget` over one. `Skip::Busy`
  (`ETXTBSY`, `EBUSY`, Windows errors 32 and 33) puts the file's unit into `Report::busy`, which
  makes exit code 2, for every guard.
- Found while writing the race test: the source of a clone was checked before the copy only, so
  bytes written into it during the copy could land under the member's names. The source is now
  stamped again after the clone (not after a link: that is the source itself). This holds for
  every guard.
- `Report::quiet`; the table prints `no build lock, weaker checks: <unit>`, `--json` a `quiet`
  list per group.
- `DESIGN.md` "Safety tier without a build lock" with what it does not promise;
  `docs/architecture.md` notes T29 as done. No adapter uses `Quiet` yet.

#### Verified

`just check` (151 tests) and `just check-cross` green. `tests/quiet.rs` 15 runs in a row green;
the unit test of the busy-error classifier is in `src/engine.rs`. `~/.cache/dunnage` absent after
every run.

### T33. Compress an immutable content-addressed store

One mode instead of five adapters: `~/.cabal/store`, the Zig caches, dune's shared cache and
the like hold immutable files under hashed names. `dedupe` finds nothing there by
construction, and `compress` is safe without a lock for the same reason — a name never gets
different bytes — with `min-age` keeping the pass off what is being written. The user names the
dir; nothing is discovered or guessed, and stores that compress themselves (ccache, sccache)
are refused by their marker files. Done: mtime, mode and content of every file unchanged, the
owning tool's own verification green on a fixture store, numbers in `docs/bench.md`.

#### Execution plan

1. `src/eco/store.rs`: a `Store` adapter — one unit, the dir itself; `Guard::Immutable`;
   `ClonesOnly`. `store::check` refuses a dir that is not one: ccache and sccache (a
   `CACHEDIR.TAG` naming them, `ccache.conf`, their default dir names), and a dir inside a
   build dir a registered adapter claims or a cargo home.
2. Engine: `Guard::Immutable` takes no lock, is listed in `Report::quiet`, is always unsure (no
   lossy pass), and files younger than `engine::IMMUTABLE_MIN_AGE` (one hour) are left out.
3. `Request::stores`, `--store <DIR>` on `run` (repeatable), `stores = [...]` in the config. A
   run of stores only needs no root, like `--cargo-home`. Only `compress` runs on a store.
4. `tests/store.rs`: mtime, mode and content of every file unchanged; ccache refused; young
   files left alone; a `GOCACHE` fixture (skipped without `go`): after the pass
   `GODEBUG=gocacheverify=1 go build` is green and `go build -x` compiles nothing.
5. `docs/bench.md` numbers on a `go build std` fixture cache; `DESIGN.md`, `docs/usage.md`,
   README synopsis.

#### Result

- `src/eco/store.rs`: the `Store` adapter (one unit, `Guard::Immutable`, `ClonesOnly`) and
  `store::check`, which refuses what is not a dir, ccache and sccache, and dirs inside a claimed
  build dir or a cargo home — before the run lock is taken.
- Engine: `Guard::Immutable` takes no lock, is listed in `Report::quiet`, is always unsure (no
  lossy pass) and leaves out files younger than `engine::IMMUTABLE_MIN_AGE` (one hour).
- `--store DIR` (repeatable) on `run`, `stores = [...]` in the config, `Request::stores`. A run
  of stores alone needs no root; each store is a group of its own with `compress` only.
- The oracle changed from the card's "the owning tool's own verification": Go's
  `GODEBUG=gocacheverify=1` did not notice a flipped byte in a data entry, so it proves nothing.
  The oracle is the store's own invariant instead — every `*-d` entry of `GOCACHE` hashes to its
  name under SHA-256 — plus `go build -x` compiling nothing afterwards.
- Numbers in `docs/bench.md`: a `GOCACHE` after `go build std`, 215.6 MiB → 63.6 MiB (−70.5%),
  1131 entries still matching their names, the rebuild compiled nothing.
- Docs: `DESIGN.md` "Immutable stores" (with what it does not promise: Zig's `h/` manifests are
  rewritten, so name `o/`), `docs/usage.md`, README, `docs/architecture.md`.

#### Verified

`just check` (158 tests) and `just check-cross` green; `tests/store.rs` covers mtime, mode and
content unchanged, the young entry, no lossy pass, the refusals, the CLI and the `GOCACHE`
oracle. `~/.cache/dunnage` absent after every run.

### T37. Monorepo: `seed` every position

`seed::choose` already looks "at the same place inside the sibling checkout", but for one dir
with the hardcoded name `target`. A monorepo checkout has many build dirs. `seed` in a checkout
root seeds every position: for each build dir of the sibling checkouts whose adapter allows
seeding, if the owner's project exists in the new checkout and the position is empty, clone it
from the sibling where *that position* was built most recently — no single worktree is the
newest everywhere. Positions whose project is absent on this branch are skipped. `seed <dir>`
keeps today's meaning. `dunnage worktree add` (T26) gets it for free. Uses T28's owner and
position. Done: on the monorepo fixture, two workspaces are seeded from two different siblings
and the project that exists on one branch only is left alone; the oracle is green for both.


#### Execution plan

1. `seed::positions(checkout)`: for every sibling checkout of the family, `eco::discover` its
   build dirs; keep those at their adapter's default place (`build_dir(owner) == dir`) whose
   owner belongs to that sibling (not to a worktree nested in it); the same project path in
   `checkout` must be a dir with no build dir yet. Per position, the sibling built most recently.
2. `Session::seed`: with no `--from` and a checkout root as the dir, every position under one run
   lock, returning `Vec<Seeding>`; otherwise today's single seed. `worktree_add` from a checkout
   root seeds every position, from a subdir only its own.
3. CLI prints one line per position; exit 2 when any source unit was busy.
4. `tests/monorepo.rs`: two workspaces seeded from two different siblings, the project absent on
   this branch left alone. `tests/worktree.rs`: a real two-workspace repository, `worktree add`
   from the root, the oracle (third-party units fresh) for both workspaces.
5. `docs/usage.md`, `DESIGN.md` seed section.

#### Result

- `seed::positions(checkout)` and `seed::Position { project, eco, source }`: every build dir of
  every sibling found by the shared walk, at its adapter's default place, owned by that sibling
  (a worktree nested in it is its own sibling), whose project dir exists in the checkout with no
  build dir yet; per position the sibling whose units were used last.
- `Session::seed` returns `Vec<Seeding>`. In a checkout root with no `--from` it seeds every
  position under one run lock and is an error when there is none; otherwise it is the single
  seed it was. `WorktreeAdded::seeding` is a `Vec` too: from the checkout root every position,
  from a subdir its own. The CLI prints one line per position; exit 2 when any source unit was
  busy.
- Docs: `docs/usage.md` (`seed`, `worktree add`), `DESIGN.md` seed section,
  `docs/architecture.md`.

#### Verified

`just check` (160 tests) and `just check-cross` green. `tests/monorepo.rs`: `api` comes from the
worktree that built it last, `cli` from the main checkout, `legacy` (absent on the new branch) is
left alone, a second seed has nothing to do. `tests/worktree.rs`: `worktree add` from the root
of a real two-workspace repository seeds both, and cargo reports the vendored dependency fresh in
both — the oracle. `~/.cache/dunnage` absent.

### T38. Monorepo: `orphans` for a project that is gone

Today an orphan is a worktree whose git record is gone. In a monorepo the common orphan is
smaller: a project deleted, renamed or absent on this branch, whose build dir stays behind in a
live checkout. A second reason for the same lossy pass, from T28's owner: the owner's manifest
no longer exists. A branch switch produces the same picture as a deletion, so this reason
removes only units idle for longer than a threshold (`evict`'s `idle-days`, or one of its own)
and is otherwise only reported, by `status` and `advise` too. The *checkout gone* reason now
also covers build dirs outside the checkout (`build.build-dir`), which have no family today.
Done: fixture tests for both reasons, for "reported, not removed" without a threshold, and for
an out-of-tree build dir landing in its owner's family.

#### Result

- `Ecosystem::manifest(project)` (cargo: `Cargo.toml`); `inventory::Target::project_gone` when
  it is missing from a checkout that is otherwise there (JSON `project_gone`).
- `orphans::Reason::{CheckoutGone, ProjectGone { manifest, idle_days }}`, printed with every
  removal. A gone project goes only with `--orphans-project-idle-days N` /
  `[orphans] project-idle-days`, which needs `--lossy orphans`, and only when the newest
  `last_used` of its locked profiles is N days old; no `last_used` means not idle. Under the
  lock the manifest must still be missing, so a switch back to the branch keeps the target.
- Without the threshold: reported only, `PROJECT GONE` in `status` and a note in `advise`.
- The out-of-tree half (`build.build-dir` targets getting an owner) is T38.1: the build dir
  records no owner, and how to learn it is the creator's call.
- Test fixtures' `fake_target` writes a `Cargo.toml` next to its target.
- Docs: `README.md`, `docs/usage.md`, `DESIGN.md` orphans section, `docs/architecture.md`.
- `todo.md`, emptied by accident in the T33 commit, restored.

#### Verified

`just check` (164 tests) and `just check-cross` green. `tests/orphans.rs`: a gone project idle
ten days loses its target with a seven-day threshold while a file next to it stays; built two
days ago, or with no threshold, it is only reported (`PROJECT GONE` in `status`, `planned 0`);
a `Cargo.toml` back between the inventory and the lock keeps the target; the threshold without
`--lossy orphans` is an error. The worktree orphan tests still pass unchanged.
`~/.cache/dunnage` absent.

### T30. Swift: SwiftPM `.build/` (DerivedData split off as T30.1)

The most promising target after cargo: APFS is where the tool is strongest, DerivedData runs to
tens of GB, and the only known cure is deleting it. SwiftPM first — `.build/` sits in the
package like `target/` does, and SwiftPM refuses a second instance on it, so there is a lock to
find. DerivedData second: `info.plist` records `WorkspacePath`, which makes `orphans` and
`evict` direct; it needs T29. Spike: confirm the lock file and the call, the plist keys on the
current Xcode, the compress and dedupe yield, and an oracle (`swift build` twice, the second
compiles nothing). Reading a plist may need a crate or `plutil`; a new dependency is the
creator's call at claim time. `seed` does not apply — the dir name is a hash of the path.

#### Result

- Spike (Swift 6.4, Xcode toolchain): `swift build` holds `flock` on
  `<temp dir>/<scratch path, / as _>.lock` (TSCBasic `FileLock`, last 255 bytes of the name) for
  the whole command; `.build/.lock` is only a pid note. swiftbuild writes into `.build/out`, the
  native build system into `.build/<triple>`. `out/CompilationCache.noindex` holds mmapped,
  sparse databases of 12–25 GiB logical size. A switch between debug and release recompiles by
  itself; only `swift build -v` names compile tasks.
- `src/eco/swiftpm.rs`, registered after cargo: claim `.build` with `workspace-state.json`,
  owner the package dir, manifest `Package.swift`, units `out/` and `<triple>/`,
  `Guard::Shared` on the temp lock of the canonical scratch path, `CompilationCache.noindex`
  private, clones only, no `seed`.
- `sys::temp_dir`: `TMPDIR`, else `getconf DARWIN_USER_TEMP_DIR` on macOS.
- The engine creates a missing `Guard::Shared` lock file, as the build tool does.
- `model::scan` leaves private dirs out whole.
- `advise` looks at cargo targets only.
- Yield on swift-argument-parser (debug + release): `.build` 356.7 MiB → 157.4 MiB (−55.9%).
- DerivedData is T30.1: none exists here and none may be made in the real `~/Library`.
- Docs: `README.md`, `docs/usage.md`, `DESIGN.md` SwiftPM section, `docs/bench.md`,
  `toolchain.md` (`swift`, optional).

#### Verified

`just check` (172 tests) and `just check-cross` green. `tests/swiftpm.rs`:
- fixtures: the claim, the units (checkouts, repositories and `index-build` left out), the
  compilation cache never scanned, one lock for all units named in the temp dir, a held lock
  making every unit busy, and a missing lock created;
- with `swift` installed: `swift build` waits while the test holds the lock the adapter names,
  and after dedupe + compress `swift build -v` compiles nothing and the binary runs, while a
  new source mtime does make it compile (the oracle can say no).

By hand on swift-argument-parser: `swift build -c release -v` runs no compile task after the
run, as before it, and the `math` example still adds. `~/.cache/dunnage` is absent, and no
lock file is left in the temp dir.

### T31. .NET: `bin/` and `obj/`

The highest dedupe yield in the study: every project's `bin/` holds its own copy of every
transitive NuGet assembly, byte-identical to the one in `~/.nuget/packages`. Discovery by
`obj/project.assets.json` next to a project file, `artifacts/` with `UseArtifactsOutput`.
**Clones only, never the hardlink fallback** — MSBuild's `Copy` overwrites in place, which is
how its own hardlink option corrupts the NuGet cache (dotnet/msbuild#8273); `--link-artifacts`
must be refused here, not merely off. No lock, and on Windows worker nodes keep files open:
needs T29. Oracle: `dotnet build` twice, the second reports every target skipped — that also
settles whether `CoreCompileInputs.cache` survives a same-content, same-mtime replacement.
macOS and Linux first; Windows needs ReFS and therefore T21. `seed` does not apply.

#

#### Result

- Spike: the .NET 10.0.401 SDK here fails every build (workload manifests missing; the repair
  changes the system install and was not run). 9.0.306, pinned by `global.json`, builds offline.
  Nothing was downloaded: two apps with a `ProjectReference` to one library stand in for NuGet
  copies.
- `src/eco/dotnet.rs`, registered after SwiftPM:
  - it claims `obj/` with `project.assets.json`, and `bin/` next to such an `obj/`, each as one
    unit;
  - the owner is the project dir, and the manifest the project file that
    `obj/<file>.nuget.dgspec.json` names;
  - `Guard::Quiet`, with `dotnet`, `MSBuild` and `VBCSCompiler` as the tools;
  - `Sharing::ClonesOnly`, so `--link-artifacts` never links here;
  - no `seed`.
- `CoreCompileInputs.cache` survives a same-content, same-mtime replacement: MSBuild builds
  nothing after dedupe + compress.
- The yield is not measured: it needs NuGet packages, and the cache here is empty. This is said
  in `docs/bench.md`.
- `UseArtifactsOutput` is not found yet; `DESIGN.md` says so.
- Docs updated: `README.md`, `docs/usage.md`, the `DESIGN.md` .NET section, `docs/bench.md`, and
  `toolchain.md` (`dotnet` 9, optional).

#### Verified

- `just check` (176 tests) and `just check-cross` pass.
- `tests/dotnet.rs`, fixtures:
  - `bin/` and `obj/` of a restored project are claimed, and those of an unrestored one are not;
  - the manifest is the recorded project file even with a second one next to it, and deleting
    it marks the project gone;
  - hardlinks are refused even when asked for.
- `tests/dotnet.rs`, with a .NET 9 SDK:
  - three projects, built and then aged two days;
  - a control build is a no-op;
  - after compress + dedupe, `dotnet build -v:n` skips every `CoreCompile` and copies nothing,
    and the app still runs;
  - a new source mtime does make it build.
- The same oracle holds by hand after `dunnage run` on the spike fixture.
- `~/.cache/dunnage` is absent.

### T32. C and C++: CMake, Meson and Ninja build dirs

`compress` is the strong case — uncompressed DWARF in objects and static libraries; cargo's own
`.o` files went to ~5%. `orphans` is easier than for cargo: `CMakeCache.txt` records
`CMAKE_HOME_DIRECTORY`, so "the source is gone" is one `stat`. `dedupe` is expected to find
little (absolute paths in objects) and is measured, not assumed; never hardlinks. `seed` does
not apply — the build dir is full of absolute paths — and `advise` recommends `ccache` with
`file_clone = true` for that job instead. No lock: needs T29, and the spike first settles
whether current Ninja takes one. Plain Make has no marker and builds in the source tree: out of
scope. Oracle: `ninja -n` after a pass plans nothing.

#### Execution plan

This machine has `cmake` 3.31 and a C compiler, but no `ninja` and no `meson`, and installing
them is a new program for the creator to approve. So this task covers CMake build dirs made by
any generator and is verified with the Makefiles one. The Ninja lock question, Meson, and a
Ninja oracle are split off as T32.1.

1. `src/eco/cmake.rs`:
   - claim a dir holding `CMakeCache.txt`, as one unit;
   - the owner is `CMAKE_HOME_DIRECTORY` from the cache, and the manifest `CMakeLists.txt`;
   - `Guard::Quiet`, with `cmake`, `ninja`, `make`, `gmake` and `ctest` as the tools;
   - clones only, and no `seed`.
2. Tests (`tests/cmake.rs`):
   - fixtures for the claim, the owner read from the cache, and an owner that is gone;
   - with `cmake` and `cc`, a real project built with `-g`, then aged;
   - the oracle: after compress + dedupe, `cmake --build` builds and links nothing, and the
     binary runs; a new source mtime makes it build.
3. Measure on a fixture; docs (README, usage, DESIGN, bench), `toolchain.md`; T32.1 card.

#### Result

- `src/eco/cmake.rs`: a dir holding `CMakeCache.txt` is one `Guard::Quiet` unit, clones only.
  The owner is the cache's `CMAKE_HOME_DIRECTORY`, the manifest its `CMakeLists.txt`, so
  `project_gone` and orphans work as for other ecosystems. An in-source build (the source dir is
  the build dir or inside it, compared as real paths) is never claimed.
- Registered after .NET. Docs: README, usage, DESIGN (CMake section), bench, `toolchain.md`.
- fmt debug with tests: the build dir went from 157.7 MiB to 52.3 MiB (−66.8%); compress freed
  105.4 MiB, dedupe 2.5 MiB.
- Not done here: the `ccache` hint in `advise`, which reads cargo targets only; Ninja and Meson
  (T32.1).

#### Verified

- `just check` (179 tests) and `just check-cross` pass.
- `tests/cmake.rs`:
  - a build dir next to its source tree is claimed, owned by it, and shows as project gone once
    the source dir is removed;
  - an in-source build, and a build dir holding its source dir, are not claimed;
  - a real Makefiles project with two executables, aged two days: after compress + dedupe,
    `cmake --build` prints no `Building` or `Linking`, both binaries run, and a new source mtime
    makes it build.
- On fmt after `dunnage run`: `cmake --build` builds nothing and 23 of 23 `ctest` tests pass.
- `~/.cache/dunnage` is absent; no build lock files are left in `$TMPDIR`.

### T34. Daemon mode: `dunnage daemon`

Decided by the creator: the tool runs as a CLI and as a daemon, sharing as much code as can be
shared. Design in `docs/architecture.md`, "Process model". The daemon is the same binary in the
foreground, kept alive by the service manager; it never forks itself. Everything it *does* is a
`Session` call (T36) with the `Request` a CLI run would build from the same config. What is its
own: triggers (a slow timer that re-runs discovery, a fast one over known units, and a
filesystem watcher on the top level of known units as one more trigger), per-unit due times
(`last write + min-age`, so a unit is visited once per build, when it has gone cold), and a
state file that `daemon status` reads. There is no IPC: CLI and daemon coordinate through the
run lock and files.

`dunnage daemon install | remove | status` writes and removes the launchd agent or systemd
user unit — low CPU and I/O priority set there, not in code — and replaces the hand-written
plist in `README.md`. A Windows service waits for T21.

Hard requirements: a build never waits for the daemon (`lock_budget` set low by default; the
test starts a build during a daemon pass and bounds how long it blocks); lossy passes run only
when the config enables them; the daemon adds no code path that mutates a build dir. The
watcher crate (`notify` is the candidate) is a new dependency and the creator's call at claim
time; the timers alone are a complete first version. Logging is the observer's events on
stderr, which launchd and journald already collect — no logging crate.

#### Execution plan

Timers only: `notify` is a new dependency, so the watcher waits for the creator's word; the
card allows a first version without it. No signal handling either (it needs a crate or
`unsafe`): a killed run leaves at most `.dunnage-tmp-*` files, which the next run removes.

1. `src/daemon/` in the binary (`cli` feature), every action a `Session` call:
   - `dunnage daemon run [--config] [--index] [--once]`: a foreground loop. The slow timer
     re-runs `eco::discover` over the config's roots; each tick reads every known unit's
     `last_used` and schedules it at `last build + min-age` (the quiet floor for `Quiet` units).
     A unit built since its last visit and past its due time makes one `Session::apply` of the
     config's `Request`, with a low `lock_budget`; busy or interrupted units stay pending. The
     loop sleeps until the next due time, capped by the tick interval.
   - A state file, `daemon.json` next to the index, written atomically: units with last build,
     due and visited times, the last run's per-pass summary and busy units. It survives restarts.
   - `[daemon]` in the config: `interval-secs`, `rediscover-secs`, `lock-budget-secs`.
   - Observer events go to stderr as one line per pass.
2. `dunnage daemon install [--print] | remove | status [--json]`: a launchd agent (`Nice`,
   `LowPriorityIO`, `ProcessType Background`) or a systemd user unit (`Nice`, idle CPU and I/O
   scheduling), loaded with `launchctl` / `systemctl --user`. Windows says it waits for T21.
3. Tests:
   - unit tests for due times and the unit files;
   - `tests/daemon.rs` through the binary on fixture targets, with a temp `HOME`:
     - `--once` applies and records the visit, and a second `--once` finds nothing due;
     - a new build makes the unit due again;
     - a held `.cargo-lock` leaves the unit pending, and the tick returns without waiting;
     - no lossy pass runs unless the config enables it;
     - `install --print` names the binary and `daemon run`.
   `install` without `--print` is never run by a test: it would load a real agent.
4. Docs: README (replace the hand-written plist), usage, DESIGN, architecture.

#### Result

- `src/daemon/mod.rs` (binary, `cli` feature): `dunnage daemon run [--config] [--index]
  [--once]`. Timers only: rediscovery every `rediscover-secs` (6 h), a look at every known unit
  at most every `interval-secs` (10 min), sooner when a unit's due time comes. A unit is due at
  its last build plus `min-age` (the quiet floor for `Guard::Quiet`); any due unit starts one
  `Session::apply` of `Request::from_config` with `lock_budget` 2 s (`lock-budget-secs`). Busy
  units, and every unit of a run that let a group go early, stay pending. The daemon refuses a
  config without `roots`.
- `daemon.json` next to the index, written by rename: units with last build, due and visited
  times, the last run's per-pass counts, busy units and last error. Visits survive restarts.
- `src/daemon/service.rs`: `daemon install [--print] | remove | status [--json]`. A launchd
  agent (`Nice` 10, `LowPriorityIO`, `ProcessType Background`, `ThrottleInterval` 300, log in
  `~/Library/Logs/dunnage.log`) or a systemd user unit (`Nice=19`, idle CPU and I/O scheduling,
  `Restart=on-failure`), loaded with `launchctl bootstrap` / `systemctl --user enable --now`.
  Other platforms get an error naming `daemon run`.
- `[daemon]` in the config. Observer events go to stderr, one line per pass.
- Docs: README (the hand-written plist replaced), usage, DESIGN ("Daemon"), architecture.
- Not done, in `ideas.md`: the `notify` watcher and SIGTERM handling, both new dependencies.
  The `status` command does not print the daemon's state; `daemon status` does.

#### Verified

- `just check` (192 tests) and `just check-cross` pass.
- Unit tests: due and pending logic, the next wake time, no visit after a group let go early, a
  busy unit stays pending, the plist and systemd escaping.
- `tests/daemon.rs`, through the binary with a temp `HOME`:
  - the first `--once` visits both cold targets and dedupe shares the equal artifact; a second
    finds nothing due; a build just now is pending and due at build + 3600; the same build
    gone cold is visited once more, alone;
  - a held `.cargo-lock` is not waited for: that unit stays pending and in `busy`, the other is
    visited, and the next look takes it;
  - only lossless passes run under a config that enables no lossy one;
  - a config without roots is refused; `daemon status` before and after a run;
  - `install --print` names this binary and the config, and writes nothing under `HOME`.
- A build waiting on a group is bounded by `lock_budget`, as `tests/session.rs` checks; the
  daemon sets it. `daemon install` without `--print` was not run: it would load an agent on
  this machine.
- `~/.cache/dunnage` and `~/Library/LaunchAgents/dev.dunnage.daemon.plist` are absent; no build
  lock files are left in `$TMPDIR`.

### T39. Monorepo: grouped `status` and `skip-paths`

One line per target stops being a report at a few dozen build dirs. `status` groups family →
checkout → subtotal per ecosystem, lists the largest build dirs up to a limit and the rest with
`--all`; `--json` stays flat and gains `ecosystem`, `checkout`, `position` and `guard` per build
dir, existing keys unchanged. Config gains `skip-paths` per family — positions as prefixes, so
no glob crate — and a per-family `ecosystems` list narrower than the global one.
Done: `tests/cmd` snapshots for a fixture with many build dirs; a skipped position is neither
reported as work nor touched; `docs/usage.md` documents both.

#### Execution plan

1. The inventory's `Target` gains `checkout` (the nearest dir above the project holding a
   `.git`), `position` (the build dir inside it) and `guard` (`Guard::name` of its first unit);
   `ecosystem`, skipped until now, is serialized.
2. `[family."<dir>"]` gains `skip-paths` and `ecosystems`; `Request` carries them,
   `Request::check` rejects unknown adapter names, and `Request::keeps` drops a skipped build
   dir from the inventory before any pass chooses.
3. `status` groups family → checkout → ecosystem with a subtotal, lists the five largest build
   dirs of each group by position, and the rest with `--all`.
4. Tests on the monorepo fixture, docs.

#### Result

- As planned. There is no global `ecosystems` key yet, so a family's list narrows the registry.
- A skipped build dir is out of the inventory before evict, incremental, orphans and doc choose,
  so it no longer counts toward the `evict` cap. `skip` for a whole family behaves the same way
  and moved to the same filter.
- The status snapshot is a Rust test in `tests/monorepo.rs`, not a `tests/cmd` case. A family
  needs a git repository, and a trycmd fixture cannot commit one. The test compares the whole
  grouped output, with paths replaced.
- Docs: README (`status`, the per-family keys), usage, DESIGN.

#### Verified

- `just check` (196 tests) and `just check-cross` pass. The `status --help` snapshot was
  regenerated for `--all`.
- `tests/monorepo.rs`:
  - JSON keys per build dir;
  - `skip-paths` in every checkout, by whole components, and through a dry run: compress plans
    less;
  - `ecosystems` narrowing, and an unknown name refused;
  - the grouped `status` output with and without `--all`.
- `src/config.rs`: the new keys parse.
- `~/.cache/dunnage` is absent.

### T40. Known build dirs: a persisted inventory

Every run walks the roots to find build dirs, and in a monorepo the cost of that walk is the
source tree, not the build dirs. The daemon holds the list in memory; the CLI starts from
nothing each time. Persist what discovery found next to the hash index — build dir, adapter,
owner, marker stamp — re-validate entries by their markers, and walk only on a slow cadence,
on `--rediscover`, or when a root's own mtime says something moved. Measure first: the task
starts with a walk benchmark on a large checkout and closes with "not needed" if discovery is
already a small share of a settled re-run.

#### Execution plan

Measured first (`docs/bench.md`): a synthetic monorepo of 320,000 empty source files in 3,000
dirs with 10 cargo targets of 2,000 artifacts each, warm cache. A settled re-run from the
monorepo root takes 610 ms; naming the 10 targets as roots, 369 ms. The walk is ~40% of the
re-run: not a small share, so the task goes on.

1. `src/known.rs`: `build-dirs-v1.json` next to the hash index — the canonical roots, when they
   were walked, each root's mtime, and every build dir with its adapter. It is used when the roots
   are the same, the list is younger than `[discovery] every-secs` (default 3600; 0 always walks)
   and no root's mtime moved. Each entry is re-validated by its adapter's `claim` (its marker), so
   a removed build dir drops out without a walk. It is written by temp file and rename.
2. `inventory::inventory` splits into the walk and `inventory_of(found)`. `plan` and `apply` take
   the list through it; `status`, `advise` and `seed` keep walking. `Request::rediscover`
   (`run --rediscover`) forces the walk. `RunReport::walked` says which happened, and the CLI
   prints a line when the list was used.
3. Tests: a new build dir is not seen until the walk (`--rediscover`, cadence, root mtime); a
   removed one drops out; other roots walk. Then the benchmark again.

#### Result

- As planned. `src/known.rs` holds the list; `Settings::rediscover_every` comes from
  `[discovery] every-secs`, and a session without an index path walks every run.
- The daemon walks the roots on its own cadence for scheduling. After each of its walks it sets
  `rediscover` for the next run, so a unit it found is never marked visited by a run that used
  an older list and did not see it.
- Re-run of the settled synthetic monorepo: 591 ± 34 ms with the walk, 311 ± 8 ms from the
  list, against 369 ± 39 ms naming the 10 targets by hand.
- Docs: usage (`--rediscover`, `[discovery]`), DESIGN "Known build dirs", architecture, bench
  "Discovery in a monorepo".

#### Verified

- `just check` (199 tests) and `just check-cross` pass.
- `tests/known.rs`:
  - a new build dir below the root's top level waits for the walk, and `--rediscover` finds it;
  - a removed one drops out without a walk;
  - other roots, a moved root mtime and cadence 0 walk;
  - the list holds for the cadence and no longer.
- The benchmark ran with `HOME` and the index in a temp dir. `~/.cache/dunnage` is absent, and
  no `_.build.lock` is left in `$TMPDIR`.

### T35. Go: `GOCACHE` and `GOMODCACHE`

Lowest priority in the plan. Go has no per-project build dir: `GOCACHE` is one content-addressed
store the `go` command trims on its own, so `dedupe`, `seed`, `orphans` and `evict` have nothing
to do (`docs/ecosystems.md`). What is left is `compress`, through T33's mode: `GOCACHE` found by
`go env GOCACHE`, and `GOMODCACHE` as the counterpart of `--cargo-home` — extracted sources,
which went down 69% for cargo. `GOMODCACHE` dirs are read-only on purpose; lifting and restoring
directory modes is the risk the spike has to price, and "leave `GOMODCACHE` alone" is an
acceptable outcome. Oracle: `go build ./...` after the pass reports every package cached
(`go build -x` runs no compile step) and `go mod verify` is green.

#### Execution plan

Spike, on a copy of the machine's `GOMODCACHE` (146 MiB, 24 modules) in a temp dir: `--store`
on it today skips every file with `PermissionDenied` and leaves nothing behind. With the dirs'
owner write bit lifted and put back, compress takes it to 106 MiB (−28%); every file keeps its
bytes, mode and mtime, only dir mtimes move (the rename). Every module's `h1:` dir hash still
matches its `.ziphash`, and a module built against the copy builds, rebuilds with no compile
step and passes its tests. So the task goes on:

1. `Ecosystem::lifts_read_only_dirs` (default false). `engine::apply_compress` lifts the owner
   write bit of a read-only dir holding a member for the length of one batch and puts the mode
   back after it; a mode it cannot put back fails the run. Unix only.
2. `src/eco/go.rs`: the `GoModCache` adapter, `Guard::Immutable`, compress only, lifting; its
   units are the module cache minus `cache/` (zips and VCS clones). `Request::go_modcache`, a
   group of its own like the cargo home, refused when there is no `cache/download` in it.
3. `run --go`: `go env GOCACHE GOMODCACHE` in the front end; `GOCACHE` becomes a store,
   `GOMODCACHE` the module cache group.
4. Tests: a fake read-only module cache keeps modes, mtimes and bytes, and no dir stays
   writable; where `go` is installed, a real module downloaded offline from a fake proxy dir
   verifies and rebuilds with no compile. Bench and docs.

#### Result

- As planned. `run --go` runs `go env GOCACHE GOMODCACHE` in the binary; `GOCACHE=off` adds
  no store. The module cache is a group after the stores, `compress` only, under
  `Guard::Immutable`.
- The lift is per batch, not per file: the backend compresses a whole batch of temp copies at
  once, and the copies and the renames both need the dir writable. A mode that cannot be put
  back fails the run and names the dir.
- Found on the way, fixed in T40's commit: a run of only `--store` walked "no roots" and
  printed that it used the list of the last walk.
- Docs: README, usage, DESIGN "Go module cache", ecosystems, architecture, bench "A Go module
  cache", toolchain (`zip` for the test).

#### Verified

- `just check` (203 tests) and `just check-cross` pass.
- `tests/go.rs`:
  - a fake read-only module cache keeps every file's bytes, mode and mtime and every dir's mode,
    and gets no leftovers;
  - an adapter that does not lift fails every file with `PermissionDenied` and changes nothing;
  - `go::check` refuses a dir without `cache/download`;
  - with `go` and `zip` installed, `run --go` on a module from an offline proxy dir compresses
    it; afterwards `go mod verify` is green and a rebuild runs no `compile` step.
- The bench and the spike ran on a copy of the real module cache in a temp dir. The real one was
  only read. `~/.cache/dunnage` is absent, and no `_.build.lock` is left in `$TMPDIR`.

### T32.1. C and C++: Ninja and Meson

Split off from T32, which handles CMake build dirs and was verified with the Makefiles
generator only, because `ninja` and `meson` are not installed here. Settle whether current
Ninja takes a lock on the build dir, claim Meson build dirs (`meson-private/`, whose
`coredata.dat` records the source dir), and add the oracle `ninja -n` plans nothing after a
pass. The creator approved installing `ninja` and `meson`; both are in the global mise config
(ninja 1.13.2, meson 1.12.0).

#### Execution plan

1. Ninja's lock: a rule that sleeps, `lsof` on the running `ninja`, and a second `ninja` in the
   same dir started meanwhile. If nothing is held, CMake+Ninja stays under the quiet tier.
2. Meson: `meson setup` a small C project, read what it writes. Claim a dir holding
   `meson-private/coredata.dat` and `meson-info/meson-info.json`; the owner is the source dir
   that JSON names. `src/eco/meson.rs`, `Guard::Quiet`, tools `meson`, `ninja`, `samu`.
3. Tests in `tests/cmake.rs` and a new `tests/meson.rs`: fixtures for the claim and the owner,
   then real CMake+Ninja and Meson builds where the tools exist, aged, a run, and the oracle
   `ninja -n` reports no work. Docs, toolchain.

#### Result

- Ninja 1.13.2 takes no lock: during a slow rule `lsof` shows nothing of the build dir held
  open, and a second `ninja` in the same dir runs alongside. CMake+Ninja dirs stay quiet.
- `src/eco/meson.rs`, registered after CMake: the claim needs `meson-private/coredata.dat` and
  `meson-info/meson-info.json`, the owner is the JSON's source dir. Meson's `CACHEDIR.TAG` does
  not fool the cargo adapter, which wants the words "created by cargo".
- A bare Ninja dir (GN's) is not claimed; `docs/ecosystems.md` says so.
- Docs: README, usage, DESIGN "CMake" and "Meson", ecosystems, architecture, toolchain (ninja
  and meson through the global mise config).

#### Verified

- `just check` (207 tests) and `just check-cross` pass.
- `tests/cmake.rs`: the Ninja generator after compress and dedupe, where `ninja -n` reports no
  work and the binaries run; a touched source is planned again.
- `tests/meson.rs`: claim, owner and `project_gone` from fixtures; a dir without coredata or
  info, or holding its source, is refused; a real Meson project with the same oracle.
- `~/.cache/dunnage` is absent, and no `_.build.lock` is left in `$TMPDIR`.

### T30.1. Swift: Xcode DerivedData

Split off from T30. `~/Library/Developer/Xcode/DerivedData/<name>-<hash>/`, with `info.plist`
recording `WorkspacePath`, which makes `orphans` and `evict` direct. No lock: needs the quiet
tier (T29), with `xcodebuild`, `XCBBuildService` and `SWBBuildService` as the tools. `plutil`
reads the plist without a new dependency. Needs a way to produce a DerivedData dir for tests
without writing into the real `~/Library` (`xcodebuild -derivedDataPath` in a temp dir is the
candidate; whether it writes `info.plist` there is the first thing to check). Oracle: a second
`xcodebuild` compiles nothing.

The creator approved testing on DerivedData. Only `-derivedDataPath` dirs in temp dirs are
written; the real `~/Library/Developer/Xcode/DerivedData` is read at most.

#### Execution plan

1. Spike: `xcodebuild` on a Swift package in a temp dir with `-derivedDataPath`; what it
   writes (`info.plist`, `WorkspacePath`, `LastAccessedDate`), which processes run, and whether
   a second build is a no-op.
2. `src/eco/xcode.rs`: claim a dir holding `info.plist` with `WorkspacePath` next to `Build/`
   or `Logs/`; owner from the plist through `plutil`; `Guard::Quiet`, tools `xcodebuild`,
   `XCBBuildService`, `SWBBuildService`; `well_known` or a documented root for
   `~/Library/Developer/Xcode/DerivedData`, decided by what the spike shows.
3. Tests: fixtures for claim and owner; where `xcodebuild` exists, a real package built into a
   temp DerivedData, aged, a run, and a second `xcodebuild` compiles nothing. Docs, toolchain.

#### Result

- `xcodebuild -derivedDataPath` in a temp dir writes an XML `info.plist` with `WorkspacePath`
  and `LastAccessedDate` next to `Build/`, `Logs/` and the caches; nothing lands in the real
  `~/Library`. A second build prints no compile or link step.
- Xcode takes no lock. `SWBBuildService` works from a dir inside Xcode.app and holds `build.db`
  and the compilation cache open, so a current-dir check alone misses it: `sys::Held` now
  carries open files too, and on macOS a file inside a quiet unit held open by one of the tools
  makes the unit busy. Linux still reads current dirs only.
- `src/eco/xcode.rs`, registered last: claim needs `info.plist` with a `WorkspacePath` outside
  the dir and `Build/` or `Logs/`; a binary plist goes through `/usr/bin/plutil`; the manifest
  is the project or workspace bundle, or `Package.swift`. Tools `xcodebuild`, `SWBBuildService`,
  `XCBBuildService`, `Xcode`; clones only. Not in `well_known`: DerivedData is a root the user
  names, as the library reads no home dir.
- The build database records output mtimes: moving an entry's files back relinks, and switching
  Debug and Release recompiles a few files, with or without the tool.
- Bench on swift-argument-parser, Debug and Release: 319.9 MiB to 221.5 MiB (−30.7%), 127 MiB
  of files rewritten while the builds settled stayed under the one-day floor.
- Docs: README, usage, DESIGN "Xcode" and the no-lock tier, ecosystems, architecture, bench,
  toolchain (xcodebuild, plutil).

#### Verified

- `just check` (213 tests) and `just check-cross` pass.
- `tests/xcode.rs`: claim, owner, manifest and `project_gone` from fixtures; a plist alone, a
  workspace inside the dir and a plist without `WorkspacePath` are refused; `tail -f` under a
  tool's name holding a file open makes the entry busy, and without it the entry is worked on;
  a real package built into a temp DerivedData, after compress and dedupe, builds nothing and
  its binary runs.
- The bench entry: `xcodebuild` afterwards compiles and links nothing, and `math` runs from both
  configurations.
- `~/.cache/dunnage` is absent, no `_.build.lock` is left in `$TMPDIR`, and the real DerivedData
  gained nothing.
