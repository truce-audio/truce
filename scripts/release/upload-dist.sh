#!/usr/bin/env bash
#
# upload-dist.sh — upload every artifact in `target/dist/` to the
# GitHub release matching the current `Cargo.toml` version. Each
# file becomes its own release asset.
#
# Usage:
#   scripts/release/upload-dist.sh
#
# Run on each platform after `cargo truce package` has populated
# `target/dist/`:
#
#   # macOS
#   cargo truce package          # produces target/dist/*.pkg
#   upload-dist.sh
#
#   # Windows (Git Bash / WSL)
#   cargo truce package          # produces target/dist/*.exe
#   upload-dist.sh
#
#   # Linux
#   cargo truce package          # produces target/dist/*.tar.gz
#   upload-dist.sh
#
# GitHub release assets live in a flat namespace — there's no
# per-OS directory at the API level. `cargo truce package` already
# bakes the host OS into every filename (`Truce
# Gain-0.38.1-macos.pkg`, `Truce Gain-0.38.1-windows.exe`, etc.),
# so uploading files as-is keeps them sortable and unambiguous on
# the release page. Running this on every platform sequentially
# accumulates a full per-OS set against the same tag.
#
# Idempotency:
#   - `gh release upload --clobber` replaces an existing asset of
#     the same name; safe to re-run after re-packaging.
#   - Other OSes' uploads are untouched.
#
# Pre-reqs:
#   - The GitHub release `vX.Y.Z` already exists (run
#     `scripts/release/release.sh` first).
#   - `gh auth login` already run.
#   - `target/dist/` populated by `cargo truce package` (this
#     script does not run package itself).

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
TRUCE_ROOT="$PWD"

# ----------------------------------------------------------------------------
# Detect host OS (only used in log output — filenames already carry
# the OS via `cargo truce package`'s naming)
# ----------------------------------------------------------------------------

case "$(uname -s)" in
    Darwin)         OS=macos   ;;
    Linux)          OS=linux   ;;
    MINGW*|CYGWIN*|MSYS*) OS=windows ;;
    *)
        echo "Error: unsupported OS: $(uname -s)" >&2
        exit 1
        ;;
esac

# ----------------------------------------------------------------------------
# Read version + tag
# ----------------------------------------------------------------------------

WS_VERSION="$(awk -F\" '
    /^\[workspace\.package\]/ { p = 1 }
    p && /^version = / { print $2; exit }
' Cargo.toml)"

if [[ -z "$WS_VERSION" ]]; then
    echo "Error: could not read [workspace.package].version from $TRUCE_ROOT/Cargo.toml" >&2
    exit 1
fi

TAG="v$WS_VERSION"

# ----------------------------------------------------------------------------
# Verify dist contents + release existence
# ----------------------------------------------------------------------------

DIST_DIR="target/dist"
if [[ ! -d "$DIST_DIR" ]]; then
    echo "Error: $TRUCE_ROOT/$DIST_DIR does not exist." >&2
    echo "       Run \`cargo truce package\` first." >&2
    exit 1
fi

shopt -s nullglob
dist_files=("$DIST_DIR"/*)
shopt -u nullglob

if [[ ${#dist_files[@]} -eq 0 ]]; then
    echo "Error: $TRUCE_ROOT/$DIST_DIR is empty." >&2
    echo "       Run \`cargo truce package\` first." >&2
    exit 1
fi

if ! gh release view "$TAG" >/dev/null 2>&1; then
    echo "Error: GitHub release $TAG does not exist yet." >&2
    echo "       Run \`scripts/release/release.sh\` first." >&2
    exit 1
fi

# ----------------------------------------------------------------------------
# Upload each file as its own asset. `--clobber` replaces existing
# assets of the same name without touching others, so re-running
# after a re-package is safe.
# ----------------------------------------------------------------------------

echo "→ uploading ${#dist_files[@]} ${OS} artifact(s) to release $TAG:"
for f in "${dist_files[@]}"; do
    echo "    $(basename "$f")"
done
echo

gh release upload "$TAG" "${dist_files[@]}" --clobber

RELEASE_URL="$(gh release view "$TAG" --json url --jq .url 2>/dev/null || true)"

echo
echo "Uploaded ${#dist_files[@]} ${OS} artifact(s) → $TAG"
if [[ -n "$RELEASE_URL" ]]; then
    echo "  $RELEASE_URL"
fi
