# Beyond cargo: do the same passes work for other build systems?

A desk study, not a measurement. Nothing here was run against a real C++, Go or .NET tree; every
"yes" below means "the mechanism allows it", and each ecosystem would still need its own spike of
the kind `docs/spike/` holds for cargo before a line of code is promised. Claims that rest on
memory or on a single web source are marked *unverified*.

## What in this codebase is cargo-specific

Less than the name suggests. The engine never learned what a crate is:

| Generic today | Cargo-specific today |
| --- | --- |
| `src/sys/` — clone, compress, identity, per-filesystem probe | discovery: `CACHEDIR.TAG` with cargo's sentence (`eco::cargo::is_target`) |
| `src/model.rs` — the inode model, hardlink groups | unit of work: a profile dir, found by its `.cargo-lock` or `.cargo-build-lock` |
| `src/engine.rs` — plan / apply, re-check, temp + `rename`, mtime and mode kept | build lock: `flock` on `.cargo-lock` / `.cargo-build-lock`, and `.package-cache` for the cargo home |
| `src/index.rs` — hash cache keyed by `(dev, ino, size, mtime)` | `incremental`, `doc`, `toolchains`, `advise`, `cargo_home` |
| `src/dedupe.rs`, `src/compress.rs` — the fused lossless pass | the freshness oracle in `tests/common/mod.rs` |
| families by git common dir; `orphans` by a missing worktree record | what `seed` leaves behind (`incremental/`, lock files) |
| `evict` — idle and size-cap rules over whole dirs | "last built" = newest mtime among a profile dir's top-level entries |

So another build system is an *adapter* answering six questions, not a second engine:

1. **Discover** — which marker proves a dir is this build system's output and nobody's sources?
2. **Lock** — is there a lock the build holds that an outsider can `try_lock`? If not, what
   replaces it?
3. **Freshness** — what does the build system compare: mtime, content hash, inode, ctime?
   Does a file replaced by an identical one with the same mtime stay fresh?
4. **Volatile paths** — what is rewritten by every build and not worth touching?
5. **Owner** — which source dir does this build dir belong to (for `orphans`), and when was it
   last used (for `evict`)?
6. **Oracle** — which command proves, in a test, that nothing went stale?

Question 2 is the hard one. Cargo is unusual in holding a plain advisory lock for the whole
build. Where there is none, the engine's re-check of `(size, mtime)` right before each `rename`
narrows the race but does not close it: a compiler that writes a path between the check and the
rename has its output replaced by the older bytes. With mtime-based build systems that is
self-healing (the restored file is older than its inputs, so it is rebuilt), but it is a
different safety claim from the one `DESIGN.md` makes, and it must be stated as such.

## Verdicts

| Ecosystem | `compress` | `dedupe` | `seed` | `orphans` / `evict` | Worth doing |
| --- | --- | --- | --- | --- | --- |
| C / C++ — CMake, Meson, Ninja, Make | yes | low yield | no | yes | compress + cleanup |
| .NET — `bin/`, `obj/` | yes | **high yield**, clones only | no | yes | yes |
| Go — `GOCACHE`, `GOMODCACHE` | yes | nothing to find | not applicable | Go does it itself | compress only, low |
| Swift — Xcode DerivedData, SwiftPM `.build/` | yes | unknown | no | yes, **large** | yes, macOS-first |
| Content-addressed stores — `~/.cabal/store`, Zig cache, dune cache | yes | nothing to find | not applicable | per tool | compress only, cheap |
| JVM — Gradle, Maven | low | low | no | yes | no |
| Bazel / Buck2 output bases | yes | nothing to find | no | yes | later |
| Unreal `Intermediate/`, `DerivedDataCache/`; Unity `Library/` | likely | unknown | no | yes | needs a spike |

### C and C++

- **Discover.** `CMakeCache.txt` marks a CMake build dir and records the source dir in
  `CMAKE_HOME_DIRECTORY`; `meson-info/` marks a Meson one; `build.ninja` plus `.ninja_log` a bare
  Ninja one. Plain Make has no marker at all and builds in the source tree, so it is out.
