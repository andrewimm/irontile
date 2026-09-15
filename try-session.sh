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

# The bar is a separate binary, started from [startup]. A configuration naming
# one that was never built would leave the session bare with nothing saying
# why, so check for it here rather than in the log afterwards.
if grep -q 'irontile-bar' "${XDG_CONFIG_HOME:-$HOME/.config}/irontile/irontile.toml" 2>/dev/null \
    && [ ! -x "$HERE/target/debug/irontile-bar" ]; then
    echo "your config starts the bar: cargo build -p irontile-ui" >&2
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

# Use the real configuration if there is one, so a run tests what you actually
# have. A throwaway is generated only when there is none, which is what the very
# first runs needed and no longer the common case.
REAL="${XDG_CONFIG_HOME:-$HOME/.config}/irontile/irontile.toml"
CONFIG=""
CLEANUP=""
if [ -n "${IRONTILE_CONFIG:-}" ]; then
    CONFIG="$IRONTILE_CONFIG"
    echo "irontile: using $CONFIG"
elif [ -f "$REAL" ]; then
    CONFIG="$REAL"
    echo "irontile: using $CONFIG"
    if ! grep -q '^\[startup\]' "$CONFIG"; then
        echo "irontile: NOTE - no [startup] section, so no window will open on its own,"
        echo "irontile:        and no terminal is bound by default. Add both:"
        echo "irontile:            [startup]"
        echo "irontile:            exec = [\"$TERMINAL\"]"
        echo "irontile:            [binds]"
        echo "irontile:            \"Super+Return\" = \"spawn $TERMINAL\""
    fi
else
    CONFIG="$(mktemp -t irontile-XXXXXX.toml)"
    CLEANUP="$CONFIG"
    trap 'rm -f "$CLEANUP"' EXIT
    echo "irontile: no config at $REAL; using a throwaway that starts $TERMINAL"
    cat > "$CONFIG" <<TOML
[startup]
exec = ["$TERMINAL"]
TOML
fi

# A second graphical session for the same user shares that user's D-Bus session
# bus with the first one, and a notification daemon can only own its name once.
# Started from here without a bus of its own, swaync finds the name taken and
# exits -- and `swaync-client` then reaches the instance belonging to the other
# session, which draws the panel on the other VT. Anything on the session bus
# behaves this way: media players found by playerctl, portals, the lot.
#
# A bus per session is what makes this a session rather than a program sharing
# somebody else's. Set IRONTILE_SHARE_BUS=1 to use the outer one instead.
#
# The display has to be in the environment *before* the bus starts, not just in
# the compositor's. A program started by D-Bus activation rather than by the
# compositor inherits the bus daemon's environment, and on a bare VT that has no
# WAYLAND_DISPLAY at all -- so an activated notification daemon has no
# compositor to draw on and never appears anywhere, on any VT. It is fixed here
# because irontile is told which socket to bind rather than choosing one.
export WAYLAND_DISPLAY=irontile-0

PREFIX=""
if [ -z "${IRONTILE_SHARE_BUS:-}" ] && command -v dbus-run-session >/dev/null 2>&1; then
    PREFIX="dbus-run-session --"
    if pgrep -x swaync >/dev/null 2>&1 || pgrep -x mako >/dev/null 2>&1 \
        || pgrep -x dunst >/dev/null 2>&1; then
        echo "irontile: a notification daemon is already running elsewhere;"
        echo "irontile: this session gets its own bus so it starts its own."
    fi
fi

echo "irontile: starting $TERMINAL, logging to $LOG"
echo "irontile: Ctrl+Alt+F<n> switches VT, Super+Shift+E quits"
echo "irontile: the ${LIMIT}s timeout is the backstop if none of those work"
sleep 2

# The timeout is the point of this script: whatever happens, the machine comes
# back on its own. A first run of an untested compositor should not be able to
# hold the seat indefinitely.
RUST_LOG="${RUST_LOG:-irontile=debug}" $PREFIX timeout --signal=TERM "$LIMIT" \
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
