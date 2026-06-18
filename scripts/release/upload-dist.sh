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
# Sub-workspaces:
#   The slint / vizia / gpu-examples sub-workspaces have their own
#   `truce.toml` (see scripts/recursive-cargo-truce.sh); `cargo truce
#   package` run inside one writes artifacts to that workspace's own
#   dist dir, not the root's. This script asks cargo for each
#   workspace's real target dir (so `CARGO_TARGET_DIR` and a
#   `.cargo/config.toml` `target-dir` are honored, not assumed to be
#   `<ws>/target`) and scans every one, so a single
#   `recursive-cargo-truce.sh package` run uploads all of them.
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

# Resolve a workspace's actual dist dir. `cargo truce package` writes to
# cargo's real target directory, which honors `CARGO_TARGET_DIR` and a
# `.cargo/config.toml` `target-dir` and is NOT always `<ws>/target`, so
# ask cargo rather than assume. `python3` parses the JSON (it unescapes
# Windows backslash paths correctly); `release.sh` already requires it.
# Falls back to `<ws>/target/dist` when cargo can't be queried.
workspace_dist_dir() {
    local ws="$1" td
    td="$("${CARGO:-cargo}" metadata --no-deps --format-version=1 \
            --manifest-path "$ws/Cargo.toml" 2>/dev/null \
          | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])' 2>/dev/null)" || true
    if [[ -n "$td" ]]; then
        # python prints native separators; normalise backslashes so the
        # path globs cleanly under Git Bash / MSYS on Windows.
        printf '%s/dist' "${td//\\//}"
    else
        printf '%s/target/dist' "$ws"
    fi
}

# Workspace roots to scan: the root plus each self-contained sub-
# workspace (slint / vizia / gpu-examples), each with its own
# `truce.toml`. Keep in sync with scripts/recursive-cargo-truce.sh.
# A dist dir that doesn't exist (sub-workspace not packaged on this run
# / platform) contributes nothing under `nullglob` rather than erroring.
WORKSPACES=(
    "."
    "crates/truce-slint"
    "crates/truce-vizia"
    "crates/truce-gpu-examples"
)

DIST_DIRS=()
for ws in "${WORKSPACES[@]}"; do
    DIST_DIRS+=("$(workspace_dist_dir "$ws")")
done

shopt -s nullglob
dist_files=()
for d in "${DIST_DIRS[@]}"; do
    dist_files+=("$d"/*)
done
shopt -u nullglob

if [[ ${#dist_files[@]} -eq 0 ]]; then
    echo "Error: no artifacts found in any workspace's target/dist." >&2
    echo "       Scanned (relative to $TRUCE_ROOT): ${DIST_DIRS[*]}" >&2
    echo "       Run \`cargo truce package\` (or scripts/recursive-cargo-truce.sh package) first." >&2
    exit 1
fi

# Release assets are a flat namespace: two artifacts sharing a basename
# (across workspaces) would clobber each other on upload. Fail loudly
# rather than silently dropping one.
dupes="$(printf '%s\n' "${dist_files[@]}" | while read -r f; do basename "$f"; done | sort | uniq -d)"
if [[ -n "$dupes" ]]; then
    echo "Error: duplicate artifact name(s) across workspaces (GitHub assets are flat):" >&2
    echo "$dupes" | sed 's/^/         /' >&2
    echo "       Rename the colliding plugin/suite so each artifact is unique." >&2
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