- **Lock.** None that an outsider can rely on. Ninja and Make take no inter-process lock for the
  build (*unverified* for recent Ninja releases — check before relying on either answer). This is
  the weakest point: the adapter would need `min-age`, a look at running processes, or an
  explicit "I am not building" flag from the user.
- **Freshness.** Make compares mtimes. Ninja compares mtimes and the command hash in
  `.ninja_log`. Both survive a same-content, same-mtime replacement, which is exactly what the
  engine does. `.ninja_log`, `.ninja_deps` and `CMakeFiles/` bookkeeping are volatile paths.
- **Compress.** The strongest case: object files and static libraries carry uncompressed DWARF
  unless `-gz` was asked for, and cargo's own `.o` files went to ~5% here. PDBs compress too.
- **Dedupe.** Low. Objects embed absolute paths (`__FILE__`, DWARF `DW_AT_comp_dir`) unless the
  project passes `-ffile-prefix-map` / `/pathmap`, so two worktrees share little. Inside one
  build dir there is little to find either. **Never hardlinks**: linkers and `ar` rewrite in
  place often enough.
- **Seed.** No. `CMakeCache.txt`, `build.ninja` and `compile_commands.json` are full of absolute
  paths; a cloned build dir reconfigures and rebuilds. The tool for this job already exists:
  `ccache` with `file_clone = true` gives a new worktree a warm build and shares blocks on
  APFS / btrfs / XFS. `advise` could recommend it.
- **Orphans / evict.** Easier than cargo: the source dir is written in the cache file, so "the
  source is gone" is one `stat`. Out-of-tree build dirs that outlive their checkout are common.
- **ccache's own directory**: already zstd-compressed by ccache; nothing to win.

### .NET

- **Discover.** `obj/project.assets.json` next to a `*.csproj` / `*.fsproj` marks `obj/`; `bin/`
  is its sibling. With `UseArtifactsOutput` (.NET 8+) it is one `artifacts/` dir per repository.
- **Lock.** None. Worse on Windows: MSBuild worker nodes and `VBCSCompiler` stay alive after the
  build and keep files open, so a `rename` over them fails with a sharing violation. The engine
  would have to treat that as "busy", like a held cargo lock.
- **Freshness.** MSBuild's incremental check compares input and output timestamps, and the
  `Copy` task skips files whose size and timestamp match. Same content with the same mtime stays
  fresh (*unverified* against `CoreCompileInputs.cache` — that is what the oracle would prove:
  `dotnet build` twice, the second one must report every target as skipped).
