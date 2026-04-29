#!/usr/bin/env bash
#
# bump-pr.sh — push the current bump branch and open the release PR.
#
# Usage:
#   dev/scripts/bump-pr.sh
#
# Run after `dev/scripts/bump.sh` has committed the version bump on
# a `bump/vX.Y.Z` branch. This script:
#
#   1. Pushes the branch with `--force-with-lease` (idempotent — safe
#      to re-run after a `bump.sh` re-run that reset the branch).
#   2. Opens a PR against `main`, or surfaces the existing one if
#      one's already open for the branch.
#
# The PR must be merged using GitHub's "Rebase and merge" — branch
# protection on `main` should already enforce this; see
# DEVELOPMENT.md "Workflow rules".
#
# The new version is read from the current branch name
# (`bump/vX.Y.Z`); the previous version is read from
# `origin/main:Cargo.toml` for the PR body. Both are checked against
# `Cargo.toml` HEAD as a sanity check before pushing.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
    sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
fi

# Branch + version ------------------------------------------------------------

BRANCH="$(git rev-parse --abbrev-ref HEAD)"
if [[ "$BRANCH" != bump/v* ]]; then
    echo "Error: expected to be on a bump/vX.Y.Z branch (got '$BRANCH')." >&2
    echo "Run dev/scripts/bump.sh first." >&2
    exit 1
fi

NEW="${BRANCH#bump/v}"

# Sanity-check that HEAD's Cargo.toml actually carries this version
# — guards against running bump-pr.sh on a stale branch where the
# user hand-edited the bump commit away.
HEAD_VERSION="$(awk -F\" '
    /^\[workspace\.package\]/ { p = 1 }
    p && /^version = / { print $2; exit }
' Cargo.toml)"

if [[ "$HEAD_VERSION" != "$NEW" ]]; then
    echo "Error: branch name says v$NEW but Cargo.toml HEAD has v$HEAD_VERSION." >&2
    echo "Re-run dev/scripts/bump.sh to re-create the branch." >&2
    exit 1
fi

# Previous version, for the PR body. Fetch first so we compare
# against current origin/main.
echo "→ fetching origin/main"
git fetch origin main

CURRENT="$(git show origin/main:Cargo.toml | awk -F\" '
    /^\[workspace\.package\]/ { p = 1 }
    p && /^version = / { print $2; exit }
')"

if [[ -z "$CURRENT" ]]; then
    echo "Error: could not read [workspace.package].version from origin/main" >&2
    exit 1
fi

echo
echo "Pushing $BRANCH ($CURRENT → $NEW)"
echo

# Push ------------------------------------------------------------------------

echo "→ pushing $BRANCH"
git push -u --force-with-lease origin "$BRANCH"

# Open PR (or surface existing) ----------------------------------------------

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
