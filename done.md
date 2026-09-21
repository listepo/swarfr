

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

