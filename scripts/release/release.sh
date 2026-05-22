#!/usr/bin/env bash
#
# release.sh — tag HEAD, publish every truce workspace crate to
# crates.io, push the tag, create the GitHub Release. Idempotent:
# each step skips if it's already done.
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
# Publish order is computed from `cargo metadata` — every workspace
# member under `crates/` (i.e. excluding `examples/*`) is published
# in topological dep order. Each step is idempotent against
# crates.io, so re-running after a partial failure picks up where
# it left off.
#
# Rate-limit handling
# -------------------
# crates.io throttles new-crate publishes more aggressively than
# version updates of existing crates. The script:
#   1. Sleeps `INDEX_PROP_DELAY` after every successful publish so
#      the next dependent crate's resolver sees the new index entry.
#   2. Detects rate-limit errors in cargo's stderr (`429`,
#      "rate limit", "too many requests") and retries the same
#      crate with exponential backoff up to `MAX_RETRY_ATTEMPTS`.
# A first-time-publishing-everything run hits the new-crate token
# bucket; the retry loop absorbs that without operator intervention.
#
# Pre-reqs:
#   - `cargo login <token>` already run.
#   - `gh auth login` already run.
#   - HEAD's Cargo.toml contains the version we want to ship.
#   - Every internal workspace dep in [workspace.dependencies] has
#     `version = "<workspace version>"` (verified below).

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

# ---------------------------------------------------------------------------
# Tunables
# ---------------------------------------------------------------------------

# Wait for the sparse index to surface a freshly-published crate
# before publishing anything that depends on it. ~30 s has been a
# safe margin against historical index lag.
INDEX_PROP_DELAY="${INDEX_PROP_DELAY:-30}"

# Initial backoff after a rate-limit failure, in seconds. Doubles
# each retry. With the defaults (60 s, 5 doublings) total worst-case
# wait per crate is ~63 minutes — comfortably above crates.io's
# strictest sustained throttle window for new-crate publishes.
RATE_LIMIT_INITIAL_DELAY="${RATE_LIMIT_INITIAL_DELAY:-60}"
MAX_RETRY_ATTEMPTS="${MAX_RETRY_ATTEMPTS:-6}"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

is_published_on_crates_io() {
    # Args: <crate> <version>. Returns 0 if the crate@version is on
    # crates.io. Uses the public HTTP API (no cargo dependency).
    local crate="$1" version="$2"
    curl -sf -o /dev/null \
        "https://crates.io/api/v1/crates/$crate/$version" \
        2>/dev/null
}

crate_exists_on_crates_io() {
    # Args: <crate>. Returns 0 if any version of the crate is on
    # crates.io (i.e. publishing this version is an UPDATE, not a
    # first publish).
    local crate="$1"
    curl -sf -o /dev/null \
        "https://crates.io/api/v1/crates/$crate" \
        2>/dev/null
}

is_tag_on_origin() {
    local tag="$1"
    git ls-remote --tags origin "refs/tags/$tag" 2>/dev/null \
        | grep -q "refs/tags/$tag$"
}

is_github_release_present() {
    local tag="$1"
    gh release view "$tag" >/dev/null 2>&1
}

# ---------------------------------------------------------------------------
# Read + verify workspace version, then verify every internal dep
# pins the same version (cargo strips `path` at publish time and
# embeds the registry version; drift here breaks downstream
# resolution after publish).
# ---------------------------------------------------------------------------

echo "→ reading + verifying versions in Cargo.toml"

WS_VERSION="$(awk -F\" '
    /^\[workspace\.package\]/ { p = 1 }
    p && /^version = / { print $2; exit }
' Cargo.toml)"

if [[ -z "$WS_VERSION" ]]; then
    echo "Error: could not read [workspace.package].version" >&2
    exit 1
fi

