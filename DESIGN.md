# swarfr — design

Swarf: the chips and shavings left after machining — waste that is not the product. Much of
`target/` is swarf. The project was called `cargo-tare` until T42 and `dunnage` until T46;
`done.md` and `docs/spike/` keep the old names.

`swarfr` shrinks Cargo build directories without slowing builds down, by combining several
independent approaches in one planner instead of chaining separate tools. Measurements behind
every choice are in `docs/research.md`.

## Goals

- Cut on-disk size of many `target/` dirs (the measured case: 44 dirs, ~158 GB; 30 of them,
  113.9 GB, sit in worktree checkouts that git no longer registers and were last built a week
  earlier; the rest are live and built daily).
- Never make cargo rebuild anything it would not have rebuilt anyway.
- Make new worktrees cheap in both disk and build time.
- Be independent of cargo's internal directory layout.
- Re-runs after a build take seconds, not minutes.

## Non-goals

- Cleaning `~/.cargo` (cargo does it itself since 1.88; `cargo-cache` / `cargo-trim` exist).
- Speeding up compilation itself (sccache, hakari, profile tuning are orthogonal; `advise` only
  points at them).
- Replacing cargo features once they ship (target GC, per-user cache): overlapping passes retire.

## What the measurements say

| Approach | Measured size effect | Build-time cost | Decision |
| --- | --- | --- | --- |
| Age-based sweep in active targets | ~50 MB of 14 GB | none | not a pass; useless here |
| Transparent APFS compression | deps to ~20–30% of logical size | small CPU on read | **core pass** |
| Content dedupe via `clonefile` | 36% across sibling targets, 33% inside one `deps/`, 3% for a diverged worktree | none | **core pass** |
| Clone-seeding a new worktree's target | 0 bytes until divergence | removes the third-party rebuild | **core command** |
| Whole-dir removal of orphaned / idle targets | **113.9 GB of ~158 GB** (30 unregistered worktree checkouts, idle 6–7 days) | rebuild on return | opt-in pass, highest payoff on the measured machine |
| Unit-level prune (cargo-gc style) | unmeasured (~3 GB of incremental variants seen) | needs a build | roadmap, needs layout v2 |
| Shared `build-dir` per repo | est. 20–35% | parallel agents serialize on one lock; workspace members get the same unit hash in every worktree (T2), so worktrees overwrite each other's member artifacts | `advise` only; automation on roadmap |
| Symlinked / shared `target` | same as shared `target-dir` | one lock, binaries overwrite each other | rejected |

## Core model

1. **The unit of work is an inode, not a path.** A single target already holds ~71k hardlinked
   files (rustc links `incremental/` ↔ `deps/` ↔ profile root). Every mutation replaces an inode
   for *all* of its paths or not at all. A group whose link count exceeds the paths we found is
   skipped.
2. **Layout independence.** Passes look at directories, inodes and content. No pass parses
   `name-<hash>` file names, so `target-dir`, `build-dir` and build-dir layout v2 all work.
   The only layout knowledge: a cargo `CACHEDIR.TAG` (checked by its cargo-specific text, because
   gradle / uv / huggingface caches carry the tag too) and profile dirs holding `.cargo-lock`.
3. **Families.** Target dirs whose projects share a git common dir (worktrees) or a remote form a
   family. Dedupe candidates and seeding sources are searched inside a family first, which is where
   the measured duplication is.
4. **Lossless vs lossy.** `seed`, `dedupe`, `compress` never lose data and are on by default.
   `orphans`, `evict` (and later `prune`) delete rebuildable data and are off until configured.

## Pipeline

`scan → plan → apply → report`. One walk, one model, one lock acquisition per target.

| Order | Pass | Kind | What it does |
| --- | --- | --- | --- |
| 0 | `seed` (command, at worktree creation) | lossless | clone a family member's target into the new worktree |
| 1 | `orphans` | lossy | targets whose project dir / worktree is gone or whose branch is merged |
| 2 | `evict` | lossy | whole profile dirs idle for N days; least-recently-built first until under a global size cap |
| 3 | `incremental` | lossy | the `incremental/` cache of profile dirs idle for N days |
| 4 | `doc` | lossy | `<target>/doc`, what `cargo doc` writes and no build reads |
| 5 | `prune` | lossy | roadmap: units the current build graph no longer references |
| 6 | `dedupe` + `compress` (fused) | lossless | see below |
| 7 | `report` | — | bytes before / after per pass, per target, JSON or table |

Ordering rule: delete first so lossless passes never hash or compress bytes that are about to
disappear; lossless passes last so they see the final set of inodes.

One round of the passes does not always reach a fixed point: dedupe's clones are files compress
never saw (46 actions for a second run on the benchmark workspace, `docs/bench.md`). So `run`
repeats the rounds on a group, under the locks it already holds, until a round applies nothing
(`Options::until_settled`, at most 8 rounds; the stop flag and the lock budget end it too). The
test is "applied nothing", not "planned nothing": a group that is skipped for good — a file that
will not compress, a hardlink from outside — is planned again by every round. Counts are summed
over the rounds; a later round adds what it applied and what it skipped for the first time.
`--dry-run` is one round, since it changes nothing a second one could see.

### Why one tool beats a chain of tools

- **Fused dedupe + compress.** Chained tools fight: compressing after dedupe rewrites each clone
  and un-shares it; deduping after compressing re-hashes everything on every run. The planner
  instead groups inodes by content, compresses **one** canonical inode per group, then clones the
  compressed canonical over the other members. T2 confirmed both halves: compressing a clone
  *increases* usage (+16 MB on a 52 MB target, the rewrite un-shares it), while cloning a
  compressed file shares blocks and keeps the compressed flag (−14.5 MB of −16 MB expected).
- **Shared hash index.** Content hashes are cached by `(device, inode, size, mtime)`. After a
  build only new inodes are hashed. Hashing is also skipped for any file whose size is unique in
  its family (size-bucket prefilter).
- **Hot-file filter.** Inodes younger than `min-age` (default 1 h) are left alone. Workspace crates
  and incremental state are rewritten by the next build; compressing them is wasted CPU. Third-party
  artifacts are stable for weeks and carry most of the bytes.
- **One inventory, one lock.** Every pass reads the same scan and mutates under the same cargo
  lock, so a build never observes a half-applied state.

### Pass details

**compress** — per-file transparent APFS compression (decmpfs). Skips files that are already
compressed, smaller than 8 KB, or younger than `min-age`. Backend: the `applesauce` library
(LZFSE). In T2 its CLI kept all hardlink groups, mtimes and modes and left every unit fresh.
`ditto --hfsCompression` compressed nothing on the test volume and is not used.

**dedupe** — `clonefile(2)` of the canonical inode to a temp name in the same directory, restore
the member's mtime / mode / flags, then `rename(2)` over each path of the member's hardlink group.
Clones are copy-on-write, so a later in-place write by rustc cannot leak into siblings.

**seed** — `swarfr seed --from <worktree> [<new-worktree>]`, or automatic source selection
inside the family (largest recently built target). Recursive clone of the target dir, excluding
`incremental/` and lock files. Verified in T2: registry dependencies are fresh in the new
worktree, only workspace members rebuild (their sources have new mtimes; their unit hashes are
the same in every worktree). Preserving mtimes during the clone is not required. Seeding from an
already compressed target keeps both the sharing and the compression.

**orphans / evict** — operate on whole target or profile dirs only, which is layout-proof and
needs no knowledge of units. "Last built" is the newest mtime among the profile dir's top-level
entries.

## Worktrees: clone-seed vs symlink vs shared build-dir

| | Symlinked `target` | Shared `build-dir` (cargo ≥ 1.91) | Per-unit symlinks into a store | Clone-seed + dedupe |
| --- | --- | --- | --- | --- |
| Third-party deps built once | yes | yes | yes | yes |
| Disk shared | yes | yes | yes | yes (CoW) |
| Build lock | one for all worktrees | one for all worktrees | per worktree | per worktree |
| Final binaries collide | **yes** — wrong binary gets tested | no | no | no |
| Cross-worktree corruption | possible | cargo-managed | **yes**, cargo writes through the link | impossible |
| Needs | nothing | nothing | layout v2 | APFS / reflink fs |
| `cargo clean` blast radius | everything | everything shared | one worktree, store leaks | one worktree |

Decision: clone-seed + dedupe is the default on reflink filesystems. Shared `build-dir` is offered
by `advise` for users who do not build in parallel. Symlink modes are kept only as a roadmap item
for filesystems without reflinks.

