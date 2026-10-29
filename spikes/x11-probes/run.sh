#!/usr/bin/env bash
# Runs both probes against a fresh Xvfb. Run inside the dev container: ./dev spikes/x11-probes/run.sh
set -euo pipefail
IFS=$'\n\t'

cd "$(dirname "$0")"
cargo build --quiet --release
bin="$CARGO_TARGET_DIR/release"

Xvfb :99 -screen 0 1920x1080x24 -nolisten tcp &
xvfb_pid=$!
trap 'kill "$xvfb_pid" 2>/dev/null || true' EXIT
export DISPLAY=:99
for _ in $(seq 50); do xdpyinfo >/dev/null 2>&1 && break; sleep 0.1; done
xdpyinfo | grep -E '^X.Org version|vendor release' || true

echo "== barrier"
"$bin/barrier"

echo "== keyhold (keycode 50 = Shift_L, 38 = a on evdev)"
# Another client stays connected throughout: Xvfb resets when its last client leaves, and a reset
# clears every key, which made the first version of this probe report keys released (2026-10-29).
echo hold | xclip -loops 1000 -selection clipboard &
other_pid=$!
trap 'kill "$xvfb_pid" "$other_pid" 2>/dev/null || true' EXIT
sleep 0.3
for how in exit abort; do
  "$bin/keyhold" press 38 "$how" || true
  echo -n "after $how: "; "$bin/keyhold" query 38
  xdotool keyup a
done
"$bin/keyhold" press 50 hang &
hang_pid=$!
sleep 0.5
kill -KILL "$hang_pid"; wait "$hang_pid" 2>/dev/null || true
echo -n "after SIGKILL: "; "$bin/keyhold" query 50
