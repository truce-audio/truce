#!/usr/bin/env bash
#
# bump.sh — open a release-bump PR.
#
# Usage:
#   development/scripts/bump.sh patch          # X.Y.Z → X.Y.(Z+1)
#   development/scripts/bump.sh minor          # X.Y.Z → X.(Y+1).0
#   development/scripts/bump.sh 0.15.0         # explicit version
#
# Branches off `dev/latest`, bumps both version strings in
# `Cargo.toml` (the only two post-deduplication), refreshes
# `Cargo.lock`, commits on `release/vX.Y.Z`, pushes, and opens a PR
# against `main`.
#
# Does NOT tag, push to main, or publish. Run `release.sh` from
# `main` after the PR is reviewed and merged.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

BUMP="${1:-}"

if [[ -z "$BUMP" ]]; then
    echo "Usage: bump.sh patch | minor | X.Y.Z" >&2
    exit 1
fi

# Pre-flight ------------------------------------------------------------------

current_branch="$(git symbolic-ref --short HEAD)"
if [[ "$current_branch" != "dev/latest" ]]; then
    echo "Error: must be on dev/latest (currently on $current_branch)" >&2
    exit 1
fi

if ! git diff --quiet || ! git diff --cached --quiet; then
    echo "Error: working tree is dirty — commit or stash first" >&2
    exit 1
fi

git pull --ff-only

# Read current version + compute new -----------------------------------------

CURRENT="$(awk -F\" '
    /^\[workspace\.package\]/ { p = 1 }
    p && /^version = / { print $2; exit }
' Cargo.toml)"

if [[ -z "$CURRENT" ]]; then
    echo "Error: could not read [workspace.package].version from Cargo.toml" >&2
    exit 1
fi

IFS=. read -r MAJOR MINOR PATCH <<< "$CURRENT"

case "$BUMP" in
    patch) NEW="$MAJOR.$MINOR.$((PATCH + 1))" ;;
    minor) NEW="$MAJOR.$((MINOR + 1)).0" ;;
    *.*.*) NEW="$BUMP" ;;
    *)
        echo "Usage: bump.sh patch | minor | X.Y.Z" >&2
        exit 1
        ;;
esac

echo "Bumping $CURRENT → $NEW"

# Edit Cargo.toml -------------------------------------------------------------

# Portable in-place sed (BSD on macOS uses `-i ''`, GNU on Linux uses `-i`).
sed_inplace() {
    if [[ "$(uname)" == "Darwin" ]]; then
        sed -i '' "$@"
    else
        sed -i "$@"
    fi
}

# Both occurrences of the version string live in Cargo.toml:
# `[workspace.package].version` (source of truth, every member crate
# inherits) and the `truce-shim-types` entry in
# `[workspace.dependencies]` (load-bearing for crates.io publish).
sed_inplace "s/\"$CURRENT\"/\"$NEW\"/g" Cargo.toml

# Refresh Cargo.lock ----------------------------------------------------------

cargo check --workspace

# Commit, push, PR ------------------------------------------------------------

BRANCH="release/v$NEW"

if git rev-parse --verify "$BRANCH" >/dev/null 2>&1; then
    echo "Error: branch $BRANCH already exists locally — delete it first" >&2
    exit 1
fi

git checkout -b "$BRANCH"
git add Cargo.toml Cargo.lock
git commit -m "Release v$NEW"
git push -u origin "$BRANCH"

gh pr create --base main --title "Release v$NEW" --body "$(cat <<EOF
Mechanical version bump: \`$CURRENT\` → \`$NEW\`.

Diff should be limited to the two version strings in \`Cargo.toml\`
(\`[workspace.package].version\` + the \`truce-shim-types\` entry in
\`[workspace.dependencies]\`) and the corresponding entries in
\`Cargo.lock\`. Reject anything else.

Once CI is green and the PR is merged, ship via:

\`\`\`sh
git checkout main
git pull --ff-only
development/scripts/release.sh
\`\`\`
EOF
)"

echo
echo "Bump PR opened. After merge, run development/scripts/release.sh."
