#!/usr/bin/env bash
# Undo docs/demo-setup.sh.
#
# Split out of the tape so a render that dies halfway — vhs timeout, ^C,
# a panic in the TUI — can be cleaned up with one command instead of
# leaving a stray muxad and a labelled tmux server behind.
#
# Deliberately not `set -e`: every step here is best-effort, and the
# common case is tearing down a partial setup where half of it never
# existed.

# Hardcoded for the same reason demo-setup.sh hardcodes them: this script
# deletes a data directory, and honouring an inherited XDG_DATA_HOME would
# aim that `rm -rf` at the caller's real one.
MUXA_SOCKET=/tmp/muxa-demo.sock
MUXA_CONFIG=/tmp/muxa-demo-config.toml
XDG_DATA_HOME=/tmp/muxa-demo-data

TM="${MUXA_DEMO_TMUX:-/usr/bin/tmux}"
PID_FILE=/tmp/muxa-demo.pid

"$TM" -u -L muxa-demo kill-server 2>/dev/null

if [ -f "$PID_FILE" ]; then
  kill "$(cat "$PID_FILE")" 2>/dev/null
  rm -f "$PID_FILE"
fi

# Sweep any demo daemon the pidfile lost track of — an interrupted setup, or
# a render killed before its postlude. Two of these racing for the same
# socket makes muxa look like it dies at random, which costs an hour to
# diagnose the first time.
#
# Matched by config path, then confirmed by process name. `pkill -f` alone
# is wrong twice over: the pattern also matches the shell running *this*
# script, and `pkill muxad` would take out the user's real daemon.
for pid in $(pgrep -f 'muxa-demo-config\.toml' 2>/dev/null); do
  case "$(ps -o comm= -p "$pid" 2>/dev/null | tr -d ' ')" in
    muxad) kill "$pid" 2>/dev/null ;;
  esac
done

rm -rf /tmp/muxa-demo-shim /tmp/muxa-demo-bashrc /tmp/muxa-demo-transcripts /tmp/muxa-demo-frames
rm -f "$MUXA_SOCKET" "$MUXA_CONFIG"
# Only ever the demo's own data dir — refuse to touch a real one.
case "$XDG_DATA_HOME" in
  /tmp/muxa-demo-data*) rm -rf "$XDG_DATA_HOME" ;;
esac
