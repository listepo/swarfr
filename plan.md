# swarfr

https://github.com/listepo/swarfr

A tool (a CLI and a daemon) that shrinks live `target/` directories without slowing
builds: transparent filesystem compression, copy-on-write dedupe across targets, clone-seeding
of new worktrees, and opt-in removal of orphaned or idle targets — planned together so the
approaches reinforce each other. Called `cargo-tare` until T42
and `dunnage` until T46. Design in `DESIGN.md`, measurements in `docs/research.md`.

## Cloud review findings (2026-10-08)

New bugs, dead code and moves from a read-only Cursor cloud review of `main` at `ff84e5f` (agent `bc-16f3d65c-2589-5440-9fe9-dd0f5b59b28b`; full report: `cloud/swarfr.md` in the private `listepo/roadmap` repo). They take ids T54–T66, ordered P0, P1, P2. **confirmed** means seen in the tree or reproduced; **suspected** means plausible from the code but not proven (nothing was run on Windows or macOS). Line numbers are as of the review. None of these is in the task table or `todo.md` yet: to take one, add its row and card the usual way.

| ID | Priority | Kind | Status | Where | Fix |
| --- | --- | --- | --- | --- | --- |
| T54 | P1 | bug | confirmed (reproduced) | `src/engine.rs:835-837`; `src/eco/cargo/mod.rs:151-160`; `src/daemon/mod.rs:43-50`, `:136-152`; `src/sys/mod.rs:85-100` | swarfr poisons its own `last_built`: a rename into `deps/` updates that directory's mtime, and `last_built` is the newest child mtime, so a lossless run makes a week-old profile look built now. Idle evict/incremental never trigger and the daemon sees a new build on every visit. Restore the top-level entry mtimes after apply (`probing_in` restores only the profile dir). |
| T55 | P1 | bug | confirmed (mechanism; no wrong answer shown) | `src/sys/unix.rs:142-164`, `:170-178` | `caps_of` caches the first probe per device: one failed write probe (read-only dir, full disk) stores `Caps::NONE` for the whole filesystem, and a writable first probe makes later read-only units look capable. Do not cache a failed write as a filesystem capability. |
| T56 | P1 | bug | confirmed (code only) | `src/sys/windows.rs:70-72`, `:92-94`; contract test `src/sys/mod.rs:152-162` | On Windows `caps()` is `NONE` but `clone_file` is `fs::copy` and succeeds, while the contract test needs `Err` when `!caps.clone`. Return `ErrorKind::Unsupported`. Shows up once T24 gives Windows a test run. |
| T57 | P1 | bug | confirmed | `src/eco/cargo/mod.rs:23-24`, `:126-148` | `profile_dirs` looks only for `.cargo-lock`, but Cargo 1.97+ `build.build-dir` units use `.cargo-build-lock` (MSRV is 1.98). Those dirs are claimed with zero units, so locks, compress, dedupe, evict and seed never run there. Look for `.cargo-build-lock` too. |
| T58 | P2 | bug | confirmed (reproduced) | `src/inventory.rs:264-266` vs `src/orphans.rs:93-95` | `is_project_gone` treats any `symlink_metadata` error as gone, so an unsearchable project prints `PROJECT GONE`. Check for `NotFound`, as orphans do. |
| T59 | P2 | bug | confirmed | `src/main.rs:596-606`; `src/seed.rs:24-27`, `:177` | `seed` always prints "that the clones share with it", even when `shared_blocks` is false and the bytes were copied (e.g. ext4). Print the byte-copy case. |
| T60 | P2 | bug | suspected | `src/sys/mod.rs:62-70`; tools in `src/eco/cmake.rs:68-69`, `src/eco/dotnet.rs:87-88` | `tool_running` matches a tool whose cwd is any ancestor of the unit, so a `make` or `dotnet` running in `$HOME` marks every unit under it busy. Stop the walk at the project/owner. |
| T61 | P2 | bug | suspected | `src/eco/swiftpm.rs:75-80`; `src/sys/mod.rs:75-83` | The SwiftPM lock path uses swarfr's own temp dir, so a `swift build` started with a different `TMPDIR` is missed; `lock_name` also rewrites only `/`. Use Swift's temp dir, or document the requirement. |
| T62 | P2 | bug | suspected | `src/engine.rs:150-158`; `src/main.rs:644` | With `--until-settled` (the `run` default), `absorb` adds later rounds' `applied`/`freed_bytes` to `planned` again. Count each inode once. |
| T63 | P2 | dead code | confirmed | `src/config.rs:136-138` (`Config::skips`); `src/inventory.rs:91-96` (`inventory::discover`); `src/index.rs:135-141` (`HashIndex::len`/`is_empty`) | Used only by tests. Delete them or put them under `#[cfg(test)]` (keep `HashIndex` methods if they are library API). Every `Cargo.toml` dependency is used. |
| T64 | P2 | move | confirmed | `src/session.rs:882-926` (`git`, `git_command`) → a shared crate such as crates-packages `git-changed-paths` | Its own comment (`:884-887`) says the three copies must not drift (`GIT_OPTIONAL_LOCKS=0`, missing-git error, strict UTF-8). |
| T65 | P2 | move | confirmed | `last_built` in `src/eco/cargo/mod.rs:151-160` → an `eco` helper | CMake, .NET and SwiftPM call it too (`cmake.rs:63-64`, `dotnet.rs:80-81`, `swiftpm.rs:87-88`). Move it first so T54's fix lands in one place. |
| T66 | P2 | move | suspected | `scripts/dist-generate.sh`, `scripts/release.sh` → a `pyrlyn/ci` reusable release step | Only if they really are near-copies of the rtok/runa scripts (not verified). `bump.yml` stays local. |

