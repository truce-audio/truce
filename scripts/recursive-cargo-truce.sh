#!/usr/bin/env bash
# Thin wrapper over recursive-cargo.sh that runs `cargo truce <args>` in
# the main workspace and in every truce sub-workspace. See
# recursive-cargo.sh for the workspace list, color handling, and
# [OK]/[SKIP]/[FAIL] semantics. truce reports a plugin that lives in a
# different workspace as "No plugin with crate name", which we add to the
# skip-pattern alternation so that miss is treated as [SKIP], not [FAIL].
#
# Usage: recursive-cargo-truce.sh <cargo-truce-args>
# Examples:
#   recursive-cargo-truce.sh build -p truce-example-gain --clap
#   recursive-cargo-truce.sh install -p truce-example-gain --clap
#   recursive-cargo-truce.sh package --vst3
set -uo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [[ $# -eq 0 ]]; then
    cat >&2 <<EOF
usage: $(basename "$0") <cargo-truce-args>

Runs 'cargo truce <args>' in the main workspace and in each sub-workspace.

Sub-workspaces:
  crates/truce-slint
  crates/truce-vizia
  crates/truce-gpu-examples
EOF
    exit 64
fi

export RECURSIVE_CARGO_SKIP_PATTERN='no plugin with crate name'
exec "$script_dir/recursive-cargo.sh" truce "$@"
