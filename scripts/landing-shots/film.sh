#!/usr/bin/env bash
# Capture the landing page's hero film from a real muxa.
#
#   scripts/landing-shots/film.sh
#
# Stands up the isolated sandbox (scripts/muxa-sandbox.sh), gives its tmux the
# status line and peek binding `muxa init` writes, paints four agent panes
# (paint.py), drives their states through real `muxa hook` calls, attaches a
# client of a fixed size inside a second, private tmux server, and captures
# that client's whole screen at each beat: tmux borders, the muxa status
# line, `muxa attend` moving the client, and the `muxa peek` popup are all
# the real thing. ansi2html.py turns the captures into
# crates/muxa/src/dashboard/web/landing-frames.mjs.
#
# Needs tmux, python3, and muxa/muxad binaries (target/release, else PATH).
# Never touches your own tmux server or daemon; teardown runs on exit.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
SANDBOX="$REPO/scripts/muxa-sandbox.sh"
NAME=muxa-film
# The outer tmux lives on a socket in the private work dir, not under
# /tmp/tmux-$UID/: a running muxad scans that directory and would list the
# capture session next to the operator's real ones.
OUTER_NAME=outer.sock
COLS=100
ROWS=30
WORK=$(mktemp -d "${TMPDIR:-/tmp}/muxa-film.XXXXXX")
OUTER="$WORK/$OUTER_NAME"
OUT="$REPO/crates/muxa/src/dashboard/web/landing-frames.mjs"

TM_REAL="${MUXA_FILM_TMUX:-$(command -v tmux)}"
BIN_DIR="$REPO/target/release"
[ -x "$BIN_DIR/muxad" ] || BIN_DIR="$(dirname "$(command -v muxad)")"

