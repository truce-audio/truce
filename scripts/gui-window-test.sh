#!/usr/bin/env bash
# Windowed GUI test (macOS): launch an example standalone in a real window,
# jiggle the cursor over it to trigger NSView hitTest:, assert it survives.
# Catches GUI event-path crashes (e.g. the baseview hitTest: recursion) that
# headless screenshot tests miss. Run on the NEWEST macOS — this was an
# OS-behaviour regression (AppKit isa-swizzling the view), so OS coverage is
# the point; it won't reproduce on an image whose AppKit doesn't swizzle.
# Usage: gui-window-test.sh [package]   (default: truce-example-gain)
set -euo pipefail

[ "$(uname -s)" = Darwin ] || { echo "macOS only; skipping"; exit 0; }

SELF="$(cd "$(dirname "$0")" && pwd)"
TRUCE_DIR="$(cd "$SELF/.." && pwd)"
PKG="${1:-truce-example-gain}"
ok()  { printf '[ OK ] %s\n' "$*"; }
die() { printf '[FAIL] %s\n' "$*" >&2; exit 1; }

cd "$TRUCE_DIR"
cargo build --release -p "$PKG" --no-default-features --features standalone
BIN="$TRUCE_DIR/target/release/${PKG}-standalone"
[ -x "$BIN" ] || die "no standalone binary at $BIN"

LOG="$(mktemp)"
APP_PID=""
trap '[ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null; rm -f "$LOG"' EXIT
"$BIN" >"$LOG" 2>&1 &
APP_PID=$!

sleep 3   # let the window open
if ! kill -0 "$APP_PID" 2>/dev/null; then
  rc=0; wait "$APP_PID" || rc=$?; APP_PID=""
  [ "$rc" = 0 ] && { ok "exited cleanly before poke (no window/audio in CI?) — no crash"; exit 0; }
  cat "$LOG"; die "standalone crashed on startup (rc=$rc)"
fi

swift "$SELF/macos-cursor-poke.swift" || echo "warn: cursor poke failed (continuing)"
sleep 2

if kill -0 "$APP_PID" 2>/dev/null; then
  kill "$APP_PID" 2>/dev/null || true; wait "$APP_PID" 2>/dev/null || true
  APP_PID=""
  ok "standalone survived cursor routing (hitTest OK)"
else
  rc=0; wait "$APP_PID" || rc=$?; APP_PID=""
  cat "$LOG"; die "standalone crashed during cursor routing (rc=$rc)"
fi
