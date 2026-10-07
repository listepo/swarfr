# Benchmarks

What the passes cost and what they win, measured on a copy of a real workspace. The numbers
below are one run of `scripts/bench.sh`; a second run with the same script produced the same
sizes and the same freshness result, and is quoted where the two disagree. The raw
`key<TAB>value` lines and the hyperfine JSON stay in the work dir the script prints at the end.

## How to reproduce

```
scripts/bench.sh ~/code/your-workspace                # RUNS=5 WITH_SCCACHE=1 by default
scripts/bench-cargo-home.sh                           # the cargo home, on a clone of it
```

`WITH_ACROSS=0` skips the second independent clone, `WITH_SCCACHE=0` the sccache comparison, and
`MIN_FREE_GIB` is the free-space guard the script refuses to run under.

The script shallow-clones the workspace into a temp dir and adds one git worktree of that clone,
so the two checkouts form a family and dedupe has siblings to compare. It builds offline after a
single `cargo fetch`. **The workspace's own `target/` is never touched** — the tool only ever
sees the copies. Every measurement uses `--min-age 0`, because everything just built is younger
than the default age floor.

Per stage it records the tool's wall clock, target size before and after (`du -sk`), free space
on the volume before and after (`df -k`), how many units cargo rebuilds afterwards, and the mean
of `RUNS` incremental builds after touching the top-level crate (`hyperfine`, 3 warmups).

`du` counts every block a file is charged with, so it sees compression but **not** copy-on-write
sharing. For dedupe, only the free-space delta is the truth.

## The workspace

A private workspace: 587 crates in the lock file, two checkouts of one family. Apple Silicon, APFS,
stable Rust 1.98, nothing else running.

| | checkout `a` | checkout `b` |
| --- | --- | --- |
| clean build | 61.6 s | 73.4 s |
| target after that build | 1.77 GiB | 1.77 GiB |

## What the passes win

Sizes are the two targets together, taken immediately before and after each pass.

| Pass | `du` before | `du` after | `du` delta | Free space delta |
| --- | --- | --- | --- | --- |
| compress | 3.63 GiB | 1.32 GiB | **−2.31 GiB (−63.7%)** | +1.0 GiB |
| dedupe (after compress) | 1.48 GiB | 1.48 GiB | 0 | **+407 MiB** |

Read the two columns differently, as above: compression shows up in `du`, sharing does not.
Together the passes take 3.63 GiB of freshly built targets down to about 0.93 GiB of blocks
actually on disk — a **74%** cut, on targets an age-based cleaner would not touch at all because
every file in them is minutes old.

The dry runs, taken on the pristine targets before anything was rewritten, predicted 3.61 GiB
compressible and 1.08 GiB duplicated. Dedupe then runs on already compressed files, which is why
it recovers 407 MiB rather than the full gigabyte.

## Linux, btrfs

A second machine, a different workload, and the reason the table above has a twin: on btrfs the
win does not show up where macOS shows it. Measured in a Linux VM (Ubuntu 24.04, 4 cores, a
6 GiB btrfs loopback image mounted with default options) on an unshared copy of a real cargo
target — `swarfr`'s own, 1.33 GiB, one checkout and therefore no family for dedupe to
compare against.

| Pass | `du` before | `du` after | `du` delta | Free space delta | Wall clock |
| --- | --- | --- | --- | --- | --- |
| compress (1553 files) | 1.33 GiB | 1.34 GiB | **0** | **+818 MiB** | 9 s |
| dedupe (14 files) | 1.34 GiB | 1.34 GiB | 0 | +76 KiB | <1 s |

**On btrfs, read only the free-space column.** btrfs reports the *uncompressed* size in
`st_blocks`, so `du` cannot see compression there and neither can the tool: the run above prints
`applied 1553 (0 bytes)` while the volume gained 818 MiB. That zero is the platform telling the
truth about what it measures, not a pass that did nothing — `compsize` per file and `df` per
volume are where the win is visible. On APFS the same figure is real, which is why
`sys::ALLOCATED_SHOWS_COMPRESSION` exists and why the A/B tests branch on it.

