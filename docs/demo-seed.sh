#!/usr/bin/env bash
# Seed a fresh muxa daemon with a realistic mix of agents for the demo.
# Called from docs/demo.tape.

set -euo pipefail

seed() {
  local pane="$1" agent="$2" event="$3" payload="$4"
  TMUX_PANE="$pane" bash -c "printf '%s' '$payload' | muxa hook $agent --event $event"
}

seed '%10' claude 'session_start'      '{"session_id":"s-a","cwd":"/home/you/proj"}'
seed '%10' claude 'user_prompt_submit' '{"session_id":"s-a","prompt":"refactor the ipc module to use length-prefixed frames"}'

seed '%11' codex  'session_start'      '{"session_id":"s-b","cwd":"/home/you/legacy"}'
seed '%11' codex  'permission_request' '{"session_id":"s-b","tool_name":"shell"}'

seed '%12' gemini 'session_start' '{"session_id":"s-c","cwd":"/home/you/review"}'
seed '%12' gemini 'before_agent'  '{"session_id":"s-c","prompt":"summarize this PR"}'
seed '%12' gemini 'after_agent'   '{"session_id":"s-c"}'
