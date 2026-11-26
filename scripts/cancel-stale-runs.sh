#!/usr/bin/env bash
# Cancel queued / in-progress GitHub Actions runs on a PR's branch that
# aren't for its current head commit - stale runs left behind when the PR
# moved on. With --all, also every other active run in the repo (other
# PRs, main, release runs).
#
# Usage:
#   scripts/cancel-stale-runs.sh                  # latest open PR, dry run
#   scripts/cancel-stale-runs.sh --pr 249         # a specific PR, dry run
#   scripts/cancel-stale-runs.sh --pr 249 --yes   # actually cancel
#   ... --all                                     # repo-wide, not just the PR's branch
#
# Needs an authenticated `gh` and `jq`. Runs GitHub refuses to cancel (e.g.
# a run whose jobs all finished but which never closed) are reported, not
# fatal; failing to list runs at all is fatal.
set -euo pipefail

REPO="truce-audio/truce"
pr=""
yes=0
branch_only=1

while [ $# -gt 0 ]; do
    case "$1" in
        --pr) [ $# -ge 2 ] && [ -n "$2" ] || { echo "--pr needs a PR number" >&2; exit 2; }
              pr="$2"; shift 2 ;;
        --yes) yes=1; shift ;;
        --branch) branch_only=1; shift ;;   # the default; kept for old callers
        --all) branch_only=0; shift ;;
        -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

if [ -z "$pr" ]; then
    pr="$(gh pr list --repo "$REPO" --state open --limit 1 --json number --jq '.[0].number')"
    [ -n "$pr" ] || { echo "no open PRs on $REPO" >&2; exit 1; }
fi
read -r head branch < <(gh pr view "$pr" --repo "$REPO" --json headRefOid,headRefName \
    --jq '"\(.headRefOid) \(.headRefName)"')
echo "PR #$pr ($branch) head ${head:0:8}: keeping its runs"
if [ "$branch_only" -eq 1 ]; then
    echo "scope: runs on $branch only (pass --all for every branch)"
else
    echo "scope: every active run in $REPO, other PRs and main included"
fi

# List every active run up front, failing loudly: a listing error (auth,
# rate limit, network) must not read as "nothing to cancel". Filter with a
# real jq so the head / branch are passed as data (`--arg`), never spliced
# into the program.
runs=""
for status in queued in_progress waiting pending requested; do
    json="$(gh run list --repo "$REPO" --status "$status" --limit 500 \
        --json databaseId,headSha,headBranch,workflowName)" \
        || { echo "failed to list $status runs" >&2; exit 1; }
    runs+="$(jq -r --arg head "$head" --arg branch "$branch" --argjson only "$branch_only" '
        .[] | select(.headSha != $head and ($only == 0 or .headBranch == $branch))
        | [.databaseId, (.workflowName // "?"), (.headBranch // "?"), (.headSha // "")]
        | map(tostring) | join("\u001f")' <<<"$json")"$'\n'
done
runs="$(printf '%s' "$runs" | sed '/^$/d' | sort -u)"

cancelled=0
refused=0
listed=0
# One run per line (unit-separated fields, so an empty field can't shift
# the others) - never word-split a list of ids.
while IFS=$'\x1f' read -r id wf br sha; do
    [ -n "$id" ] || continue
    listed=$((listed + 1))
    printf '  %s  %s  %s@%s' "$id" "$wf" "$br" "${sha:0:8}"
    if [ "$yes" -eq 0 ]; then
        echo "  (would cancel)"
        continue
    fi
    if out="$(gh run cancel "$id" --repo "$REPO" 2>&1)"; then
        echo "  cancelled"
        cancelled=$((cancelled + 1))
    else
        echo "  refused: ${out##*$'\n'}"
        refused=$((refused + 1))
    fi
done <<<"$runs"

if [ "$yes" -eq 1 ]; then
    echo "cancelled $cancelled, refused $refused"
else
    echo "would cancel $listed (pass --yes to cancel)"
fi
