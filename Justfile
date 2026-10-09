# Everything CI would run.
check: && swarfr
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo check --lib --no-default-features
    cargo test --no-fail-fast

# The other platforms compile. Only `src/sys/` differs between them, and only a build says
# so; the tests still run where the machine is. The Windows suite runs in a Windows 11 ARM
# guest (`docs/windows.md`), so `aarch64-pc-windows-msvc` is checked here too. Needs the std
# of these targets, in the toolchain `rust-toolchain.toml` pins -- without `--toolchain` rustup
# installs them into the default one and this fails with `E0463: can't find crate for core`:
#   rustup target add --toolchain "$(rustc --version --verbose | sed -n 's/^release: //p')" \
#       x86_64-unknown-linux-gnu x86_64-pc-windows-msvc aarch64-pc-windows-msvc
check-cross:
    cargo check --target x86_64-unknown-linux-gnu
    cargo check --target x86_64-pc-windows-msvc
    cargo check --target aarch64-pc-windows-msvc

# Lossless cleanup of this checkout's cargo target dir (compress + dedupe); never deletes.
# Uses the installed binary, not `cargo run`: a broken working tree must not wreck its own target.
swarfr:
    #!/usr/bin/env sh
    command -v swarfr >/dev/null || { echo "swarfr not found; install it with: ketch install swarfr"; exit 0; }
    [ -d target ] || exit 0
    swarfr run target || test $? -eq 2

# Benchmarks on a COPY of a real workspace; see docs/bench.md.
bench workspace:
    scripts/bench.sh {{workspace}}

# The same for the cargo home: it works on a clone of it, never on ~/.cargo itself.
bench-home:
    scripts/bench-cargo-home.sh

# Regenerate .github/workflows/release.yml from dist-workspace.toml (T43). Never hand-edit
# release.yml; change dist-workspace.toml or .github/build-setup.yml and run this.
dist-generate:
    scripts/dist-generate.sh

# Release the version in Cargo.toml, or the next one if that is already tagged (T43). The same
# script the Bump workflow runs, so local and CI cannot disagree. Preview: `just release patch --dry-run`.
release level="patch" *flags:
    scripts/release.sh {{level}} {{flags}}

# Regenerate CHANGELOG.md from git history (git-cliff, config in cliff.toml).
changelog:
    git-cliff -o CHANGELOG.md
