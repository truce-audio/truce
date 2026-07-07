#!/usr/bin/env bash
#
# rt-paranoid.sh - run the rt-paranoid audio-thread allocation check on
# every example crate that opts into it.
#
# Each such crate has an `rt-paranoid` feature, `truce::enable_rt_paranoid!()`
# at its root, and at least one `assert_no_audio_alloc` test. Building the
# crate with `--features rt-paranoid` installs the checking global
# allocator; the `assert_no_audio_alloc` / `assert_audio_alloc` helpers
# gate each test on the allocation count directly, so no mode needs setting.
#
# Only the DSP-distinct examples carry the check. The GUI-backend variants
# (gain-egui / -iced / -vizia / -gpu, gui-zoo-*) share their `process`
# byte-for-byte with their base example, so re-checking them adds build
# cost with no new coverage.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

if command -v cargo >/dev/null 2>&1; then
    CARGO=cargo
elif command -v cargo.exe >/dev/null 2>&1; then
    CARGO=cargo.exe
else
    echo "Error: cargo not found on PATH" >&2
    exit 1
fi

# Collect example crates that declare the `rt-paranoid` feature.
pkgs=()
for manifest in examples/*/Cargo.toml; do
    grep -qE '^rt-paranoid[[:space:]]*=' "$manifest" || continue
    pkg="$(sed -nE 's/^name = "(.*)"/\1/p' "$manifest" | head -1)"
    pkgs+=("$pkg")
done

if [[ ${#pkgs[@]} -eq 0 ]]; then
    echo "::error::no example crate declares the rt-paranoid feature" >&2
    exit 1
fi

echo "rt-paranoid: checking ${#pkgs[@]} example crates"

fail=0
for pkg in "${pkgs[@]}"; do
    echo "::group::rt-paranoid $pkg"
    if ! "$CARGO" test -p "$pkg" --features rt-paranoid --lib; then
        echo "::error::$pkg allocates on the audio thread in process()"
        fail=1
    fi
    echo "::endgroup::"
done

exit "$fail"
