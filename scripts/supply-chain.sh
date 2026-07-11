#!/usr/bin/env bash
# Supply-chain audit for the whole workspace tree: runs `cargo audit`
# (RustSec advisory scan) and `cargo deny check` (advisories + licenses +
# bans + sources) in the main workspace and every sub-workspace.
#
# Usage: supply-chain.sh
#
# Requires `cargo audit` (cargo-audit) and `cargo deny` (cargo-deny) on
# PATH. Exits non-zero if either tool fails in any workspace.
#
# cargo deny uses a per-workspace policy: most workspaces share the root
# deny.toml, but truce-vizia - the only one pulling git-sourced deps -
# gets crates/truce-vizia/deny.toml so its git allow-list doesn't leak
# into the others. cargo audit needs no config (it scans Cargo.lock) and
# exits non-zero only on a real vulnerability; unmaintained-crate notices
# stay exit 0.
set -uo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root_dir="$(cd "$script_dir/.." && pwd)"
# shellcheck source=truce-workspaces.sh
source "$script_dir/truce-workspaces.sh"

# The audit sweep also covers the fuzz workspace (committed
# Cargo.lock, real third-party deps like libfuzzer-sys). It lives in
# truce-audio/truce-fuzz-tests, mounted at fuzz/ - CI checks it out
# there, locally it's an optional clone - so audit it when present.
# Kept out of `truce_workspaces` because the build/release scripts
# that share that list have no business in fuzz/.
audit_workspaces() {
    truce_workspaces "$1"
    if [[ -f "$1/fuzz/Cargo.toml" ]]; then
        printf '%s\n' "$1/fuzz"
    else
        printf 'note: fuzz/ not checked out; skipping the fuzz workspace\n' >&2
    fi
}

for tool in cargo-audit cargo-deny; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        printf 'error: %s not found on PATH (install with `cargo install %s`)\n' \
            "$tool" "$tool" >&2
        exit 127
    fi
done

# Keep colored output when our own stdout is a real terminal (off for a
# piped / redirected run). Mirrors recursive-cargo.sh.
if [[ -t 1 ]]; then
    : "${CARGO_TERM_COLOR:=always}"
    : "${CLICOLOR_FORCE:=1}"
    export CARGO_TERM_COLOR CLICOLOR_FORCE
fi

status=0

# Pretty label for a workspace path: "(main)" for the root, else the
# path relative to it.
ws_label() {
    local l="${1#"$root_dir"}"
    l="${l#/}"
    [[ -z "$l" ]] && l="(main)"
    printf '%s' "$l"
}

# Most workspaces share the root policy; truce-vizia carries its own.
deny_config_for() {
    case "$1" in
        */crates/truce-vizia) printf '%s' "$1/deny.toml" ;;
        *) printf '%s' "$root_dir/deny.toml" ;;
    esac
}

report() {
    local label="$1" rc="$2"
    if [[ $rc -eq 0 ]]; then
        printf '[ OK ] %s\n' "$label"
    else
        printf '[FAIL] %s (exit %d)\n' "$label" "$rc" >&2
        status=$rc
    fi
}

# cargo-deny relocated `--config` from a `check` subcommand flag (through
# ~0.19) to a top-level flag that must precede the subcommand (newer
# releases). CI installs whatever's latest, so detect which form the
# installed version accepts rather than pin a version.
if cargo deny check --help 2>&1 | grep -q -- '--config'; then
    run_deny() { cargo deny check --config "$1"; }
elif cargo deny --help 2>&1 | grep -q -- '--config'; then
    run_deny() { cargo deny --config "$1" check; }
else
    printf 'error: cargo-deny exposes no --config flag in either position; update this script\n' >&2
    exit 1
fi

printf '\n########## cargo audit ##########\n'
while IFS= read -r ws; do
    label="$(ws_label "$ws")"
    printf '\n=== cargo audit [%s] ===\n' "$label"
    ( cd "$ws" && cargo audit )
    report "$label" "$?"
done < <(audit_workspaces "$root_dir")

printf '\n########## cargo deny check ##########\n'
while IFS= read -r ws; do
    label="$(ws_label "$ws")"
    cfg="$(deny_config_for "$ws")"
    printf '\n=== cargo deny check [%s] (%s) ===\n' "$label" "${cfg#"$root_dir"/}"
    ( cd "$ws" && run_deny "$cfg" )
    report "$label" "$?"
done < <(audit_workspaces "$root_dir")

exit "$status"
