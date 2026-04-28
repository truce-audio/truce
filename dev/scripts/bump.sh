#!/usr/bin/env bash
#
# bump.sh — open a release-bump PR.
#
# Usage:
#   development/scripts/bump.sh patch                      # X.Y.Z → X.Y.(Z+1)
#   development/scripts/bump.sh minor                      # X.Y.Z → X.(Y+1).0
#   development/scripts/bump.sh 0.15.0                     # explicit version
#   development/scripts/bump.sh patch --release            # use release/ prefix
#                                                          # (post-1.0 stable line)
#
# Pre-flight: asserts current branch is `dev/latest` with a clean
# tree, pulls latest, then FFs dev/latest to origin/main (catches
# drift from a squash-merged previous bump PR — see release.md
# "Branch sync model").
#
# Then: branches off `dev/latest`, bumps both version strings in
# `Cargo.toml` (the only two post-deduplication), refreshes
# `Cargo.lock`, commits on `<prefix>/vX.Y` (a per-minor bump branch,
# distinct from the train `<prefix>/X.Y` by the `v` prefix), pushes,
# and opens a PR against `main`. The PR must be merged using GitHub's
# "Rebase and merge" — squash-merging breaks the FF invariant.
# Re-running on the same minor (e.g., 0.15.1 → 0.15.2 after a
# previous bump merged) reuses the same branch name; the local
# branch is reset to the new commit.
#
# Prefix selection:
#   --preview (default)  pre-1.0 trains and post-1.0 pre-release testing
#   --release            post-1.0 stable trains
# Pre-1.0 always uses preview/. Post-1.0, both `preview/X.Y` and
# `release/X.Y` may coexist (preview/ for the next minor's RC line,
# release/ for the current stable). The flag picks which one the bump
# targets.
#
# Does NOT tag, push to main, or publish. Run `release.sh` from
# `main` after the PR is reviewed and merged.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

BUMP=""
PREFIX="preview"

for arg in "$@"; do
    case "$arg" in
        --preview) PREFIX="preview" ;;
        --release) PREFIX="release" ;;
        patch|minor) BUMP="$arg" ;;
        *.*.*) BUMP="$arg" ;;
        *)
            echo "Unknown argument: $arg" >&2
            echo "Usage: bump.sh [--preview|--release] patch | minor | X.Y.Z" >&2
            exit 1
            ;;
    esac
done

if [[ -z "$BUMP" ]]; then
    echo "Usage: bump.sh [--preview|--release] patch | minor | X.Y.Z" >&2
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

# Catch dev/latest drift from main before any bump work.
# Under the "rebase-and-merge bump PRs" rule, dev/latest is always
# reachable from main, so this FF should always succeed. If it
# rejects, someone squash-merged the previous bump PR (or
# something equivalent) — investigate before doing anything else,
# don't force.
git fetch origin main
if ! git merge --ff-only origin/main; then
    echo >&2
    echo "Error: dev/latest can't fast-forward to origin/main." >&2
    echo "" >&2
    echo "This usually means the previous bump PR was squash-merged" >&2
    echo "instead of rebase-merged. See release.md \"Branch sync" >&2
    echo "model\" for the recovery — do NOT force-push." >&2
    exit 1
fi
git push origin dev/latest

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

echo "Bumping $CURRENT → $NEW (prefix: $PREFIX)"

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

# Per-minor bump branch — `<prefix>/v0.15` for any patch on 0.15.x,
# `<prefix>/v0.16` for the minor bump that initiates the 0.16 train.
# Named after the NEW version's minor so a minor bump's branch
# matches the train it's introducing. Distinct from the train branch
# `<prefix>/X.Y` (no `v` prefix) so they don't collide. Reused across
# patches on the same minor: `git checkout -B` resets the branch if
# a previous bump's local branch is still around.
IFS=. read -r NEW_MAJOR NEW_MINOR _ <<< "$NEW"
BRANCH="$PREFIX/v$NEW_MAJOR.$NEW_MINOR"

git checkout -B "$BRANCH"
git add Cargo.toml Cargo.lock
git commit -m "Release v$NEW"
git push -u --force-with-lease origin "$BRANCH"

gh pr create --base main --title "Release v$NEW" --body "$(cat <<EOF
Mechanical version bump: \`$CURRENT\` → \`$NEW\`.

Diff should be limited to the two version strings in \`Cargo.toml\`
(\`[workspace.package].version\` + the \`truce-shim-types\` entry in
\`[workspace.dependencies]\`) and the corresponding entries in
\`Cargo.lock\`. Reject anything else.

**Merge via "Rebase and merge"** — not squash. Squash-merging
breaks the dev/latest fast-forward invariant (see release.md
"Branch sync model").

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
