#!/bin/bash
# First run of the session backend, with the safety rails on.
#
# Run this from a TTY other than the one your normal session is on, so that
# session is still there to switch back to. Nothing here needs root.
set -u

LOG="${IRONTILE_LOG:-$HOME/irontile-session.log}"
LIMIT="${IRONTILE_TIMEOUT:-120}"
HERE="$(cd "$(dirname "$0")" && pwd)"
BIN="$HERE/target/debug/irontile"

if [ ! -x "$BIN" ]; then
    echo "build it first: cargo build -p irontile-comp" >&2
    exit 1
fi

# Pick a terminal to start with. A compositor showing no windows and a
# compositor that is failing to draw look identical, so the run needs something
# on screen that is not the background colour.
TERMINAL="${IRONTILE_TERMINAL:-}"
if [ -z "$TERMINAL" ]; then
    for candidate in foot alacritty kitty ghostty wezterm xterm; do
        if command -v "$candidate" >/dev/null 2>&1; then
            TERMINAL="$candidate"
            break
        fi
    done
fi
if [ -z "$TERMINAL" ]; then
    echo "no terminal found; set IRONTILE_TERMINAL" >&2
    exit 1
fi

# A throwaway config, so the run does not depend on what is in yours.
CONFIG="$(mktemp -t irontile-session-XXXXXX.toml)"
trap 'rm -f "$CONFIG"' EXIT
cat > "$CONFIG" <<TOML
[startup]
exec = ["$TERMINAL"]
TOML

echo "irontile: starting $TERMINAL, logging to $LOG"
echo "irontile: Ctrl+Alt+F<n> switches VT, Super+Shift+E quits, Super+Return opens a terminal"
echo "irontile: the ${LIMIT}s timeout is the backstop if none of those work"
sleep 2

# The timeout is the point of this script: whatever happens, the machine comes
# back on its own. A first run of an untested compositor should not be able to
# hold the seat indefinitely.
RUST_LOG="${RUST_LOG:-irontile=debug}" timeout --signal=TERM "$LIMIT" \
    "$BIN" --session --wayland-display irontile-0 --config "$CONFIG" > "$LOG" 2>&1
status=$?

case $status in
    0)   echo "irontile: exited cleanly" ;;
    124) echo "irontile: stopped by the ${LIMIT}s timeout" ;;
    *)   echo "irontile: exited with status $status" ;;
esac

echo
echo "irontile: log at $LOG. What to look for:"
echo "  'display lit'    the connector was modeset"
echo "  'spawned'        the terminal was launched"
echo "  'placed'         it was given a cell"
echo "  'queued a frame' / 'vblank'   the flip cycle is turning"
echo "  'alive'          a heartbeat every 5s; its absence means a wedged loop"
echo "  'key pressed'    input arriving, with the keysym and whether it matched"
