#!/usr/bin/env bash
# Bootstrap the muxa-watch hero recording.
#
# Steps:
#   1. Stand up an isolated tmux server (`-L muxa-demo`) so we don't touch
#      the user's real tmux state. Several sessions give session-view rows
#      for multi-agent, single-agent, and bare-session states.
#   2. Drop a `tmux` shim into a temp dir and prepend it to PATH so muxa's
#      `Command::new("tmux") ...` calls hit the labelled server.
#   3. Pick three of those panes and feed them into muxad as Claude /
#      Codex / Gemini agents. The remaining panes surface as bare session
#      rows in `muxa watch`.
#   4. Seed a demo-local activity ledger so `muxa stats` and
#      `muxa activity` have readable duration rows in the recording.
#
# Idempotent — safe to re-run between vhs renders.

set -euo pipefail

: "${MUXA_SOCKET:=/tmp/muxa-demo.sock}"
TMUX_LBL=muxa-demo
SHIM_DIR=/tmp/muxa-demo-shim
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PAINT="$SCRIPT_DIR/demo-paint.sh"  # believable agent frames instead of bare `cat`

rfc3339_ago() {
  local seconds="$1"
  if date -u -d "1970-01-01 UTC" '+%Y-%m-%dT%H:%M:%SZ' >/dev/null 2>&1; then
    date -u -d "-${seconds} seconds" '+%Y-%m-%dT%H:%M:%SZ'
  else
    date -u -v-"${seconds}"S '+%Y-%m-%dT%H:%M:%SZ'
  fi
}

# 1) PATH shim — every later `tmux` call (ours and muxa's children) routes
#    through `tmux -L muxa-demo`.
# Real tmux binary, resolved absolutely so it bypasses the PATH shim below.
# Docker/CI has it at /usr/bin/tmux; a Homebrew mac has it elsewhere, so the
# tape passes MUXA_DEMO_TMUX after resolving `command -v tmux` pre-shim.
TM="${MUXA_DEMO_TMUX:-/usr/bin/tmux}"

mkdir -p "$SHIM_DIR"
cat > "$SHIM_DIR/tmux" <<EOF
#!/bin/sh
exec $TM -u -L $TMUX_LBL "\$@"
EOF
chmod +x "$SHIM_DIR/tmux"

# 2) Isolated demo server.
#    - main:0 runs an interactive bash with a dead-simple rcfile so we
#      can type `muxa status-line` inside the recording without dragging
#      the user's Starship/zinit setup along.
#    - Every other pane sits on `cat` so the popup has clean space
#      around it (no extra prompts, no scrollback noise).
"$TM" -u -L "$TMUX_LBL" kill-server 2>/dev/null || true
cat > /tmp/muxa-demo-bashrc <<'BASHRC'
export PS1='$ '
unset PROMPT_COMMAND
BASHRC

"$TM" -u -L "$TMUX_LBL" new-session -d -s main -x 200 -y 40 \
  "bash --rcfile /tmp/muxa-demo-bashrc"
# `-d` keeps the current window as main:0 — without it new-window switches
# focus to itself, so attach would land on `vim` (cat-stdin) instead of
# the bash pane.
# `review` hosts codex mid-approval — this is where `muxa attend` lands, so it
# paints a believable approval prompt instead of an empty shell.
"$TM" -u -L "$TMUX_LBL" new-window -d -t main: -n review "bash '$PAINT' codex"
"$TM" -u -L "$TMUX_LBL" new-window -d -t main: -n vim    cat
"$TM" -u -L "$TMUX_LBL" new-session -d -s ops  -x 200 -y 40 "bash '$PAINT' gemini"
"$TM" -u -L "$TMUX_LBL" new-window -d -t ops:  -n logs cat
"$TM" -u -L "$TMUX_LBL" new-session -d -s lab  -x 200 -y 40 cat
# Belt-and-suspenders: be explicit about which window the recording
# should attach to.
"$TM" -u -L "$TMUX_LBL" select-window -t main:0

# 3) Seed muxad. Pin panes by target rather than relying on tmux's
#    list-panes order: main gets two agents (multi-agent gutter), ops gets
#    one agent (single-agent gutter), and lab stays bare.
PA=$("$TM" -u -L "$TMUX_LBL" display-message -p -t main:0.0 '#{pane_id}')
PB=$("$TM" -u -L "$TMUX_LBL" display-message -p -t main:1.0 '#{pane_id}')
PC=$("$TM" -u -L "$TMUX_LBL" display-message -p -t ops:0.0 '#{pane_id}')

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

# --- Claude: prompt + response, then a fresh in-flight turn -----------------
# Seed Claude last so the initial session row preview opens on the current
# pane. The final PromptSubmitted leaves the row Working while preserving the
# captured response from the previous Stop, which makes both the status line
# and preview richer in the README GIF.
TMUX_PANE="$PA" muxa hook claude --event user_prompt_submit \
  <<<'{"session_id":"s-a","prompt":"refactor the ipc module to use length-prefixed frames"}'
