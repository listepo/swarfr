# swarfr — user guide

How to install the tool, what to run first, and what each command does. Why it works the way it
does is in `DESIGN.md`; the numbers behind the defaults are in `docs/bench.md`.

## Install

Every release on the [releases page](https://github.com/listepo/swarfr/releases) carries
binaries for Apple Silicon macOS (signed), x86_64 Linux and x86_64 Windows. The shell installer
puts `swarfr` and its updater `swarfr-update` into cargo's bin directory:

```
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/listepo/swarfr/releases/latest/download/swarfr-installer.sh | sh
```

`swarfr-update` later fetches the newest release in place. With
[ketch](https://github.com/listepo/ketch), `ketch install listepo/swarfr` does the same. How a release is cut is in
`docs/release.md`.

The crate is not on crates.io yet. Or build it from a checkout, with the toolchain the repository
pins (`rust-toolchain.toml`):

```
git clone https://github.com/listepo/swarfr
cargo install --locked --path swarfr
```

The binary is `swarfr`; `swarfr --version` tells you it is on the `PATH`. If you prefer
`cargo swarfr <command>`, link it once under the name cargo looks for:

```
ln -s "$(command -v swarfr)" "$(dirname "$(command -v swarfr)")/cargo-swarfr"
```

What it can do depends on the filesystem under your target dirs, not on the operating system:

| Filesystem | `compress` | `dedupe` | `seed` |
| --- | --- | --- | --- |
| APFS (macOS) | yes | yes, clones | free clone |
| btrfs | yes | yes, clones | free clone |
| XFS with `reflink=1`, bcachefs | no | yes, clones | free clone |
| ext4 and the rest | no | cargo home sources only, as hardlinks | a real copy |
| NTFS, ReFS | not yet (`T21`) | not yet (`T21`) | a real copy |

`status` says which row you are on; nothing has to be configured.

## The first five minutes

Everything here is read-only until the last step.

```
swarfr status ~/code              # 1. what is there and what it weighs
swarfr advise ~/code              # 2. what makes it bigger than it needs to be
swarfr run --dry-run ~/code       # 3. what a run would do
swarfr run ~/code                 # 4. do it
```

1. `status` finds every cargo target dir under the root, groups them by repository (a *family*:
   one repository and its worktrees), and prints the size on disk, the days since the last
   build, `ORPHANED` for worktrees git no longer knows, `PROJECT GONE` for targets whose
   `Cargo.toml` is gone, and how much is still uncompressed or
   could be shared.
2. `advise` reads manifests and cargo configs and names the keys that inflate the targets —
   debuginfo levels, a missing `strip`, ignored `[unstable]` keys — with the file each one
   belongs in.
3. `run --dry-run` prints the full plan and touches nothing.
4. `run` compresses and deduplicates, repeating both until nothing is left for them. It deletes
   nothing, rebuilds nothing, and can be repeated at any time; a later run is fast because
   content hashes are cached, and finds only what builds wrote since.

Check the result with `swarfr status ~/code` again, and with `cargo build` in any of the
projects: it must report nothing to recompile.

## What is safe and what deletes

| Pass | Kind | What it does | Enabled |
| --- | --- | --- | --- |
| `compress` | lossless | transparent filesystem compression of files ≥ 8 KB older than 1 h | always |
| `dedupe` | lossless | equal files become copy-on-write clones of one copy | always |
| `orphans` | **deletes** | the whole `target/` of a worktree git no longer registers, and a target moved out (`CARGO_TARGET_DIR`) whose checkout is gone; with `--orphans-project-idle-days N`, also of a project whose `Cargo.toml` is gone and that was not built for N days | `--lossy orphans` |
| `evict` | **deletes** | profile dirs idle for N days, or the least recently built above a size cap | `--lossy evict` + a threshold |
| `incremental` | **deletes** | `incremental/` of profile dirs idle for N days | `--lossy incremental --incremental-idle-days N` |
| `doc` | **deletes** | `<target>/doc` | `--lossy doc` |

Lossless passes never change a file's content or modification time, which is all cargo looks at,
so nothing is rebuilt. A lossy pass removes only what cargo can build again, only when you name
it, and prints every removal with its reason — on `--dry-run` too. Only `target/` contents are
ever removed; sources are never touched.

Every pass takes the build tool's own lock where it has one: cargo's per profile dir, and the
one `swift build` takes for a SwiftPM package. A unit with a build running is skipped and
reported, and the exit code says so. Build systems without such a lock (.NET, CMake, Xcode) get
weaker checks instead: `DESIGN.md`, "Safety tier without a build lock".

SwiftPM packages go through the same passes as cargo targets, under the lock `swift build`
takes: `status` lists a `.build` dir next to a `Package.swift`, `run` compresses and dedupes its
build outputs (`.build/out`, or `.build/<triple>`), and `--lossy evict` or `--lossy orphans`
remove them. Dependency checkouts in `.build/checkouts` are not touched. A package built through
a symlinked `--package-path`, or with `--scratch-path`, takes its lock under another name: run
the tool when no such build is going on.

.NET projects (`bin/` and `obj/` next to a restored project file) go through the passes as
well, with no lock to take: only files older than a day are touched, a `dotnet` process working
in the project makes it busy, and dedupe never hardlinks there. MSBuild's worker nodes and the
compiler server stay alive for minutes after a build; `dotnet build-server shutdown` ends them
if they keep a project busy.

CMake build dirs (any dir holding `CMakeCache.txt`) are treated the same way: files older than
a day only, a `cmake`, `make`, `gmake`, `ninja` or `ctest` process working there makes the dir
busy, clones only. A build dir configured in the source tree itself (`cmake .`) is skipped.

## Commands

### `swarfr status [--json] [--all] [--cargo-home [DIR]] [ROOT]...`

Read-only inventory. `ROOT` defaults to the current directory; a target dir itself works too.
Build dirs are grouped by family, then checkout, with a subtotal per ecosystem; each group lists
its five largest build dirs by their place in the checkout, and `--all` lists them all.
`--cargo-home` adds the unpacked registry sources and git checkouts in `~/.cargo` (or
`$CARGO_HOME`, or `DIR`) to the report, at the price of a second walk. `--json` prints the same
as one JSON document, flat: one entry per build dir with its `ecosystem`, `checkout`, `position`
in the checkout and `guard` (`lock`, `shared`, `quiet`, `immutable`).

### `swarfr advise [--json] [ROOT]...`

Read-only findings about manifests and cargo configs, followed by what only the inventory shows:
the weight of `incremental/`, families that could share a `build-dir`, checkouts `seed` would
fill, orphaned worktrees, and units built by a toolchain you no longer use.

### `swarfr run [OPTIONS] [ROOT]...`

Plans and applies the passes, one family at a time. Without a `ROOT` it uses `roots` from the
config file.

| Option | Meaning |
| --- | --- |
| `--dry-run` | print the plan, change nothing |
| `--pass <PASS>` | run only the named passes; repeatable |
| `--lossy <PASS>` | enable a deleting pass; repeatable |
| `--evict-idle-days <DAYS>` | with `--lossy evict`: profile dirs not built for this long |
| `--evict-max-total-gib <GIB>` | with `--lossy evict`: then the least recently built, until everything fits |
| `--evict-whole-target` | with `--lossy evict`: remove the target dir itself once all its profiles went |
| `--incremental-idle-days <DAYS>` | with `--lossy incremental` |
| `--min-age <SECS>` | leave younger files alone; default 3600 |
| `--min-size <BYTES>` | leave smaller files alone; default 8192 for compress, 4096 for dedupe |
| `--cargo-home [DIR]` | also compress the cargo home's unpacked sources, under cargo's `.package-cache` lock |
| `--store DIR` | also compress a content-addressed store (`GOCACHE`, `~/.cabal/store`, Zig's `o/`); repeatable, no lock, entries older than an hour only |
| `--go` | also compress Go's caches where `go env` puts them: `GOCACHE` as a store, and the unpacked modules of `GOMODCACHE`, each read-only dir made writable only while its files are swapped |
| `--across-families` | compare targets of unrelated repositories too; holds every lock for the whole run |
| `--link-artifacts` | **hazard**: on filesystems without clones, share build artifacts as hardlinks |
| `--rediscover` | walk the roots for build dirs even if the last walk still holds |
| `--index <FILE>` | content-hash cache; default `~/.cache/swarfr/hashes-v1.bin` |
| `--config <FILE>` | another config file; it must exist |
| `--json` | the report as JSON |

`--link-artifacts` is off for a reason: rustc rewrites its outputs in place, so a build that
rewrites one linked artifact rewrites it in every target sharing the inode. Use it only for
targets nobody builds in parallel, or not at all.

In a monorepo most of a run is the walk of the source tree for build dirs. A walk is kept in
`build-dirs-v1.json` next to the index, and the next run with the same roots reuses it for an
hour (`[discovery] every-secs`) unless a root's own mtime moved. A build dir that is gone drops
out at once; a new one deeper in the tree waits for the next walk, and the run says so. Pass
`--rediscover` to walk now.

### `swarfr seed [--from DIR] [--dry-run] [--index FILE] [DIR]`

Fills the empty target of a fresh checkout from a sibling checkout of the same repository, so
the first build does not compile every third-party crate again. `DIR` is the checkout to seed
(default: the current directory); `--from` names the source checkout or target dir, otherwise
the family's most recently built target is used. A checkout that already has a target is
refused. Workspace members and path dependencies are still compiled — their absolute paths
changed — and everything else is reused.

Run in a checkout root without `--from`, `seed` fills every position a sibling can: each build
dir that some other checkout has for a project which exists here and has no build dir yet, and
each from the checkout that built *that* project most recently. A project absent on this branch
is skipped. In a monorepo with several workspaces this seeds all of them in one run; a
checkout that has nothing left to fill is an error, as before.

### `swarfr worktree add [--dry-run] [--index FILE] GIT ARGS...`

`git worktree add GIT ARGS...`, then `seed` into the new worktree in one step. Run from a
workspace inside the repository, the new worktree is seeded at the same relative path, from the
most recently built checkout; run from the checkout root, every position is seeded, as `seed`
in a checkout root does. Git's own failure is shown as is and nothing is seeded; with no
built checkout to copy from the worktree is still added and the tool says there was nothing to
seed. `--dry-run` still adds the worktree and only reports what seeding would copy. Only the
dependencies whose sources stay where they are — registry crates, a vendor dir outside the
repository — build warm; the workspace itself moved and is compiled again.

### `swarfr daemon run [--config FILE] [--index FILE] [--once]`

The passes of `run`, with no flags: the config file decides the roots and the lossy passes, and
names at least one root. The daemon walks the roots for build dirs (again every
`rediscover-secs`), and each time it looks, reads when every unit was last built. A unit built
since its last visit and older than `min-age` (a day for build systems without a lock) is due;
any due unit starts one run of the config's request, holding a build's locks for at most
`lock-budget-secs` per group. What a build kept busy stays due for the next look. Between looks
it sleeps until the next unit is due, at most `interval-secs`.

It stays in the foreground and logs to stderr; the service manager keeps it alive. `--once`
looks once, runs if anything is due, and exits. A manual `run` meanwhile is refused with the run
lock, and so is the daemon's look while a manual run goes on: it tries again at the next look.
The daemon handles no signal: killed mid-run it leaves at most `.swarfr-tmp-*` files, which the
next run removes.

State: `daemon.json` next to the hash index — the units, their last build, due and visited
times, the last run's per-pass counts and busy units. A restarted daemon starts from it.

### `swarfr daemon install [--config FILE] [--index FILE] [--print]`, `remove`, `status`

`install` writes a launchd agent (`~/Library/LaunchAgents/dev.swarfr.daemon.plist`, logging
to `~/Library/Logs/swarfr.log`) or a systemd user unit (`swarfr.service`), both at low CPU
and I/O priority, and starts it; `--config` and `--index` are passed on to `daemon run`.
`--print` only shows the unit. `remove` stops the daemon and deletes the unit. `status [--index
FILE] [--json]` says whether the unit is installed and prints the state file.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | everything planned was done |
| `1` | the run failed: bad flags, bad config, I/O |
| `2` | at least one profile dir was skipped because a build held its lock, or another run of the tool (a manual one or the daemon) held the run lock — run again later |

## Recipes

A new worktree that builds warm:

```
swarfr worktree add ../feature-x -b feature-x
```

or, for a worktree that is already there, `swarfr seed ../feature-x`.

Reclaim the worktrees an agent or a script left behind — look first, then delete:

```
swarfr run --dry-run --lossy orphans ~/code
swarfr run --lossy orphans ~/code
```

The targets of crates deleted from a checkout that is still in use, once they were not built for
a week (a crate that only another branch has looks the same, hence the wait):

```
swarfr run --lossy orphans --orphans-project-idle-days 7 ~/code
```

Keep all targets under a budget:

```
swarfr run --lossy evict --evict-idle-days 30 --evict-max-total-gib 50 ~/code
```

The registry sources as well (no target dirs needed):

```
swarfr run --cargo-home
```

A script that must tell "nothing to do" from "a build was in the way":

```
swarfr run ~/code || test $? -eq 2
```

`swarfr daemon install` does the scheduling; a `just` recipe is shown in `README.md`, "Running
it automatically".

## Configuration

`$XDG_CONFIG_HOME/swarfr/config.toml`, else `~/.config/swarfr/config.toml`. Every key is
optional, a flag always wins over the file, and an unknown key stops the run.

```toml
roots = ["~/code"]
lossy = ["orphans"]
min-age = 3600
min-size = 8192
across-families = false
stores = []  # content-addressed stores to compress, as --store does

[evict]
idle-days = 30
max-total-gib = 50
whole-target = true

[incremental]
idle-days = 7

[index]
idle-days = 30

[family."/Users/me/code/monorepo/.git"]
skip = true

[family."/Users/me/code/big/.git"]
skip-paths = ["vendor"]  # build dirs at these places in every checkout
ecosystems = ["cargo"]   # only these adapters' build dirs

[orphans]
project-idle-days = 7  # as --orphans-project-idle-days

[discovery]
every-secs = 3600  # how long a walk of the roots for build dirs holds; 0 walks every run

[daemon]
interval-secs = 600      # the longest sleep between two looks
rediscover-secs = 21600  # how often the roots are walked for new build dirs
lock-budget-secs = 2     # how long a group may hold a build's locks
```

The family key is the git common dir that `status` prints for the family. A `skip-paths` entry
is a prefix of the build dir's position in its checkout, compared by whole components; a
skipped build dir is neither worked on nor counted by any pass. `ecosystems` takes the adapter
names `status` prints: `cargo`, `swiftpm`, `dotnet`, `cmake`.
`[index] idle-days` is how long the hash index keeps a file's hash that no run has looked up
(default 30); forgetting one costs a single rehash.

## Troubleshooting

- **`status` finds nothing.** Only dirs carrying cargo's own `CACHEDIR.TAG` count. A target
  created by a very old cargo, or a dir whose tag was deleted, is not recognised on purpose.
- **"this filesystem neither shares blocks nor compresses".** The probe tried both and both
  failed — ext4, NTFS, a network mount. Nothing is planned there rather than copied for no gain.
- **Exit code 2 every time.** Something holds the build lock: a running `cargo build`, a
  `cargo watch`, or rust-analyzer's check. Run when the editor is idle, or schedule it at night.
- **`du` shows no change on btrfs.** btrfs reports uncompressed sizes in `stat`; look at `df` for
  the volume or `compsize` for a directory.
- **Files a build just wrote are skipped.** Expected: `min-age` leaves anything younger than an
  hour alone, because the next build rewrites it anyway.
- **Leftover `.swarfr-tmp-*` files.** A run was killed mid-replace. The original files are intact
  and the next run removes the leftovers.
