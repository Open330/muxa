#!/usr/bin/env bash
# Bootstrap the muxa-watch hero recording.
#
# Everything here is fixture. The recording has to show a *fleet* — a dozen
# agents spread across every state, mid-collaboration, with answered
# questions in the history — and none of that can be produced by waiting
# around for real agents during a 40-second take. So we stand up an
# isolated tmux server, feed muxad the same hook payloads the real agent
# CLIs send, and seed the mailbox through the real `muxa msg` path.
#
# Steps:
#   1. Write the demo config and the PATH shims. The `tmux` shim routes
#      every later tmux call (ours *and* muxa's children) at the labelled
#      demo server; the `claude`/`codex` shims make `muxa watch`'s ask
#      feature answer instantly, for free, and identically on every render.
#   2. Stand up the isolated tmux server and its sessions.
#   3. Start muxad against that server only.
#   4. Seed agents across every state muxa can render, including two with
#      live Task subagents so the swarm view has trees to expand.
#   5. Seed the collaboration mailbox and the ask history.
#   6. Seed the activity ledger so durations read as real.
#
# Idempotent — safe to re-run between vhs renders. `demo-teardown.sh`
# undoes all of it.

set -euo pipefail

# Hardcoded, not `${VAR:=default}`. This script starts a daemon and then
# feeds it a dozen fabricated agents; if it inherited a real MUXA_SOCKET
# from the caller's shell it would inject that fixture fleet into the
# user's actual registry. There is no scenario where the demo wants to
# talk to a non-demo daemon, so the override simply is not offered.
MUXA_SOCKET=/tmp/muxa-demo.sock
MUXA_CONFIG=/tmp/muxa-demo-config.toml
XDG_DATA_HOME=/tmp/muxa-demo-data
export MUXA_SOCKET MUXA_CONFIG XDG_DATA_HOME

TMUX_LBL=muxa-demo
SHIM_DIR=/tmp/muxa-demo-shim
PID_FILE=/tmp/muxa-demo.pid
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PAINT="$SCRIPT_DIR/demo-paint.sh"  # believable agent frames instead of bare `cat`

# Real tmux binary, resolved absolutely so it bypasses the PATH shim below.
# Docker/CI has it at /usr/bin/tmux; a Homebrew mac has it elsewhere, so the
# tape passes MUXA_DEMO_TMUX after resolving `command -v tmux` pre-shim.
TM="${MUXA_DEMO_TMUX:-/usr/bin/tmux}"

rfc3339_ago() {
  local seconds="$1"
  if date -u -d "1970-01-01 UTC" '+%Y-%m-%dT%H:%M:%SZ' >/dev/null 2>&1; then
    date -u -d "-${seconds} seconds" '+%Y-%m-%dT%H:%M:%SZ'
  else
    date -u -v-"${seconds}"S '+%Y-%m-%dT%H:%M:%SZ'
  fi
}

# ---------------------------------------------------------------------------
# 0) Clear whatever the last run left behind
# ---------------------------------------------------------------------------
# Not belt-and-braces — required. This script unlinks the demo socket and
# starts a fresh muxad, but a daemon left over from an interrupted render
# is still running and still holds its own copy. Two of them then trade the
# socket back and forth and the recording shows "daemon not reachable" at
# random points, which reads as muxa crashing.
bash "$SCRIPT_DIR/demo-teardown.sh"

# ---------------------------------------------------------------------------
# 1) Config + PATH shims
# ---------------------------------------------------------------------------

# Config lives here rather than in the tape so the tape's prelude stays
# short: every line the tape types is a line vhs has to type at 45ms/char
# on every render.
#
# `wake = "never"` matters. The default `idle_only` lets muxad inject the
# wake prompt into a recipient's pane — which, in a recording whose panes
# are painted fixtures, would scribble over the frame mid-take.
cat > "$MUXA_CONFIG" <<'TOML'
[ui]
theme = 'oh-my-muxa'

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
enabled = true
wake = 'never'
# The composer addresses whatever row the cursor is on, and the demo fleet
# spans many tmux windows — window scope would refuse most of them.
scope = 'host'

[ask]
enabled = true
agent = 'claude'
cwd = '/tmp'

[watch]
theme = 'oh-my-muxa'
view = 'session'
sort = ['state']

[watch.preview]
default_content = 'prompt_response'
TOML