The dedupe row is small because this target has no sibling: the pass only had one checkout to
work with, so the 407 MiB of the macOS run has no counterpart here. Sharing itself works —
`tests/caps.rs` asserts the clone happens on a filesystem that can, and nothing is planned on
one that cannot.

ext4 is the other side of the same measurement: `caps` finds neither capability there, both
lossless passes plan nothing, and `status` says so in a line under the target. Making ext4 win
anything needs the link fallback, which is `T22`.

## Windows, NTFS (a fixture, not a workspace)

No workspace bench has been run on Windows. A development build is never pointed at a real
`target/`, and this section does not invent one. What was measured is the compression unit
test on an NTFS volume: a 256 KiB file filled with one repeated byte (`0x09`), allocated size
from `GetCompressedFileSizeW`.

| File | Logical | Allocated after LZNT1 |
| --- | --- | --- |
| 256 KiB of one byte | 262144 | 16384 (6.25%) |

`ALLOCATED_SHOWS_COMPRESSION` is true here, so `du` and the tool's `freed_bytes` both see that
drop. A file of one repeated byte is the best case LZNT1 can show; real object files will do
worse, and that number is not a prediction for a target dir. Bytes that do not shrink below 95%
of their logical size are put back to uncompressed. WOF / LZX is a different API and is not what
this measures (`ideas.md`).

ReFS block cloning is implemented and unmeasured on this machine: `TEMP` is NTFS, so
`clone_file` returns `Unsupported` and leaves no copy. The same tests assert a real clone when
`TEMP` points at a ReFS volume.

## What the passes cost

| | wall clock |
| --- | --- |
| compress, both targets (3.6 GiB) | 81.7 s |
| dedupe, both targets | 12.4 s |
| both again, right afterwards | 2.7 s |

Incremental build after touching the top-level crate, mean of 5 runs:

| | mean | range |
| --- | --- | --- |
| baseline | 3.70 s | 2.39 – 5.10 s |
| after compress | 4.96 s | 2.78 – 10.17 s |
| after compress + dedupe | 3.53 s | 2.77 – 5.14 s |

The means are inside each other's spread, so the honest reading is: **no measurable slowdown**.
The one real effect is the first build right after a pass — 10.17 s, then 2.8 s for the rest of
the runs — because the pass rewrote every file and the page cache is cold. The first run of the
whole benchmark, with a single warmup, showed the same picture with much more noise (baseline
28 s falling to 5 s over five runs), which is why the script now warms up three times.

## Does anything get rebuilt afterwards?

No. After each pass, `cargo build --message-format=json` reported **0** units not fresh, in both
runs, on all 587 crates. That is the point of the design: mtimes are preserved, hardlink groups
stay groups, and cargo cannot tell that the bytes moved.

## A second run is still needed

Right after both passes, with nothing rebuilt in between, running the tool again still applied
**46** more actions (2.7 s). Dedupe's clones are new files that compress had never seen, so one
pipeline run does not reach a fixed point. Nothing is lost by it — the next scheduled run picks
them up — but a `run` that loops until it stops finding work would finish the job in one go.

Since T25 `run` does loop: it repeats the passes on a group until a round applies nothing. Not
measured again on this workspace yet; the fixture in `tests/settle.rs` is too small to show the
second round at all.

## Across families

`--across-families` compares every target under the roots instead of one repository at a time.
`scripts/bench.sh` measures it with a third checkout that is a **second independent clone** of
the repository, not another worktree: its own `.git`, so its own family, holding the same
dependencies built the same way — two unrelated projects, as far as the tool is concerned.

This part was measured on **`swarfr` itself** rather than on that workspace: the machine had
10 GiB free at the time and three checkouts of it do not fit under the script's own
free-space guard. The targets are therefore an order of magnitude smaller, and only the ratio is
worth reading.

| | |
| --- | --- |
| the third clone's target, freshly built | 351.8 MiB |
| deduped inside its own family first | nothing left to share |
| then `--across-families` over all three | **+172.6 MiB free** in 1.9 s |
| units not fresh afterwards, in either checkout | **0** |

