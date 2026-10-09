# AGENTS.md

Notes for coding agents working in this repository.

## Mandatory for every agent

- The human is the only author. No agent adds a Co-Authored-By trailer, a "Generated with …" line
  or itself as author to a commit, merge or PR.
- If a directory above this repository contains an `AGENTS.md` or `CLAUDE.md`, follow it too. If it
  conflicts with this file, ask the creator.
- **Config files.** A config file this project owns has a schema generated from its types (Rust: `schemars`), committed and checked by a drift test, and one module owns all config loading, validation and editing. A config file another program owns (an agent host's or an editor's) gets no schema from us: check only our own entry in it and leave the rest byte-for-byte, comments included.

## What swarfr is

A cargo subcommand that shrinks live `target/` directories without slowing builds. Read
`DESIGN.md` before touching code — the inode model, pass ordering and safety invariants there are
the contract. Measurements that justify the design are in `docs/research.md`.

## Safety rules (the tool mutates build directories)

- Tests and experiments run **only** on fixture targets inside temp dirs. Never point a
  development build of the tool at a real project's `target/`.
- Lossy passes (`orphans`, `evict`, later `prune`) are never enabled by default, in code or in
  test config.
- A lossless pass is not done until the freshness oracle (see `DESIGN.md`) is green for it.
- Do not parse `name-<hash>` artifact file names; cargo's layout is changing (build-dir layout v2).
- Never delete or rewrite anything in the creator's real target dirs while debugging; ask first.

## Commands

- `just check` — `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo check --lib --no-default-features` (the library without the CLI), `cargo test`.
  Run it before calling a task done. Ends with a lossless `swarfr` cleanup of `target/`
  (`just swarfr`); a no-op when `swarfr` is not installed.
- `cargo run -- <args>` — the binary is `swarfr`. `cargo swarfr <args>` works through a
  `cargo-swarfr` link to it: cargo passes `swarfr` as the first argument and `main` drops it.

## Working agreements

- Tasks live in `plan.md` (table + cards), mirrored in `todo.md`; finished tasks move whole to
  `done.md`. Claim a task in the table before starting and write the execution plan into its card.
- Version-gated work lives in `roadmap.md`; re-check the cargo changelog before moving an item.
- Dependency candidates are listed in `DESIGN.md`.
- macOS / APFS is the only supported platform for 0.x; keep platform calls behind the backend
  boundary described in `DESIGN.md`.

## Windows tests (T24)

The suite runs in a local Windows 11 ARM virtual machine, not in CI. `docs/windows.md` says how
to bring the guest up, make the ReFS Dev Drive, and run `just check` with `TEMP` and `TMP` on
NTFS or on that volume. `just check-cross` only type-checks `x86_64-unknown-linux-gnu`,
`x86_64-pc-windows-msvc` and `aarch64-pc-windows-msvc`.