mkdir -p "$SHIM_DIR"
cat > "$SHIM_DIR/tmux" <<EOF
#!/bin/sh
exec $TM -u -L $TMUX_LBL "\$@"
EOF
chmod +x "$SHIM_DIR/tmux"

# Ask stand-ins. `muxa watch`'s `a` shells out to the real agent CLI, which
# would bill the user, take an unpredictable 10-40s, and answer differently
# on every render — three separate reasons a recording can't use it. These
# emit exactly what the parsers in ask.rs read: claude's single result
# object, codex's JSONL event stream.
#
# The answers lead with five lines that matter. The ask panel's detail pane
# renders a fixed line budget and then elides, no matter how tall the
# terminal is, so anything past the fifth line is written for the person
# who scrolls — not for the GIF.
cat > "$SHIM_DIR/claude" <<'EOF'
#!/bin/sh
# Demo stand-in for the claude CLI (headless `-p` mode only).
case " $* " in *" -p "*) ;; *) exec cat ;; esac
sleep 1.6
cat <<'JSON'
{"type":"result","subtype":"success","is_error":false,"session_id":"demo-thread-claude","total_cost_usd":0.0118,"result":"Eight working. Three carry live Task subagents:\n\n  • main    3 subagents\n  • api     2 subagents\n  • search  1 subagent\n\nTwo need you: web on a permission prompt, auth on a choice. Start with\nsdk though — it is in error, not blocked, so no prompt will clear it."}
JSON
EOF
chmod +x "$SHIM_DIR/claude"

cat > "$SHIM_DIR/codex" <<'EOF'
#!/bin/sh
# Demo stand-in for the codex CLI (headless `exec` mode only).
case " $* " in *" exec "*) ;; *) exec cat ;; esac
sleep 1.4
cat <<'JSON'
{"type":"thread.started","thread_id":"demo-thread-codex"}
{"type":"item.completed","item":{"type":"agent_message","text":"Shortest path is `muxa attend` — it focuses whichever agent has been\nblocked longest, so you do not have to pick.\n\nRight now that is `web`, blocked 4m12s on a permission prompt for\ncrates/muxa/src/dashboard/server.rs. `auth` is second at 2m48s.\n\nIf you would rather clear them in order, `muxa attend --cycle` rotates\nthrough the queue; bind it to a tmux key and it becomes a doorbell."}}
{"type":"turn.completed"}
JSON
EOF
chmod +x "$SHIM_DIR/codex"

export PATH="$SHIM_DIR:$PATH"

# Ask history, written before muxad starts because the ask store is read
# once at boot. Statuses and field names mirror ask.rs.
seed_ask_history() {
  local ask_file="$XDG_DATA_HOME/muxa/ask.json"
  mkdir -p "$(dirname "$ask_file")"
  cat > "$ask_file" <<EOF
{
  "threads": { "claude": "demo-thread-claude", "codex": "demo-thread-codex" },
  "entries": [
    {
      "id": "ask-0001",
      "prompt": "what is the difference between wake = 'never' and wake = 'idle_only'?",
      "answer": "\`never\` is pull-only: a request lands in the recipient's mailbox and\nsits there until that agent calls muxa_inbox of its own accord. Nothing\nis ever written into its pane.\n\n\`idle_only\` adds one narrow push: when the recipient is idle at its\ntop-level prompt — not mid-turn, not mid-tool-call — muxad injects a\nshort wake line telling it to check the mailbox.\n\nThe restriction is the whole design. Waking a working agent would\ninterleave text into whatever it was composing.",
      "status": "answered",
      "agent": "claude",
      "agent_session_id": "demo-thread-claude",
      "cwd": "/tmp",
      "asked_at": "$(rfc3339_ago 1420)",
      "answered_at": "$(rfc3339_ago 1409)",
      "cost_usd": 0.0091
    },
    {
      "id": "ask-0002",
      "prompt": "which of my agents have been blocked longest?",
      "answer": "web has been on a permission prompt for 4m12s, auth on a multiple\nchoice for 2m48s. sdk is in error rather than blocked — no prompt will\nclear it.\n\n\`muxa attend\` jumps to web; \`muxa attend --cycle\` rotates through all\nthree.",
      "status": "answered",
      "agent": "codex",
      "agent_session_id": "demo-thread-codex",
      "cwd": "/tmp",
      "asked_at": "$(rfc3339_ago 900)",
      "answered_at": "$(rfc3339_ago 892)",
      "cost_usd": 0.0067
    },
    {
      "id": "ask-0003",
      "prompt": "summarize the length-prefixed framing change for the changelog",
      "answer": "**Changed** — IPC frames are now length-prefixed (u32 BE + body)\ninstead of newline-delimited JSON.\n\nNewline delimiting assumed payloads never contained literal newlines,\nwhich stopped being true once transcript snippets started crossing the\nwire. Wire-incompatible with pre-0.8 clients.",
      "status": "answered",
      "agent": "claude",
      "agent_session_id": "demo-thread-claude",
      "cwd": "/tmp",
      "asked_at": "$(rfc3339_ago 520)",
      "answered_at": "$(rfc3339_ago 508)",
      "cost_usd": 0.0134
    }
  ]
}
EOF
}

