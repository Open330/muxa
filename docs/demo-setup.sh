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
#    - main:0 runs an interactive bash with a dead-simple rcfile so we
#      can type `muxa status-line` inside the recording without dragging
#      the user's Starship/zinit setup along.
#    - Every other pane sits on `cat` so the popup has clean space
#      around it (no extra prompts, no scrollback noise).
"$TM" -L "$TMUX_LBL" kill-server 2>/dev/null || true
cat > /tmp/muxa-demo-bashrc <<'BASHRC'
export PS1='$ '
unset PROMPT_COMMAND
BASHRC

"$TM" -L "$TMUX_LBL" new-session -d -s main -x 200 -y 40 \
  "bash --rcfile /tmp/muxa-demo-bashrc"
# `-d` keeps the current window as main:0 — without it new-window switches
# focus to itself, so attach would land on `vim` (cat-stdin) instead of
# the bash pane.
"$TM" -L "$TMUX_LBL" new-window -d -t main: -n review cat
"$TM" -L "$TMUX_LBL" new-window -d -t main: -n vim    cat
"$TM" -L "$TMUX_LBL" new-session -d -s ops  -x 200 -y 40 cat
"$TM" -L "$TMUX_LBL" new-window -d -t ops:  -n logs cat
# Belt-and-suspenders: be explicit about which window the recording
# should attach to.
"$TM" -L "$TMUX_LBL" select-window -t main:0

# 3) Seed muxad. Pick the first three pane ids so the agent rows resolve to
#    real session:window.pane labels in muxa watch. Each pane gets a
#    realistic prompt + (where the adapter supports it) a captured
#    `last_response`, so the preview popup has actual content to show
#    instead of the "no agent record" fallback.

mapfile -t PANES < <("$TM" -L "$TMUX_LBL" list-panes -a -F '#{pane_id}')
PA=${PANES[0]}
PB=${PANES[1]}
PC=${PANES[2]}

# --- Claude transcript fixture ---------------------------------------------
# Claude's `Stop` hook only carries a `transcript_path` — the response body
# itself is read from the JSONL file. We stage a small fake transcript that
# matches Claude Code's on-disk format closely enough for
# `transcript::last_assistant_text` to find the assistant turn.
TRANSCRIPT_DIR=/tmp/muxa-demo-transcripts
mkdir -p "$TRANSCRIPT_DIR"
CLAUDE_TRANSCRIPT="$TRANSCRIPT_DIR/claude-s-a.jsonl"
cat > "$CLAUDE_TRANSCRIPT" <<'JSONL'
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"refactor the ipc module to use length-prefixed frames"}]}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Switched the on-the-wire framing from newline-delimited JSON to length-prefixed (u32 BE + body) frames. The old framing assumed payloads never contained literal newlines, which broke the moment we started ingesting transcript snippets. Updated:\n  • crates/muxa/src/ipc.rs — Server::handle_conn / Client::recv now share a length_prefixed::read_frame helper that returns Result<Vec<u8>, FrameError>.\n  • Migrated every existing call site, including the dashboard SSE bridge.\n  • Added round-trip tests for empty / 1-byte / 4 KiB / 4 MiB payloads.\n\nLet me know if you want me to also bump the protocol version constant in event.rs — strictly speaking this is wire-incompatible with older clients."}]}}
JSONL

TMUX_PANE="$PA" muxa hook claude --event user_prompt_submit \
  <<<'{"session_id":"s-a","prompt":"refactor the ipc module to use length-prefixed frames"}'
TMUX_PANE="$PA" muxa hook claude --event stop \
  <<<"{\"session_id\":\"s-a\",\"transcript_path\":\"$CLAUDE_TRANSCRIPT\"}"

# --- Codex: prompt + permission request ------------------------------------
# Codex's adapter doesn't read transcripts, so `last_response` stays None;
# the preview's `{last_response|last_prompt}` template still renders rich
# content via the `last_prompt` fallback we wired up in 59312be.
TMUX_PANE="$PB" muxa hook codex --event user_prompt_submit \
  <<<'{"session_id":"s-b","prompt":"audit the legacy auth middleware for token handling — flag anything that stores raw bearer tokens at rest, and propose a redaction layer that survives the `pre_tool_use` hook fan-out."}'
TMUX_PANE="$PB" muxa hook codex --event permission_request \
  <<<'{"session_id":"s-b","tool_name":"shell"}'

# --- Gemini: prompt + after_agent (no transcript reader) -------------------
TMUX_PANE="$PC" muxa hook gemini --event before_agent \
  <<<'{"session_id":"s-c","prompt":"review PR #482 — focus on the new sorting knob in muxa watch and whether the [Session, Activity] default surprises power users with many short-lived panes."}'
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
