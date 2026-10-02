#!/usr/bin/env bash
# Benchmark swarfr on a copy of a real workspace: build time before and after the passes,
# target size, the tool's own runtime, and whether cargo rebuilds anything afterwards.
#
#   scripts/bench.sh <path-to-git-workspace> [work-dir]
#
# The copy is a shallow clone plus one git worktree of it, so the two targets form a family and
# dedupe has siblings to compare. The tool never sees the source workspace's own target dir.
# Results go to <work-dir>/results.tsv, one `key<TAB>value` line per measurement.
set -euo pipefail

SRC=${1:?usage: bench.sh <path-to-git-workspace> [work-dir]}
WORK=${2:-${TMPDIR:-/tmp}/swarfr-bench}
RUNS=${RUNS:-5}
MIN_FREE_GIB=${MIN_FREE_GIB:-30}
# Everything just built is younger than the default age floor, so a benchmark has to lift it.
SWARFR_ARGS=(--min-age 0)

SRC=$(cd "$SRC" && pwd)
SWARFR_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
case $WORK in
    /tmp/* | /private/tmp/* | /var/folders/*) ;;
    *) echo "work dir must live in a temp dir, got $WORK" >&2; exit 1 ;;
esac
[ -d "$SRC/.git" ] || { echo "$SRC is not a git workspace" >&2; exit 1; }

free_gib=$(df -g "$(dirname "$WORK")" | awk 'NR==2 {print $4}')
[ "$free_gib" -ge "$MIN_FREE_GIB" ] || {
    echo "only ${free_gib} GiB free, want ${MIN_FREE_GIB}" >&2; exit 1
}

rm -rf "$WORK"
mkdir -p "$WORK"
RESULTS=$WORK/results.tsv
: >"$RESULTS"
INDEX=$WORK/hashes.bin
# The copies carry the workspace's own mise config, and they live outside the trusted tree.
# mise refuses an untrusted config, and a benchmark must not depend on one machine's paths:
# trust this checkout and the work dir the script made itself.
REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
export MISE_TRUSTED_CONFIG_PATHS=$REPO_ROOT:$WORK

say() { printf '\n=== %s\n' "$*"; }
record() { printf '%s\t%s\n' "$1" "$2" >>"$RESULTS"; echo "    $1 = $2"; }
# Allocated KiB, the same thing `du` reports and the only size that matters after compression.
size_kib() { du -sk "$1" | awk '{print $1}'; }
# Seconds of wall clock, so the tool's own runtime is comparable to a build.
# BSD `date` has no sub-second format, so the clock comes from python.
now() { /usr/bin/python3 -c 'import time; print(time.time())'; }
timed() {
    local key=$1 start
    shift
    start=$(now)
    "$@" >"$WORK/$key.log" 2>&1
    record "$key" "$(/usr/bin/python3 -c "print(f'{$(now) - $start:.3f}')")"
}

say "clone $SRC (shallow, sources only; the source target dir is never touched)"
git clone --quiet --depth 1 "file://$SRC" "$WORK/a"
git -C "$WORK/a" worktree add --quiet --detach "$WORK/b" HEAD
CRATE=$(basename "$SRC")

say "build swarfr"
cargo build --quiet --release --manifest-path "$SWARFR_DIR/Cargo.toml"
SWARFR=$SWARFR_DIR/target/release/swarfr
swarfr() { "$SWARFR" "$@"; }

say "fetch dependencies once, then build offline"
cargo fetch --quiet --manifest-path "$WORK/a/Cargo.toml"

build() { cargo build --offline --quiet --manifest-path "$1/Cargo.toml"; }
# The oracle: after a pass, no unit may be rebuilt.
stale_units() {
    cargo build --offline --manifest-path "$1/Cargo.toml" --message-format=json 2>/dev/null |
        grep -c '"fresh":false' || true
}
# The file an edit-build loop touches: the top-level crate, so only it recompiles.
touched() { ls "$1/src/main.rs" 2>/dev/null || ls "$1/src/lib.rs"; }
incremental() {
    local key=$1 dir=$2 file
    file=$(touched "$dir")
    # Three warmups, not one: right after a clean build the machine is still settling (the first
    # timed builds came in at 30s and the last at 5s when this was --warmup 1).
    hyperfine --warmup 3 --runs "$RUNS" --style basic \
        --prepare "touch $file" \
        --export-json "$WORK/$key.json" \
        "cargo build --offline --quiet --manifest-path $dir/Cargo.toml" >"$WORK/$key.log" 2>&1
    record "$key" "$(awk -F'[,:]' '/"mean"/ {printf "%.3f", $2; exit}' "$WORK/$key.json")"
}

for dir in a b; do
    say "clean build in $dir ($CRATE)"
    timed "build_clean_$dir" build "$WORK/$dir"
    record "size_after_build_kib_$dir" "$(size_kib "$WORK/$dir/target")"
done

say "baseline incremental build"
incremental "incremental_baseline" "$WORK/a"

say "what each pass would win on its own (dry run, nothing touched)"
for pass in compress dedupe; do
    swarfr run --dry-run --pass "$pass" "${SWARFR_ARGS[@]}" --index "$INDEX" "$WORK" \
        >"$WORK/dry_$pass.log" 2>&1
    record "dry_planned_bytes_$pass" \
        "$(awk -F'[(),]' "/  $pass: planned/ {sum += \$2} END {print sum + 0}" "$WORK/dry_$pass.log")"
done

# Sizes are taken immediately around each pass: the builds a stage runs afterwards add
# artifacts of their own, so only a before/after pair measures what that pass did.
# `du` counts a clone's blocks in full, so it sees compression but not copy-on-write sharing;
# the volume's free space is the only ground truth for dedupe.
sizes() {
    local when=$1 pass=$2 dir
    for dir in a b; do
        record "size_${when}_${pass}_kib_$dir" "$(size_kib "$WORK/$dir/target")"
    done
    record "free_${when}_${pass}_kib" "$(df -k "$WORK" | awk 'NR==2 {print $4}')"
}

for pass in compress dedupe; do
    say "apply $pass"
    sizes before "$pass"
    timed "tool_secs_$pass" swarfr run --pass "$pass" "${SWARFR_ARGS[@]}" --index "$INDEX" "$WORK"
    sizes after "$pass"
    record "stale_units_after_$pass" "$(stale_units "$WORK/a")"
    if [ "$pass" = dedupe ]; then
        # Both passes have run and nothing has been rebuilt since, so a second run must find
        # almost nothing left to do.
        say "second run: the passes must find almost nothing left"
        timed "tool_secs_second_run" swarfr run "${SWARFR_ARGS[@]}" --index "$INDEX" "$WORK"
        record "second_run_applied" \
            "$(awk '/planned/ {for (i = 1; i < NF; i++) if ($i == "applied") sum += $(i + 1)}
                END {print sum + 0}' "$WORK/tool_secs_second_run.log")"
    fi
    incremental "incremental_after_$pass" "$WORK/a"
done

if [ "${WITH_ACROSS:-1}" = 1 ]; then
    say "dedupe across families: a second, independent clone of the same repository"
    # A clone, not a worktree: its own .git, so its own family. Two checkouts of the same
    # dependencies in unrelated projects is the case one run per family cannot reach.
    git clone --quiet --depth 1 "file://$SRC" "$WORK/d"
    timed "build_clean_d" build "$WORK/d"
    record "size_after_build_kib_d" "$(size_kib "$WORK/d/target")"
    # Its own family first, so what is left is only what the other family holds.
    swarfr run --pass dedupe "${SWARFR_ARGS[@]}" --index "$INDEX" "$WORK/d" \
        >"$WORK/dedupe_d.log" 2>&1
    record "free_before_across_kib" "$(df -k "$WORK" | awk 'NR==2 {print $4}')"
    timed "tool_secs_across" swarfr run --pass dedupe --across-families "${SWARFR_ARGS[@]}" \
        --index "$INDEX" "$WORK"
    record "free_after_across_kib" "$(df -k "$WORK" | awk 'NR==2 {print $4}')"
    record "stale_units_after_across_a" "$(stale_units "$WORK/a")"
    record "stale_units_after_across_d" "$(stale_units "$WORK/d")"
fi

if [ "${WITH_SCCACHE:-1}" = 1 ]; then
    say "sccache: cold then warm cache, clean builds in a third checkout"
    git -C "$WORK/a" worktree add --quiet --detach "$WORK/c" HEAD
    export RUSTC_WRAPPER=sccache SCCACHE_DIR=$WORK/sccache-cache
    sccache --stop-server >/dev/null 2>&1 || true
    timed "build_clean_sccache_cold" build "$WORK/c"
    rm -rf "$WORK/c/target"
    timed "build_clean_sccache_warm" build "$WORK/c"
    record "size_after_build_kib_c" "$(size_kib "$WORK/c/target")"
    record "sccache_cache_kib" "$(size_kib "$SCCACHE_DIR")"
    sccache --stop-server >/dev/null 2>&1 || true
    unset RUSTC_WRAPPER SCCACHE_DIR
fi

say "results"
cat "$RESULTS"
echo
echo "work dir kept at $WORK (delete it yourself when the numbers are written up)"