# ---------------------------------------------------------------------------
# 2) Isolated tmux server
# ---------------------------------------------------------------------------
#
# main:0 runs an interactive bash with a dead-simple rcfile so the recording
# can type into it without dragging the user's Starship/zinit setup along.
# Every other pane sits on `cat` or a painted frame so there is no prompt
# noise behind the TUI.

"$TM" -u -L "$TMUX_LBL" kill-server 2>/dev/null || true
cat > /tmp/muxa-demo-bashrc <<'BASHRC'
export PS1='$ '
unset PROMPT_COMMAND
BASHRC

# Bring the server up on a throwaway session first, purely to learn its
# socket path. MUXA_TMUX_SOCKET has to be exported *before* the real
# sessions are created, because every pane inherits its environment at
# creation time — and a pane without the pin enumerates every tmux server
# on the host, so the recording fills up with the maintainer's own
# sessions. Learned the hard way: the first take showed 59.
"$TM" -u -L "$TMUX_LBL" new-session -d -s _boot cat
MUXA_TMUX_SOCKET=$("$TM" -u -L "$TMUX_LBL" display-message -p '#{socket_path}')
export MUXA_TMUX_SOCKET
# Belt-and-braces for panes created by hand later, which inherit from the
# server rather than from this script.
"$TM" -u -L "$TMUX_LBL" set-environment -g MUXA_TMUX_SOCKET "$MUXA_TMUX_SOCKET"

# Every agent pane gets a painted frame. The inspector and the preview both
# render the selected pane's live screen, so a fleet parked on bare `cat`
# makes those features look broken in the recording. The frame is staged as
# a file and the pane just prints it, which keeps prompt text — em dashes,
# quotes, backticks — away from tmux's shell quoting entirely.
FRAME_DIR=/tmp/muxa-demo-frames
rm -rf "$FRAME_DIR"; mkdir -p "$FRAME_DIR"
frame() { # <slug> <agent> <state> <prompt> [tool]... → echoes the pane command
  local slug="$1"; shift
  bash "$PAINT" "$@" > "$FRAME_DIR/$slug"
  # `exec cat` holds the pane open on a quiet stdin, keeping the frame on
  # screen with clean scrollback for the whole take.
  printf 'cat %s; exec cat' "$FRAME_DIR/$slug"
}

# `ctl` is the operator's own terminal — the session the recording attaches
# to and types into. It is deliberately *not* one of the interesting agents:
# the inspector renders the selected pane's live screen, so running the TUI
# from a pane the cursor might land on makes muxa render a mirror of itself,
# which reads as a bug. It is still seeded as an agent further down, because
# the collaboration mailbox is scoped to its origin pane and `b`/`m` need
# one.
"$TM" -u -L "$TMUX_LBL" new-session -d -s ctl -x 220 -y 46 \
  "bash --rcfile /tmp/muxa-demo-bashrc"

"$TM" -u -L "$TMUX_LBL" new-session -d -s main -x 220 -y 46 \
  "$(frame main claude working 'continue with protocol compatibility tests and update the watch session summary docs' \
      'editing  crates/muxa/src/ipc.rs' 'Task ×3  Explore, general-purpose, code-reviewer')"
# `-d` keeps the current window as main:0 — without it new-window switches
# focus to itself, so attach would land on `vim` (cat-stdin) instead of
# the painted pane.
# `review` hosts codex mid-approval — this is where `muxa attend` lands, so it
# paints a believable approval prompt instead of an empty shell.
"$TM" -u -L "$TMUX_LBL" new-window -d -t main: -n review \
  "$(frame review codex approval 'audit the legacy auth middleware for raw bearer tokens' \
      'ran  rg -n "bearer" src/            18 matches' \
      'ran  cat src/auth/middleware.rs')"
