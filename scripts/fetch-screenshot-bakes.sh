#!/usr/bin/env bash
# Pull the reference screenshots baked by the "Bake screenshots" workflow
# and copy them over the checked-in references.
#
# Usage:
#   scripts/fetch-screenshot-bakes.sh                 # latest open PR's newest bake run
#   scripts/fetch-screenshot-bakes.sh --pr 251        # a specific PR's newest bake run
#   scripts/fetch-screenshot-bakes.sh --run 123456789 # a specific workflow run
#   ... --platforms linux,macos                       # subset (default: all four)
#   ... --dry-run                                     # report changes, copy nothing
#
# Needs an authenticated `gh`. Only PNGs that differ from the checked-in
# file are copied; new references are added. Nothing is committed.
set -euo pipefail

REPO="truce-audio/truce"
WORKFLOW="bake-screenshots.yml"
platforms="linux,macos,windows,ios"
pr=""
run=""
dry_run=0

while [ $# -gt 0 ]; do
    case "$1" in
        --pr) pr="$2"; shift 2 ;;
        --run) run="$2"; shift 2 ;;
        --platforms) platforms="$2"; shift 2 ;;
        --dry-run) dry_run=1; shift ;;
        -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

root="$(git rev-parse --show-toplevel)"
cd "$root"

if [ -z "$run" ]; then
    if [ -z "$pr" ]; then
        pr="$(gh pr list --repo "$REPO" --state open --limit 1 --json number --jq '.[0].number')"
        [ -n "$pr" ] || { echo "no open PRs on $REPO" >&2; exit 1; }
    fi
    branch="$(gh pr view "$pr" --repo "$REPO" --json headRefName --jq .headRefName)"
    run="$(gh run list --repo "$REPO" --workflow "$WORKFLOW" --branch "$branch" \
        --limit 1 --json databaseId --jq '.[0].databaseId')"
    [ -n "$run" ] || { echo "no \"Bake screenshots\" run for PR #$pr ($branch)" >&2; exit 1; }
    echo "PR #$pr ($branch) -> run $run"
fi

gh run view "$run" --repo "$REPO" --json status,conclusion,headSha,url \
    --jq '"run \(.url)\n  status: \(.status) / \(.conclusion // "-")  sha: \(.headSha[0:8])"'

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

changed=0
added=0
same=0
IFS=',' read -r -a plats <<<"$platforms"
for plat in "${plats[@]}"; do
    name="baked-screenshots-$plat"
    dest="$tmp/$plat"
    if ! gh run download "$run" --repo "$REPO" -n "$name" -D "$dest" 2>/dev/null; then
        echo "  $plat: no artifact (job failed, skipped, or expired)"
        continue
    fi
    # upload-artifact strips the common prefix of its path globs, which is
    # the repo root when several roots matched but can be deeper when only
    # one did. Rebuild each file's repo path from its `examples/` or
    # `crates/` component, or from a bare `<crate>/screenshots/` when the
    # prefix was stripped down to the example dir.
    while IFS= read -r -d '' png; do
        rel="${png#"$dest"/}"
        case "$rel" in
            examples/*|crates/*) target="$rel" ;;
            *)
                match="$(git ls-files -- "*/$rel" | head -n 1)"
                if [ -n "$match" ]; then
                    target="$match"
                else
                    echo "  ?? $plat: can't place $rel, skipped" >&2
                    continue
                fi
                ;;
        esac
        if [ -f "$target" ] && cmp -s "$png" "$target"; then
            same=$((same + 1))
            continue
        fi
        if [ -f "$target" ]; then
            echo "  M $target"
            changed=$((changed + 1))
        else
            echo "  A $target"
            added=$((added + 1))
        fi
        if [ "$dry_run" -eq 0 ]; then
            mkdir -p "$(dirname "$target")"
            cp "$png" "$target"
        fi
    done < <(find "$dest" -type f -name '*.png' -print0)
done

verb="updated"
[ "$dry_run" -eq 1 ] && verb="would update"
echo "$verb: $changed changed, $added new, $same unchanged"
