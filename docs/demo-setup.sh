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

# 2) Isolated demo server. Each pane runs `cat` so it sits on stdin with
#    no prompt visible — keeps the recording free of $PS1 / Starship
#    clutter that would otherwise bleed around the muxa watch popup.
"$TM" -L "$TMUX_LBL" kill-server 2>/dev/null || true
"$TM" -L "$TMUX_LBL" new-session -d -s main -x 200 -y 40 cat
"$TM" -L "$TMUX_LBL" new-window  -t main: -n review cat
"$TM" -L "$TMUX_LBL" new-window  -t main: -n vim    cat
"$TM" -L "$TMUX_LBL" new-session -d -s ops  -x 200 -y 40 cat
"$TM" -L "$TMUX_LBL" new-window  -t ops:  -n logs cat

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

# 4) Wire muxa into the demo server so the recording shows the integration.
"$TM" -L "$TMUX_LBL" set-option -g status-interval 1
# Status bar at the top — much more legible in a 1200×720 GIF than the
# default bottom bar squeezed against the recording's edge.
"$TM" -L "$TMUX_LBL" set-option -g status-position top
"$TM" -L "$TMUX_LBL" set-option -g status-style "bg=#1a1b26,fg=white"
"$TM" -L "$TMUX_LBL" set-option -g status-left  "#[bg=#7aa2f7,fg=#1a1b26,bold] muxa-demo #[default] "
"$TM" -L "$TMUX_LBL" set-option -g status-left-length 20
"$TM" -L "$TMUX_LBL" set-option -g status-right \
  "#[fg=#bb9af7,bold]#(muxa status-line --pane #{pane_id})#[default]   #[fg=cyan]%H:%M#[default] "
"$TM" -L "$TMUX_LBL" set-option -g status-right-length 120

# Bind prefix + s to the muxa watch popup so we can demo the flagship flow.
"$TM" -L "$TMUX_LBL" bind-key s display-popup -E -w 90% -h 85% "muxa watch"