DRIFT="$(awk -v want="$WS_VERSION" '
    /^\[workspace\.dependencies\]/ { in_deps = 1; next }
    in_deps && /^\[/ { in_deps = 0 }
    in_deps && /^truce/ && /version *=/ && /path *=/ {
        if (match($0, /version *= *"[^"]*"/)) {
            v = substr($0, RSTART, RLENGTH)
            sub(/^version *= *"/, "", v)
            sub(/"$/, "", v)
            name = $0
            sub(/ *=.*/, "", name)
            if (v != want) printf "  %s = \"%s\"\n", name, v
        }
    }
' Cargo.toml)"

if [[ -n "$DRIFT" ]]; then
    echo "Error: workspace dependency version drift vs $WS_VERSION:" >&2
    echo "$DRIFT" >&2
    exit 1
fi

TAG="v$WS_VERSION"

echo
echo "Releasing $TAG (HEAD: $(git rev-parse --short HEAD))"
echo

# ---------------------------------------------------------------------------
# Step 1 — local tag
# ---------------------------------------------------------------------------

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

# ---------------------------------------------------------------------------
# Step 2 — compute publish order from cargo metadata
#
# Filter: workspace members under `crates/` (examples are scaffolded
# demonstrations, not published). Topo-sort by intra-workspace
# dependencies so every dep is on the registry before its dependents.
# ---------------------------------------------------------------------------

echo
echo "→ computing publish order from cargo metadata"

# Inline-Python topo sort over `cargo metadata`. Stashed in a
# tempfile rather than fed inline via `$(python3 - <<'PY' …)` so
# the script stays compatible with bash 3.2 (the system bash that
# ships with macOS), which has a long-standing parser bug around
# heredocs nested inside command substitution.
TOPO_PY="$(mktemp -t truce-topo.XXXXXX.py)"
trap 'rm -f "$TOPO_PY"' EXIT
cat >"$TOPO_PY" <<'PY'
import json
import subprocess
import sys

meta = json.loads(subprocess.check_output(
    ["cargo", "metadata", "--format-version", "1", "--no-deps"]))

ws_members = set(meta["workspace_members"])
pkgs = [
    p for p in meta["packages"]
    if p["id"] in ws_members
    and "/crates/" in p["manifest_path"]
    and p.get("publish") != []  # opt-out via `publish = false`
]
names = {p["name"] for p in pkgs}

# Edges: package -> set of intra-workspace dep names. A dep listed
# multiple times (e.g. once per target / per kind) collapses naturally
# through the set. Dev-deps (`kind == "dev"`) are stripped at publish
# time, so excluding them here matches the actual on-registry shape.
# The current workspace is cycle-free in normal deps (test crates
# that need a `truce` dev-dep live in their own publish=false crate,
# `truce-loader-tests`), but the filter is kept defensively: a
# future test relocation that re-introduces a back-edge shouldn't
# silently break the publish topology.
incoming = {p["name"]: set() for p in pkgs}
for p in pkgs:
    for d in p["dependencies"]:
        if d.get("kind") == "dev":
            continue
        if d["name"] in names and d["name"] != p["name"]:
            incoming[p["name"]].add(d["name"])

# Kahn topo sort, alphabetical tie-break for determinism.
order = []
ready = sorted(n for n, deps in incoming.items() if not deps)
while ready:
    n = ready.pop(0)
    order.append(n)
    for m, deps in list(incoming.items()):
        if n in deps:
            deps.discard(n)
            if not deps and m not in order and m not in ready:
                ready.append(m)
    ready.sort()

remaining = [n for n in incoming if n not in order]
if remaining:
    sys.exit(f"cycle: unresolved={remaining}")

print("\n".join(order))
PY

ORDER="$(python3 "$TOPO_PY")"

if [[ -z "$ORDER" ]]; then
    echo "Error: no publishable crates found under crates/" >&2
    exit 1
fi

echo
echo "Publish order:"
printf '%s\n' "$ORDER" | sed 's/^/  /'

# ---------------------------------------------------------------------------
# Step 3 — publish each crate in topo order
# ---------------------------------------------------------------------------

publish_one() {
    # Run `cargo publish -p <crate>`, retrying on rate-limit errors
    # with exponential backoff. Returns non-zero on a non-rate-limit
    # failure or after MAX_RETRY_ATTEMPTS rate-limit retries.
    local crate="$1"
    local log delay attempts
    log="$(mktemp -t truce-publish.XXXXXX)"
    delay="$RATE_LIMIT_INITIAL_DELAY"
    attempts=0

    while (( attempts < MAX_RETRY_ATTEMPTS )); do
        attempts=$((attempts + 1))
        # pipefail (set above) propagates cargo's exit through tee.
        if cargo publish -p "$crate" 2>&1 | tee "$log"; then
            rm -f "$log"
            return 0
        fi

        if grep -qiE '429|rate.?limit|too many requests' "$log"; then
            if (( attempts >= MAX_RETRY_ATTEMPTS )); then
                echo "Error: $crate still rate-limited after $attempts attempts" >&2
                rm -f "$log"
                return 1
            fi
            echo "  rate-limited; sleeping ${delay}s before retry $((attempts + 1))/$MAX_RETRY_ATTEMPTS"
            sleep "$delay"
            delay=$((delay * 2))
            continue
        fi

        # Non-rate-limit failure — bail immediately so the operator
        # can see the compile / network / auth error.
        rm -f "$log"
        return 1
    done

    rm -f "$log"
    return 1
}

echo
echo "→ publishing crates"

while IFS= read -r crate; do
    [[ -z "$crate" ]] && continue
    echo
    echo "→ $crate $WS_VERSION"

    if is_published_on_crates_io "$crate" "$WS_VERSION"; then
        echo "  already on crates.io at $WS_VERSION; skipping"
        continue
    fi

    if crate_exists_on_crates_io "$crate"; then
        echo "  (existing crate — publishing new version)"
    else
        echo "  (NEW crate — first publish; rate-limit retries enabled)"
    fi

    publish_one "$crate"

    echo "  sleeping ${INDEX_PROP_DELAY}s for index propagation"
    sleep "$INDEX_PROP_DELAY"
done <<< "$ORDER"

# ---------------------------------------------------------------------------
# Step 4 — push tag
# ---------------------------------------------------------------------------

echo
echo "→ pushing tag $TAG"
if is_tag_on_origin "$TAG"; then
    echo "  already on origin; skipping"
else
    git push origin "$TAG"
fi

# ---------------------------------------------------------------------------
# Step 5 — GitHub Release
# ---------------------------------------------------------------------------

echo
echo "→ creating GitHub Release"
if is_github_release_present "$TAG"; then
    release_url="$(gh release view "$TAG" --json url --jq .url 2>/dev/null || true)"
    echo "  already exists: $release_url"
else
    gh release create "$TAG" \
        --title "truce $WS_VERSION"
fi

# ---------------------------------------------------------------------------
# Done
# ---------------------------------------------------------------------------

echo
echo "Released $TAG."
echo "  https://crates.io/crates/cargo-truce/$WS_VERSION"
echo
echo "Smoke-test from a clean install:"
echo "  cargo install --force cargo-truce --version $WS_VERSION"
echo "  cargo truce --help"
