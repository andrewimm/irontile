#!/bin/bash
# Multi-display test run, with the safety rails on.
#
# Run from a TTY other than your normal session, with the external monitor
# already plugged in. Nothing here needs root.
set -u

LOG="${IRONTILE_LOG:-$HOME/irontile-session.log}"
PROBE_LOG="${IRONTILE_PROBE_LOG:-$HOME/irontile-multihead.log}"
LIMIT="${IRONTILE_TIMEOUT:-300}"
HERE="$(cd "$(dirname "$0")" && pwd)"
BIN="$HERE/target/debug/irontile"
PROBE="$HERE/target/debug/xtask"

# Built ahead of time: the probe runs as a startup command inside the session,
# where a compile would be a poor thing to discover was needed.
cargo build -p irontile-comp -p xtask >/dev/null 2>&1 || {
    echo "build failed; run: cargo build -p irontile-comp -p xtask" >&2
    exit 1
}

TERMINAL="${IRONTILE_TERMINAL:-}"
if [ -z "$TERMINAL" ]; then
    for candidate in foot alacritty kitty ghostty wezterm xterm; do
        command -v "$candidate" >/dev/null 2>&1 && { TERMINAL="$candidate"; break; }
    done
fi
[ -n "$TERMINAL" ] || { echo "no terminal found; set IRONTILE_TERMINAL" >&2; exit 1; }
# The probe opens its own windows over the control socket and asks for this by
# name, because the compositor no longer guesses at one.
export IRONTILE_TERMINAL="$TERMINAL"

# The probe has to be launched from the config, so a real one is copied and the
# probe appended rather than used directly.
REAL="${XDG_CONFIG_HOME:-$HOME/.config}/irontile/irontile.toml"
SOURCE="${IRONTILE_CONFIG:-$REAL}"
CONFIG="$(mktemp -t irontile-multihead-XXXXXX.toml)"
trap 'rm -f "$CONFIG"' EXIT
if [ -f "$SOURCE" ]; then
    echo "irontile: testing your configuration at $SOURCE"
    grep -v '^\[startup\]' "$SOURCE" | grep -v '^exec *=' > "$CONFIG"
else
    echo "irontile: no config at $SOURCE; testing the defaults"
    : > "$CONFIG"
fi
cat >> "$CONFIG" <<TOML

[startup]
exec = ["$TERMINAL", "$PROBE multihead --log $PROBE_LOG"]
TOML

echo "irontile: two logs — compositor at $LOG, probe at $PROBE_LOG"
echo "irontile: the probe drives the displays over the control socket by itself"
echo
echo "  While it runs, please check by eye:"
echo "    1. do BOTH monitors show something (background, borders, a terminal)?"
echo "    2. does the pointer look like your cursor theme, not a crude white arrow?"
echo "    3. does it become an I-beam over the terminal's text?"
echo "    4. can you move it from one monitor onto the other?"
echo "    5. when the probe says so, UNPLUG the external monitor, wait ~10s,"
echo "       then plug it back in. Its desktop should return to it."
echo
echo "irontile: Ctrl+Alt+F<n> to leave, Super+Shift+E to quit, ${LIMIT}s timeout as backstop"
sleep 4

RUST_LOG="${RUST_LOG:-irontile=debug}" timeout --signal=TERM "$LIMIT" \
    "$BIN" --session --wayland-display irontile-0 --config "$CONFIG" > "$LOG" 2>&1
status=$?

case $status in
    0)   echo "irontile: exited cleanly" ;;
    124) echo "irontile: stopped by the ${LIMIT}s timeout" ;;
    *)   echo "irontile: exited with status $status" ;;
esac
echo "irontile: send me both $LOG and $PROBE_LOG"
