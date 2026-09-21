# Toolchain

Only what the project actually uses. Approved but not yet wired: `blake3` (only if `sha2` proves
too slow, see `DESIGN.md`).

## Programs

| Program | How to install | Why here | Source |
| --- | --- | --- | --- |
| mise | brew / curl, then `mise install` | Pinned tool versions | https://github.com/jdx/mise |
| rustc | mise (pin in `rust-toolchain.toml`, mirrored in `mise.toml`) | Build | https://github.com/rust-lang/rust |
| cargo | mise (with rust) | Build, and the tool under study | https://github.com/rust-lang/cargo |
| just | global (cargo install / brew) | `just check`: fmt, clippy, test | https://github.com/casey/just |
| rust-std for `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc` | `rustup target add --toolchain $(rustc --version --verbose \| sed -n 's/^release: //p') <triple>` — without `--toolchain` rustup installs into the *default* toolchain, not the one `rust-toolchain.toml` pins, and the cross build then fails with `E0463: can't find crate for core` | `just check-cross`: the other two platforms must compile | https://github.com/rust-lang/rust |
| hyperfine | global (brew; mise ships an x86_64 build that will not run on arm64) | `scripts/bench.sh`: build timings | https://github.com/sharkdp/hyperfine |
| sccache | global (mise) | `scripts/bench.sh`: the variant the tool is compared against | https://github.com/mozilla/sccache |
| swift | global (Xcode), optional | `tests/swiftpm.rs`: the lock and rebuild oracles for SwiftPM, skipped without it; `docs/bench.md` numbers | https://github.com/swiftlang/swift |
| xcodebuild, plutil | global (Xcode, macOS), optional | `tests/xcode.rs`: a real DerivedData entry and the no-op oracle, skipped without it; `plutil` reads a binary `info.plist`; `docs/bench.md` numbers | https://developer.apple.com/xcode/ |
| dotnet | global (installer), optional, a 9.0 SDK | `tests/dotnet.rs`: the MSBuild no-op oracle, skipped without it | https://github.com/dotnet/sdk |
| cmake | global (mise), optional, with the system `cc` | `tests/cmake.rs`: the Makefiles and Ninja no-op oracles, skipped without it; `docs/bench.md` numbers | https://github.com/Kitware/CMake |
| ninja | global (mise), optional | `tests/cmake.rs`, `tests/meson.rs`: `ninja -n` plans nothing after a run, skipped without it | https://github.com/ninja-build/ninja |
| meson | global (mise), optional | `tests/meson.rs`: a real Meson build dir, skipped without it | https://github.com/mesonbuild/meson |
| go | global (brew), optional | `tests/store.rs`: the `GOCACHE` oracle for `--store`; `tests/go.rs`: `go mod verify` after `run --go`; skipped without it; `docs/bench.md` numbers | https://github.com/golang/go |
| zip | system (macOS, most Linux distributions), optional | `tests/go.rs`: a module zip for the offline proxy dir, skipped without it | https://infozip.sourceforge.net/Zip.html |
| ps, lsof, getconf | system (macOS) | The no-lock tier's process check and the `tmpdir` pass's open files (`lsof -Ffn`); `getconf DARWIN_USER_TEMP_DIR` for the temp dir without `TMPDIR` | https://opensource.apple.com/source/lsof/ |
| lima | global (brew / mise) | A Linux VM with a btrfs loopback image: the only way to test the Linux half from a Mac | https://github.com/lima-vm/lima |

## cargo

| Package | Where | Source | Why here |
| --- | --- | --- | --- |
| clap | local, `cli` feature | https://github.com/clap-rs/clap | CLI parsing |
| rustix | local, Linux only (`[target.'cfg(target_os = "linux")'.dependencies]`) | https://github.com/bytecodealliance/rustix | `FICLONE` and `FS_IOC_GET/SETFLAGS` without hand-written `unsafe` |
| walkdir | local | https://github.com/BurntSushi/walkdir | Walk a profile dir without following symlinks or leaving the device |
| anyhow | local, `cli` feature | https://github.com/dtolnay/anyhow | Error context in the binary |
| sha2 | local | https://github.com/RustCrypto/hashes | Content hash for dedupe |
| applesauce | local, macOS only (`[target.'cfg(target_os = "macos")'.dependencies]`) | https://github.com/Dr-Emann/applesauce | Backend of the compress pass: transparent APFS compression |
| rayon | local | https://github.com/rayon-rs/rayon | Hash files in parallel |
| serde | local | https://github.com/serde-rs/serde | Serialize the inventory |
| serde_json | local | https://github.com/serde-rs/json | `status --json`, `run --json`; cargo's JSON messages in the test oracle |
| toml | local | https://github.com/toml-rs/toml | Read `config.toml` |
| tempfile | local (dev) | https://github.com/Stebalien/tempfile | Throwaway profile dirs and the cargo fixture in tests |
| trycmd | local (dev) | https://github.com/assert-rs/snapbox | Full CLI output cases in `tests/cmd/` |
| assert_cmd | local (dev) | https://github.com/assert-rs/assert_cmd | Exit codes of the binary |
| predicates | local (dev) | https://github.com/assert-rs/predicates-rs | Matchers for `assert_cmd` |
