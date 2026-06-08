#!/usr/bin/env bash
# Run `cargo truce <args>` in the main workspace and in every truce sub-
# workspace (currently `crates/truce-vizia` and `crates/truce-slint`).
# Each sub-workspace has its own `truce.toml` because vizia / slint pin
# incompatible skia-bindings revs and can't share a Cargo workspace with
# the main truce build.
#
# Usage: recursive-cargo-truce.sh <cargo-truce-args>
# Examples:
#   recursive-cargo-truce.sh build -p truce-example-gain
#   recursive-cargo-truce.sh install -p truce-example-gain --format clap
#   recursive-cargo-truce.sh package --format vst3
#
# Sub-workspaces only see packages declared in their own `truce.toml` /
# `Cargo.toml`; passing `-p` for a package the sub-workspace doesn't
# know about produces a "no matching packages" error which the script
# reports but does not treat as fatal. Run sequentially (not in
# parallel) to keep stdout / stderr interleaved cleanly.
set -uo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root_dir="$(cd "$script_dir/.." && pwd)"

if [[ $# -eq 0 ]]; then
    cat >&2 <<EOF
usage: $(basename "$0") <cargo-truce-args>

Runs 'cargo truce <args>' in the main workspace and in each sub-workspace.

Sub-workspaces:
  crates/truce-slint
  crates/truce-vizia
EOF
    exit 64
fi

workspaces=(
    "$root_dir"
    "$root_dir/crates/truce-slint"
    "$root_dir/crates/truce-vizia"
)

overall_status=0
for ws in "${workspaces[@]}"; do
    label="${ws#"$root_dir"}"
    label="${label#/}"
    [[ -z "$label" ]] && label="(main)"
    printf '\n=== cargo truce %s [%s] ===\n' "$*" "$label"
    if ( cd "$ws" && cargo truce "$@" ); then
        printf '[ OK ] %s\n' "$label"
    else
        rc=$?
        printf '[FAIL] %s (exit %d)\n' "$label" "$rc" >&2
        overall_status=$rc
    fi
done

exit "$overall_status"
