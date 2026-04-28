#!/usr/bin/env bash
#
# release.sh — tag, publish, and announce a release.
#
# Usage:
#   dev/scripts/release.sh             # default: preview/ train
#   dev/scripts/release.sh --release   # post-1.0 stable train
#
# Run from `main` (or the train branch for a hotfix) after the bump
# PR opened by `bump.sh` has been reviewed and merged. The merged
# release commit must be HEAD.
#
# What this does, in order:
#   1. Pull latest main
#   2. Read version from Cargo.toml; verify both strings agree
#   3. Create annotated tag locally
#   4. Dry-run publish truce-shim-types (catches metadata gaps
#      before any irreversible upload)
#   5. Publish truce-shim-types to crates.io
#   6. Sleep 30s for crates.io index propagation
#   7. Publish cargo-truce to crates.io
#   8. Fast-forward <prefix>/X.Y to the tag
#   9. Push main, <prefix>/X.Y, and the tag in one go
#  10. Create the GitHub Release with auto-generated notes
#  11. Fast-forward dev/latest to main and push it (closes the
#      rebase-and-merge sync loop — see release.md "Branch sync
#      model"). Best-effort: a release-already-shipped failure
#      surfaces as a warning, not an error.
#
# Prefix selection:
#   --preview (default)  pre-1.0 trains and post-1.0 pre-release testing
#   --release            post-1.0 stable trains
# Pre-1.0 always uses preview/. Post-1.0, both `preview/X.Y` and
# `release/X.Y` may coexist (preview/ for the next minor's RC line,
# release/ for the current stable). The flag picks which one this
# release advances.
#
# Pre-reqs:
#   - `cargo login <token>` already run (check ~/.cargo/credentials.toml)
#   - `gh auth login` already run (check `gh auth status`)
#   - main is at the bump commit and <prefix>/X.Y exists for this train
#     (for a brand-new minor release, create <prefix>/X.Y from main
#      before running this script)
#
# Recovery: see dev/docs/DEVELOPMENT.md or
# truce-docs/docs/internal/release.md for what to do when a
# particular step fails. The script is linear, not idempotent —
# re-running after a partial failure requires manual cleanup first.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

PREFIX="preview"

for arg in "$@"; do
    case "$arg" in
        --preview) PREFIX="preview" ;;
        --release) PREFIX="release" ;;
        *)
            echo "Unknown argument: $arg" >&2
            echo "Usage: release.sh [--preview|--release]" >&2
            exit 1
            ;;
    esac
done

# Step 1 — sync main ----------------------------------------------------------

git checkout main
git pull --ff-only

# Step 2 — read + verify versions --------------------------------------------

WS_VERSION="$(awk -F\" '
    /^\[workspace\.package\]/ { p = 1 }
    p && /^version = / { print $2; exit }
' Cargo.toml)"

SHIM_VERSION="$(awk -F\" '
    /^truce-shim-types = / { print $2; exit }
' Cargo.toml)"

if [[ -z "$WS_VERSION" ]]; then
    echo "Error: could not read [workspace.package].version" >&2
    exit 1
fi

if [[ "$WS_VERSION" != "$SHIM_VERSION" ]]; then
    echo "Error: version drift in Cargo.toml" >&2
    echo "  [workspace.package].version              = $WS_VERSION" >&2
    echo "  [workspace.dependencies].truce-shim-types = $SHIM_VERSION" >&2
    exit 1
fi

TAG="v$WS_VERSION"
TRAIN="$PREFIX/$(echo "$WS_VERSION" | cut -d. -f1,2)"

echo "Releasing $TAG (train: $TRAIN)"

# Step 3 — annotated tag ------------------------------------------------------

if git rev-parse --verify "$TAG" >/dev/null 2>&1; then
    echo "Error: tag $TAG already exists locally — delete with 'git tag -d $TAG' to retry" >&2
    exit 1
fi

git tag -a "$TAG" -m "truce $WS_VERSION"

# Step 4 — dry-run publish (truce-shim-types) ---------------------------------

# Skip cargo-truce dry-run — it would fail because shim-types isn't
# on crates.io at the new version yet. The real publish below resolves
# that. Shim-types dry-run still catches metadata gaps before the
# immutable upload makes the version permanently claimed.
echo
echo "→ cargo publish -p truce-shim-types --dry-run"
cargo publish -p truce-shim-types --dry-run

# Step 5 — real publish (truce-shim-types) ------------------------------------

echo
echo "→ cargo publish -p truce-shim-types"
cargo publish -p truce-shim-types

# Step 6 — wait for index propagation -----------------------------------------

echo
echo "→ sleep 30 (crates.io index propagation)"
sleep 30

# Step 7 — real publish (cargo-truce) -----------------------------------------

echo
echo "→ cargo publish -p cargo-truce"
cargo publish -p cargo-truce

# Step 8 — fast-forward preview branch ---------------------------------------

echo
echo "→ fast-forwarding $TRAIN"

if ! git rev-parse --verify "$TRAIN" >/dev/null 2>&1; then
    # Local branch doesn't exist yet — fetch it.
    git fetch origin "$TRAIN:$TRAIN"
fi

git checkout "$TRAIN"
git merge --ff-only main
git checkout main

# Step 9 — push everything ----------------------------------------------------

echo
echo "→ pushing main, $TRAIN, $TAG"
git push origin main "$TRAIN" "$TAG"

# Step 10 — create GitHub Release --------------------------------------------

echo
echo "→ creating GitHub Release"
gh release create "$TAG" \
    --generate-notes \
    --title "truce $WS_VERSION"

# Step 11 — sync dev/latest to main ------------------------------------------

# Under the rebase-and-merge rule, dev/latest is reachable from
# main, so this FF should always succeed. If it rejects, the merge
# style invariant was violated — print a warning and let the
# maintainer investigate. The release itself has already shipped, so
# don't fail the script; just surface the drift loudly.

echo
echo "→ syncing dev/latest with main"

if git fetch origin dev/latest:dev/latest 2>/dev/null \
    || git rev-parse --verify dev/latest >/dev/null 2>&1; then
    git checkout dev/latest
    if git merge --ff-only main; then
        git push origin dev/latest
        echo "  dev/latest is now at main."
    else
        echo
        echo "WARNING: dev/latest can't fast-forward to main." >&2
        echo "The release itself shipped, but dev/latest is out of sync." >&2
        echo "Likely cause: bump PR was squash-merged instead of rebased." >&2
        echo "See release.md \"Branch sync model\" before forcing anything." >&2
    fi
    git checkout main
else
    echo "  (dev/latest branch not found; skipping sync)"
fi

# Done ------------------------------------------------------------------------

echo
echo "Released $TAG."
echo
echo "Smoke-test from a clean install:"
echo "  cargo install --force cargo-truce --version $WS_VERSION"
echo "  cargo truce --help"