"$TM" -u -L "$TMUX_LBL" new-window -d -t main: -n vim cat

mk() { # <session> [command] → echoes the pane id
  "$TM" -u -L "$TMUX_LBL" new-session -d -s "$1" -x 220 -y 46 "${2:-cat}"
  "$TM" -u -L "$TMUX_LBL" display-message -p -t "$1:0.0" '#{pane_id}'
}
mkwin() { # <session> <window> [command] → echoes the pane id
  "$TM" -u -L "$TMUX_LBL" new-window -d -t "$1:" -n "$2" "${3:-cat}"
  "$TM" -u -L "$TMUX_LBL" display-message -p -t "$1:$2.0" '#{pane_id}'
}

P_OPS=$(mk ops "$(frame ops gemini done 'review PR #482 — the new sorting knob in muxa watch' \
  'read  crates/muxa-cli/src/watch.rs' 'posted 4 comments')")
mkwin ops logs >/dev/null
P_API=$(mk api "$(frame api claude working 'port the reconciler onto the new PaneBackend trait' \
  'editing  crates/muxa/src/reconcile.rs' 'Task ×2  Explore, general-purpose')")
P_WEB=$(mk web "$(frame web claude approval 'fix the SSE reconnect backoff' \
  'read  crates/muxa/src/dashboard/server.rs')")
P_INFRA=$(mk infra "$(frame infra codex working 'terraform: split the network module and pin providers' \
  'ran  terraform plan -out tf.plan' 'editing  modules/network/main.tf')")
P_AUTH=$(mk auth "$(frame auth claude choice 'rotate the JWT signing keys across environments')")
P_DATA=$(mk data "$(frame data claude done 'backfill the analytics rollups for last week' \
  'ran  dbt run --select rollups+' '  14 models, 0 errors')")
P_DOCS=$(mk docs "$(frame docs claude working 'rewrite COLLABORATION.md around the mailbox contract' \
  'editing  docs/COLLABORATION.md')")
P_SDK=$(mk sdk "$(frame sdk claude error 'regenerate the typescript client from the openapi spec' \
  'ran  openapi-generator generate -i spec.yaml')")
P_SEARCH=$(mk search "$(frame search claude working 'swap the tantivy index build onto a background thread' \
  'editing  crates/index/src/build.rs' 'Task ×1  Explore')")
P_MOBILE=$(mk mobile "$(frame mobile codex approval 'bump the RN version and unbreak the notification module')")
P_BILLING=$(mk billing "$(frame billing claude done 'reconcile the proration edge case' \
  'ran  cargo test -p billing proration' '  31 passed')")
P_ETL=$(mk etl "$(frame etl claude working 'make the nightly load idempotent' \
  'editing  jobs/nightly/load.py')")
P_EDGE=$(mk edge "$(frame edge codex working 'cache the geo lookup at the edge' \
  'ran  wrangler deploy --dry-run')")
P_CRM=$(mk crm "$(frame crm claude rate-limit 'migrate the contact importer off the deprecated bulk endpoint' \
  'editing  app/importers/contacts.rb')")
mk lab >/dev/null   # bare session — no agent, proves muxa tracks the whole server

# Second agents inside existing sessions. A session row that expands into
# two children is what the session view is *for*; a fleet of singletons
# never shows it.
P_API_REV=$(mkwin api reviewer "$(frame api-rev codex working 'review the PaneBackend port' \
  'read  crates/muxa/src/reconcile.rs' 'ran  cargo clippy -p muxa')")
P_WEB_E2E=$(mkwin web e2e "$(frame web-e2e claude done 're-record the playwright traces for the timeline view' \
  'ran  npx playwright test --update-snapshots')")

# Belt-and-suspenders: be explicit about which window the recording
# should attach to.
"$TM" -u -L "$TMUX_LBL" kill-session -t _boot
"$TM" -u -L "$TMUX_LBL" select-window -t ctl:0

# The operator's pane: where the recording types, and the origin the
# collaboration mailbox is scoped to.
PCTL=$("$TM" -u -L "$TMUX_LBL" display-message -p -t ctl:0.0 '#{pane_id}')
PA=$("$TM" -u -L "$TMUX_LBL" display-message -p -t main:0.0 '#{pane_id}')
PB=$("$TM" -u -L "$TMUX_LBL" display-message -p -t main:1.0 '#{pane_id}')