cleanup() {
  "$TM_REAL" -S "$OUTER" kill-server 2>/dev/null || true
  bash "$SANDBOX" down --name "$NAME" --tmux "$TM_REAL" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT
cleanup_before() { "$TM_REAL" -S "$OUTER" kill-server 2>/dev/null || true; bash "$SANDBOX" down --name "$NAME" --tmux "$TM_REAL" >/dev/null 2>&1 || true; }
cleanup_before

cat > "$WORK/config.toml" <<'TOML'
[discovery]
enabled = false
[reconciler]
enabled = false
[state]
enabled = false
[history]
enabled = false
[activity]
enabled = false
[collaboration]
enabled = false
TOML

bash "$SANDBOX" up --name "$NAME" --config "$WORK/config.toml" --tmux "$TM_REAL" \
  --muxad "$BIN_DIR/muxad" --extra-path "$BIN_DIR" --allow-inside-tmux >/dev/null
eval "$(bash "$SANDBOX" env --name "$NAME")"
export PATH="$BIN_DIR:$PATH"
tm() { "$MUXA_SANDBOX_TMUX" -u -f "$MUXA_SANDBOX_TMUX_CONFIG" -S "$MUXA_TMUX_SOCKET" "$@"; }
bash "$SANDBOX" daemon --name "$NAME" --tmux "$TM_REAL" --muxad "$BIN_DIR/muxad" >/dev/null

hook() { # <pane> <agent> <event> <json>
  TMUX="$MUXA_SANDBOX_TMUX_ENV" TMUX_PANE="$1" muxa hook "$2" --event "$3" <<<"$4"
}
paint() { # <pane> <scene> — repaint a pane in place, keeping its id
  local file="$WORK/$2.ans" width height
  width=$(tm display-message -p -t "$1" '#{pane_width}')
  height=$(tm display-message -p -t "$1" '#{pane_height}')
  python3 "$HERE/paint.py" "$2" "$((width - 1))" "$height" > "$file"
  tm respawn-pane -k -t "$1" "printf '\\033[?25l'; cat '$file'; exec cat"
}

# The operator's tmux, as `muxa init` leaves it: tmux's own look plus the
# status line and the peek binding from crates/muxa-cli/src/init/files/tmux.rs.
tm set-option -g status-interval 1
tm set-option -g status-right-length 140
tm set-option -g status-right "#(muxa status-line --needs-attention) #(muxa status-line --pane #{pane_id}) | #[fg=white]%H:%M"
tm set-option -g pane-active-border-style "fg=green"
tm bind-key q display-popup -B -E -w 100% -h 100% -x 0 -y 0 "muxa peek"
cat > "$WORK/bashrc" <<'RC'
PS1='\[\e[32m\]~/acme\[\e[0m\] $ '
unset PROMPT_COMMAND
# The shell under the editor has just run the tests, so the bottom of the
# screen (where the film zooms in on the status line) is not empty.
npm() {
  printf '\n> acme-api@2.4.0 test\n> vitest run src/routes\n\n'
  printf ' \e[32m✓\e[0m src/routes/orders.test.ts \e[2m(38 tests)\e[0m 412ms\n'
  printf ' \e[32m✓\e[0m src/routes/limit.test.ts \e[2m(12 tests)\e[0m 96ms\n\n'
  printf ' \e[1mTests\e[0m  \e[32m50 passed\e[0m (50)\n'
}
clear
RC

# acme:code — where you are working; acme:agents — four agents.
tm new-session -d -s acme -n code -x "$COLS" -y "$((ROWS - 1))" "cat"
P_EDIT=$(tm display-message -p -t acme:code.0 '#{pane_id}')
P_SHELL=$(tm split-window -v -l 11 -d -P -F '#{pane_id}' -t acme:code "env BASH_SILENCE_DEPRECATION_WARNING=1 bash --rcfile '$WORK/bashrc' -i")
paint "$P_EDIT" editor
tm new-window -d -t acme: -n agents "cat"
P_PLAN=$(tm display-message -p -t acme:agents.0 '#{pane_id}')
P_IMPL=$(tm split-window -h -d -P -F '#{pane_id}' -t "$P_PLAN" cat)
P_REV=$(tm split-window -v -d -P -F '#{pane_id}' -t "$P_PLAN" cat)
P_DEP=$(tm split-window -v -d -P -F '#{pane_id}' -t "$P_IMPL" cat)
tm select-pane -t "$P_PLAN"
paint "$P_PLAN" planner-working; paint "$P_IMPL" impl-working
paint "$P_REV" reviewer-working; paint "$P_DEP" deploy-working
tm select-window -t acme:code; tm select-pane -t "$P_SHELL"

hook "$P_PLAN" claude user_prompt_submit '{"session_id":"f-plan","prompt":"Plan rate limiting for /v1/orders"}'
hook "$P_PLAN" claude pre_tool_use '{"session_id":"f-plan","tool_name":"Read"}'
hook "$P_IMPL" codex user_prompt_submit '{"session_id":"f-impl","prompt":"Implement the token bucket middleware"}'
hook "$P_IMPL" codex pre_tool_use '{"session_id":"f-impl","tool_name":"shell"}'
hook "$P_REV" claude user_prompt_submit '{"session_id":"f-rev","prompt":"Review the limiter diff for races"}'
hook "$P_REV" claude pre_tool_use '{"session_id":"f-rev","tool_name":"Grep"}'
hook "$P_DEP" codex user_prompt_submit '{"session_id":"f-dep","prompt":"Add a canary stage with auto rollback"}'
hook "$P_DEP" codex pre_tool_use '{"session_id":"f-dep","tool_name":"shell"}'

# A real client of a fixed size, attached from inside a private outer tmux so
# its whole screen — status line and popups included — can be captured.
"$TM_REAL" -S "$OUTER" -f /dev/null new-session -d -s film -x "$COLS" -y "$ROWS" \
  "env -u TMUX TERM=tmux-256color '$MUXA_SANDBOX_TMUX' -u -f '$MUXA_SANDBOX_TMUX_CONFIG' -S '$MUXA_TMUX_SOCKET' attach -t acme:code"
"$TM_REAL" -S "$OUTER" set-option -g status off
sleep 1.5
tm send-keys -t "$P_SHELL" "npm test" Enter
sleep 1

frames=()
capture() { # <name>
  sleep "${2:-1.6}"
  "$TM_REAL" -S "$OUTER" capture-pane -e -p -t film > "$WORK/$1.cap"
  frames+=("$1")
}

capture working
# Two agents stop: the planner asks a question first, then the reviewer
# needs permission — so the planner is the one blocked longest.
paint "$P_PLAN" planner-choice
hook "$P_PLAN" claude pre_tool_use '{"session_id":"f-plan","tool_name":"AskUserQuestion"}'
sleep 2
paint "$P_REV" reviewer-permission
hook "$P_REV" claude notification '{"session_id":"f-rev","notification_type":"permission_prompt","message":"Claude needs your permission to use Bash"}'
capture waiting 2.2
tm send-keys -t "$P_SHELL" "muxa attend"
capture typed 0.8
tm send-keys -t "$P_SHELL" Enter
capture attended 2
"$TM_REAL" -S "$OUTER" send-keys -t film C-b q
capture peek 2.5

{
  echo "// Generated by scripts/landing-shots/film.sh from a real muxa in a sandbox;"
  echo "// do not edit. Each frame is the HTML of one ${COLS}x${ROWS} terminal screen."
  echo "export const FILM_SIZE = { cols: $COLS, rows: $ROWS };"
  echo "export const FILM_FRAMES = {"
  for name in "${frames[@]}"; do
    printf '  %s: %s,\n' "$name" "$(python3 "$HERE/ansi2html.py" "$COLS" < "$WORK/$name.cap" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read().rstrip("\n")))')"
  done
  echo "};"
} > "$OUT"
echo "wrote $OUT ($(wc -c < "$OUT") bytes, frames: ${frames[*]})"