About half of a freshly built target was already on the disk, in a project that has nothing to
do with it. That is the case one run per family cannot reach, and it is the whole argument for
the flag. The argument against it is in the same numbers from the other benchmark: the run holds
every target's build locks for its whole length, which on a 587-crate workspace is over a minute
of no builds anywhere, so it stays opt-in.

For context, from the same run: within one family (two checkouts of one repository) dedupe freed
67 MiB after compression had already run.

## The cargo home

`scripts/bench-cargo-home.sh` (`just bench-home`) measures `--cargo-home` the same way, on a
**clone** of this machine's `~/.cargo`: `cp -c -R` of `registry` and `git` into a temp dir, which
on APFS costs no space and only ever reads the real home. The tool is pointed at the clone.

| | `du` before | `du` after | delta |
| --- | --- | --- | --- |
| `registry/src` (unpacked sources) | 1.52 GiB | 469 MiB | **−1.06 GiB (−69.2%)** |
| `registry/cache` (`.crate` archives) | 213.6 MiB | 213.6 MiB | 0, never touched |

44.5 s for 1.52 GiB, one `.package-cache` lock for the whole pass. The sources compress better
than a target does (69% against 64%): they are almost entirely text, while a target is mostly
object files that already carry incompressible sections.

Then the question the pass lives or dies by — does cargo unpack anything again? The script builds
a crate from the clone (`libc-0.2.189`), runs the pass, deletes the build target and builds
again with `--offline`:

| | |
| --- | --- |
| `.cargo-ok` inode, mtime and size after the pass | unchanged |
| units not fresh on the build after the pass | **0** |

Nothing is re-extracted and nothing is rebuilt, which is the same result the fixture test asserts
file by file. What the compression backend refused is the usual tail: test fixtures and images
inside the crates (`tests/images/`, `res/`), reported as "not compressible enough".

`git/checkouts` was 0 here — this machine has no git dependencies — so that half is covered by
the fixture test only.

## A content-addressed store: `GOCACHE`

`--store` on a `GOCACHE` of its own: `go build std` (go 1.27.1, darwin/arm64) into an empty
cache in a temp dir, every file's mtime moved back past the store's one-hour floor, then
`swarfr run --store <cache>` from a release build. APFS.

| | before | after | delta |
| --- | --- | --- | --- |
| `du` of the cache (2659 files) | 215.6 MiB | 63.6 MiB | **−152 MiB (−70.5%)** |
| files compressed | | 368 of 2659 | the rest are under compress's 8 KiB floor |
| wall time of the run | | 2.65 s | |

Oracle, the store's own invariant: all 1131 data entries (`*-d`) still hash to their names under
SHA-256, and `go build -x std` afterwards runs no `compile` step (0.55 s). `tests/store.rs`
checks the same on a small module whenever `go` is installed.

## A Go module cache

A copy of this machine's `GOMODCACHE` (24 modules, 9,343 unpacked files; go 1.27.1,
darwin/arm64) in a temp dir, `run --go` from a release build with `GOMODCACHE` and `GOCACHE`
pointing at the copies. APFS.

| | before | after | delta |
| --- | --- | --- | --- |
| `du` of the module cache | 145.9 MiB | 104.7 MiB | **−41 MiB (−28.3%)** |
| files compressed | | 1288 of 1293 planned | the rest not compressible enough |

The zips in `cache/download` (35 MiB of the total) are left alone, so the unpacked sources
alone went from about 111 MiB to 70. Oracle: every module's `h1:` dir hash still equals the
`.ziphash` `go` recorded when it unpacked it, every file keeps its mode and mtime and every dir
its mode, and a module built against the copy rebuilds with no `compile` step and passes its
tests. Without the lift, the same run skips every file with `PermissionDenied`.

## A SwiftPM package

swift-argument-parser (shallow clone of `main`), Swift 6.4 on macOS 27, APFS: `swift build` and
`swift build -c release` into an empty `.build` in a temp dir, then
`swarfr run --min-age 0 <package>` from a release build.

