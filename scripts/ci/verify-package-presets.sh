#!/usr/bin/env bash
# Verify that `cargo truce package` shipped a plugin's factory presets
# in the produced installer, per OS.
#
# Usage: scripts/ci/verify-package-presets.sh <macos|linux|windows>
#
# Assumes `cargo truce package -p truce-example-synth` already ran and
# wrote its artifact(s) to target/dist/. Run from the repo root. The
# synth ships six authored presets (Init, bass/Sub, lead/Bright Saw,
# lead/Square Stab, pad/Glass, pad/Warm Strings).
#
# Two layers of assertion:
#   A. Payload inspection - the preset files / VST3 component are in
#      the installer.
#   B. Install + merge-safety (Linux / Windows, where the runner is the
#      native OS) - run the installer into a throwaway location with a
#      pre-seeded *user* preset, and assert the factory presets land
#      AND the user's preset survives (the shared VST3 preset folder is
#      merged into, never wiped).

set -euo pipefail

OS="${1:?usage: verify-package-presets.sh <macos|linux|windows>}"
DIST="target/dist"
VENDOR="Truce"
PLUGIN="Truce Synth"
EXPECT_VST3=6

pass() { echo "  ok: $1"; }
fail() { echo "FAIL: $1" >&2; exit 1; }

case "$OS" in
  macos)
    pkg=$(ls "$DIST"/truce-example-synth-*-macos.pkg 2>/dev/null | head -1) \
      || fail "no .pkg in $DIST"
    exp=$(mktemp -d)
    pkgutil --expand "$pkg" "$exp/x"
    # All payload paths across every component, read from the BOMs.
    files=""
    for bom in "$exp"/x/*.pkg/Bom; do
      files+=$'\n'"$(lsbom -s "$bom" 2>/dev/null || true)"
    done
    grep -q '\.trucepreset$' <<<"$files" || fail "no .trucepreset in CLAP/AU payload"
    n=$(grep -c '\.vstpreset$' <<<"$files" || true)
    [ "$n" -eq "$EXPECT_VST3" ] || fail "expected $EXPECT_VST3 .vstpreset, found $n"
    ls "$exp"/x/*VST3-Presets.pkg >/dev/null 2>&1 || fail "no VST3-Presets component in .pkg"
    pass "macOS .pkg carries CLAP/AU presets + $n VST3 presets (component present)"
    ;;

  linux)
    tar=$(ls "$DIST"/truce-example-synth-*-linux-*.tar.gz 2>/dev/null | head -1) \
      || fail "no tarball in $DIST"
    list=$(tar tzf "$tar")
    grep -q "clap/$PLUGIN.presets/.*\.trucepreset" <<<"$list" || fail "no CLAP presets in tarball"
    grep -q 'lv2/.*\.lv2/presets/.*\.ttl'          <<<"$list" || fail "no LV2 preset TTLs in tarball"
    grep -q "vst3-presets/$VENDOR/$PLUGIN/.*\.vstpreset" <<<"$list" || fail "no VST3 presets in tarball"
    pass "Linux tarball carries CLAP + LV2 + VST3 presets"

    # Layer B: install into a throwaway HOME with a pre-seeded user
    # preset to prove the VST3 merge never wipes the user's own files.
    work=$(mktemp -d)
    fake="$work/home"
    seed="$fake/.vst3/presets/$VENDOR/$PLUGIN"
    mkdir -p "$seed"
    echo MINE >"$seed/mine.vstpreset"
    tar xzf "$tar" -C "$work"
    dir=$(ls -d "$work"/truce-example-synth-*-linux-*/ | head -1)
    ( cd "$dir" && HOME="$fake" bash ./install.sh --user --all >/dev/null )

    test -f "$seed/mine.vstpreset" || fail "install WIPED the user preset (VST3 merge unsafe!)"
    got=$(find "$fake/.vst3/presets/$VENDOR/$PLUGIN" -name '*.vstpreset' ! -name 'mine.vstpreset' | wc -l)
    [ "$got" -eq "$EXPECT_VST3" ] || fail "expected $EXPECT_VST3 factory VST3 presets installed, got $got"
    test -d "$fake/.clap/$PLUGIN.presets" || fail "CLAP preset sibling not installed"
    pass "Linux install merged $got VST3 presets; user's preset survived"
    ;;

  windows)
    exe=$(ls "$DIST"/truce-example-synth-*-windows*.exe 2>/dev/null | head -1) \
      || fail "no .exe in $DIST"
    docs="$(cygpath -u "$USERPROFILE")/Documents/VST3 Presets/$VENDOR/$PLUGIN"
    mkdir -p "$docs"
    echo MINE >"$docs/mine.vstpreset"

    # Silent per-user install (no elevation). `--ask`-built installers
    # allow /CURRENTUSER; Start-Process -Wait blocks until it finishes.
    powershell -NoProfile -Command \
      "Start-Process -FilePath '$(cygpath -w "$exe")' -ArgumentList '/VERYSILENT','/SUPPRESSMSGBOXES','/NORESTART','/CURRENTUSER' -Wait"

    test -f "$docs/mine.vstpreset" || fail "install WIPED the user preset (VST3 merge unsafe!)"
    got=$(find "$docs" -name '*.vstpreset' ! -name 'mine.vstpreset' | wc -l)
    [ "$got" -eq "$EXPECT_VST3" ] || fail "expected $EXPECT_VST3 factory VST3 presets installed, got $got"
    pass "Windows install merged $got VST3 presets; user's preset survived"
    ;;

  *)
    fail "unknown OS '$OS' (expected macos|linux|windows)"
    ;;
esac

echo "preset packaging verified for $OS"
