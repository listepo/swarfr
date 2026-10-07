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
| T30.1 | todo | P2 | 3 | 0% | |
| T32.1 | todo | P2 | 2 | 0% | |

Blockers, take these first.

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

The suite now runs on a Windows host (NTFS): compression, the hardlink fallback, path prefixes
and the freshness oracle. This task is still the local VM the creator asked for, so the same
suite can run from the Mac the way the lima VM runs the Linux half, and so ReFS has a volume
to point `TEMP` at.

Decided by the creator: a **local Windows VM**, not CI — the same choice as the lima VM for T20.
On Apple Silicon the guest is Windows on ARM, so `aarch64-pc-windows-msvc` joins `check-cross`
and is the target the suite actually runs on. Two volumes inside the guest: NTFS (the system
drive will do) and a ReFS Dev Drive made from a VHDX, with `TEMP` / `TMP` pointed at the volume
under test, the way `TMPDIR` picked btrfs or ext4 in the Linux VM. The VM software and how the
repository gets into the guest are settled with the creator when the task is claimed; nothing is
installed on the creator's machine without asking.

Done: the existing suite runs on NTFS in the VM and the result is recorded; ReFS is selected by
pointing `TEMP` at a Dev Drive; `AGENTS.md` says how to bring the VM up. `clone_file` already
returns `Unsupported` where the volume cannot clone. No FFI in this task.

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
