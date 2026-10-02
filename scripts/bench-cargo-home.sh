#!/usr/bin/env bash
# Benchmark `--cargo-home` on a COPY of the real cargo home: compression ratio, the tool's own
# runtime, and whether cargo re-extracts anything afterwards.
#
#   scripts/bench-cargo-home.sh [work-dir]
#
# The copy is an APFS clone (`cp -c`), so it costs no disk space and the real `~/.cargo` is only
# ever read. Everything the tool touches is the clone. Results go to <work-dir>/results.tsv.
set -euo pipefail

WORK=${1:-${TMPDIR:-/tmp}/swarfr-home-bench}
# A crate to build against the cloned home; any crate already in it will do.
CRATE=${CRATE:-libc}

REAL_HOME=${CARGO_HOME:-$HOME/.cargo}
SWARFR_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
case $WORK in
    /tmp/* | /private/tmp/* | /var/folders/*) ;;
    *) echo "work dir must live in a temp dir, got $WORK" >&2; exit 1 ;;
esac
[ -d "$REAL_HOME/registry/src" ] || { echo "no registry/src in $REAL_HOME" >&2; exit 1; }

rm -rf "$WORK"
mkdir -p "$WORK"
RESULTS=$WORK/results.tsv
: >"$RESULTS"
HOME_COPY=$WORK/cargo-home
# mise refuses an untrusted config, and a benchmark must not depend on one machine's paths:
# trust this checkout and the work dir the script made itself.
REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
export MISE_TRUSTED_CONFIG_PATHS=$REPO_ROOT

say() { printf '\n=== %s\n' "$*"; }
record() { printf '%s\t%s\n' "$1" "$2" >>"$RESULTS"; echo "    $1 = $2"; }
size_kib() { du -sk "$1" | awk '{print $1}'; }
now() { /usr/bin/python3 -c 'import time; print(time.time())'; }
timed() {
    local key=$1 start
    shift
    start=$(now)
    "$@" >"$WORK/$key.log" 2>&1
    record "$key" "$(/usr/bin/python3 -c "print(f'{$(now) - $start:.3f}')")"
}

say "clone $REAL_HOME (APFS clone: costs no space, the original is only read)"
mkdir -p "$HOME_COPY"
for dir in registry git; do
    if [ -d "$REAL_HOME/$dir" ]; then cp -c -R "$REAL_HOME/$dir" "$HOME_COPY/$dir"; fi
done
# Cargo's own lock, which the pass takes; a clone of a home cargo has used already has one.
: >"$HOME_COPY/.package-cache"
record "sources_before_kib" "$(size_kib "$HOME_COPY/registry/src")"
record "checkouts_before_kib" "$([ -d "$HOME_COPY/git/checkouts" ] && size_kib "$HOME_COPY/git/checkouts" || echo 0)"
record "cache_before_kib" "$(size_kib "$HOME_COPY/registry/cache")"

say "build swarfr"
cargo build --quiet --release --manifest-path "$SWARFR_DIR/Cargo.toml"
SWARFR=$SWARFR_DIR/target/release/swarfr

say "a crate that uses the cloned home, built offline before the pass"
# The newest copy of it in the clone; `tail` reads its input to the end, so no SIGPIPE.
SRC_DIR=$(/bin/ls -d "$HOME_COPY"/registry/src/*/"$CRATE"-[0-9]*/ 2>/dev/null | sort | tail -1)
[ -n "${SRC_DIR:-}" ] || { echo "no $CRATE in the cloned home; set CRATE=" >&2; exit 1; }
VERSION=$(basename "$SRC_DIR" | sed -E "s#^$CRATE-##")
record "crate" "$CRATE-$VERSION"
mkdir -p "$WORK/user/src"
cat >"$WORK/user/Cargo.toml" <<TOML
[package]
name = "bench-user"
version = "0.0.0"
edition = "2021"

[dependencies]
$CRATE = "=$VERSION"
TOML
echo 'fn main() {}' >"$WORK/user/src/main.rs"
# Not the `cargo` on PATH: a version manager's shim reads CARGO_HOME to find its own toolchain
# and tries to reinstall Rust into the clone. The rustup binary takes the toolchain from
# RUSTUP_HOME and uses CARGO_HOME for the registry only, which is exactly what is measured here.
CARGO_BIN=${CARGO_BIN:-$HOME/.cargo/bin/cargo}
[ -x "$CARGO_BIN" ] || { echo "no cargo at $CARGO_BIN; set CARGO_BIN=" >&2; exit 1; }
build() {
    CARGO_HOME=$HOME_COPY "$CARGO_BIN" build --offline --quiet \
        --manifest-path "$WORK/user/Cargo.toml"
}
timed "build_before" build
# What cargo looks at to decide whether a crate must be extracted again.
stamp() { stat -f '%i %m %z' "$SRC_DIR/.cargo-ok"; }
OK_BEFORE=$(stamp)

say "compress the cloned home"
timed "tool_secs" "$SWARFR" run --cargo-home "$HOME_COPY" --min-age 0 --index "$WORK/hashes.bin"
record "sources_after_kib" "$(size_kib "$HOME_COPY/registry/src")"
record "checkouts_after_kib" "$([ -d "$HOME_COPY/git/checkouts" ] && size_kib "$HOME_COPY/git/checkouts" || echo 0)"
record "cache_after_kib" "$(size_kib "$HOME_COPY/registry/cache")"

say "build again: nothing may be extracted a second time"
rm -rf "$WORK/user/target"
timed "build_after" build
record "cargo_ok_unchanged" "$([ "$OK_BEFORE" = "$(stamp)" ] && echo yes || echo NO)"
record "stale_units_after" "$(CARGO_HOME=$HOME_COPY "$CARGO_BIN" build --offline \
    --manifest-path "$WORK/user/Cargo.toml" --message-format=json 2>/dev/null |
    grep -c '"fresh":false' || true)"

say "results"
cat "$RESULTS"
echo
echo "work dir kept at $WORK (delete it yourself when the numbers are written up)"
