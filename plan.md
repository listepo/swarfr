# dunnage

https://github.com/listepo/dunnage

A tool (a CLI and a daemon) that shrinks live `target/` directories without slowing
builds: transparent filesystem compression, copy-on-write dedupe across targets, clone-seeding
of new worktrees, and opt-in removal of orphaned or idle targets — planned together so the
approaches reinforce each other. Called `cargo-tare` until T42.
Design in `DESIGN.md`, measurements in `docs/research.md`.

| # | Status | Priority | Complexity | Readiness | Agent |
| --- | --- | --- | --- | --- | --- |
| T24 | todo | P1 | 3 | 0% | |
| T21 | todo | P2 | 5 | 0% | |
| T38.1 | todo | P2 | 3 | 0% | |
| T45 | todo | P1 | 3 | 0% | |
| T46 | todo | P1 | 4 | 0% | |
| T47 | todo | P1 | 4 | 0% | |

Blockers, take these first. **T24** blocks T21: nothing on Windows can be tested without it.

Decisions the plan is built on, all the creator's: the tool runs as a CLI **and** as a daemon
with as much shared code as possible, and must stay embeddable as a library in a build system —
not built now, R8 in `roadmap.md` (`docs/architecture.md`, "Process model"); Windows is tested
in a local VM, not in CI (T24); publishing waits (R7).

Where the tasks came from: `ideas.md` read against `docs/usage.md` (what a user trips over
today), `docs/ecosystems.md` (which build systems the engine fits — a desk study, so each
ecosystem task starts with a spike, and a spike that says "not worth it" closes the task with
that finding in `docs/research.md`) and `docs/architecture.md` (the monorepo tasks T37–T40).

### T24. A place where the Windows tests run

T21's first open point, made a task because everything else in T21 waits for it: `check-cross`
type-checks the Windows backend and nothing has ever run there.

Decided by the creator: a **local Windows VM**, not CI — the same choice as the lima VM for T20.
On Apple Silicon the guest is Windows on ARM, so `aarch64-pc-windows-msvc` joins `check-cross`
and is the target the suite actually runs on. Two volumes inside the guest: NTFS (the system
drive will do) and a ReFS Dev Drive made from a VHDX, with `TEMP` / `TMP` pointed at the volume
under test, the way `TMPDIR` picked btrfs or ext4 in the Linux VM. The VM software and how the
repository gets into the guest are settled with the creator when the task is claimed; nothing is
installed on the creator's machine without asking.

Done: the existing suite runs on NTFS in the VM and the result is recorded; the one contract
test known to fail there (`clone_file` is `fs::copy` where `caps` says no clones — T21's
analysis, point 2) is fixed by returning `Unsupported`; `AGENTS.md` says how to bring the VM up
and run the suite on either volume. No FFI in this task.

### T21. Windows: NTFS compression and ReFS block cloning

Compression: NTFS has per-file transparent compression through `FSCTL_SET_COMPRESSION`, and the
allocated size to measure it with comes from `GetCompressedFileSize`. Dedupe: ReFS has block
cloning (`FSCTL_DUPLICATE_EXTENTS_TO_FILE`); NTFS has no copy-on-write at all, so dedupe there is
T22's hardlink fallback, which now exists and needs only `caps` to answer honestly there. File identity is `GetFileInformationByHandle`'s volume serial plus file index,
and the build lock stays `File::try_lock`, which is already cross-platform.

