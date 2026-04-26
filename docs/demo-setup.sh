#!/usr/bin/env bash
# Bootstrap the muxa-watch hero recording.
#
# Steps:
#   1. Stand up an isolated tmux server (`-L muxa-demo`) so we don't touch
#      the user's real tmux state. Several windows + sessions give us
#      enough panes for the bare-pane row to appear.
#   2. Drop a `tmux` shim into a temp dir and prepend it to PATH so muxa's
#      `Command::new("tmux") ...` calls hit the labelled server.
#   3. Pick three of those panes and feed them into muxad as Claude /
#      Codex / Gemini agents. The remaining panes will surface as
#      BarePane rows in `muxa watch`.
#
# Idempotent — safe to re-run between vhs renders.

set -euo pipefail

: "${MUXA_SOCKET:=/tmp/muxa-demo.sock}"
TMUX_LBL=muxa-demo
SHIM_DIR=/tmp/muxa-demo-shim

# 1) PATH shim — every later `tmux` call (ours and muxa's children) routes
#    through `tmux -L muxa-demo`.
mkdir -p "$SHIM_DIR"
cat > "$SHIM_DIR/tmux" <<EOF
#!/bin/sh
exec /usr/bin/tmux -L $TMUX_LBL "\$@"
EOF
chmod +x "$SHIM_DIR/tmux"

TM=/usr/bin/tmux  # use absolute path here so we don't depend on PATH ordering

# 2) Isolated demo server.
"$TM" -L "$TMUX_LBL" kill-server 2>/dev/null || true
"$TM" -L "$TMUX_LBL" new-session -d -s main   -x 200 -y 40
"$TM" -L "$TMUX_LBL" new-window  -t main: -n review
"$TM" -L "$TMUX_LBL" new-window  -t main: -n vim
"$TM" -L "$TMUX_LBL" new-session -d -s ops    -x 200 -y 40
"$TM" -L "$TMUX_LBL" new-window  -t ops:  -n logs

# 3) Seed muxad. Pick the first three pane ids so the agent rows resolve to
#    real session:window.pane labels in muxa watch.
mapfile -t PANES < <("$TM" -L "$TMUX_LBL" list-panes -a -F '#{pane_id}')
PA=${PANES[0]}
PB=${PANES[1]}
PC=${PANES[2]}

TMUX_PANE="$PA" muxa hook claude --event user_prompt_submit \
  <<<'{"session_id":"s-a","prompt":"refactor the ipc module to use length-prefixed frames"}'
TMUX_PANE="$PB" muxa hook codex --event permission_request \
  <<<'{"session_id":"s-b","tool_name":"shell"}'
TMUX_PANE="$PC" muxa hook gemini --event before_agent \
  <<<'{"session_id":"s-c","prompt":"summarize this PR"}'
TMUX_PANE="$PC" muxa hook gemini --event after_agent \
  <<<'{"session_id":"s-c"}'

# 4) Wire status-right on the demo server so attaching shows muxa live.
"$TM" -L "$TMUX_LBL" set-option -g status-interval 1
"$TM" -L "$TMUX_LBL" set-option -g status-right \
  "#(muxa status-line --pane #{pane_id})  #[fg=cyan]%H:%M#[default]"
"$TM" -L "$TMUX_LBL" set-option -g status-right-length 120
"$TM" -L "$TMUX_LBL" set-option -g status-style "bg=default,fg=white"