Already tracked here, not added again: deleting a profile while the engine still holds its open `.cargo-lock` (`src/engine.rs:359-361`, `:238-253`, `:509`), which fails on Windows, is T49.

Not added: retargeting `ketch.toml`/`Cargo.toml` URLs to `pyrlyn/swarfr` applies only if the repo moves.

| # | Status | Priority | Complexity | Readiness | Agent |
| --- | --- | --- | --- | --- | --- |
| T43 | in progress | P1 | 3 | 95% | Claude Code / opus-5.5 |
| T24 | todo | P1 | 3 | 0% | |
| T21 | todo | P2 | 5 | 0% | |
| T30.1 | todo | P2 | 3 | 0% | |
| T32.1 | todo | P2 | 2 | 0% | |
| T53 | todo | P3 | 2 | 0% | |

Blockers, take these first. **T24** blocks T21: nothing on Windows can be tested without it.

Decisions the plan is built on, all the creator's: the tool runs as a CLI **and** as a daemon
with as much shared code as possible, and must stay embeddable as a library in a build system —
not built now, R8 in `roadmap.md` (`docs/architecture.md`, "Process model"); Windows is tested
in a local VM, not in CI (T24); publishing waits (R7).

Where the tasks came from: `ideas.md` read against `docs/usage.md` (what a user trips over
today), `docs/ecosystems.md` (which build systems the engine fits — a desk study, so each
ecosystem task starts with a spike, and a spike that says "not worth it" closes the task with
that finding in `docs/research.md`) and `docs/architecture.md` (the monorepo tasks T37–T40).

### T43. Release pipeline: GitHub releases with binaries

Decided by the creator: release swarfr the way `rtok` does. Split out of R7, which keeps
crates.io and the tap's own sync workflow. Done: merging a `release: vX.Y.Z` pull request, or
running Actions → **Bump and release**, gates on the CI checks, tags `vX.Y.Z` and publishes a
GitHub Release with signed macOS, Linux and Windows archives, a shell installer, a self-updater
and a Homebrew formula asset — and `docs/release.md` says how.

Creator's answers: cargo-dist, not ketch's hand-written workflow; macOS signing on, from
`MACOS_CERTIFICATE` / `MACOS_CERTIFICATE_PWD` (the creator sets them, with `RELEASE_PLZ_TOKEN`);
Homebrew as a release asset only — nothing touches `listepo/homebrew-tap` here.

Plan:

1. `dist-workspace.toml` (dist 0.32.0, three targets, shell + homebrew installers, updater,
   `dispatch-releases`, `macos-sign`), `[profile.dist]` and `repository` in `Cargo.toml`.
2. `.github/build-setup.yml`: cargo cache and the codesign identity discovered from the cert.
3. `scripts/dist-generate.sh` generates `release.yml` and maps `CODESIGN_*` to `MACOS_*`.
4. `scripts/release.sh`, `bump.yml`, `release-plz.yml` + `release-plz.toml` (`git_only`, so the
   pull request raises the version from the tags), `cliff.toml`, `CHANGELOG.md`.
5. `ci.yml` (pull requests) and `verify.yml` (the same gate before a release): `just check` on
   Linux and macOS, `just check-cross` for Windows.
6. `mise.toml` pins just, git-cliff, cargo-dist; `Justfile` recipes; `toolchain.md`;
   `docs/release.md`; the install section of `docs/usage.md`.

Verify: `dist plan` lists the three archives, the installer and the formula; `scripts/release.sh
patch --dry-run` prints `v0.1.0`; `ci.yml` is green on the pull request.

The first CI run of the repository found test faults the local machine hides; the creator chose
to fix them here, until `ci.yml` is green: an import only macOS uses (Linux clippy), the fixture
inheriting `CARGO_INCREMENTAL=0` from rust-cache, two clone-only tests with no clone guard
(ext4), and the SwiftPM oracle reading "Compil" in `swift build -v`, which the CI images' Swift
never prints — it now watches the compile outputs under `.build` instead.

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

### T30.1. Swift: Xcode DerivedData

Split off from T30. `~/Library/Developer/Xcode/DerivedData/<name>-<hash>/`, with `info.plist`
recording `WorkspacePath`, which makes `orphans` and `evict` direct. No lock: needs the quiet
tier (T29), with `xcodebuild`, `XCBBuildService` and `SWBBuildService` as the tools. `plutil`
reads the plist without a new dependency. Needs a way to produce a DerivedData dir for tests
without writing into the real `~/Library` (`xcodebuild -derivedDataPath` in a temp dir is the
candidate; whether it writes `info.plist` there is the first thing to check). Oracle: a second
`xcodebuild` compiles nothing.

### T32.1. C and C++: Ninja and Meson

Split off from T32, which handles CMake build dirs and was verified with the Makefiles
generator only, because `ninja` and `meson` are not installed here. Settle whether current
Ninja takes a lock on the build dir, claim Meson build dirs (`meson-private/`, whose
`coredata.dat` records the source dir), and add the oracle `ninja -n` plans nothing after a
pass. Needs the creator's approval to install `ninja` and `meson` (brew or mise).

### T53. Split Session::run's pass construction out of the method

Split from T52 when it was claimed. `Session::run` (`src/session.rs:549-847`, ~300 lines) mixes request validation, store/go/home resolution, pass construction, grouping and reporting; the pass-construction block (`session.rs:633-694`) would read better as its own function. Done means: the extraction lands with no behavior change and the suite stays green.