`windows-sys` is approved by the creator for this task; it lands in `toolchain.md` and
`rust.md` in the same change that wires it. Done: the pass suite
runs on ReFS, NTFS reports no block sharing and falls back to T22 instead of failing, and paths
with drive letters and `\\?\` prefixes are covered by tests.

#### Readiness analysis (not an execution plan; nobody has claimed the task)

State: `just check` is green (128 tests run on macOS) and both cross targets compile, but no test has ever
*run* on Windows — `check-cross` only type-checks. Open points, in the order they bite:

1. **Where the tests run — the real blocker.** Decided by the creator: a local Windows VM, not
   CI; it is T24. On Apple Silicon that is `aarch64-pc-windows-msvc`, which `check-cross` does
   not cover today.
2. **A contract test fails on Windows today.** `sys::windows::clone_file` is `fs::copy`, and
   `a_clone_holds_the_bytes_of_its_source_or_refuses_to_pretend` demands an error where
   `caps().clone` is false. `seed` already falls back to `fs::copy` on its own, so the fix is to
   return `Unsupported` — but it shows the suite needs a first run there before any FFI.
3. **Identity comes first.** `file_id` is a path hash and `nlink` is always 1, so after T22's
   hardlink fallback links two files, the next scan sees two unrelated files and plans the same
   link again, and `compress` would replace one name of a group and break it. Real identity
   (`GetFileInformationByHandle`) must land before `caps` answers anything but `NONE`.
4. **The `sys` signatures change.** `nlink(&Metadata)` and `allocated(&Metadata)` cannot be
   answered from `Metadata` on Windows (std's by-handle accessors are unstable); both need the
   path, on all three backends. `GetCompressedFileSizeW` takes a path, so `allocated` costs no
   handle, and `ALLOCATED_SHOWS_COMPRESSION` becomes `true` on NTFS.
5. **ReFS file ids are 128-bit.** The 64-bit index from `GetFileInformationByHandle` is not
   guaranteed unique on ReFS; `FILE_ID_INFO` is. `Stamp` holds `(u64, u64)`, so either it widens
   or the id is folded — a decision for the card, since the hash index format depends on it.
6. **NTFS and ReFS never overlap.** NTFS compresses and cannot clone; ReFS clones and has no
   per-file `FSCTL_SET_COMPRESSION`. The fused dedupe + compress path therefore never runs on
   Windows, and each half needs its own oracle test. Whether NTFS should use LZNT1 at all or
   WOF / LZX is in `ideas.md`.
7. **`FSCTL_DUPLICATE_EXTENTS_TO_FILE` details.** Destination must be sized first, ranges are
   cluster-aligned except at end of file, one call moves at most 4 GiB, both files on one
   volume with matching sparse and integrity state. It must fail on NTFS, never copy.
8. **The probe.** `probing_in` and `probe_path` are `cfg(not(windows))`. Restoring a directory's
   mtime on Windows needs a handle opened with `FILE_FLAG_BACKUP_SEMANTICS`; without that, the
   probe makes idle profiles look freshly built (the bug T20 already found once on btrfs).
9. **`rename` over an open file.** Another process holding the destination without
   `FILE_SHARE_DELETE` (an editor, antivirus, a running test binary) fails the `rename` with a
   sharing violation. The engine must count that as a skipped group, not a failed run.
10. **Paths.** `canonicalize` returns `\\?\C:\…` while git prints `C:/…`; family keys, the
    `[family."…"]` config key and `seed`'s sibling search must compare equal. Target dirs also
    routinely exceed `MAX_PATH`.
11. **Bookkeeping.** `windows-sys` under `[target.'cfg(windows)'.dependencies]` with the
    `Win32_Foundation`, `Win32_Storage_FileSystem`, `Win32_System_IO` and `Win32_System_Ioctl`
    features; `toolchain.md`, `rust.md`, the README platform table, the `DESIGN.md` platform
    section, and Windows numbers in `docs/bench.md`. This is the first hand-written `unsafe` in
    the crate (Linux avoided it through `rustix`), so each FFI call wants a safe wrapper with
    its invariants written down.

Suggested split if the creator wants it smaller: (a) test environment + item 2 — now T24,
(b) identity and the signature change, (c) NTFS compression, (d) ReFS cloning, (e) paths and
docs.

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

### T45. `dunnage sweep`: after tests or builds, remove everything that is not needed

Asked by the creator: one command to call after `cargo test` or a build — `cargo test; dunnage
sweep` — that frees as much as it safely can, without naming each pass and its threshold.

What it runs, in the pipeline order of `run`, over the roots (or the config's `roots`):

- every lossy pass that removes nothing the next build needs: `orphans` (worktree gone, and a
  project whose manifest is gone), `doc`, and `tmpdir` for what other programs left in the
  temp dir;
- `evict` and `incremental` for profile dirs idle past a threshold, so the profile the tests
  just used stays whole and the next incremental build is not slowed;
- `worktrees` (T47) at 14 idle days, every one of its keep criteria applied: abandoned git
  worktrees go, checkout and build dirs together;
- `compress` and `dedupe` over everything not busy, the cargo home's sources, and Go's caches
  when `go` is installed;
- the stale `.dunnage-tmp-*` files, as every run does.

Naming the command is the opt-in, as `--lossy` is for `run`: `run` with no flags stays lossless.
It prints every removal with its reason, `--dry-run` shows the plan, `--json` reports it, and a
busy unit is skipped with exit code 2. The thresholds have defaults, a `[sweep]` table in the
config overrides them, and a flag overrides the config. What needs cargo's layout v2 — removing
single stale units inside a live profile — is R1 and stays out.

Done when: `dunnage sweep --dry-run` on the fixture roots lists every removal each pass would make
on its own with the same thresholds, `dunnage sweep` makes them, the next `cargo build` of the
profile just tested is a no-op, and `docs/usage.md` has it under Commands and Recipes.

Decided by the creator: the name `sweep`, and the default thresholds — `evict` and
`incremental` at 7 idle days, `orphans` projects at 7, `tmpdir` at 1 day, `worktrees` at 14,
`min-age` as `run`'s. It includes `worktrees`, so T47 comes first.

### T46. `dunnage watch`: optimize the artifact dirs as soon as a build or test run ends

Asked by the creator: a foreground `dunnage watch [ROOT]...` that follows the build dirs under
the roots and runs the passes after each build or test run, instead of waiting for a timer. The
daemon (T34) looks on timers; this is the filesystem-watcher trigger `docs/architecture.md`
reserved for it, and the daemon gets the same trigger.

How it works:

- A watcher (`notify`, listed in `rust.md`: FSEvents on macOS, inotify on Linux,
  `ReadDirectoryChangesW` on Windows) on the **top level** of each known unit only — a cargo
  profile dir, a SwiftPM `.build`, a .NET `obj` — not recursively: a handful of watches, never
  the source tree, so inotify limits hold in a monorepo. Discovery re-runs on the `known` cadence
  and on `--rediscover`, and the watch set follows it.
- An event marks the unit written. It becomes due once no event has come for a settle window and
  its guard is free: the build lock taken and dropped for cargo and SwiftPM, the quiet checks for
  the rest. A build still running keeps it pending, as in the daemon.
- A due unit starts one run of the same request as `run` with the same flags (or as `sweep`, with
  `--sweep`, after T45), over all roots, since dedupe and the caps need families whole. Its own
  writes are ignored: events inside `.dunnage-tmp-*` and the passes' renames do not re-trigger it.
- The spinner of T44 shows what it waits for; `--json` prints one report per run; Ctrl-C stops
  after the action in progress (`Control::stop`).

Done when: on a fixture target in a temp dir, `dunnage watch` sees a `cargo build` start, waits
while it holds the lock, runs once after it ends and not again until the next build; the next
build is a no-op; a hundred units stay within the default inotify limit on Linux (lima); and the
daemon uses the same trigger.

Decided by the creator: `watch` does not wait `run`'s hour (`min-age` keeps a file the next
build rewrites from being compressed for nothing); its floor is the settle window, 30 s, and a
`--min-age` flag raises it.

### T47. `worktrees`: find abandoned git worktrees and remove them

Asked by the creator: find the linked git worktrees nobody works in any more and remove them —
the checkout and its build dirs — without ever removing one that is still needed. Unlike every
other pass this one removes sources, so the docs' promise "only build output is ever removed"
gets its one exception, named in `README.md`, `docs/usage.md` and `DESIGN.md`.

A lossy pass, `--lossy worktrees --worktrees-idle-days N`, off unless named. `--dry-run` lists
every worktree it looked at, and for each one kept the first criterion that kept it; every
removal is printed with its reason.

**A worktree is removed only when every criterion holds.** Any one that fails, or that cannot be
checked, keeps it.

1. **A linked worktree of a repository under the roots.** It comes from `git worktree list
   --porcelain` of that repository. The main worktree, a bare repository, and a submodule's
   checkout are never candidates. A worktree outside the roots is never touched, even when its
   repository is under them.
2. **Not locked.** `git worktree lock` is how its owner says "mine": a lock with or without a
   reason keeps it (the `worktrees` skill locks every agent worktree this way).
3. **Its directory is there and readable, on a mounted volume.** A missing directory (`prunable`)
   is reported, not pruned: an unmounted disk looks the same.
4. **Clean.** `git status --porcelain` is empty: no modified, staged, or untracked file, and no
   merge, rebase, cherry-pick, revert, or bisect in progress in its git dir.
5. **No ignored file outside build dirs.** Ignored files are not in `git status`, and
   `git worktree remove` deletes them without asking. Local `.env` files, editor state and
   secrets live there. Every ignored path must lie inside a build dir an adapter claims
   (`target/`, `.build/`, `bin/` and `obj/`, …); anything else keeps the worktree, and is named.
6. **Nothing only here.** Its HEAD commit is reachable from the default branch or from a
   remote-tracking branch: merged, or pushed. An unmerged, unpushed branch is work in progress.
   A detached HEAD that no other ref reaches would become unreachable, so it keeps the worktree
   whatever else holds.
7. **Idle for N days.** No file in it, its build dirs included, modified for N days, and nothing
   in its own git dir (`.git/worktrees/<name>`: `HEAD`, `index`, `logs/HEAD`) younger either. A
   checkout, commit, `git status` refresh or build counts as use.
8. **Not in use now.** No process of the user has its current dir or an open file in it (the
   check `tmpdir` uses), no build holds a lock in its build dirs, and it does not contain the
   directory `dunnage` was started from.
9. **Not kept by the config.** A family with `skip = true`, and a new `[worktrees] keep` list of
   paths or globs, keep it.

**How it is removed.** Its build dirs go first under their own guards, as `orphans` removes
them. Then `git worktree remove` runs without `--force`, so git refuses on its own if it finds
anything the checks missed. Never `rm -rf`, never `--force`, never `git worktree prune`. The branch
is kept: removing a worktree deletes no commit.

Done when: on fixture repositories in temp dirs, each criterion has a test that keeps the
worktree when it alone fails (locked, dirty, untracked, ignored `.env`, merge in progress,
unpushed branch, orphan detached HEAD, young file, held by a process, a lock held, config keep,
missing dir), and one where every criterion holds and it is removed while its branch remains.

Decided by the creator: no default for `run` — `--worktrees-idle-days` is required with
`--lossy worktrees`, as `evict` needs its threshold; `sweep` (T45) includes the pass at 14 idle
days.

