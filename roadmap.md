# Roadmap

Approved work that is not yet in `plan.md`, mostly because the cargo feature it needs is not on
stable. Stable at the time of writing: cargo 1.97–1.98. Version gates marked *estimate* have no
official announcement — re-check the cargo changelog before moving an item into the plan.
Complexity is the same scale `plan.md` uses: `1` (easier) … `5` (harder).

| # | Item | Complexity | Cargo gate | Target dunnage version |
| --- | --- | --- | --- | --- |
| R1 | Unit-level `prune` | 4 | build-dir layout v2 on stable — reported for ~1.100, to be confirmed in the changelog | 0.3 |
| R2 | Automated shared `build-dir` for worktree families | 5 | fine-grained build locking on stable — nightly only (`-Zfine-grain-locking`), *estimate* not before 1.102 | 0.4 |
| R3 | Symlink / shared-store mode for filesystems without reflinks | 5 | layout v2 (~1.100) and R1 | 0.5 |
| R4 | Re-tune for `embed-metadata=no` | 2 | stabilization of `-Zembed-metadata=no` — nightly default since 2026-08, *estimate* not before 1.101 | 0.3 |
| R5 | Retire passes that cargo takes over | 3 | cargo target / build-dir GC (rust-lang/cargo#5026) and per-user artifact cache (#5931) — no version announced | when they land |
| R6 | Raise dedupe yield with path trimming | 2 | `trim-paths` profile option on stable — nightly only (`-Ztrim-paths`), no version announced | after it lands |
| R7 | Publish 0.1: crates.io and a homebrew tap | 2 | none — waits for the creator's go-ahead and a license | 0.1 |
| R8 | Embed the library in a build system | 5 | none — waits for the creator's go-ahead and a build system that wants it | after T36 |

### R1. Unit-level `prune`

Delete build units the current build graph no longer references (old feature sets, old
dependency versions, the ~3 GB of incremental variants seen for one crate). Source of truth is
cargo's own artifact list (`--message-format=json`), mapped onto layout v2's per-unit directories;
no file-name parsing. Waits for layout v2 because the v1 layout mixes all units in one `deps/`.

### R2. Automated shared `build-dir` for worktree families

Today `advise` only suggests it, because one build lock serializes parallel agents. Once locking is
per unit, `dunnage` can write the family's `build.build-dir` config and garbage-collect the
workspace-member units that removed worktrees leave behind (needs R1).

### R3. Symlink / shared-store mode

For ext4 / NTFS, where `clonefile` has no equivalent: third-party unit dirs live in a read-only
store and are symlinked into each worktree's build-dir. Rejected on reflink filesystems (see the
comparison in `DESIGN.md`): cargo writes through the link, so the store must be protected and
invalidated explicitly.

### R4. Re-tune for `embed-metadata=no`

When rlibs stop embedding metadata, `.rmeta` / `.rlib` overlap disappears (−5…−8% dev, up to −33%
release per the Inside Rust post). Re-measure compression and dedupe ratios, adjust `min-size`
defaults, and make `advise` recommend the stable key instead of flagging the ignored one.

### R5. Retire passes that cargo takes over

When cargo ships its own GC for target / build dirs or a per-user artifact cache, disable the
overlapping passes (`orphans`, `evict`, `prune`, possibly `seed`) on toolchains that have them and
keep what cargo does not do (compression, cross-target dedupe).

### R6. Raise dedupe yield with path trimming

Hypothesis from T2: independently built worktrees share only 57% of artifact bytes (21–28% on the
measured machine) because some rlibs / rmeta embed absolute paths such as `OUT_DIR`. When
`trim-paths` is stable, measure whether enabling it makes more artifacts byte-identical, and let
`advise` recommend it if it does. First step when picked up: confirm the cause by diffing two
differing rlibs.

### R7. Publish 0.1: crates.io and a homebrew tap

Was T27; the creator's answer was "not yet", so it waits here instead of sitting in the plan.

`docs/usage.md` opens with "not on crates.io yet, clone and build". Before it can be:
`Cargo.toml` has no `license` and the repository no `LICENSE` file — the creator picks one;
`publish = false` goes; `repository`, `readme`, `keywords`, `categories` are filled in;
`cargo publish --dry-run` is clean. The tap formula builds from the tagged source. Publishing is
outward-facing and cannot be undone: the agent prepares everything and the creator runs, or
explicitly orders, the `cargo publish` and the tag. Done: `cargo install dunnage` works and
the install section of `docs/usage.md` and `README.md` says so.

### R8. Embed the library in a build system

Decided by the creator: the possibility is kept, the work is not started. What keeps it open is
in T36 and `docs/architecture.md`, "Process model" — a library that never prints, exits or reads
the environment, typed errors, a synchronous API with no runtime of its own, `clap` and `anyhow`
behind the `cli` feature, and the reserved `Guard::Held` for a caller that already holds the
build's lock. When picked up, the first questions are which build system and through what: a
Rust caller links the crate; MSBuild, Gradle or CMake need a C ABI or a small command-line shim,
and the reasons a post-build hook is a poor default (hot files, cross-dir passes, committed
files) decide what such a caller is allowed to ask for.
