

### T21. Windows: NTFS compression and ReFS block cloning

Compression: NTFS has per-file transparent compression through `FSCTL_SET_COMPRESSION`, and the
allocated size to measure it with comes from `GetCompressedFileSize`. Dedupe: ReFS has block
cloning (`FSCTL_DUPLICATE_EXTENTS_TO_FILE`); NTFS has no copy-on-write at all, so dedupe there is
the hardlink fallback, which needs only `caps` to answer honestly. File identity is the volume
serial plus a 128-bit file id, and the build lock stays `File::try_lock`.

`windows-sys` is a Windows-only dependency. Done: the pass suite runs on NTFS, NTFS reports no
block sharing and falls back to hardlinks instead of failing, and paths with drive letters and
`\\?\` prefixes are covered by tests. ReFS cloning runs when `TEMP` is a ReFS volume.

#### Result

- `src/sys/windows.rs`: `FILE_ID_INFO` (inode widened to `u128`, hash index `DUNIDX03`), link
  count from `FILE_STANDARD_INFO`, size on disk from `GetCompressedFileSizeW`, LZNT1 via
  `FSCTL_SET_COMPRESSION`, block clone via `FSCTL_DUPLICATE_EXTENTS_TO_FILE` (returns
  `Unsupported` and leaves no copy where the volume cannot clone).
- The compress pass copies bytes where the volume cannot clone. A `rename` blocked by a handle
  without delete share is `Skip::Busy`. `sys::plain` makes `\\?\`, `C:\` and `C:/` one family
  key, and git is given those paths.
- `tests/windows.rs` plus the existing suite, run on this NTFS host. A 256 KiB repeated-byte
  fixture went from 262144 allocated bytes to 16384; that number is in `docs/bench.md` and is
  not a workspace bench. WOF stays in `ideas.md`.

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


### T38.1. Monorepo: an owner for a build dir outside its checkout

Split off from T38. A cargo target moved out of the checkout by `build.build-dir` (or
`CARGO_TARGET_DIR`) has no family: `inventory::git_link` walks up from the target, not from the
project, and the build dir records no path back to the workspace that built it. Such a dir gets
no dedupe partner, no `seed` source and no *checkout gone* orphan status. Done: an out-of-tree
build dir lands in its owner's family, and `orphans` removes it when that owner's checkout is
gone, with a fixture test for both.

**Question for the creator before this starts:** where does the owner come from? Options:
(a) read the absolute source paths in the profile's dep-info `.d` files — present in every
build, but it is parsing cargo's output, a heuristic; (b) a record dunnage writes itself when
`seed`/`worktree add`/the daemon sees a build dir being used from a workspace — exact, but only
for dirs it has seen; (c) configuration: `[owners]` mapping build dirs to workspaces.

**Answer:** (a), for target dirs only. Checked on cargo 1.97: a `build.build-dir` holds no text
record with an absolute path to its workspace (rustc's `deps/*.d` are relative to the workspace
root, fingerprints are package-relative; only `.rmeta` and object debug info name it), and its
profile dirs carry `.cargo-build-lock`, not `.cargo-lock`. The build-dir half goes to
`ideas.md`.

#### Result

- `eco::cargo::depinfo::workspace_roots`: cargo's `<profile>/*.d` name member sources by absolute
  path, rustc's `<profile>/deps/*.d` name them relative to the workspace root; the absolute path
  minus the relative tail is the root. Candidates are intersected over every rustc file that
  pairs, and a candidate with one of rustc's absolute sources under it (a path dependency outside
  the workspace) is dropped; generated sources under the build dir do not count.
- `Cargo::owner`: the dir above when it holds `Cargo.toml`, as before; otherwise the dep-info
  root — an existing one of several, none when they are in different repositories — and the dir
  above when the dep-info names none (`cargo check` only, `build.dep-info-basedir`).
- `inventory::git_link`: a project that no longer exists, with nothing above it in a git
  checkout, is orphaned (`Target::orphaned`, and `is_orphaned` under the lock). Missing inside a
  live checkout it stays *project gone*.
- `orphans::Reason::CheckoutGone` prints "the checkout is gone: git has no worktree record for
  it, or its dir is gone".
- `build.build-dir` owners are in `ideas.md`: no text record there names the workspace, and its
  profile dirs carry `.cargo-build-lock`, which `profile_dirs` does not look for.
- Docs: `README.md`, `docs/usage.md`, `DESIGN.md` (adapter owner, orphans section and its known
  limits), `docs/architecture.md`.

#### Verified

`just check` (212 tests) and `just check-cross` green. `tests/out_of_tree.rs` builds with real
cargo into `CARGO_TARGET_DIR` outside a git checkout: the target lands in the repository's family
and checkout, next to the checkout's own target; after `git worktree remove` the worktree's
out-of-tree target goes with `--lossy orphans` ("the checkout is gone") while the main
checkout's target and the vendored dependency stay; a workspace moved elsewhere inside the live
checkout is *project gone*, not orphaned. All three fail without the change. Unit tests cover the
dep-info parsing, the single-crate case with a path dependency, and a check-only target.

### T45. Clean up target dirs with dunnage after tests

Wired the installed `dunnage` binary into the local test loop instead of leaving cleanup manual.
`just check` (the local/CI test recipe) now runs `dunnage` as a post-dependency: `dunnage run target`
compresses and dedupes the checkout's `target/` losslessly (never deletes, keeps mtimes) after
tests pass. The recipe is tolerant of machines without the tool (`command -v dunnage` guard, a
no-op exit 0), skips a checkout with no `target/` yet, and treats dunnage's exit code 2 (a build held the lock) as success, not a failure.
`dunnage` is installed with `ketch install dunnage`; `toolchain.md` and `AGENTS.md` note it.

### T46. Rename to swarfr

The project is `swarfr` now: swarf is the chips and shavings left after machining, waste that is
not the part. The GitHub repository moved from `listepo/dunnage` to `listepo/swarfr` (GitHub
redirects the old URL). The package, library and binary are `swarfr` (`swarfr::` in code,
`cargo swarfr` through a `cargo-swarfr` link), and so are the config file
(`~/.config/swarfr/config.toml`), the hash index (`~/.cache/swarfr/hashes-v1.bin`), the
temporary-file prefix (`.swarfr-tmp-`), the launchd label (`dev.swarfr.daemon`), the systemd
unit (`swarfr.service`), the log (`~/Library/Logs/swarfr.log`) and the release assets. The tool
read no environment variables of its own, so there is no old name to keep as a fallback. Nothing
migrates by itself: a v0.1.0 install keeps its `dunnage` paths until they are moved or removed
by hand. The entries above keep the old name; they describe what was done then.
