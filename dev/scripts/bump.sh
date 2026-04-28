#!/usr/bin/env bash
#
# bump.sh — open a release-bump PR.
#
# Usage:
#   dev/scripts/bump.sh patch                # X.Y.Z → X.Y.(Z+1)
#   dev/scripts/bump.sh minor                # X.Y.Z → X.(Y+1).0
#   dev/scripts/bump.sh major                # X.Y.Z → (X+1).0.0
#   dev/scripts/bump.sh 1.0.0-rc.1           # explicit version (any SemVer)
#   dev/scripts/bump.sh 0.16.5               # explicit version (e.g., hotfix)
#
#   dev/scripts/bump.sh --edit-only <bump>   # edit files only, no git/PR
#
# Branches off origin/main, bumps both version strings in
# `Cargo.toml`, refreshes `Cargo.lock`, commits on `bump/vX.Y.Z`,
# pushes, opens a PR against `main`. The PR must be merged using
# GitHub's "Rebase and merge" — branch protection on `main` should
# already enforce this; see DEVELOPMENT.md "Workflow rules".
#
# Idempotent: re-running with the same version resets the bump
# branch to a fresh state and force-pushes. Re-opening the PR is a
# no-op if one's already open for the branch.
#
# With --edit-only, the script only rewrites `Cargo.toml` +
# `Cargo.lock` in the working tree and exits. No clean-tree check,
# no branch, no commit, no push, no PR.

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

# Pre-flight ------------------------------------------------------------------

if (( ! EDIT_ONLY )); then
    echo "→ pre-flight: clean working tree"
    if ! git diff --quiet || ! git diff --cached --quiet; then
        echo "Error: working tree is dirty — commit or stash first" >&2
        exit 1
    fi
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

# Sync main locally + branch off it ------------------------------------------

if (( ! EDIT_ONLY )); then
    echo "→ fetching origin/main"
    git fetch origin main

    BRANCH="bump/v$NEW"

    echo "→ creating bump branch $BRANCH from origin/main"
    git checkout -B "$BRANCH" origin/main
fi

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

# Commit, push, PR ------------------------------------------------------------

if (( EDIT_ONLY )); then
    echo
    echo "Edited Cargo.toml + Cargo.lock for v$NEW. No git operations performed."
    exit 0
fi

echo "→ committing"
git add Cargo.toml Cargo.lock
git commit -m "Release v$NEW"

echo "→ pushing $BRANCH"
git push -u --force-with-lease origin "$BRANCH"

echo "→ opening PR (or surfacing existing)"
existing_pr="$(gh pr list --head "$BRANCH" --state open --json url --jq '.[0].url' 2>/dev/null || true)"
if [[ -n "$existing_pr" ]]; then
    echo "  PR already open: $existing_pr"
else
    gh pr create --base main --title "Release v$NEW" --body "$(cat <<EOF
Mechanical version bump: \`$CURRENT\` → \`$NEW\`.

Diff should be limited to the two version strings in \`Cargo.toml\`
(\`[workspace.package].version\` + the \`truce-shim-types\` entry in
\`[workspace.dependencies]\`) and the corresponding entries in
\`Cargo.lock\`. Reject anything else.

**Merge using "Rebase and merge"** — branch protection on \`main\`
enforces this; the green button should only offer that option.

After merging, ship via:

\`\`\`sh
git checkout main && git pull --ff-only
dev/scripts/release.sh
\`\`\`
EOF
)"
fi

echo
echo "Bump PR ready. After merge, run dev/scripts/release.sh."