## Safety invariants

1. Hold cargo's own lock (`<profile>/.cargo-lock`, exclusive `flock`) while mutating a profile dir;
   `try_lock`, and skip the dir when a build holds it. Cross-target operations take locks in sorted
   path order.
2. Re-check `(size, mtime)` of source and member immediately before replacing; any change aborts
   that group.
3. Replacement is always temp-file + `rename` inside the same directory; a crash leaves either the
   old or the new file, plus at most a `.swarfr-tmp-*` file that the next run removes.
4. mtime and mode of every replaced path are preserved. T2: a workspace-member rlib with a new
   mtime makes its dependents rebuild; registry artifacts are not mtime-checked, but the rule is
   applied to everything. BSD flags: the compressed flag follows the content (a clone of a
   compressed file is compressed); an inode carrying any other flag is skipped, not rewritten.
5. Lossy passes never run unless enabled in config or by flag, and always support `--dry-run`.
6. Never follow symlinks out of a target dir; never cross a device boundary.

## Safety tier without a build lock

Invariant 1 needs a lock the build holds while it writes. Cargo has one; Make, Ninja, MSBuild
and Xcode hold nothing an outsider can test. An adapter says so with `Guard::Quiet`, and its
units get a weaker tier, named as such in every report (`no build lock, weaker checks` in the
table, `quiet` per group in `--json`):

1. **Process check.** `Ecosystem::tools` names the build tool's processes. A unit is busy —
   skipped, exit code 2 — when one of them has its current dir in the unit, below it, or in a
   dir around it (`make` in the project root builds into `build/`); a process in a filesystem
   root counts for nothing. macOS reads `ps` and `lsof`, Linux `/proc`. Where the check cannot
   run — Windows, or an adapter that names no tool — the unit is *unsure*.
2. **Age floor.** Files younger than `engine::QUIET_MIN_AGE` (one day) are left out of the model,
   whatever a pass's own `min-age` says. A unit that had any is unsure.
3. **Busy files.** A file found in use while it is replaced — `ETXTBSY`, `EBUSY`, a Windows
   sharing or lock violation — is skipped as `Busy` and its unit is reported busy (exit code 2)
   instead of failed. This holds for every guard.
4. **Lossy passes** skip an unsure unit (`Unsure`); `RemoveTarget` skips a build dir holding
   one.
5. **Invariants 2–6 hold unchanged**, and one check is added for every guard: the source of a
   clone is stamped again after the copy, so bytes written into it during the copy never
   land under the member's names.
6. Two runs of the tool itself — the daemon and a manual one — are kept apart by the session's
   run lock, not by this tier.

What the tier does **not** promise:

- It does not close the race, it narrows it. A build that opens a member between the last
  `(size, mtime)` check and the `rename` writes into the unlinked old inode, and those bytes are
  lost; the next build rebuilds the file. `tests/quiet.rs` rewrites a file during a dedupe run
  and ends with the newer bytes, which shows the window is small, not that it is closed.
- A build of another user, in a container, on another machine over a network filesystem, or
  under a process name the adapter does not list, is not seen.
- A build that starts after the process check is not seen either; only the age floor and the
  per-file checks stand between it and a lossy pass that is already removing.
- A build tool that writes a file and leaves its mtime in the past (an extracted archive, a copy
  that keeps times) gets past the age floor.

**Freshness oracle** (the acceptance test for every lossless pass): build a fixture workspace, run
the pass, then `cargo build --message-format=json` must report every unit as `fresh` and
`cargo test` must pass. Size is measured in allocated blocks (`st_blocks`), not logical length.

The harness is `tests/common/mod.rs`, shared by every integration test:

- `Fixture` — a workspace in a temp dir: a bin + lib package with a build script that writes into
  `OUT_DIR`, unit and integration tests, a proc-macro member, and a path dependency outside the
  workspace standing in for a third-party crate. It builds `--offline`, so tests need no network;
  a registry crate differs only in how cargo fingerprints its sources, which no pass touches.
  One source tree can be built into several target dirs (sibling targets for dedupe).
- `Fixture::stale_units` / `assert_fresh` — the oracle: the JSON messages of `cargo build` and
  `cargo test --no-run` must all say `fresh`, then `cargo test` and the binary must succeed.
- `allocated_bytes` — size of a dir the way the engine counts it, every inode once.
- `run_unbusy` — repeats an engine run that reads as busy because of the fork race between test
  threads (see "Engine") and sums the attempts: with several dirs, an attempt that found one busy
  has still worked on the others. The race is real — about one in seven rounds of
  `tests/compress.rs` hit it — and cannot happen in the tool itself, which spawns no processes.

`tests/harness.rs` proves the oracle can fail: after the mtimes of the `.rlib` files are bumped,
it reports stale units. A new lossless pass gets one test of the shape "build the fixture, run the
pass, `assert_fresh`". CLI tests follow `rust.md`: full output in `tests/cmd/*.trycmd`, exit codes
and messages with paths through `assert_cmd` + `predicates` in `tests/cli.rs`.

## Engine (`src/engine.rs`, `src/model.rs`)

`engine::run(profile_dirs, passes, options)` is the only code that mutates a target.

1. **Lock.** Sort and dedupe the profile dirs, `try_lock` each `.cargo-lock`. A dir held by a
   build goes to `report.busy` and is not even scanned. Locks live until `run` returns.
2. **Scan.** `model::scan` turns each locked profile dir into `Inode`s: `Stamp`
   (`dev`, `ino`, `size`, `mtime`), mode, flags, link count, allocated bytes and every path found.
   Symlinks are not followed, other devices are not entered, `.cargo-lock` is left out, and
   `.swarfr-tmp-*` leftovers are collected and removed (not on `--dry-run`).
3. **Plan.** Each `Pass` gets the scanned profiles and returns `Action`s without touching the
   disk. A lossy pass is asked only when named in `Options::lossy`. After a pass that applied
   anything the profiles are rescanned, so the next pass sees the new inodes.
4. **Apply.** `Action::Replace { source, source_stamp, member }` swaps every path of `member` for
   a copy-on-write clone of `source`: clone to a temp name next to the first path, restore the
   member's mtime and mode, `rename` over it; every other path gets a hardlink to the new inode
   through its own temp name and `rename`, so the group stays one inode. The engine trusts the
   pass that the bytes are equal and checks everything else.
5. **Report.** Per pass: planned and applied groups, allocated bytes (an upper bound on the
   saving), and every skipped group with its reason.

A group is skipped, never half-done, when: a path is outside the locked profile dirs
(`Unlocked`), the inode has links we did not find (`ForeignLinks`), it carries flags other than
the compressed one (`Flags`), source and member differ in device or size or are the same inode,
any stamp differs from the scan (`Changed`), or an I/O call fails (`Failed`). A crash in the
middle of a group leaves each path on the old or the new inode; both hold the same bytes, and the
next run joins them again.

`eco::cargo::profile_dirs` maps a target dir to its profile dirs and refuses a dir whose
`CACHEDIR.TAG` was not written by cargo.

## Adapter boundary (`src/eco/`)

