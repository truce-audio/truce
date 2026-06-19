#!/usr/bin/env bash
# Supply-chain audit for the whole workspace tree: runs `cargo audit`
# (RustSec advisory scan) and `cargo deny check` (advisories + licenses +
# bans + sources, policy in deny.toml) in the main workspace and every
# sub-workspace. Both pass through scripts/recursive-cargo.sh, which owns
# the workspace list, color forcing, and per-workspace [OK]/[FAIL]
# reporting.
#
# Usage: supply-chain.sh
#
# Requires `cargo audit` (cargo-audit) and `cargo deny` (cargo-deny) on
# PATH. Exits non-zero if either tool fails in any workspace. deny.toml
# at the repo root is shared across all workspaces via `--config`, so the
# policy stays DRY (each sub-workspace pulls a different crate subset).
set -uo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root_dir="$(cd "$script_dir/.." && pwd)"
recurse="$script_dir/recursive-cargo.sh"

for tool in cargo-audit cargo-deny; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        printf 'error: %s not found on PATH (install with `cargo install %s`)\n' \
            "$tool" "$tool" >&2
        exit 127
    fi
done

status=0

printf '\n########## cargo audit ##########\n'
"$recurse" audit || status=$?

printf '\n########## cargo deny check ##########\n'
"$recurse" deny check --config "$root_dir/deny.toml" || status=$?

exit "$status"
