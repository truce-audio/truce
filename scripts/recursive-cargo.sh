#!/usr/bin/env bash
# Run `cargo <args>` in the main workspace and in every sub-workspace
# (currently `crates/truce-vizia`, `crates/truce-slint`, and
# `crates/truce-gpu-examples`). Each sub-workspace is its own Cargo
# workspace: vizia / slint pin incompatible skia-bindings revs and can't
# share a workspace with the main truce build; truce-gpu-examples is
# split out because its `truce-gui/gpu` feature request would unify
# across the parent workspace and silently flip every CPU screenshot
# test to the GPU renderer.
#
# Usage: recursive-cargo.sh <cargo-args>
# Examples:
#   recursive-cargo.sh build -p truce-example-gain
#   recursive-cargo.sh test
#   recursive-cargo.sh clippy --all-targets
#
# A `-p` for a package a given sub-workspace doesn't contain makes cargo
# exit non-zero with a "no matching packages" message. Since each plugin
# lives in exactly one workspace, that miss is expected - the script
# reports it as [SKIP] and does NOT fail the overall run. Callers that
# wrap a subcommand with its own miss message can extend the skip match
# via $RECURSIVE_CARGO_SKIP_PATTERN (an extended-regex alternation). A
# genuine build error is still [FAIL] and propagates. Run sequentially
# (not in parallel) to keep stdout / stderr interleaved cleanly.
set -uo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root_dir="$(cd "$script_dir/.." && pwd)"

# Pick the cargo binary. On Windows the truce build must use the
# Windows toolchain, so prefer `cargo.exe` whenever it's on PATH -
# including under WSL, where bare `cargo` is the Linux toolchain that
# can't build the Windows plug-ins (and where slint's deps need
# fontconfig/pkg-config that aren't installed). `cargo.exe` only
# exists on Windows / WSL / Git Bash, so this is a no-op on native
# Linux/macOS. Override explicitly with `CARGO=...`.
if [[ -n "${CARGO:-}" ]]; then
    cargo_bin="$CARGO"
elif command -v cargo.exe >/dev/null 2>&1; then
    cargo_bin="cargo.exe"
else
    cargo_bin="cargo"
fi

# Keep colored output through the `tee` pipe below. cargo and rustc
# disable ANSI when stdout isn't a TTY, and the pipe to `tee` is not
# one - so a direct run is colored but this wrapper's was plain. Force
# color on when *our* own stdout is a real terminal (leave it off for a
# redirected / piped run so files don't fill with escape codes).
# `CARGO_TERM_COLOR` covers cargo + the rustc it drives; `CLICOLOR_FORCE`
# covers clicolors-spec subcommands like cargo-truce. Respect either if
# the caller already set it.
if [[ -t 1 ]]; then
    : "${CARGO_TERM_COLOR:=always}"
    : "${CLICOLOR_FORCE:=1}"
    export CARGO_TERM_COLOR CLICOLOR_FORCE
fi

if [[ $# -eq 0 ]]; then
    cat >&2 <<EOF
usage: $(basename "$0") <cargo-args>

Runs 'cargo <args>' in the main workspace and in each sub-workspace.

Sub-workspaces:
  crates/truce-slint
  crates/truce-vizia
  crates/truce-gpu-examples
EOF
    exit 64
fi

# Packages absent from a sub-workspace are an expected miss; cargo phrases
# it as "no matching packages" or "did not match any packages" depending
# on the subcommand. Callers can add their own subcommand-specific miss
# message to the alternation.
skip_pattern='no matching packages|did not match any packages'
if [[ -n "${RECURSIVE_CARGO_SKIP_PATTERN:-}" ]]; then
    skip_pattern="$skip_pattern|$RECURSIVE_CARGO_SKIP_PATTERN"
fi

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
    printf '\n=== %s %s [%s] ===\n' "$cargo_bin" "$*" "$label"
    # `tee` to a temp file so output still streams live while we
    # inspect it for the "package not in this sub-workspace" miss.
    # `PIPESTATUS[0]` is cargo's exit code, not tee's.
    tmp="$(mktemp)"
    ( cd "$ws" && "$cargo_bin" "$@" ) 2>&1 | tee "$tmp"
    rc=${PIPESTATUS[0]}
    if [[ $rc -eq 0 ]]; then
        printf '[ OK ] %s\n' "$label"
    elif grep -qiE "$skip_pattern" "$tmp"; then
        # Expected: the requested package lives in a different workspace.
        printf '[SKIP] %s (package not in this workspace)\n' "$label"
    else
        printf '[FAIL] %s (exit %d)\n' "$label" "$rc" >&2
        overall_status=$rc
    fi
    rm -f "$tmp"
done

exit "$overall_status"
