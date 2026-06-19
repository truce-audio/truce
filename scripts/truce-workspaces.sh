#!/usr/bin/env bash
# Single source of truth for the workspace tree: the main workspace plus
# the three sub-workspaces that live in their own Cargo workspaces
# (truce-slint / truce-vizia pin incompatible skia-bindings revs;
# truce-gpu-examples is split so its `truce-gui/gpu` feature can't unify
# across the parent and flip CPU screenshot tests to the GPU renderer).
#
# Sourced by recursive-cargo.sh and supply-chain.sh. Not executable on
# its own. `truce_workspaces <repo-root>` prints each workspace's
# absolute path, one per line, main first.
truce_workspaces() {
    local root="$1"
    printf '%s\n' \
        "$root" \
        "$root/crates/truce-slint" \
        "$root/crates/truce-vizia" \
        "$root/crates/truce-gpu-examples"
}