- **Dedupe.** The best yield of any ecosystem here. Every project's `bin/` holds its own copy of
  every transitive NuGet assembly, byte-identical to the file in `~/.nuget/packages`, so a
  solution with fifty projects stores the same DLLs fifty times. **Clones only, never
  hardlinks**: MSBuild's own hardlink option (`CreateHardLinksForCopyLocalIfPossible`) is known
  to corrupt the NuGet cache, because `Copy` overwrites in place
  (https://github.com/dotnet/msbuild/issues/8273). On Windows that means ReFS / Dev Drive, which
  is `T21`; on macOS and Linux the engine could do it today.
- **Compress.** Yes; managed assemblies and portable PDBs are not compressed internally.
- **Seed.** No. `project.assets.json` and `*.csproj.FileListAbsolute.txt` record absolute paths,
  so a cloned `obj/` restores and rebuilds. A project's own assemblies also differ between
  worktrees unless `PathMap` / `ContinuousIntegrationBuild` is set.
- **Orphans / evict.** Yes: an `obj/` whose project file is gone is an orphan.

### Go

Go has no per-project build dir. `GOCACHE` is one content-addressed store per user, the `go`
command trims entries unused for five days on its own, and `GOMODCACHE` is a read-only tree of
module sources.

- **Dedupe, seed, orphans, evict:** nothing to do. The store is already deduplicated by
  construction, a new worktree is warm from the first build, and eviction is built in.
- **Compress:** the one pass that applies. Entries are immutable once written — the name is the
  hash of the content — so a same-content replacement is safe even without a lock, and
  `min-age` keeps the pass away from what a running build is writing. `GOMODCACHE` is the
  equivalent of `--cargo-home`: extracted sources, which went down 69% for cargo. Its dirs are
  read-only on purpose, so the pass has to lift and restore directory modes.
- Done: `GOCACHE` through `--store` (T33); `run --go` finds both caches and compresses the
  module cache with its dirs' write bit lifted one batch at a time (T35, `DESIGN.md`, "Go module
  cache"): 146 MiB to 105 MiB on a real one.

### Swift and Xcode

The most promising target after cargo, because the tool was built for macOS and APFS first.

- **Discover / owner.** Each `~/Library/Developer/Xcode/DerivedData/<Name>-<hash>/` holds an
  `info.plist` with `WorkspacePath` — the source it belongs to — and a last-accessed date
  (*unverified* for current Xcode). That makes `orphans` and `evict` direct. SwiftPM's `.build/`
  sits in the package, like `target/`.
- **Lock.** SwiftPM refuses a second instance on the same `.build/`, so it holds a lock an
  outsider can test (*unverified* which file and which call). Xcode's build system: unknown.
- **Size.** DerivedData is routinely tens of GB, and the usual advice is to delete it whole.
  Nothing lossless exists for it.
- **Compress:** yes. **Dedupe:** unmeasured — `ModuleCache.noindex` and per-project copies of the
  same package builds look promising. **Seed:** no, the dir name is a hash of the workspace path.

### Content-addressed stores

`~/.cabal/store`, Zig's `.zig-cache` and global cache, dune's shared cache, Nix-like stores.
Immutable entries under hashed names: `dedupe` finds nothing, `compress` is safe without a lock
for the same reason as `GOCACHE`. One generic mode — "compress every file older than `min-age`
under this dir, keep mtime and mode" — covers all of them and needs no adapter. It must leave
alone stores that compress themselves (ccache, sccache).

### JVM

`.jar` files are zip archives and `.class` dirs are small; `~/.gradle/caches` cleans itself.
Gradle snapshots inputs and outputs by content hash, so replacements are safe, but there is
little to win. Deleting idle `build/` dirs is what `kondo` already does.

### Bazel and Buck2

One output base per workspace under `~/.cache/bazel`, named by a hash of the workspace path,
with the workspace path recorded inside (*unverified*: `DO_NOT_BUILD_HERE`) — so `orphans`
works. Bazel tracks outputs by inode and ctime as well as digest, so every replaced file is
re-hashed on the next build: safe, but a cost cargo does not have. Later, if at all.

## What exists already

| Tool | Ecosystems | Lossless | Knows about build locks or freshness |
| --- | --- | --- | --- |
| `kondo`, `devclean`, `npkill` | many | no — deletes whole dirs | no |
| `cargo-sweep`, `cargo-cache` | cargo | no | no |
| `jdupes`, `rmlint`, `fclones`, `duperemove`, `hyperspace` | any files | yes, clones or hardlinks | no |
| `ccache`, `sccache` | C/C++, Rust | caches, compressed | their own store only |
| `applesauce`, `compsize`, `compact.exe` | any files | yes, compression | no |

The gap is the same in every ecosystem: generic dedupers and compressors do not know when a
build is running, which files a build is about to rewrite, or that a hardlink into a build dir
is a hazard; build-aware cleaners only delete. No tool combines the two outside cargo either.

## If this were built

1. Split the crate: a generic core (everything in the left column of the first table) and
   cargo as the first adapter on top of it. No behavior change, and the existing suite is
   the proof.
2. An adapter trait with the six questions above; the cargo one is extracted from `model.rs`,
   `inventory.rs` and `engine.rs`'s lock step.
3. A "no lock" safety tier, named as such in the report: `min-age`, busy-on-sharing-violation,
   and a process check, for build systems that have no lock to take.
4. First adapters by value over effort: Xcode DerivedData / SwiftPM (APFS, large, owner
   recorded), then .NET (`dedupe` yield), then CMake (`compress` + `orphans`), then the generic
   immutable-store mode.

How the pieces fit — one library under a CLI and a daemon, the adapter trait, guard tiers,
discovery, and what a monorepo changes — is in `docs/architecture.md`. In `plan.md`: T36 (the
library boundary), T28 (the adapter boundary), T29 (the no-lock tier), T30 Swift, T31 .NET, T32
C / C++, T33 the immutable-store mode, and T35 Go, which rides on T33 and has the lowest
priority.