TMUX_PANE="$PA" muxa hook claude --event stop \
  <<<"{\"session_id\":\"s-a\",\"transcript_path\":\"$CLAUDE_TRANSCRIPT\"}"
TMUX_PANE="$PA" muxa hook claude --event user_prompt_submit \
  <<<'{"session_id":"s-a","prompt":"continue with protocol compatibility tests and update the watch session summary docs"}'

# 4) Seed activity.ndjson. The live hooks above prove agent ingest works, but
#    a short GIF cannot wait minutes for durations to accumulate. These closed
#    intervals make the stats/activity scenes representative while staying
#    fully isolated under XDG_DATA_HOME=/tmp/muxa-demo-data.
DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
ACTIVITY_FILE="$DATA_HOME/muxa/activity.ndjson"
mkdir -p "$(dirname "$ACTIVITY_FILE")"

T_WORK_START=$(rfc3339_ago 1560)
T_WORK_END=$(rfc3339_ago 1140)
T_WAIT_START=$(rfc3339_ago 900)
T_WAIT_END=$(rfc3339_ago 720)
T_ERR_START=$(rfc3339_ago 660)
T_ERR_END=$(rfc3339_ago 540)
T_TMUX_START=$(rfc3339_ago 1800)
T_TMUX_END=$(rfc3339_ago 300)
T_PROMPT_START=$(rfc3339_ago 870)
T_PROMPT_END=$(rfc3339_ago 760)
T_ATTACH_START=$(rfc3339_ago 700)
T_ATTACH_END=$(rfc3339_ago 620)
T_WATCH_START=$(rfc3339_ago 520)
T_WATCH_END=$(rfc3339_ago 400)

cat > "$ACTIVITY_FILE" <<EOF
{"type":"state_transition","v":1,"at":"$T_WORK_END","kind":"claude_code","session_id":"s-a","pane":"$PA","session_name":"main","cwd":"/home/you/proj","from":"working","to":"waiting_input","state_entered_at":"$T_WORK_START","duration_secs":420}
{"type":"state_transition","v":1,"at":"$T_WAIT_END","kind":"claude_code","session_id":"s-a","pane":"$PA","session_name":"main","cwd":"/home/you/proj","from":"waiting_input","to":"working","state_entered_at":"$T_WAIT_START","duration_secs":180}
{"type":"state_transition","v":1,"at":"$T_ERR_END","kind":"codex","session_id":"s-b","pane":"$PB","session_name":"main","cwd":"/home/you/legacy","from":"error","to":"working","state_entered_at":"$T_ERR_START","duration_secs":120}
{"type":"session_foreground","v":1,"session_id":"demo-main","session_name":"main","started_at":"$T_TMUX_START","ended_at":"$T_TMUX_END","duration_secs":1500}
{"type":"human_interaction","v":1,"kind":"muxa_prompt_input","pane":"$PA","session_id":"s-a","session_name":"main","started_at":"$T_PROMPT_START","ended_at":"$T_PROMPT_END","duration_secs":110}
{"type":"human_interaction","v":1,"kind":"tmux_attach","pane":"$PB","session_id":"s-b","session_name":"main","started_at":"$T_ATTACH_START","ended_at":"$T_ATTACH_END","duration_secs":80}
{"type":"human_interaction","v":1,"kind":"muxa_watch","pane":"$PA","session_id":"s-a","session_name":"main","started_at":"$T_WATCH_START","ended_at":"$T_WATCH_END","duration_secs":120}
EOF

# 5) Wire muxa into the demo server so the recording shows the integration.
"$TM" -u -L "$TMUX_LBL" set-option -g status-interval 1
# Status bar at the top — much more legible in a 1200×720 GIF than the
# default bottom bar squeezed against the recording's edge.
"$TM" -u -L "$TMUX_LBL" set-option -g status-position top
"$TM" -u -L "$TMUX_LBL" set-option -g status-style "bg=#0d1117,fg=#f0f6fc"
"$TM" -u -L "$TMUX_LBL" set-option -g status-left  "#[bg=#58a6ff,fg=#0d1117,bold] muxa-demo #[default] "
"$TM" -u -L "$TMUX_LBL" set-option -g status-left-length 20
"$TM" -u -L "$TMUX_LBL" set-option -g status-right \
  "#[fg=#f0f6fc,bold]#(muxa status-line --pane #{pane_id})#[default]   #[fg=#79c0ff,bold]%H:%M#[default] "
"$TM" -u -L "$TMUX_LBL" set-option -g status-right-length 120

# Bind prefix + s to the muxa watch popup so we can demo the flagship flow.
"$TM" -u -L "$TMUX_LBL" bind-key s display-popup -E -w 90% -h 85% "muxa watch"
