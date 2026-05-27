#!/usr/bin/env bash
# Post-publish smoke test: force-install cargo-truce from crates.io,
# scaffold a default plugin in /tmp, build + launch the standalone.
# Run under bash (Git Bash / `shell: bash` on Windows).
# Usage: post-publish-smoke.sh [cargo-truce-version]   env: RUN_SECS=N (CI auto-close), KEEP=1
set -euo pipefail

VERSION="${1:-}"
RUN_SECS="${RUN_SECS:-}"   # empty = run until manually closed
ok()  { printf '[ OK ] %s\n' "$*"; }
die() { printf '[FAIL] %s\n' "$*" >&2; exit 1; }

case "$(uname -s)" in
  Darwin)               OS=macos;   EXE=""     ;;
  Linux)                OS=linux;   EXE=""     ;;
  MINGW*|MSYS*|CYGWIN*) OS=windows; EXE=".exe" ;;
  *) die "unsupported OS: $(uname -s)" ;;
esac
command -v cargo >/dev/null || die "cargo not on PATH"

WORK="$(mktemp -d /tmp/truce-smoke.XXXXXX)"
APP_PID=""
cleanup() {
  [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null || true
  [ "${KEEP:-0}" = 1 ] && { echo "kept: $WORK"; return; }
  rm -rf "$WORK"
}
trap cleanup EXIT

# 1. force-install the published CLI
args=(cargo-truce --force --locked)
[ -n "$VERSION" ] && args+=(--version "$VERSION")
cargo install "${args[@]}"
cargo truce --help >/dev/null || die "'cargo truce' not runnable"
ok "installed cargo-truce"

# 2. scaffold defaults
( cd "$WORK" && cargo truce new smoketest )
PROJ="$WORK/smoketest"
[ -f "$PROJ/Cargo.toml" ] || die "scaffold produced no Cargo.toml"
ok "scaffolded $PROJ"

# 3. build standalone against published crates
( cd "$PROJ" && cargo build --release )
BIN="$PROJ/target/release/smoketest-standalone$EXE"
[ -x "$BIN" ] || die "no standalone binary at $BIN"
ok "built standalone"

# 4. launch (xvfb on headless Linux). Default: foreground, close manually.
#    RUN_SECS=N auto-closes after N seconds (CI); fails on non-zero exit.
cmd=()
if [ "$OS" = linux ] && [ -z "${DISPLAY:-}" ] && [ -z "${WAYLAND_DISPLAY:-}" ]; then
  command -v xvfb-run >/dev/null && cmd+=(xvfb-run -a) || echo "warn: no DISPLAY / xvfb-run"
fi
cmd+=("$BIN")

if [ -z "$RUN_SECS" ]; then
  echo "launching standalone — close the window to finish"
  rc=0; "${cmd[@]}" || rc=$?
  [ "$rc" = 0 ] || die "standalone exited $rc"
else
  LOG="$WORK/standalone.log"
  "${cmd[@]}" >"$LOG" 2>&1 &
  APP_PID=$!
  sleep "$RUN_SECS"
  if kill -0 "$APP_PID" 2>/dev/null; then
    kill "$APP_PID" 2>/dev/null || true; wait "$APP_PID" 2>/dev/null || true
    ok "standalone ran ${RUN_SECS}s, no crash"
  else
    rc=0; wait "$APP_PID" || rc=$?
    [ "$rc" = 0 ] || { cat "$LOG" || true; die "standalone exited $rc"; }
  fi
  APP_PID=""
fi
ok "smoke test passed"
