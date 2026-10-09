#!/usr/bin/env bash
# Regenerate .github/workflows/release.yml from dist-workspace.toml, then map
# dist's CODESIGN_* secret names to the MACOS_* secrets this repository uses
# (same names as listepo/ketch and listepo/rtok). CODESIGN_IDENTITY is not a secret: the
# github-build-setup step discovers it on macOS runners.
#
# Invoked by `just dist-generate`. Do not hand-edit release.yml; change
# dist-workspace.toml (or .github/build-setup.yml) and re-run this.
#
# allow-dirty = ["ci"] is set so `dist plan` / `dist build` accept the patched
# workflow. That same flag makes bare `dist generate` skip writing release.yml,
# so this script briefly clears it, generates, then restores the file.

set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

dist_bin="${DIST:-}"
if [ -z "$dist_bin" ]; then
  dist_bin="mise x cargo:cargo-dist@0.32.0 -- dist"
fi

cfg="dist-workspace.toml"
cfg_backup="$(mktemp)"
cp "$cfg" "$cfg_backup"
cleanup() { mv "$cfg_backup" "$cfg"; }
trap cleanup EXIT

# Drop allow-dirty for the generate pass so release.yml is rewritten.
python3 - "$cfg" <<'PY'
from pathlib import Path
import re
import sys
path = Path(sys.argv[1])
text = path.read_text()
# Remove allow-dirty lines (and a preceding comment line about the patch, if present).
text2 = re.sub(
    r"(?m)^(?:#.*post-patched.*\n)?allow-dirty\s*=\s*\[[^\]]*\]\s*\n",
    "",
    text,
    count=1,
)
path.write_text(text2)
PY

# shellcheck disable=SC2086
$dist_bin generate

# Restore config (with allow-dirty) before patching, so the working tree matches intent.
mv "$cfg_backup" "$cfg"
trap - EXIT

workflow=".github/workflows/release.yml"
if [ ! -f "$workflow" ]; then
  echo "expected $workflow after dist generate" >&2
  exit 1
fi

python3 - "$workflow" <<'PY'
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
text = path.read_text()
original = text

replacements = [
    (
        "CODESIGN_CERTIFICATE: ${{ secrets.CODESIGN_CERTIFICATE }}",
        "CODESIGN_CERTIFICATE: ${{ secrets.MACOS_CERTIFICATE }}",
    ),
    (
        "CODESIGN_CERTIFICATE_PASSWORD: ${{ secrets.CODESIGN_CERTIFICATE_PASSWORD }}",
        "CODESIGN_CERTIFICATE_PASSWORD: ${{ secrets.MACOS_CERTIFICATE_PWD }}",
    ),
]

for old, new in replacements:
    if old not in text:
        if "CODESIGN_CERTIFICATE:" in text and "secrets.MACOS_CERTIFICATE" not in text:
            print(f"dist-generate patch: missing expected line:\n  {old}", file=sys.stderr)
            sys.exit(1)
    else:
        text = text.replace(old, new)

identity_line = "      CODESIGN_IDENTITY: ${{ secrets.CODESIGN_IDENTITY }}\n"
if identity_line in text:
    text = text.replace(
        identity_line,
        "      # CODESIGN_IDENTITY: set on macOS by .github/build-setup.yml (not a secret)\n",
    )

REPORT = """      - name: Report artifact sizes
        shell: bash
        run: |
          set -euo pipefail
          root="target/distrib"
          if [ ! -d "$root" ]; then
            echo "no $root yet; skipping size report"
            exit 0
          fi
          {
            echo "### Release artifact sizes"
            echo
            echo "| File | Size |"
            echo "|---|---:|"
            find "$root" -maxdepth 1 -type f \\( \\
              -name '*.tar.xz' -o -name '*.zip' -o -name '*-update' -o -name 'swarfr-installer.sh' -o -name 'swarfr.rb' -o -name 'sha256.sum' -o -name 'source.tar.gz' \\
            \\) -print0 | sort -z | while IFS= read -r -d '' f; do
              bytes=$(wc -c <"$f" | tr -d ' ')
              human=$(awk -v b="$bytes" 'BEGIN {
                if (b < 1024) { printf "%d B", b; exit }
                if (b < 1048576) { printf "%.1f KiB", b/1024; exit }
                printf "%.2f MiB", b/1048576
              }')
              echo "| $(basename "$f") | ${human} (${bytes} bytes) |"
            done
          } | tee -a "$GITHUB_STEP_SUMMARY"
"""