# ---------------------------------------------------------------------------
# 3) muxad, scoped to the demo server
# ---------------------------------------------------------------------------
# MUXA_TMUX_SOCKET was pinned back in step 2 and muxad inherits it here, so
# its scan cannot reach a real fleet on the same host.

# The ask store is read once, at daemon start. Seeding it afterwards and
# restarting muxad to pick it up would be worse than useless: `[state]` is
# disabled for the demo, so the restart would drop the whole seeded fleet.
seed_ask_history

rm -f "$MUXA_SOCKET"
# `nohup` so the daemon outlives this script's shell. Without it, running
# setup on its own — seed now, render later — leaves you with a fleet and
# no daemon to serve it. demo-teardown.sh kills it by pid.
nohup muxad --config "$MUXA_CONFIG" >/dev/null 2>&1 &
echo $! > "$PID_FILE"
until [ -S "$MUXA_SOCKET" ]; do sleep 0.1; done

# ---------------------------------------------------------------------------
# 4) Seed the fleet
# ---------------------------------------------------------------------------

# muxad is pinned to the demo tmux server, and it drops any event stamped
# with a different one. A hook client reads that stamp from `$TMUX` — which,
# when you regenerate this GIF from inside your own tmux session, points at
# your real server and every seeded event is silently skipped. So each
# seeding call claims the demo server explicitly rather than inheriting
# whatever the caller happens to be sitting in.
DEMO_TMUX_ENV="$MUXA_TMUX_SOCKET,0,0"

hook() { # <pane> <agent> <event> <json>
  TMUX="$DEMO_TMUX_ENV" TMUX_PANE="$1" muxa hook "$2" --event "$3" <<<"$4"
}

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

# --- main: claude working with subagents, codex blocked on approval --------
# Codex's adapter doesn't read transcripts, so `last_response` stays None;
# the preview's `{last_response|last_prompt}` template still renders rich
# content via the `last_prompt` fallback we wired up in 59312be.
hook "$PB" codex user_prompt_submit \
  '{"session_id":"s-b","prompt":"audit the legacy auth middleware for token handling — flag anything that stores raw bearer tokens at rest, and propose a redaction layer that survives the `pre_tool_use` hook fan-out."}'
hook "$PB" codex permission_request '{"session_id":"s-b","tool_name":"shell"}'

# Seed Claude last so the initial session row preview opens on the current
# pane. The final PromptSubmitted leaves the row Working while preserving the
# captured response from the previous Stop, which makes both the status line
# and preview richer in the README GIF.
hook "$PA" claude user_prompt_submit \
  '{"session_id":"s-a","prompt":"refactor the ipc module to use length-prefixed frames"}'
hook "$PA" claude stop \
  "{\"session_id\":\"s-a\",\"transcript_path\":\"$CLAUDE_TRANSCRIPT\"}"
hook "$PA" claude user_prompt_submit \
  '{"session_id":"s-a","prompt":"continue with protocol compatibility tests and update the watch session summary docs"}'

# In-flight Task subagents so `muxa watch --view swarm` renders a populated
# subagent tree (each is a pre_tool_use{Task} with no matching post, so they
# stay live for the recording).
subagents() { # <pane> <session_id> <kind:description>...
  local pane="$1" sid="$2"; shift 2
  for spec in "$@"; do
    hook "$pane" claude pre_tool_use \
      "{\"session_id\":\"$sid\",\"tool_name\":\"Task\",\"tool_input\":{\"subagent_type\":\"${spec%%:*}\",\"description\":\"${spec#*:}\"}}"
  done
}
subagents "$PA" s-a \
  "Explore:trace the pane→session cache" \
  "general-purpose:write the round-trip tests" \
  "code-reviewer:review the framing diff"

# --- api: two agents in one session, the claude one with its own tree ------
hook "$P_API" claude user_prompt_submit \
  '{"session_id":"s-api","prompt":"port the reconciler onto the new PaneBackend trait"}'
subagents "$P_API" s-api \
  "Explore:map the reconcile call sites" \
  "general-purpose:port and de-dup"
