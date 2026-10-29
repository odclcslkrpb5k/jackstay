#!/usr/bin/env bash
# Runs the M4 input probe against a fresh Xvfb. Run inside the dev container:
# ./dev spikes/x11-probes/run-input.sh
set -euo pipefail
IFS=$'\n\t'

cd "$(dirname "$0")"
cargo build --quiet --release
bin="$CARGO_TARGET_DIR/release"

Xvfb :98 -screen 0 1920x1080x24 -nolisten tcp &
xvfb_pid=$!
trap 'kill "$xvfb_pid" 2>/dev/null || true' EXIT
export DISPLAY=:98
for _ in $(seq 50); do xdpyinfo >/dev/null 2>&1 && break; sleep 0.1; done
xdpyinfo | grep -E '^X.Org version|vendor release' || true
"$bin/input"
