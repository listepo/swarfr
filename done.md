

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

