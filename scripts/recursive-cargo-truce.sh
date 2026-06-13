#!/usr/bin/env bash
# Run `cargo truce <args>` in the main workspace and in every truce sub-
# workspace (currently `crates/truce-vizia`, `crates/truce-slint`, and
# `crates/truce-gpu-examples`). Each sub-workspace has its own
# `truce.toml`. vizia / slint live in their own workspaces because they
# pin incompatible skia-bindings revs and can't share a Cargo workspace
# with the main truce build; truce-gpu-examples is split out because its
# `truce-gui/gpu` feature request would unify across the parent
# workspace and silently flip every CPU screenshot test to the GPU
# renderer.
#
# Usage: recursive-cargo-truce.sh <cargo-truce-args>
# Examples:
#   recursive-cargo-truce.sh build -p truce-example-gain --clap
#   recursive-cargo-truce.sh install -p truce-example-gain --clap
#   recursive-cargo-truce.sh package --vst3
#
# Sub-workspaces only see plugins declared in their own `truce.toml` /
# `Cargo.toml`; passing `-p` for a plugin the sub-workspace doesn't
# know about makes `cargo truce` exit non-zero with a "No plugin with
# crate name" / "no matching packages" message. Since each plugin lives
# in exactly one workspace, that miss is expected - the script reports
# it as [SKIP] and does NOT fail the overall run. A genuine build error
# is still [FAIL] and propagates. Run sequentially (not in parallel) to
# keep stdout / stderr interleaved cleanly.
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

# Keep colored output through the `tee` pipe below. cargo, rustc, and
# cargo-truce all disable ANSI when stdout isn't a TTY, and the pipe to
# `tee` is not one - so a direct run is colored but this wrapper's was
# plain. Force color on when *our* own stdout is a real terminal (leave
# it off for a redirected / piped run so files don't fill with escape
# codes). `CARGO_TERM_COLOR` covers cargo + the rustc it drives;
# `CLICOLOR_FORCE` covers cargo-truce's own output (clicolors spec).
# Respect either if the caller already set it.
if [[ -t 1 ]]; then
    : "${CARGO_TERM_COLOR:=always}"
    : "${CLICOLOR_FORCE:=1}"
    export CARGO_TERM_COLOR CLICOLOR_FORCE
fi

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
    printf '\n=== %s truce %s [%s] ===\n' "$cargo_bin" "$*" "$label"
    # `tee` to a temp file so output still streams live while we
    # inspect it for the "plugin not in this sub-workspace" miss.
    # `PIPESTATUS[0]` is cargo's exit code, not tee's.
    tmp="$(mktemp)"
    ( cd "$ws" && "$cargo_bin" truce "$@" ) 2>&1 | tee "$tmp"
    rc=${PIPESTATUS[0]}
    if [[ $rc -eq 0 ]]; then
        printf '[ OK ] %s\n' "$label"
    elif grep -qiE 'no plugin with crate name|no matching packages' "$tmp"; then
        # Expected: the requested plugin lives in a different workspace.
        printf '[SKIP] %s (plugin not in this workspace)\n' "$label"
    else
        printf '[FAIL] %s (exit %d)\n' "$label" "$rc" >&2
        overall_status=$rc
    fi
    rm -f "$tmp"
done

exit "$overall_status"