hook "$P_API_REV" codex user_prompt_submit \
  '{"session_id":"s-api-rev","prompt":"review the PaneBackend port — focus on whether the reap path still distinguishes a closed pane from an unreachable tmux server"}'
hook "$P_API_REV" codex pre_tool_use '{"session_id":"s-api-rev","tool_name":"shell"}'

# --- web: claude waiting on a permission prompt, e2e agent idle beside it --
hook "$P_WEB" claude user_prompt_submit \
  '{"session_id":"s-web","prompt":"fix the SSE reconnect backoff"}'
hook "$P_WEB" claude notification \
  '{"session_id":"s-web","notification_type":"permission_prompt","message":"Allow edit to crates/muxa/src/dashboard/server.rs?"}'
hook "$P_WEB_E2E" claude user_prompt_submit \
  '{"session_id":"s-web-e2e","prompt":"re-record the playwright traces for the timeline view"}'
hook "$P_WEB_E2E" claude stop '{"session_id":"s-web-e2e"}'

# --- auth: blocked on a choice (distinct from waiting on input) ------------
hook "$P_AUTH" claude user_prompt_submit \
  '{"session_id":"s-auth","prompt":"rotate the JWT signing keys across environments"}'
hook "$P_AUTH" claude pre_tool_use '{"session_id":"s-auth","tool_name":"AskUserQuestion"}'

# --- infra / docs: more working spinners -----------------------------------
hook "$P_INFRA" codex user_prompt_submit \
  '{"session_id":"s-infra","prompt":"terraform: split the network module and pin providers"}'
hook "$P_INFRA" codex pre_tool_use '{"session_id":"s-infra","tool_name":"shell"}'
hook "$P_DOCS" claude user_prompt_submit \
  '{"session_id":"s-docs","prompt":"rewrite COLLABORATION.md around the mailbox contract rather than the wake mechanism"}'

# --- sdk: hard error. Nobody unblocks this by answering a prompt, which is
#     exactly why it gets its own colour instead of sharing "needs you".
hook "$P_SDK" claude user_prompt_submit \
  '{"session_id":"s-sdk","prompt":"regenerate the typescript client from the openapi spec"}'
hook "$P_SDK" claude stop_failure \
  '{"session_id":"s-sdk","error":"api_error","error_details":"500 from the codegen service after 3 retries — spec fetch succeeded, generation did not"}'

# --- crm: rate-limited. Its own row state, not an error: the agent is fine,
#     the account is out of budget, and nothing you type will move it.
hook "$P_CRM" claude user_prompt_submit \
  '{"session_id":"s-crm","prompt":"migrate the contact importer off the deprecated bulk endpoint"}'
hook "$P_CRM" claude stop_failure \
  '{"session_id":"s-crm","error":"rate_limit","last_assistant_message":"Hit the 5-hour limit mid-migration. Picking back up from the importer once it resets."}'

# --- the long tail: enough working/blocked agents that the list reads as a
#     fleet rather than a handful of rows. This is the state muxa exists for
#     — one screen you can actually triage from.
hook "$P_SEARCH" claude user_prompt_submit \
  '{"session_id":"s-search","prompt":"swap the tantivy index build onto a background thread and add a progress channel"}'
subagents "$P_SEARCH" s-search "Explore:find every synchronous index write"
hook "$P_MOBILE" codex user_prompt_submit \
  '{"session_id":"s-mobile","prompt":"bump the RN version and unbreak the notification module"}'
hook "$P_MOBILE" codex permission_request '{"session_id":"s-mobile","tool_name":"shell"}'
hook "$P_ETL" claude user_prompt_submit \
  '{"session_id":"s-etl","prompt":"make the nightly load idempotent — it double-counts when the retry lands after midnight"}'
hook "$P_EDGE" codex user_prompt_submit \
  '{"session_id":"s-edge","prompt":"cache the geo lookup at the edge and add a stale-while-revalidate window"}'
hook "$P_EDGE" codex pre_tool_use '{"session_id":"s-edge","tool_name":"shell"}'
hook "$P_BILLING" claude user_prompt_submit \
  '{"session_id":"s-billing","prompt":"reconcile the proration edge case when a plan changes twice in one period"}'
hook "$P_BILLING" claude stop '{"session_id":"s-billing"}'

