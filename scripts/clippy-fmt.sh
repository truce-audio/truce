#!/usr/bin/env bash
# Run `cargo clippy --fix --allow-dirty --all-features --all-targets`
# followed by `cargo fmt` in the main workspace and every truce sub-
# workspace. The verification gate before declaring a change done.
#
# Sub-workspaces (each with its own Cargo.toml):
#   crates/truce-slint
#   crates/truce-vizia
#   crates/truce-gpu-examples
#
# Sequential (not parallel) so stdout / stderr interleave cleanly.

set -uo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root_dir="$(cd "$script_dir/.." && pwd)"

workspaces=(
    "$root_dir"
    "$root_dir/crates/truce-slint"
    "$root_dir/crates/truce-vizia"
    "$root_dir/crates/truce-gpu-examples"
)

overall_status=0
for ws in "${workspaces[@]}"; do
    label="${ws#"$root_dir"}"
    label="${label#/}"
    [[ -z "$label" ]] && label="(main)"
    printf '\n=== clippy --fix [%s] ===\n' "$label"
    if ! ( cd "$ws" && cargo clippy --fix --allow-dirty \
        --all-features --all-targets ); then
        rc=$?
        printf '[FAIL] clippy %s (exit %d)\n' "$label" "$rc" >&2
        overall_status=$rc
        continue
    fi
    printf '\n=== fmt [%s] ===\n' "$label"
    if ! ( cd "$ws" && cargo fmt ); then
        rc=$?
        printf '[FAIL] fmt %s (exit %d)\n' "$label" "$rc" >&2
        overall_status=$rc
        continue
    fi
    printf '[ OK ] %s\n' "$label"
done

exit "$overall_status"
