#!/usr/bin/env bash
#
# bump.sh — bump the workspace version and commit on the current branch.
#
# Usage:
#   bump.sh patch                # X.Y.Z → X.Y.(Z+1)
#   bump.sh minor                # X.Y.Z → X.(Y+1).0
#   bump.sh major                # X.Y.Z → (X+1).0.0
#   bump.sh 1.0.0-rc.1           # explicit version (any SemVer)
#   bump.sh 0.16.5               # explicit version (e.g., hotfix)
#
#   bump.sh --edit-only <bump>   # rewrite files only, no commit
#
# Edits both version strings in `Cargo.toml`, refreshes `Cargo.lock`,
# and commits the result on whatever branch you're currently on. No
# branch creation, no fetch, no push, no PR — that's `bump-pr.sh`'s
# job.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

EDIT_ONLY=0
BUMP=""
for arg in "$@"; do
    case "$arg" in
        --edit-only) EDIT_ONLY=1 ;;
        -h|--help)
            sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        -*)
            echo "Error: unknown flag $arg" >&2
            exit 1
            ;;
        *)
            if [[ -n "$BUMP" ]]; then
                echo "Error: unexpected extra argument $arg" >&2
                exit 1
            fi
            BUMP="$arg"
            ;;
    esac
done

if [[ -z "$BUMP" ]]; then
    echo "Usage: bump.sh [--edit-only] patch | minor | major | <X.Y.Z>" >&2
    exit 1
fi

# Read current version + compute new -----------------------------------------

echo "→ reading current version"
CURRENT="$(awk -F\" '
    /^\[workspace\.package\]/ { p = 1 }
    p && /^version = / { print $2; exit }
' Cargo.toml)"

if [[ -z "$CURRENT" ]]; then
    echo "Error: could not read [workspace.package].version" >&2
    exit 1
fi

case "$BUMP" in
    patch|minor|major)
        # Strip pre-release suffix (e.g., -rc.1) before SemVer math.
        BASE="${CURRENT%%-*}"
        IFS=. read -r MAJOR MINOR PATCH <<< "$BASE"
        case "$BUMP" in
            patch) NEW="$MAJOR.$MINOR.$((PATCH + 1))" ;;
            minor) NEW="$MAJOR.$((MINOR + 1)).0" ;;
            major) NEW="$((MAJOR + 1)).0.0" ;;
        esac
        ;;
    *)
        # Explicit version — accept any SemVer string verbatim
        # (including pre-release suffixes like 1.0.0-rc.1).
        NEW="$BUMP"
        ;;
esac

echo
echo "Bumping $CURRENT → $NEW"
echo

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
# `[workspace.package].version` (source of truth) and the
# `truce-shim-types` entry in `[workspace.dependencies]`
# (load-bearing for crates.io publish).
echo "→ editing Cargo.toml"
sed_inplace "s/\"$CURRENT\"/\"$NEW\"/g" Cargo.toml

# Refresh Cargo.lock ----------------------------------------------------------

echo "→ refreshing Cargo.lock (cargo check --workspace)"
cargo check --workspace

# Commit ----------------------------------------------------------------------

if (( EDIT_ONLY )); then
    echo
    echo "Edited Cargo.toml + Cargo.lock for v$NEW. No commit made."
    exit 0
fi

echo "→ committing on $(git rev-parse --abbrev-ref HEAD)"
git add Cargo.toml Cargo.lock
git commit -m "Release v$NEW"

echo
echo "Bump committed. Run bump-pr.sh to push and open the PR."