# --- ctl: the operator's own session. Registered as an agent for one
#     reason: the collaboration mailbox is scoped to its origin pane, so
#     `b` and `m` need this pane to be hook-correlated. Left idle — it is
#     the seat you are sitting in, not work in flight.
hook "$PCTL" claude user_prompt_submit \
  '{"session_id":"s-ctl","prompt":"keep an eye on the fleet while the migration lands"}'
hook "$PCTL" claude stop '{"session_id":"s-ctl"}'

# --- data: idle, just finished a turn (the resting state) ------------------
hook "$P_DATA" claude user_prompt_submit \
  '{"session_id":"s-data","prompt":"backfill the analytics rollups for last week"}'
hook "$P_DATA" claude stop '{"session_id":"s-data"}'

# --- ops: gemini, idle -----------------------------------------------------
hook "$P_OPS" gemini before_agent \
  '{"session_id":"s-c","prompt":"review PR #482 — focus on the new sorting knob in muxa watch and whether the [Session, Activity] default surprises power users with many short-lived panes."}'
hook "$P_OPS" gemini after_agent '{"session_id":"s-c"}'

# ---------------------------------------------------------------------------
# 5) Collaboration mailbox
# ---------------------------------------------------------------------------
# Sent through the real `muxa msg` path rather than written into
# collaboration.json, so the recording exercises the same code an agent
# would. `$PA` is the pane the recording runs `muxa watch` from, and the
# mailbox is scoped to its origin pane — so these have to point at it.

msg() { # <from-pane> <to-pane> <kind> <body>
  TMUX="$DEMO_TMUX_ENV" TMUX_PANE="$1" muxa msg send "$2" "$4" --kind "$3" >/dev/null
}

msg "$P_API_REV" "$PCTL" review \
  'The PaneBackend port looks right, but reap() now treats an unreachable tmux server the same as a closed pane. Every agent on that server goes stale in one tick. Want me to split the error, or is that intentional for the demo fleet?'
msg "$P_DOCS" "$PCTL" question \
  'COLLABORATION.md still leads with the wake mechanism. Should I document `wake = never` as the default posture and treat idle_only as the opt-in, or keep the current framing?'
msg "$P_INFRA" "$PCTL" review \
  'Provider pins are in. One question before I apply: the network module split moves the NAT gateway into its own state file, which is a destroy/recreate. Confirm before I run apply?'
# One outgoing, so the Sent tab is not empty either.
msg "$PCTL" "$P_SDK" task \
  'The codegen service is 500ing on the spec fetch retry path. Capture the request id from the last failure and check whether the spec URL still resolves from inside the container.'


# ---------------------------------------------------------------------------
# 6) Activity ledger
# ---------------------------------------------------------------------------
# The live hooks above prove agent ingest works, but a short GIF cannot wait
# minutes for durations to accumulate. These closed intervals make the
# stats/activity scenes representative while staying fully isolated under
# XDG_DATA_HOME.
ACTIVITY_FILE="$XDG_DATA_HOME/muxa/activity.ndjson"
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

# ---------------------------------------------------------------------------
# 7) Wire muxa into the demo server so the recording shows the integration
# ---------------------------------------------------------------------------
"$TM" -u -L "$TMUX_LBL" set-option -g status-interval 1
# Status bar at the top — much more legible in a GIF than the default bottom
# bar squeezed against the recording's edge.
"$TM" -u -L "$TMUX_LBL" set-option -g status-position top
"$TM" -u -L "$TMUX_LBL" set-option -g status-style "bg=#0d1117,fg=#f0f6fc"
"$TM" -u -L "$TMUX_LBL" set-option -g status-left  "#[bg=#58a6ff,fg=#0d1117,bold] muxa-demo #[default] "
"$TM" -u -L "$TMUX_LBL" set-option -g status-left-length 20
"$TM" -u -L "$TMUX_LBL" set-option -g status-right \
  "#[fg=#f0f6fc,bold]#(muxa status-line --pane #{pane_id})#[default]   #[fg=#79c0ff,bold]%H:%M#[default] "
"$TM" -u -L "$TMUX_LBL" set-option -g status-right-length 120

# The real binding muxa init writes: full-client popup, caller pinned so
# Enter attaches the terminal the popup was opened from.
"$TM" -u -L "$TMUX_LBL" bind-key s run-shell -b \
  "tmux display-popup -c '#{client_name}' -B -E -w 100% -h 100% -x 0 -y 0 \"muxa watch --caller-client '#{client_name}' --caller-pane '#{pane_id}'\""
