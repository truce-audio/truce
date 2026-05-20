#!/usr/bin/env bash
#
# release.sh — tag HEAD, publish to crates.io, push, create the
# GitHub Release. Idempotent: each step skips if it's already done.
#
# Usage:
#   scripts/release/release.sh
#
# Run from `main` after the bump PR has been merged. The version is
# read from `Cargo.toml`'s [workspace.package].version, and the tag
# is `vX.Y.Z` against HEAD. (For a hotfix on a previous release,
# check out the earlier commit / tag, run bump.sh + bump-pr.sh from
# there to create a new feature branch + PR, merge, then run this
# script from main with main pointing at the merged hotfix commit.)
#
# Idempotency:
#   - If the tag already exists locally + on origin: skip create + push.
#   - If a crate version is already on crates.io: skip publish.
#   - If the GitHub Release exists: skip create.
#   - Re-running after a partial failure picks up where it left off.
#
# Pre-reqs:
#   - `cargo login <token>` already run.
#   - `gh auth login` already run.
#   - HEAD's Cargo.toml contains the version we want to ship.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

# ----------------------------------------------------------------------------
# Helpers
# ----------------------------------------------------------------------------

is_published_on_crates_io() {
    # Args: <crate> <version>. Returns 0 if the crate@version is on
    # crates.io. Uses the public HTTP API (no cargo dependencies).
    local crate="$1" version="$2"
    curl -sf -o /dev/null \
        "https://crates.io/api/v1/crates/$crate/$version" \
        2>/dev/null
}

is_tag_on_origin() {
    # Args: <tag>. Returns 0 if origin has the tag.
    local tag="$1"
    git ls-remote --tags origin "refs/tags/$tag" 2>/dev/null \
        | grep -q "refs/tags/$tag$"
}

is_github_release_present() {
    # Args: <tag>. Returns 0 if a GitHub Release exists for the tag.
    local tag="$1"
    gh release view "$tag" >/dev/null 2>&1
}

# ----------------------------------------------------------------------------
# Read + verify versions
# ----------------------------------------------------------------------------

echo "→ reading + verifying versions in Cargo.toml"

WS_VERSION="$(awk -F\" '
    /^\[workspace\.package\]/ { p = 1 }
    p && /^version = / { print $2; exit }
' Cargo.toml)"

SHIM_VERSION="$(awk -F\" '
    /^truce-shim-types = / { print $2; exit }
' Cargo.toml)"

BUILD_VERSION="$(awk -F\" '
    /^truce-build = / { print $2; exit }
' Cargo.toml)"

UTILS_VERSION="$(awk -F\" '
    /^truce-utils = / { print $2; exit }
' Cargo.toml)"

if [[ -z "$WS_VERSION" ]]; then
    echo "Error: could not read [workspace.package].version" >&2
    exit 1
fi

if [[ "$WS_VERSION" != "$SHIM_VERSION" \
   || "$WS_VERSION" != "$BUILD_VERSION" \
   || "$WS_VERSION" != "$UTILS_VERSION" ]]; then
    echo "Error: version drift in Cargo.toml" >&2
    echo "  [workspace.package].version               = $WS_VERSION" >&2
    echo "  [workspace.dependencies].truce-shim-types = $SHIM_VERSION" >&2
    echo "  [workspace.dependencies].truce-build      = $BUILD_VERSION" >&2
    echo "  [workspace.dependencies].truce-utils      = $UTILS_VERSION" >&2
    exit 1
fi

TAG="v$WS_VERSION"

echo
echo "Releasing $TAG (HEAD: $(git rev-parse --short HEAD))"
echo

# ----------------------------------------------------------------------------
# Step 1 — local tag
# ----------------------------------------------------------------------------

echo "→ local tag $TAG"
if git rev-parse --verify "$TAG" >/dev/null 2>&1; then
    existing_sha="$(git rev-parse "$TAG^{commit}")"
    head_sha="$(git rev-parse HEAD^{commit})"
    if [[ "$existing_sha" != "$head_sha" ]]; then
        echo "Error: tag $TAG already exists locally but points at" >&2
        echo "  $existing_sha" >&2
        echo "and HEAD is at" >&2
        echo "  $head_sha" >&2
        echo "Delete the stale tag with 'git tag -d $TAG' or check out" >&2
        echo "the right commit before re-running." >&2
        exit 1
    fi
    echo "  already exists at HEAD; using it"
else
    git tag -a "$TAG" -m "truce $WS_VERSION"
    echo "  created"
fi

# ----------------------------------------------------------------------------
# Step 2 — publish leaf crates consumed by cargo-truce
#
# `truce-shim-types`, `truce-build`, and `truce-utils` are all pulled
# in by `cargo-truce`'s lib half. None of them depend on each other,
# so order between them doesn't matter — but all three must land on
# the registry before `cargo publish -p cargo-truce` will resolve.
# ----------------------------------------------------------------------------

PUBLISHED_ANY=0

echo
echo "→ publishing truce-shim-types $WS_VERSION"
if is_published_on_crates_io truce-shim-types "$WS_VERSION"; then
    echo "  already on crates.io; skipping"
else
    cargo publish -p truce-shim-types --dry-run
    cargo publish -p truce-shim-types
    PUBLISHED_ANY=1
fi

echo
echo "→ publishing truce-build $WS_VERSION"
if is_published_on_crates_io truce-build "$WS_VERSION"; then
    echo "  already on crates.io; skipping"
else
    cargo publish -p truce-build --dry-run
    cargo publish -p truce-build
    PUBLISHED_ANY=1
fi

echo
echo "→ publishing truce-utils $WS_VERSION"
if is_published_on_crates_io truce-utils "$WS_VERSION"; then
    echo "  already on crates.io; skipping"
else
    cargo publish -p truce-utils --dry-run
    cargo publish -p truce-utils
    PUBLISHED_ANY=1
fi

# ----------------------------------------------------------------------------
# Step 3 — wait for index propagation (only if we just published)
# ----------------------------------------------------------------------------

if [[ "$PUBLISHED_ANY" == "1" ]]; then
    echo
    echo "→ sleeping 30s for crates.io index propagation"
    sleep 30
fi

# ----------------------------------------------------------------------------
# Step 4 — publish cargo-truce
# ----------------------------------------------------------------------------

echo
echo "→ publishing cargo-truce $WS_VERSION"
if is_published_on_crates_io cargo-truce "$WS_VERSION"; then
    echo "  already on crates.io; skipping"
else
    cargo publish -p cargo-truce
fi

# ----------------------------------------------------------------------------
# Step 5 — push tag
# ----------------------------------------------------------------------------

echo
echo "→ pushing tag $TAG"
if is_tag_on_origin "$TAG"; then
    echo "  already on origin; skipping"
else
    git push origin "$TAG"
fi

# ----------------------------------------------------------------------------
# Step 6 — GitHub Release
# ----------------------------------------------------------------------------

echo
echo "→ creating GitHub Release"
if is_github_release_present "$TAG"; then
    release_url="$(gh release view "$TAG" --json url --jq .url 2>/dev/null || true)"
    echo "  already exists: $release_url"
else
    gh release create "$TAG" \
        --title "truce $WS_VERSION"
#
#        --generate-notes \
fi

# ----------------------------------------------------------------------------
# Done
# ----------------------------------------------------------------------------

echo
echo "Released $TAG."
echo "  https://crates.io/crates/cargo-truce/$WS_VERSION"
echo
echo "Smoke-test from a clean install:"
echo "  cargo install --force cargo-truce --version $WS_VERSION"
echo "  cargo truce --help"
