# Ideas

Not approved. Nothing here moves to `roadmap.md` or `plan.md` without the creator's approval.

Moved out by the creator's word. Into `plan.md`: `dunnage worktree add` (T26), other build
systems (T28–T33, T35), watch mode — now the daemon (T34) and its filesystem watcher (T46) — and the monorepo behaviors
(T37–T39). Into `roadmap.md`: publishing (R7). Already done and dropped from this list: the
Linux backend (T20) and ReFS cloning as a Windows backend (part of T21).

## Park / unpark

`tar | zstd` an idle worktree's target (~75% smaller) and restore it on demand. Written down
before `compress` and `dedupe` were measured: two built targets now lose 74% and stay buildable
(`docs/bench.md`), so parking would buy a few points on top at the price of a target nobody can
use until it is unpacked, plus two new dependencies. Left here as not worth a task unless a
measurement on a compressed target says otherwise.

## Skip seeding workspace-member artifacts

Ask `cargo metadata` for member names and leave their artifacts out of `seed`, since they are
rebuilt anyway (`docs/usage.md`, `seed`). Two things against it today: mapping a member to its
files means parsing `name-<hash>` file names, which `DESIGN.md` rules out until layout v2 gives
each unit a directory (R1); and on a filesystem that clones, the skipped files cost no bytes to
begin with. It only pays where `seed` is a real copy — ext4, NTFS — and only after R1.

## A CI job on a btrfs loopback image

T20 was verified in a local lima VM by the creator's choice. A GitHub Actions job that creates a
btrfs loopback image, mounts it, points `TMPDIR` at it and runs the suite would keep the Linux
half honest without a VM on hand — the same two-line setup the VM used. Not approved on its own;
T24 asks the CI-or-VM question again for Windows, and if the answer there is CI, this job rides
along in the same workflow.

## Windows: WOF compression instead of `FSCTL_SET_COMPRESSION`

Raised by the T21 readiness analysis. NTFS's classic compression is LZNT1: a weak ratio,
fragmentation, and it stays on for every later write. The closer analogue of what the APFS
backend does is WOF (`compact /EXE:LZX`, `FSCTL_SET_EXTERNAL_BACKING`): a much better ratio,
and a file that is rewritten simply becomes a plain file again, exactly like decmpfs. It is a
different API with its own edge cases, so it is a choice to measure in T21's spike, not to
assume. T21's card names `FSCTL_SET_COMPRESSION`; changing that is the creator's call.

## Daemon: stop cleanly on SIGTERM

`launchctl bootout` and `systemctl stop` send SIGTERM, which kills the daemon mid-action; every
action is whole, so the cost is leftover `.dunnage-tmp-*` files the next run removes. Raising
`Control::stop` from the signal needs `signal-hook` or `ctrlc` (a new dependency) or `unsafe`
FFI.
