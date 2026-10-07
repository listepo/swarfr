# swarfr

https://github.com/listepo/swarfr

A tool (a CLI and a daemon) that shrinks live `target/` directories without slowing
builds: transparent filesystem compression, copy-on-write dedupe across targets, clone-seeding
of new worktrees, and opt-in removal of orphaned or idle targets — planned together so the
approaches reinforce each other. Called `cargo-tare` until T42
and `dunnage` until T46. Design in `DESIGN.md`, measurements in `docs/research.md`.

| # | Status | Priority | Complexity | Readiness | Agent |
| --- | --- | --- | --- | --- | --- |
| T43 | in progress | P1 | 3 | 95% | Claude Code / opus-5.5 |
| T24 | todo | P1 | 3 | 0% | |
| T21 | todo | P2 | 5 | 0% | |
| T30.1 | todo | P2 | 3 | 0% | |
| T32.1 | todo | P2 | 2 | 0% | |
| T48 | todo | P1 | 3 | 0% | |
| T49 | todo | P2 | 2 | 0% | |
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

### T48. macOS caps() overclaims clone/compress on non-APFS volumes

`src/sys/macos.rs:62-74` returns `clone: true, compress: true` after only a write test — no `fclonefileat` probe (unlike `unix.rs`'s FICLONE probe) — and `clone_file` is `fs::copy` (`macos.rs:92-94`), which silently byte-copies. On HFS+/exFAT/SMB/FAT volumes, dedupe plans Replace actions whose "clones" are full copies: nothing is freed, disk briefly grows, and `freed_bytes`/`applied` are wrongly reported (`engine.rs:458-465`); the clone contract test (`sys/mod.rs:152-171`) is vacuous there. This contradicts the invariant at `sys/mod.rs:5-7`. Done means: `caps()` probes `fclonefileat` like unix.rs probes FICLONE, and the non-clone path reports honestly.

### T49. Windows lossy passes: README claim vs the held .cargo-lock

README:9-10 says Windows "builds and reports but plans no work", but `orphans`/`evict`/`incremental`/`doc` are not gated on `caps` and plan removals on any platform; and while the engine holds the `.cargo-lock` `File` open (`engine.rs:359`, dropped only at `engine.rs:509`), `remove()` (`engine.rs:245`) deletes the profile dir containing that lock — on Windows a delete-pending open file keeps its directory entry, so the removal fails and the pass reports it skipped. Done means: the README matches reality (lossy passes either work on Windows or are gated off), and the lock is released before destructive removals or the failure is handled deliberately.

### T53. Split Session::run's pass construction out of the method

Split from T52 when it was claimed. `Session::run` (`src/session.rs:549-847`, ~300 lines) mixes request validation, store/go/home resolution, pass construction, grouping and reporting; the pass-construction block (`session.rs:633-694`) would read better as its own function. Done means: the extraction lands with no behavior change and the suite stays green.