Everything that knows a build system by name is under `src/eco/`, the way everything that
knows a platform is under `src/sys/`. The engine, the inode model, the index, `seed` and the
generic passes ask through `eco::Ecosystem`: `claim` (is this dir a build dir), `owner` (the
project it was built from), `build_dir` (where a project's build goes, for `seed`), `units`,
`guard`, `private` (never scanned, never copied: `.cargo-lock`), `volatile` (left behind by
`seed`: `incremental/`), `last_used` and `policy` (how dedupe may share). `eco::discover` is the
one walk: every dir is offered to the registry in order, the first claim wins, a claimed dir is
not entered and `.git` is never entered — so a CMake dir or a whole cargo target that a build
script left inside a target is nobody's.

The engine takes the adapter instead of a lock kind and locks what `guard` names:
`Guard::Lock(file)` per unit, `Guard::Shared(file)` once for every unit under it, and nothing for
`Guard::Quiet`, which gets the tier of "Safety tier without a build lock", or for
`Guard::Immutable` ("Immutable stores"). `Held` (an embedding caller holds the build's lock, R8)
is in the enum and refused until its task. `model::scan` records the unit's `last_used` at scan time, so
`evict` and `incremental` re-check a unit's last build under its lock without asking cargo.
Family and orphan status come from the owner's project, not from the build dir's parents. For
cargo the owner is the dir above the target when it holds a `Cargo.toml`. A target moved out of
its checkout (`CARGO_TARGET_DIR`, `build.target-dir`) is owned by the workspace its dep-info
names (`eco::cargo::depinfo`): cargo's `<profile>/*.d` give member sources by absolute path,
rustc's `<profile>/deps/*.d` give the same files relative to the workspace root, and the
absolute path minus the relative tail is the root. A relative path alone is ambiguous
(`src/lib.rs` ends every package's path), so the root must explain a path of every rustc file
that pairs at all, and none of rustc's absolute sources may lie under it — cargo passes every
path package under the root relative to it. Several roots (a target shared by checkouts): an
existing one, so the target is an orphan only once all are gone; roots in different
repositories: no owner. No dep-info names a root — a target only `cargo check` wrote, or
`build.dep-info-basedir` — and the dir above stays the owner. A `build.build-dir` records no
absolute path to its workspace in text at all (`ideas.md`).

`eco::cargo::Cargo` is the only registered adapter; `eco::cargo::home::Home` is the cargo home,
never discovered, named by a run, guarded by `Guard::Shared(.package-cache)`. Cargo's own passes
and reports (`incremental`, `doc`, `toolchains`, `advise`, the home's stats) live under
`src/eco/cargo/` too; the session still wires them by name, and the inventory still carries
their cargo-shaped fields, until the status report is per ecosystem.

After a group is replaced the engine calls `Pass::replaced(replace, new_stamp)`, so a pass can
keep its own bookkeeping about the inode that now sits at the member's paths.

`engine::run_with` takes an `Interrupt`: a stop flag and a deadline, looked at before every
action and every compress batch. When either says so the run stops starting new work, releases
its locks and says why in `Report::interrupted`. Every action is whole, so what it leaves behind
is old or new, never half of either.

## Session (`src/session.rs`)

The run above one engine call, shared by every front end: the CLI and the daemon (`DESIGN.md`, "Daemon"), an
embedding build system later (R8). `main.rs` only parses flags, merges them over the config into
a `Request`, prints what the session returns and picks the exit code.

- `Session::open(Settings)` — `Settings` names the hash index; the run lock `run.lock` sits
  next to it. The library reads no environment and no config file on its own:
  `session::default_index`, `config::default_path` and `cargo_home::path` resolve them for a
  front end that wants the defaults.
- `inventory` and `advise` only read and take no lock.
- `plan` (a dry run) and `apply` check the `Request` (pass names, each lossy pass with its
  threshold), read the inventory, then take the run lock, load the index, choose what the lossy
  passes take, and run the engine once per group — per family, or one group across families —
  plus the cargo home under its own lock. The index is saved before the lock is released.
  `seed` takes the same lock. A second session gets `Error::RunLockHeld` and the CLI exits 2:
  a manual run and the daemon coordinate through that file, without IPC.
- `Control` steers a run from outside: an `Observer` hears each group before and after (the CLI
  prints its table from there), `stop` ends the run between two actions, and `lock_budget`
  bounds how long a group's build locks are held — a group out of budget lets go, so a build
  waiting on its lock gets it, and is visited once more after the other groups; what is still
  left then counts as busy. The CLI sets neither.
- One `Error` type (`src/error.rs`). `anyhow` and `clap` belong to the binary behind the default
  `cli` feature; `cargo check --lib --no-default-features` is part of `just check`.

## Dedupe pass and hash index (`src/dedupe.rs`, `src/index.rs`)

Works across every profile dir given to one run; give two worktrees' targets together to share
files between them.

1. **Filter.** An inode takes part when it is at least `min-size` (default 4096 bytes, one APFS
   block), at least `min-age` old (default 1 h), has no links outside its profile dir and no
   flags other than the compressed one. The same rule holds for a source.
2. **Size buckets.** Inodes are bucketed by `(device, size)`. A size that occurs once has no twin
   and is never read; on a real target this removes most of the I/O.
3. **Hash.** SHA-256 of the remaining inodes, in parallel (`rayon`), the index first. Reading a
   transparently compressed file yields its plain bytes, so a compressed and a plain copy match.
4. **Group and choose.** Equal `(device, hash)` forms a group. The canonical inode is, in order:
   one already marked shared, one that is compressed (its clones stay compressed — this is the
   fusion with the compress pass, which only has to run first and compress canonicals and unique
   files), the oldest, the first path. Every other inode of the group that is not marked shared
   becomes a `Replace` action.
5. **Remember.** In `replaced` the source is marked shared, the member's old entry is dropped and
   the new inode is stored with the known hash and the shared mark — no rehash, and the next run
   plans nothing for it.

### Sharing without copy-on-write (`T22`)

A `Replace` carries *how* it is to be shared, and the pass decides which:

| | `Share::Clone` | `Share::Link` |
| --- | --- | --- |
| what it is | a copy-on-write clone | one inode under every name |
| needs | a filesystem with reflinks | nothing |
| a later rewrite of one name | touches that name only | rewrites every other name too |
| where it is used | wherever `caps.clone` is true | only where it cannot be, and only on safe files |

The danger is specific and worth naming: rustc opens its output files with truncate, so it
rewrites an artifact's inode in place. Cargo's own hardlinks (`target/debug/fx` to
`deps/fx-<hash>`) are safe because cargo replaces the *name* — ours would not be. So the
fallback is split by what the file is, not by what the filesystem allows:

- **Cargo home sources** are linked with no flag: cargo extracts a crate into a fresh directory
  and writes `.cargo-ok` last, and never rewrites an extracted file in place. `run --cargo-home`
  therefore runs `dedupe` beside `compress`, with `link_fallback` on.
- **Build artifacts** need `--link-artifacts`, whose help text carries the hazard, and the run
  prints it again on stderr while it works.

Two rules the engine keeps whatever the pass asked for. It refuses to link files whose modes
differ (`Skip::ModeMismatch`) — one inode holds one mode, and silently changing the other name's
permissions is not a saving. And the shared inode keeps the **later** of the two modification
times, because a name that suddenly reads older than the files it was built from is a name cargo
rebuilds: the pass would then cost a build instead of saving space. The source's own mtime moves
forward with it, which is the safe direction.

On a filesystem that clones, none of this happens and `--link-artifacts` changes nothing: a
clone is better and needs no permission.

The **index** maps `(device, inode)` to `(size, mtime, hash, shared, seen)`; a lookup with a different
size or mtime misses, so a rewritten file is rehashed and loses its shared mark. It is one flat
file of fixed little-endian records behind a magic string (`~/.cache/swarfr/hashes-v1.bin`,
`--index` to override), written through a temp file and `rename`, saved on `--dry-run` too. It is
only a cache: a missing, truncated or foreign file reads as empty, and so does one of an older
format (the magic's digit), which the next save replaces. `seen` is when a run last hit the
entry; on save, entries idle longer than `[index] idle-days` (default 30) are dropped. Those are
the inodes of targets that were deleted or rebuilt, or of files no pass needs to hash any more,
so the file does not grow for as long as the machine lives; dropping one still wanted costs one
rehash of that file.

Why the `shared` mark exists: APFS cannot be asked whether two files share blocks, and a clone is
a different inode with equal content — without the mark every run would clone everything again
and report savings that are not there. Known limits: two clusters shared by separate runs are not
merged with each other; a target seeded by `cp -c` or `swarfr seed` is unknown to the index
and is cloned once more on its first run (T8 can register seeded inodes); losing the index costs
one full rehash and one redundant round of cloning; like cargo itself, the index trusts
`(size, mtime)`, so a file rewritten with the same size within the same nanosecond timestamp
would keep a stale hash.

## Compress pass (`src/compress.rs`)

The backend never sees a live file. `applesauce` refuses files with more than one link and
replaces the inode it compresses — both wrong inside a target, where final artifacts and all of
`incremental/` are hardlink groups. So compression is an engine action, `Action::Compress(inode)`:

1. The engine checks the group as for a replacement (locked paths, no foreign links, no flags but
   the compressed one, stamps unchanged) and clones its first path into a sibling temp. A clone
   costs no space.
2. Up to 256 such private copies go to `Pass::compress` in one call; the backend compresses them
   in parallel (LZFSE, level 5, keep only below 95% of the size, verify by reading back).
3. A copy that came back with the compressed flag and the same length is swapped in after the
   group's stamps are checked once more: mtime and mode restored by the engine, `rename` over the
   first path, `hard_link` + `rename` for the others — the group stays one inode. Any other copy
   is removed and the group reads `NotCompressed`; the backend's reason is kept in
   `Compress::notes`.
4. `Pass::rewritten(old, new)` tells every pass that the content moved to a new, unshared inode;
   dedupe moves its index entry, so the file is not hashed again.

The pass plans inodes of at least 8 KB, older than `min-age`, with no flags (so not compressed
yet) and not marked shared in the hash index: compressing a clone un-shares it. It runs before
dedupe, which prefers a compressed canonical, so its clones are compressed too — after one run a
family holds one compressed copy of each file. `freed_bytes` is allocated bytes before minus
after, measured, not estimated.

Known limits: duplicates are all compressed before dedupe folds them, which costs CPU, not
space; a cluster that was shared while uncompressed (a run with only dedupe) stays uncompressed;
a copy the backend refuses is tried again on every run; a file whose mode denies its owner
reading or writing ends as `NotCompressed` or `Failed`.

## Seed command (`src/seed.rs`)

Not a pass: it runs on its own, before there is anything to shrink.

- **Source choice.** `--from` names a checkout or a target dir. Without it, `choose` takes the
  git common dir of the destination (`inventory::family`), asks `inventory::checkouts` for every
  checkout registered under it (the repository itself and each `worktrees/<name>/gitdir`), looks
  in each one at the same relative path the destination has inside its own checkout — a
  workspace can sit anywhere in a repository — and takes the target built most recently.
- **Every position.** In a checkout root with no `--from`, `positions` runs the shared walk
  (`eco::discover`) over each sibling checkout and keeps the build dirs that sit at their
  adapter's default place (`build_dir(owner) == dir`) and whose owner belongs to that sibling,
  not to a worktree nested inside it. The same project path must be a dir in the destination
  with no build dir yet: a project deleted or absent on this branch is left alone. Per position
  the sibling that built it last wins, so two workspaces may come from two checkouts. All
  positions are copied under one run lock. `worktree add` from a checkout root does the same.
- **The copy is a clone.** `fs::copy` is `clonefile` on APFS, so the new target shares every
  block with the old one and the volume loses nothing. Dirs are recreated, symlinks are
  recreated as symlinks, and `incremental/`, `.cargo-lock` and leftover `.swarfr-tmp-` files are
  left behind: a cache of another checkout's build, a lock that is not ours, and rubbish.
- **Under the source's locks.** Every profile dir of the source is locked with
  `ProfileLock::try_guard` on the adapter's guard for the length of the walk; one that a build holds is reported and
  skipped whole, so nothing half-written is ever copied. Exit code 2, as in `run`.
- **Never into a live target.** A destination that already has a target dir is refused: seeding
  merges nothing.
- **The index.** For every copied file whose source stamp the index knows, the copy is stored
  with the same hash and both sides are marked shared, so the next dedupe leaves the pair alone.
  A source the index has never hashed stays unknown — seeding must not read gigabytes to fill an
  index that the next `run` fills anyway; the cost is one needless clone for that file.

Known limits: the yield depends on what moved — units whose absolute path is part of their
fingerprint (workspace members, path dependencies) are compiled again in the new checkout, and
the test suite measures this against an empty target instead of assuming it; the shared fixture
has no registry dependencies, which are exactly the units that keep their paths across
worktrees, so `tests/worktree.rs` builds a repository whose one dependency is vendored outside
it; `--from` is not checked for being in the same family.

`swarfr worktree add GIT ARGS...` (`Session::worktree_add`) is `git worktree add` followed by
`seed`: it finds the new worktree by comparing `git worktree list --porcelain` before and after,
seeds it at the path the current dir has inside its checkout, and reports git's failure without
seeding. No built checkout to copy from is not an error: the worktree stays and nothing is
copied.

## Orphans pass (`src/orphans.rs`)

Lossy, so it runs only with `--lossy orphans`. Two reasons (`orphans::Reason`), both printed:

- **Checkout gone.** No threshold: an orphan either is one or is not. The rest of this section.
- **Project gone.** The adapter's manifest (`Ecosystem::manifest`, `Cargo.toml` for cargo) is
  missing from a checkout that is otherwise alive (`Target::project_gone`). A branch switch
  produces the same picture as a deletion, so this reason needs a threshold of its own,
  `--orphans-project-idle-days` / `[orphans] project-idle-days`, and the newest `last_used` of
  the target's locked profiles must be at least that old; a profile with no `last_used` is not
  idle. Without the threshold the target is only reported (`status`, `advise`). Under the lock
  the manifest must still be missing: switching back to the branch keeps the target.

- **What an orphan is.** A project whose `.git` file points at a worktree record
  (`<common dir>/worktrees/<name>`) that no longer exists — the repository was deleted, moved,
  or the record removed by hand. The inventory already reports it (`Target::orphaned`). Also a
  project that no longer exists at all while what is left above it is in no git checkout: the
  checkout went with it (`git worktree remove`, a deleted clone). Only a target moved out of its
  checkout can outlive its owner that way. A project missing inside a live checkout is a
  *project gone* instead.
- **Only `target/` goes.** The checkout next to it stays untouched. Such a checkout can hold
  uncommitted work that git can no longer report, and `target/` is the only part of it that is
  rebuildable.
- **The whole target, not its profiles.** `Action::RemoveTarget` takes `doc/`, `package/`,
  `tmp/` and `CACHEDIR.TAG` with the profile dirs; nothing is left for the next run to find.
- **Re-checked under the lock,** the way `evict` re-reads the last build: the pass plans a
  removal only for a target that holds at least one dir the engine has locked and that
  `inventory::is_orphaned` still calls an orphan. A `git worktree repair` between the inventory
  and the lock keeps the target.
- **The engine removes, not the pass.** `RemoveTarget` is refused (`Skip::Unlocked`) unless we
  hold a lock inside the target and no dir under it is busy, so a target with a running build is
  never touched. The removed dirs leave the set later passes scan.
- `orphans` is first in the pipeline: no point evicting or compressing inside a target that is
  about to go whole.

Known limits: a target with one busy profile is kept entirely, even when its other profiles are
free; the removal is not atomic, so an interrupted run can leave a half-removed target, which
the next run finishes; a checkout whose repository is merely unreachable (an unmounted volume
holding the common dir) reads as an orphan — the `.git` file points at a path that does not
exist, and nothing else distinguishes the two. The same goes for a target moved out of its
checkout while the checkout sits on an unmounted volume.

## Evict pass (`src/evict.rs`)

Lossy, so it runs only with `--lossy evict`, and only together with at least one limit.

- **Selection is global and pure.** `evict::select(profiles, now, limits)` sees every profile dir
  under the roots (`ProfileInfo` from the inventory: dir, allocated bytes, last build) and returns
  the dirs to remove, each with its reason. First the idle rule: last build at least
  `idle_days` old. Then the cap: while the rest exceeds `max_total_bytes`, the least recently
  built goes next (ties broken by path, so a run is reproducible). Idle removals count toward
  the cap. A profile whose last build is unknown is never chosen.
- **The choice is made before any lock is held**, so the pass re-checks under the lock: it plans
  `Action::Remove` only for a dir the engine has locked and whose last build still equals
  the inventory's. A build that slipped in between keeps its profile.
- **The engine removes, not the pass.** `Remove` is refused (`Skip::Unlocked`) unless the dir is
  a locked profile dir or inside one; then the whole dir goes, lock file included,
  which is what `cargo clean --profile` does. The dir leaves the set that later passes scan.
  `CACHEDIR.TAG` and the other profiles of the target stay.
- **Whole targets, with `--evict-whole-target`.** `evict::whole_targets(targets, chosen)` returns
  the targets whose every profile dir the selection took; the pass then plans one
  `Action::RemoveTarget` for such a target instead of the per-profile `Remove` actions inside it,
  so `doc/`, `package/`, `tmp/` and `CACHEDIR.TAG` go with them — what `cargo-clean-all` and
  `kondo` do, and what evicting profile by profile leaves behind. Each `Whole` carries its
  profiles, so the re-check under the lock covers all of them: one profile busy or built since
  the inventory keeps the target dir, and the free profiles are still evicted one by one. Only
  `target/` goes; the project around it is never touched.
- **Always reported.** Every planned removal and its reason land in `PassReport::removals`, on a
  dry run too; the CLI prints them as `would remove` / `remove`.
- `evict` runs after `orphans` and before the lossless passes: no point compressing what is
  about to go.

Known limits: a busy profile is skipped, so a run may end above the cap; the cap is checked
against sizes from the inventory, taken before compress and dedupe shrink the rest; a profile
dir that disappears between the inventory and the lock (a concurrent `cargo clean`) fails the
run with the I/O error instead of being skipped. Thresholds are flags until the config (T10).

## Incremental pass (`src/eco/cargo/incremental.rs`)

Lossy, so it runs only with `--lossy incremental --incremental-idle-days <N>`; the flags need
each other, as `evict`'s do.

- **What it drops.** `<profile>/incremental/`, rustc's incremental-compilation cache, in every
  profile dir whose last build is at least N days old. Cargo writes it for workspace members
  only, so no dependency has anything there to lose.
- **Measured cost: nothing, until the next edit.** The cache is not part of cargo's fingerprint.
  After the pass the build oracle reports zero stale units (`tests/incremental.rs`); the price
  is paid on the next change to a workspace member, which is then compiled non-incrementally
  once. That is why the pass is for profiles you are *not* working in, and why `--min-age` style
  floors do not apply to it: the age that matters is the profile's last build.
- **Selection is pure**, except for one `is_dir` check: a profile with no cache is never planned.
  A profile whose last build is unknown is never chosen, as in `evict`.
- **Re-checked under the lock**: the cache goes only if the engine holds that profile's lock and
  its last build, read under the lock, still equals the inventory's reading. A build in between keeps it.
- **The engine removes, not the pass.** `Action::Remove` accepts a locked profile dir *or a dir
  inside one*, which is what makes this pass one selector instead of a second removal path.
  The profile dir itself stays, so its lock stays valid for the passes that follow.

Known limits: the whole cache of a profile goes or none of it — cargo's per-crate session dirs
are not read; a busy profile is skipped; `docs/research.md` measured `incremental = false` at
−5.8 GB on one target, but the pass's own yield across a machine is not benchmarked yet.

## Inventory and `status` (`src/inventory.rs`)

Read-only: takes no locks and changes nothing, so it is safe next to running builds.

- **Place.** Each build dir carries its adapter, the checkout its project is in (the nearest dir
  above holding a `.git`), its position inside that checkout, and its guard tier. `status`
  groups by family, checkout and ecosystem and lists the five largest build dirs of each group
  (`--all` every one); `--json` stays flat, one entry per build dir.

- **Discovery.** Walk the roots without following symlinks; a dir whose `CACHEDIR.TAG` holds
  cargo's sentence is a target. A found target is not entered, so a target nested inside another
  is part of the outer one. Caches of other tools carry the same tag file and are ignored.
- **Sizes.** The whole target is scanned with the engine's inode model (`doc/`, `package/` and
  `tmp/` weigh too, not only profile dirs). `allocated_bytes` is `st_blocks` of every inode
  counted once — what `du` reports. `compressed_bytes` / `compressible_bytes` split the inodes by
  the compressed flag and the compress pass's size floor.
- **Family.** The nearest `.git` above the target. A directory is the common dir itself. A file
  (`gitdir: <path>`) is a worktree: the common dir is `<gitdir>/commondir`. Git's files are read
  directly, because the interesting case is the one where `git` refuses to answer.
- **Orphan.** The `.git` file points at a `gitdir` that no longer exists. The family is still
  known: the path has the form `<common dir>/worktrees/<name>`.
- **Dedupe estimate.** Per target, bytes of files of at least the dedupe size floor whose exact
  size also occurs in another target of the family. An upper bound: equal size is not equal
  content, and nothing is hashed for a report.
- **Last built.** Newest mtime among the top-level entries of the profile dirs.

`run` builds the same inventory and starts one engine run per family (a target without a family
runs alone), so locks are held only in targets that are compared with each other. Known limit:
equal files in unrelated projects are not shared.

## Advise command (`src/eco/cargo/advise.rs`)

Read-only, and the only command that reads anything outside a target dir. Two lists:

- **Findings** come from files. `review(file, kind, doc, nightly)` is pure over a parsed
  `toml::Table`, so every check is a unit test over a literal document. A manifest contributes the
  `[profile.*]` checks (`debug` spelled any of the three ways cargo accepts, the `"*"` dependency
  override, `strip` for release, `split-debuginfo = "packed"`, `codegen-units = 1` in a dev
  profile); a config contributes `build.incremental` and, on a stable toolchain, the `[unstable]`
  table cargo ignores without a word; the cargo home's config is also asked for
  `cache.auto-clean-frequency`, which cargo has cleaned by since 1.88. A missing key is a finding
  as much as a wrong one — cargo's own default for `profile.dev.debug` is full debuginfo — so a
  finding names the file and the key it is *about*, which is not always a key the file has.
- **Notes** come from the inventory, because no single file explains them: what `incremental/`
  weighs under the roots, families whose targets could share a `[build] build-dir` (stable since
  1.91, at the price of serializing parallel builds on one lock), checkouts with no target dir
  that `seed` would fill, and orphaned worktrees for `--lossy orphans`.

The toolchain channel is only asked for (`rustc --version` in the project dir) when a config has
an `[unstable]` table, since that is the only check it decides; a rustc that cannot be run counts
as stable. Numbers quoted in the advice are the measured ones in `docs/research.md`.

Out of scope on purpose: `cargo-hakari` and `sccache` help with rebuild time, not with the size
of a live target, and nothing in a file says whether a workspace wants them — they stay in
`docs/research.md` rather than in the output.

## Doc pass (`src/eco/cargo/doc.rs`)

Lossy, so it runs only with `--lossy doc`, and the smallest pass there is: `<target>/doc` is what
`cargo doc` writes from scratch and no build reads, which is why `cargo clean --doc` exists.

`doc/` sits beside the profile dirs rather than inside one, so the lock that guards it is the
target's own: the pass plans `Action::RemoveTarget { target, dir }` with `dir` the `doc/` dir, and
the engine applies it only while it holds a profile lock inside that target and nothing in the
target is busy. That is the same guard `orphans` and whole-target eviction use, which is why
`RemoveTarget` names the target and the dir separately instead of assuming they are the same.
The size comes from the inventory (`Target::doc_bytes`), since the engine itself scans only
profile dirs. The build oracle is untouched by it: after the pass cargo reports nothing stale.

## Across families (`--across-families`)

One engine run per family keeps a run's locks inside the repository it is working on, and that
is where most duplicates are. It is not where all of them are: the same version of the same crate
built with the same features is byte-identical in two unrelated projects, and the hash index
already knows it.

The flag changes nothing but the grouping — every target under the roots goes into one group
instead of one per family, and the report names that group `<across families>`. The engine
already compares whatever profile dirs it is given and already takes its locks in sorted order,
so nothing else had to move and deadlock stays impossible. What it costs is the lock: a run holds
every target's profile locks for its whole length, which on the benchmark workspace is more than
a minute of no builds anywhere. That is why it is opt-in and stays opt-in.

Per-family `skip` still applies, because it is decided before the grouping.

## Toolchain report (`src/eco/cargo/toolchains.rs`)

A toolchain upgrade does not clean up after itself: cargo compiles every unit again under new
hashes and never looks at what the old rustc produced. `cargo-sweep --installed` finds those by
parsing hashed file names; this reads cargo's own fingerprints instead —
`<profile>/.fingerprint/<unit>/*.json`, whose `rustc` field is cargo's hash of the compiler it
used. The unit is a directory and the compiler is a number cargo wrote, so no name is parsed.

The groups are sorted by their newest fingerprint, which makes the head the compiler in use and
everything after it stale. Bytes are an **estimate**, and say so in the field name
(`stale_bytes_estimate`): the profile dirs' size in the share of the units. An exact number needs
the unit-to-file map that only cargo's newer build-dir layout gives (roadmap `R1`), and a
fingerprint names no artifact.

This is why the task stops at a report: nothing can be deleted safely without that map, so
`status` prints a line per target and `advise` adds a note pointing at `cargo clean`. A target
built by one rustc — the ordinary case — reports nothing at all.

## Cargo home (`src/eco/cargo/home.rs`)

The registry sources are the one big pile of compressible text outside the targets: every crate
cargo builds is unpacked there once and then only read. `--cargo-home` treats it as one more
group, with two differences from a target.

- **Only `compress` runs.** Nothing there is a build artifact: there is nothing to dedupe against,
  nothing stale to evict. The two dirs it touches are `registry/src` and `git/checkouts` — the
  extracted sources. `registry/cache` (the `.crate` archives) and `registry/index` are left alone;
  the archives are already compressed, and the index is cargo's own cache to invalidate.
- **One lock for the whole group, not one per dir.** Cargo does not write `.cargo-lock` files
  there; what it holds while it fetches or extracts is `<home>/.package-cache`. So
  the `Home` adapter's `Guard::Shared` makes the engine take that one file lock and either all the dirs are
  ours or none are, which is also why a home cargo has never used (no `.package-cache`) is refused
  rather than locked into existence.

What decides whether cargo re-extracts a crate is `.cargo-ok` and the files beside it, and
compression changes neither the content nor the mtime of any of them — the test asserts that over
every file in a fake home, and the benchmark confirms it against a real one by rebuilding
afterwards. The flag takes an optional value: `--cargo-home` alone resolves `CARGO_HOME`, else
`$HOME/.cargo`. `status --cargo-home` reports the same dirs without touching them, and is opt-in
because measuring them costs a second full walk.

## Immutable stores (`src/eco/store.rs`)

`--store DIR` names a content-addressed store: `GOCACHE`, `~/.cabal/store`, Zig's `o/`, dune's
shared cache. Nothing is discovered. The `Store` adapter makes the whole dir one unit under
`Guard::Immutable`:

- No lock is taken and none exists. What makes compression safe anyway is the store's own rule:
  a name never gets other bytes. An entry is written once — most tools write a temp file and
  rename it in — so the only file a pass could race is one still being written, and
  `engine::IMMUTABLE_MIN_AGE` (one hour) leaves those out of the model whatever `--min-age` says.
- Only `compress` runs. Dedupe finds nothing where every name is a different content, and no
  lossy pass ever runs: the unit is always unsure (`Skip::Unsure`), because the store's own tool
  evicts from it.
- The group is listed in `Report::quiet` like a `Guard::Quiet` unit.
- `store::check` refuses, before the run lock is taken: what is not a dir; ccache and sccache (a
  `ccache.conf`, a `CACHEDIR.TAG` naming ccache, or their default dir names), which compress their
  own entries; and a dir inside a build dir a registered adapter claims or inside a cargo home,
  whose files are rewritten under old names. A dir holding a build dir somewhere below is not
  looked for — that would be a walk of the whole store.

What it does not promise: a tool that rewrites a file under its old name — Zig's `h/`
manifests, which it rewrites under its own lock — is not a content-addressed store, and naming
it anyway races like `Guard::Quiet` does, with no process check. The harm is bounded by what a
cache is: a manifest update lost to the race is a cache miss, not a wrong build. Name `o/` for
Zig, not the whole cache.

`tests/store.rs`: mtime, mode and content of every entry unchanged; a young entry left alone; no
lossy pass; `check`'s refusals; the CLI with no root; and, where `go` is installed, a `GOCACHE`
fixture whose data entries still hash to their names and whose rebuild compiles nothing.

## Go module cache (`src/eco/go.rs`)

`run --go` asks `go env GOCACHE GOMODCACHE` in the binary (the library runs no tools and reads
no environment): `GOCACHE` goes to the stores, `GOMODCACHE` to `Request::go_modcache`, one more
group after the stores.

- **Units.** Every top-level dir but `cache/`: the unpacked `<path>@<version>` trees. `cache/`
  holds the downloaded zips, already compressed, and VCS clones `go` updates under its own locks.
  `go::check` refuses a dir without `cache/download`.
- **Guard.** `Guard::Immutable`, as for a store: `go` unpacks into a temp dir and renames it into
  place, and checks a module later by the hash of its files alone (`go mod verify`), which no
  compression changes. Only `compress` runs, and files younger than an hour are left out.
- **Read-only dirs.** `go` makes every module dir `0555`. The engine's replace needs a temp copy
  next to the file and a `rename` over it, so on such a dir each file fails with
  `PermissionDenied`, harmlessly — what `--store` on a module cache does. The adapter answers
  `Ecosystem::lifts_read_only_dirs`, and `engine::apply_compress` then adds the owner write bit
  to each read-only dir holding a member of the batch, and puts the recorded mode back once the
  batch is swapped in. A mode that cannot be put back stops the run with the dir's path; a dir
  that cannot be lifted is left as it is. A crash inside a batch leaves at most those dirs
  writable, which `go` does not check. Only dir mtimes move, as with any `rename`. Unix only: a
  read-only dir on Windows does not stop anyone from creating files in it.
- **Spike.** On a copy of a real module cache (146 MiB, 24 modules), with no lifting every file
  was skipped; with it, 105 MiB, every module's `h1:` dir hash still equal to its `.ziphash`,
  and a module built against the copy rebuilt with no compile step.

`tests/go.rs`: a fake read-only module cache keeps every mode, mtime and byte and gets no
leftovers; an adapter that does not lift fails every file with `PermissionDenied` and changes
nothing; `check`'s refusal; and, where `go` and `zip` are installed, `run --go` on a module
served from a proxy dir, after which `go mod verify` is green and a build compiles nothing.

## SwiftPM (`src/eco/swiftpm.rs`)

- **Claim.** A dir named `.build` holding `workspace-state.json`, which every SwiftPM writes when
  it resolves a package. The owner is the dir above it; the manifest `Package.swift`.
- **Lock.** `swift build` (and `test`, `run`, `package`) takes TSCBasic's `FileLock` on the
  scratch dir for the whole command: `flock` on `<temp dir>/<scratch path, / as _>.lock`, the
  name cut to its last 255 bytes. Not `.build/.lock`, which only notes a pid. The temp dir is
  `TMPDIR`, else the per-user one (`getconf DARWIN_USER_TEMP_DIR`), where Foundation looks when
  `TMPDIR` is unset. The adapter names that file for the canonical scratch path as a
  `Guard::Shared` over every unit; the engine creates it when a temp dir cleaner took it, as
  `swift build` does. `tests/swiftpm.rs` holds it and watches `swift build` wait.
- **Units.** The build outputs: `out/` (swiftbuild, the default build system) and
  `<triple>/` dirs with a `debug` or `release` inside (the native one). Not `checkouts/` or
  `repositories/`, which are dependency sources, and not `index-build/`, sourcekit-lsp's own
  scratch dir under a lock of its own name.
- **Private.** `out/CompilationCache.noindex`: mmapped databases, sparse, 12–25 GiB of logical
  size each. `model::scan` leaves private dirs out whole, not only private files.
- **Sharing.** Clones only: nothing says an output is never rewritten in place.
- **No seed.** Absolute paths run through the build description and the swiftmodules.

The oracle is `swift build -v`, whose output names compile tasks (`Compile`, `Compiling`) only
when there are any; the plain output never does. A switch between debug and release recompiles
by itself in swiftbuild, so the oracle builds the configuration that was built last.

Known limit: a build started through a symlinked `--package-path`, or with `--scratch-path`,
locks a file under a name derived from that other path, which the adapter cannot know.

## .NET (`src/eco/dotnet.rs`)

- **Claim.** `obj/` holding `project.assets.json`, which every restore writes, and `bin/` next
  to such an `obj/`. Each dir is one unit: MSBuild writes all of it in one build and nothing
  guards a part of it. The owner is the project dir; the manifest the project file restore
  recorded as `obj/<project file>.nuget.dgspec.json`, since one dir may hold several.
- **No lock.** `Guard::Quiet`: the one-day floor, and `dotnet`, `MSBuild` and `VBCSCompiler`
  as the tools whose current dir makes a unit busy. Worker nodes and the compiler server stay
  alive after a build and keep their project busy until they exit; that is the safe side.
- **Clones only.** MSBuild's `Copy` overwrites a destination in place, which is how its own
  hardlink option corrupts the NuGet cache (dotnet/msbuild#8273). `Sharing::ClonesOnly` makes
  `--link-artifacts` a no-op here, whatever the filesystem.
- **No seed.** Outputs carry absolute paths (`*.FileListAbsolute.txt`, `project.assets.json`).

The oracle is `dotnet build -v:n`: every `CoreCompile` it reaches is skipped as up to date and
no `Copy` task copies a file. `CoreCompileInputs.cache` survives a same-content, same-mtime
replacement: the build after dedupe and compress is a no-op.

Not found yet: `UseArtifactsOutput`, whose `artifacts/obj/<project>` has no project file next to
it. A .NET 10 SDK with missing workload manifests fails every build, the case on the machine
the tests were written on; the tests pin a 9.0 SDK with `global.json`.

## CMake (`src/eco/cmake.rs`)

- **Claim.** A dir holding `CMakeCache.txt`, whatever the generator. The whole dir is one unit.
  The owner is the source dir the cache records as `CMAKE_HOME_DIRECTORY`; the manifest is its
  `CMakeLists.txt`. A build dir sits anywhere, so its place says nothing.
- **Never in-source.** A dir whose source dir is itself or lies inside it is not claimed,
  compared as real paths: a lossy pass removing that unit would remove the sources.
- **No lock.** `Guard::Quiet`, with `cmake`, `ninja`, `make`, `gmake` and `ctest` as the tools.
- **Clones only.** `ar` may update an archive in place.
- **No seed.** The cache and the generated build files hold absolute paths.

The oracle is `cmake --build` with the Makefiles generator: after compress and dedupe it prints
no `Building` and no `Linking` line, and the binaries still run; a new mtime on a source makes it
build again. Ninja and Meson build dirs are T32.1.

## Known build dirs (`src/known.rs`)

In a monorepo the walk for build dirs costs the source tree, not the build dirs (`docs/bench.md`,
"Discovery in a monorepo"). `Session` keeps the last walk in `build-dirs-v1.json` next to the
index: the canonical roots, when they were walked, each root's own mtime, and every build dir
with its adapter's name. The next run uses the list when the roots are the same, the list is
younger than `[discovery] every-secs` (1 h), and no root's mtime moved; otherwise it walks and
writes the list back through a temp file and a rename.

- **Every entry is claimed again.** A listed dir goes through its adapter's `claim`, so a removed
  build dir drops out without a walk, and a dir that stopped being one is not worked on.
- **A new build dir waits.** One created below a root's top level does not move the root's mtime:
  it is seen at the next walk. `run` says when it used the list, and `--rediscover` walks now.
  The daemon sets `rediscover` for the run after each of its own walks, so a unit it found is
  never marked visited by a run that did not see it.
- **Best effort.** A list that cannot be read or written costs a walk, never a failure. A
  session without an index path walks every time.
- **Not a file-system watcher.** Watching the tree is the `notify` question in `ideas.md`.

## Daemon (`src/daemon/`)

In the binary, not the library: only a process has triggers. Every change it makes is one
`Session::apply` of `Request::from_config`, so it runs exactly what a scheduled `run` with no
flags would, lossy passes included only when the config names them.

- **Triggers: timers only.** A slow one re-runs `eco::discover`; each look reads every known
  unit's `last_used`. A filesystem watcher (`notify`) waits for the creator's word on the
  dependency.
- **Due times.** A unit is pending when built since its last visit, and due at its last build
  plus `min-age` — `QUIET_MIN_AGE` at least for `Guard::Quiet` units. Any due unit starts one run
  over all the roots: dedupe and the caps need the families whole. After it, a unit is visited
  unless it was busy or a group let go early; the report does not say which units an
  interrupted group reached. The daemon sleeps until the next due time still ahead, capped by
  the interval, so a busy unit waits for the interval rather than spinning.
- **A build never waits long.** `Control::lock_budget` is 2 s by default: a group lets go of
  build locks after that, and is visited once more. A held lock is `try_lock`, so the daemon never
  waits for a build either.
- **State.** `daemon.json` next to the index, written to a temp file and renamed. It keeps the
  visit of each unit across restarts; `daemon status` prints it.
- **Service units.** `daemon install` writes a launchd agent (`Nice`, `LowPriorityIO`,
  `ProcessType Background`, `ThrottleInterval`) or a systemd user unit (`Nice=19`, idle CPU and
  I/O scheduling, `Restart=on-failure`), with this binary's absolute path, and starts it through
  `launchctl bootstrap` / `systemctl --user enable --now`. No test writes one: it would load a
  real agent. `--print` is what the tests check.
- **No signal handling.** It needs a crate or `unsafe`; every action is whole, so a killed run
  leaves at most temp files the next run removes.


```
swarfr status [--json] [--all] [--cargo-home [DIR]] [ROOT]...  # inventory, families, potential
                                                          # savings; read-only
swarfr run [--dry-run] [--lossy <PASS>]... [--index <FILE>] [<ROOT>]...
               [--config <FILE>] [--json]          # file: see below; json: the report as data
               [--cargo-home [DIR]]                # compress the registry sources too
               [--store <DIR>]... [--go]           # content-addressed stores; Go's caches
               [--evict-idle-days <N>] [--evict-max-total-gib <N>]   # with --lossy evict
               [--evict-whole-target]                                # with --lossy evict
               [--incremental-idle-days <N>]        # with --lossy incremental
               [--orphans-project-idle-days <N>]    # with --lossy orphans: projects gone
               [--pass <PASS>]... [--min-age <SECS>] [--min-size <BYTES>]  # benchmarks
               [--rediscover]                       # walk the roots even if the list holds
swarfr seed [--from <DIR>] [--dry-run] [--index <FILE>] [<DIR>]  # clone a sibling's target
swarfr worktree add [--dry-run] [--index <FILE>] <GIT ARGS>... # git worktree add, then seed
swarfr advise [--json] [ROOT]...  # what makes these targets bigger than they need to be
swarfr daemon run [--config <FILE>] [--index <FILE>] [--once]  # the passes, as dirs go cold
swarfr daemon install [--config <FILE>] [--index <FILE>] [--print] | remove | status [--json]
```

Config (`src/config.rs`): `$XDG_CONFIG_HOME/swarfr/config.toml`, else
`~/.config/swarfr/config.toml` — `roots`, `lossy`, `min-age`, `min-size`,
`[evict] idle-days / max-total-gib / whole-target`, `[incremental] idle-days`, `[index] idle-days`, `[orphans] project-idle-days`,
`[discovery] every-secs`, `[daemon] interval-secs / rediscover-secs / lock-budget-secs`,
`[family."<dir>"] skip / skip-paths / ecosystems`. Keys are
kebab-case and unknown ones are an error: a typo that silently does nothing is worse than a stop.
A flag always wins over the file, and a file named with `--config` must exist. Per family there
is only what to leave alone — `skip`, `skip-paths` (positions in a checkout, as whole-component
prefixes, so no glob crate) and `ecosystems` — because the thresholds are decided over
everything under the roots at once. `Request::keeps` drops a skipped build dir from the
inventory before any pass chooses, so it is neither worked on nor counted toward the `evict` cap.

Exit codes: `0` done, `1` failed, `2` a profile dir was left alone because a build held its lock,
or another run of the tool holds the run lock.
A scheduled run needs that difference; anything else it wants is in `--json`, which prints the
groups, the busy dirs, the per-pass counts, every removal with its reason and every skip.

## Dependencies

Order of preference: std, then crates already in the shared Rust inventory, then new crates. Each
crate is wired by the task that first needs it and lands in `toolchain.md` in the same change.
The creator approved every crate in the table below; anything outside it still needs approval.

std already covers: inode identity and allocated size (`MetadataExt`: `dev`, `ino`, `nlink`,
`blocks`, `st_flags` for the compressed flag), file locks (`File::try_lock`, 1.89+), mtime restore
(`File::set_times`), atomic `rename`, `env::home_dir`, reading a worktree's `.git` file and
`commondir` for family grouping.

| Task | From the shared inventory | Why |
| --- | --- | --- |
| T3 | `tempfile`, `serde`, `serde_json`, `assert_cmd`, `predicates`, `trycmd` | Fixture in a temp dir; parse `cargo build --message-format=json` for the oracle; exit codes via `assert_cmd`, full CLI output via `trycmd` fixtures |
| T4 | `walkdir`, `rayon`, `serde_json`, `anyhow` | Tree walk, parallel `stat`, `status --json`, one error type for the binary |
| T5 | `rustix`, `thiserror`, `tracing`, `tracing-subscriber` | `clonefile` without hand-written `unsafe`; typed engine errors that name the path; `-v` logs |
| T7 | `sha2`, `rayon` | Content hashing across files |
| T10 | `toml`, `indicatif`, `owo-colors` | Config file, progress, coloured report |
| T11 | `divan` | Micro-benches for scan and hash; whole-build timings stay a script |
| T12 | `toml_edit` | Read `~/.cargo/config.toml` and print exact suggested edits |
| when it helps | `rstest`, `proptest`, `pretty_assertions`, `strum` | Per-pass test cases, plan invariants over random hardlink groups, pass names |
| release | `clap_complete`, `clap_mangen`, `cargo-dist`, `git-cliff`, `cargo-nextest` | Only when the tool is published |

Deliberately not used: `ignore` (its filters would hide git-ignored `target/`; `walkdir` + `rayon`
is enough), `nix` and `libc` (`rustix` covers the same calls safely; `libc` only if `rustix` lacks
one), `figment` (one config file), `directories` / `dirs` (the config path is fixed), `chrono`
(ages are `SystemTime` arithmetic), `heed` / `diesel` (the hash index starts as a flat file; `heed`
is the fallback if loading it shows up in T11), `insta` (CLI output goes through `trycmd`),
`notify` and `tar` (only for the watch-mode and park ideas in `ideas.md`).

`applesauce` (compression backend) was approved outside the inventory and joined it with T6.
Not in the inventory: `blake3` (approved, not
needed yet: hardware SHA-256 measured 1484 MB/s per core — the earlier slowness was perl `shasum`,
389 MB/s — so `sha2` + `rayon` across files comes first). Human-readable sizes and durations
(`bytesize`, `humantime`) are not in the inventory either; a small function each, and `min-age` in
the config is a number of hours.

## T2 spike results

Run inside a throwaway APFS image; scripts and raw output are in `docs/spike/`.

| Question | Result |
| --- | --- |
| Seed by recursive clone | 0.09 s, +108 KB for a 52 MB target; 13 of 15 units fresh, 2 members rebuilt; 0.7 s vs 3.0 s cold; mtime preservation irrelevant |
| Byte-identical artifacts between independently built worktrees | 41 of 52 files, 57% of bytes; differing: members, the proc-macro dylib, build-script binaries, 5 rlibs / 3 rmeta (absolute paths such as `OUT_DIR` are the suspected cause) |
| Clone-replace with restored mtime | −24.2 MB of −28.1 MB expected; all units fresh |
| Clone-replace without restored mtime | registry rlib: still fresh; member rlib: dependent rebuilt |
| Hardlink-group replacement (clone → `ln` + `rename` per path) | group keeps one inode and its link count; all units fresh; groups link `deps/` ↔ `incremental/` |
| `applesauce` on a target with hardlinks and clones | 52.3 → 18.2 MiB (−65%), 46 hardlinked files before and after, mtime + mode signature identical, all fresh, 0.68 s, re-run 0.16 s |
| Compressing a cloned target | image usage **+16 MB**: un-shares the clones |
| Seeding from a compressed target | +180 KB, clones stay compressed, 13 of 15 fresh |
| Re-cloning separately compressed files from a compressed canonical | −14.5 MB of −16 MB expected, still compressed, all fresh |
| Relink against compressed rlibs | works; files rewritten by the build lose compression, which justifies `min-age` |
| cargo lock | cargo blocks on a foreign `flock` of `target/debug/.cargo-lock` ("artifact directory") |
| Hash throughput | openssl SHA-256 1484 MB/s, perl shasum 389 MB/s, BLAKE2b 826 MB/s |

Open after T2: `applesauce` left a single 119 MB blob uncompressed with all three codecs
(reason not captured). T6 did not reproduce it: a 130 MiB file compresses through the library, so
size is not the limit (the backend's source refuses only files of 4 GiB and more — not tested
here). Every codec gave up on the blob in 0.13 s, which reads as a skip, not as a failed attempt;
the pass now keeps the backend's skip reasons and errors and `run` prints them.
Decompression cost at link time was not measurable on the small fixture — T11.

## Platform layer (`src/sys/`)

Everything that differs between platforms lives in one module and nothing above it imports
`std::os`. One file per platform, picked at compile time:

```rust
#[cfg_attr(target_os = "macos", path = "macos.rs")]
#[cfg_attr(all(unix, not(target_os = "macos")), path = "unix.rs")]
#[cfg_attr(windows, path = "windows.rs")]
mod imp;
```

Each file supplies the same items: `file_id`, `nlink`, `allocated`, `flags`, `mode`, `set_mode`,
`symlink`, `clone_file`, a `Compressor`, the constants `COMPRESSED` and
`ALLOCATED_SHOWS_COMPRESSION`, and `caps`.

**`caps(dir)` is what the passes read.** A pass whose capability is false plans nothing there —
`Dedupe::plan` and `Compress::plan` filter the profiles by it before reading a single file. That
is not politeness: a "clone" that the filesystem cannot share is a second copy of the bytes, and
a compression pass with no backend would clone every candidate only to throw the copy away.
Doing nothing is the correct answer, and `status` printing *this filesystem shares no blocks:
dedupe finds nothing here* is the honest one.

It is a question about a filesystem, not about a platform, so it is asked per directory and
cached per `st_dev` — a run over forty target dirs on one disk probes once. On Linux the probe
writes a 64 KiB temp file and tries `FICLONE` into a second one. A table of filesystem names
would get btrfs mounted `nodatacow`, XFS made with `reflink=0` and a bind-mounted ext4 wrong;
trying does not. Compression is the exception, and it is asked by name — `statfs().f_type`
against `BTRFS_SUPER_MAGIC` — because there the attempt lies: ext4 accepts `FS_COMPR_FL`, keeps
it where `lsattr` shows it, and compresses nothing. macOS
answers true for both without asking (APFS is what every number in `docs/bench.md` came from),
Windows answers `Caps::NONE` until `T21`.

| | macOS (APFS) | Linux | Windows |
| --- | --- | --- | --- |
| identity | `st_dev` + `st_ino` | `st_dev` + `st_ino` | the path (no handle, no inode) |
| link count | `st_nlink` | `st_nlink` | 1 — links are invisible without a handle |
| size on disk | `st_blocks × 512` | `st_blocks × 512` | logical length |
| flags | `st_flags` (`UF_COMPRESSED`) | `FS_IOC_GETFLAGS`, masked to `COMPR`, `IMMUTABLE`, `APPEND`, `NOCOW` | `FILE_ATTRIBUTE_*`, masked to the ones that mean something |
| clone | `fs::copy` → `fclonefileat` | `FICLONE` (btrfs, XFS `reflink=1`, bcachefs) | plain copy |
| compress | applesauce (LZFSE) | `FS_COMPR_FL` + rewrite (btrfs) | — |
| probe | write test only | `FICLONE` and `FS_IOC_SETFLAGS` | none; `Caps::NONE` |

Two Linux details that are easy to get wrong:

- `clone_file` is `FICLONE`, not `fs::copy`. `fs::copy` reflinks on btrfs and silently writes a
  second copy on ext4, which is the one failure mode a dedupe pass must not have. `seed` is the
  caller that wants a copy either way, so it asks `caps` once and falls back to `fs::copy`
  itself, reporting `shared_blocks: false` when it did.
- Setting `FS_COMPR_FL` compresses *new writes*, so the flag alone shrinks nothing. The
  compressor sets it on the engine's private copy and rewrites the copy through itself —
  `btrfs filesystem defragment -c` for one file. The engine already hands it a clone nobody else
  can see, so the rewrite costs a copy that was going to be made anyway.
- Reading the flags costs an `open` per file, because Linux keeps them behind an ioctl rather
  than in `stat`. `flags` pays it only where `caps` says compression exists at all; scanning a
  70 000-file target on ext4 opens nothing.
- **The probe puts the directory's mtime back.** Creating and removing a file moves the mtime of
  the directory it is in, and that mtime is how `evict` and `incremental` tell a profile nobody
  has built for a week from one built this morning. Probing inside a profile dir without
  restoring it makes every target look freshly built, and the only symptom is a pass quietly
  planning nothing — found exactly that way, by `tests/incremental.rs` failing on btrfs and
  nowhere else.

`FIDEDUPERANGE` is deliberately unused. The engine already replaces whole inode groups
atomically, re-checks every stamp under cargo's lock and restores mode and mtime; an in-place
dedupe would be a second apply path with the same invariants to maintain and nothing the first
one does not give.

`applesauce` is a macOS-only dependency (`[target.'cfg(target_os = "macos")'.dependencies]`), so
the other two platforms do not build it at all.

Windows deserves its own caveat. Identity from a path means two hardlinks read as two files and
a link count always reads 1. That is consistent — `model::scan` groups by the same id it later
checks against — and it is inert, because nothing is planned there. `GetFileInformationByHandle`
replaces it in `T21`.

What the tests cover: `src/sys/mod.rs` holds the facts that must hold on every platform — a file
has an identity of its own and a size on disk, a clone holds the bytes of its source, an empty
batch costs nothing, and the probe answers the same thing twice, cleans up after itself and
claims nothing about a directory it cannot read. `tests/caps.rs` states both outcomes for each
pass and picks by `caps`, so the same test is an assertion on every filesystem: on btrfs the
twin becomes a clone, on ext4 nothing is planned and nothing is touched. Point `TMPDIR` at a
mount to choose the side. `just check-cross` compiles both other targets, which is what catches
a port that stopped building.