| | before | after | delta |
| --- | --- | --- | --- |
| `du` of `.build` | 356.7 MiB | 157.4 MiB | **−199 MiB (−55.9%)** |
| compress | | 1684 files, 199.2 MiB freed | |
| dedupe | | 43 files, 1.1 MiB freed | debug and release share little |

Oracle: `swift build -c release -v` afterwards runs no compile task, as it did not before the
run, and the `math` example still adds. The largest share is `SDKExplicitPrecompiledModules` and
`ModuleCache.noindex`, per package copies of SDK modules: more packages on one machine would
give dedupe more to share, not measured yet.

## A CMake project

fmt (shallow clone of `master`, `6d71f74`), AppleClang on macOS 27, APFS: configured with
`-DCMAKE_BUILD_TYPE=Debug -DFMT_TEST=ON`, Unix Makefiles, `cmake --build -j 8`, every file then
moved back two days past the quiet tier's floor, and `swarfr run --min-age 0 build` from a
release build.

| | before | after | delta |
| --- | --- | --- | --- |
| `du` of `build` | 157.7 MiB | 52.3 MiB | **−105 MiB (−66.8%)** |
| compress | | 158 files, 105.4 MiB freed | |
| dedupe | | 16 files, 2.5 MiB freed | |

Oracle: `cmake --build build -j 8` afterwards prints no `Building` or `Linking` line, and all 23
tests pass under `ctest`.

## Discovery in a monorepo

A synthetic monorepo of 320,030 files: 60 × 50 source dirs of 100 empty `.rs` files each, and 10
cargo targets of 2,000 artifacts at `src/m{0..9}/p0/target`, already settled by a first run.
Release build, warm page cache, `hyperfine -N --warmup 1`.

| re-run of a settled tree | mean |
| --- | --- |
| `run mono`, the roots walked (before, and `--rediscover` now) | 591 ± 34 ms |
| `run` naming the 10 targets | 369 ± 39 ms |
| `run mono`, build dirs from the last walk | **311 ± 8 ms** |

The walk was about 40% of a run that had nothing left to do; the list takes it out, and a run
over the whole tree costs what naming each target by hand did.

## sccache, for comparison

| | clean build | target | cache |
| --- | --- | --- | --- |
| plain | 61.6 s | 1.77 GiB | — |
| `RUSTC_WRAPPER=sccache`, cold cache | 83.5 s | 1.46 GiB | 304 MiB |
| `RUSTC_WRAPPER=sccache`, warm cache | 32.3 s | 1.46 GiB | 304 MiB |

sccache answers a different question: it makes a *rebuild from scratch* about twice as fast, at
the price of a slower first build and a 304 MiB cache of its own. It does not shrink a live
target — its targets are smaller here only because a wrapper turns cargo's incremental
compilation off. The two are complementary, and nothing in `swarfr` conflicts with it.

## What the defaults are worth

- `min-size` 8 KiB for compress, 4 KiB for dedupe: a file smaller than a block cannot win a
  block. These numbers were not swept on this workspace — compression's win is so large that the
  floor only decides how many tiny files are walked for nothing.
- `min-age` 1 h: not a size decision at all. It keeps the tool away from files a build may still
  be writing, and this benchmark had to set it to 0 to measure anything. Leave it alone unless
  you are benchmarking.

## Not measured

- **.NET.** The expected win is `bin/` copies of NuGet assemblies, and the NuGet cache here is
  empty; nothing was downloaded to fill it. The two-app fixture of `tests/dotnet.rs` is a few
  hundred KiB: it proves the passes safe (the next `dotnet build` copies and compiles nothing),
  not what they are worth.

- **Seeded worktree** — not measured yet.
- **Shared `build-dir`** — cargo's `build.build-dir` is nightly-only (`-Z build-dir`); this
  machine builds on stable, where the key is ignored. T12's `advise` reports exactly that.
- **One workspace, one machine.** Every number above is that one workspace on one Apple Silicon laptop.
  The compression ratio depends on what the crates emit; the build-time result should not.