if "Report artifact sizes" not in text:
    for anchor in (
        "          name: artifacts-build-local-${{ join(matrix.targets, '_') }}",
        "          name: artifacts-build-global",
    ):
        idx = text.find(anchor)
        if idx < 0:
            print(f"dist-generate patch: missing upload anchor {anchor}", file=sys.stderr)
            sys.exit(1)
        step_start = text.rfind('      - name: "Upload artifacts"', 0, idx)
        if step_start < 0:
            print("dist-generate patch: Upload step missing", file=sys.stderr)
            sys.exit(1)
        text = text[:step_start] + REPORT + "\n" + text[step_start:]

create_old = """          # Write and read notes from a file to avoid quoting breaking things
          echo \"$ANNOUNCEMENT_BODY\" > $RUNNER_TEMP/notes.txt

          gh release create \"${{ needs.plan.outputs.tag }}\" --target \"$RELEASE_COMMIT\" $PRERELEASE_FLAG --title \"$ANNOUNCEMENT_TITLE\" --notes-file \"$RUNNER_TEMP/notes.txt\" artifacts/*
"""

create_new = """          # Write and read notes from a file to avoid quoting breaking things
          echo \"$ANNOUNCEMENT_BODY\" > $RUNNER_TEMP/notes.txt

          # Append archive sizes so the Release page shows MiB without opening Assets.
          {
            echo
            echo \"## Download sizes\"
            echo
            echo \"| File | Size |\"
            echo \"|---|---:|\"
            find artifacts -maxdepth 1 -type f \\( \\
              -name '*.tar.xz' -o -name '*.zip' -o -name '*-update' -o -name 'swarfr-installer.sh' -o -name 'swarfr.rb' -o -name 'source.tar.gz' \\
            \\) -print0 | sort -z | while IFS= read -r -d '' f; do
              bytes=$(wc -c <\"$f\" | tr -d ' ')
              human=$(awk -v b=\"$bytes\" 'BEGIN {
                if (b < 1024) { printf \"%d B\", b; exit }
                if (b < 1048576) { printf \"%.1f KiB\", b/1024; exit }
                printf \"%.2f MiB\", b/1048576
              }')
              echo \"| $(basename \"$f\") | ${human} |\"
            done
          } >> \"$RUNNER_TEMP/notes.txt\"
          sed -n '/^## Download sizes$/,$p' \"$RUNNER_TEMP/notes.txt\" | tee -a \"$GITHUB_STEP_SUMMARY\"

          # Every release is a dev build ([profile.dist] inherits `dev` in Cargo.toml), published
          # as a prerelease: this repository's decision (scripts/dist-generate.sh).
          { printf '%s\\n%s\\n\\n' '> [!WARNING]' '> Dev build: compiled with the Cargo `dev` profile (unoptimized, with debug info) and published as a prerelease.'; cat \"$RUNNER_TEMP/notes.txt\"; } > \"$RUNNER_TEMP/notes.dev.txt\"
          mv \"$RUNNER_TEMP/notes.dev.txt\" \"$RUNNER_TEMP/notes.txt\"

          gh release create \"${{ needs.plan.outputs.tag }}\" --target \"$RELEASE_COMMIT\" --prerelease --title \"$ANNOUNCEMENT_TITLE (dev build)\" --notes-file \"$RUNNER_TEMP/notes.txt\" artifacts/*
"""

if "## Download sizes" not in text:
    if create_old not in text:
        print("dist-generate patch: Create GitHub Release block missing/changed", file=sys.stderr)
        sys.exit(1)
    text = text.replace(create_old, create_new, 1)

if (
    text == original
    and "secrets.MACOS_CERTIFICATE" not in text
    and "macos-sign" in pathlib.Path("dist-workspace.toml").read_text()
):
    print(
        "dist-generate patch: macos-sign is on but CODESIGN/MACOS mapping not applied",
        file=sys.stderr,
    )
    sys.exit(1)

path.write_text(text)
print(
    f"patched {path}: MACOS_* secrets, artifact size reports, release notes sizes"
)
PY
