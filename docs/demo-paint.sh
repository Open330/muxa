#!/usr/bin/env bash
# Paint a believable, static agent frame into a demo pane, then hold on
# `cat` so the pane stays put (and keeps clean scrollback) for the
# recording. Replaces the bare `cat` panes in docs/demo-setup.sh so that
# `muxa attend` / Enter actually land the viewer on something that looks
# like a real agent session instead of an empty shell.
#
# Usage (from a tmux pane command):
#   bash docs/demo-paint.sh claude
#   bash docs/demo-paint.sh codex
#   bash docs/demo-paint.sh gemini
#
# Colors are raw ANSI so this has no dependency on the agent binaries —
# the frames are fixtures, matched closely enough to each CLI's look to
# read as genuine at GIF resolution.

set -euo pipefail

C_RESET=$'\033[0m'
C_DIM=$'\033[2m'
C_BOLD=$'\033[1m'
C_GREEN=$'\033[1;32m'
C_YELLOW=$'\033[1;33m'
C_BLUE=$'\033[1;34m'
C_CYAN=$'\033[1;36m'
C_MAG=$'\033[1;35m'

clear

case "${1:-}" in
  claude)
    printf '%s● claude%s %s· muxa%s\n'      "$C_MAG"  "$C_RESET" "$C_DIM" "$C_RESET"
    printf '%s~/src/muxa on  ipc-frames%s\n\n' "$C_DIM" "$C_RESET"
    printf '%s›%s refactor the ipc module to use length-prefixed frames\n\n' "$C_GREEN" "$C_RESET"
    printf '  %s⚙%s editing  crates/muxa/src/ipc.rs\n'  "$C_DIM" "$C_RESET"
    printf '  %s⚙%s running  cargo test --lib ipc\n\n'  "$C_DIM" "$C_RESET"
    printf '  %s▶ working…%s\n' "$C_BLUE" "$C_RESET"
    ;;
  codex)
    printf '%s● codex%s %s· legacy-auth%s\n'      "$C_CYAN" "$C_RESET" "$C_DIM" "$C_RESET"
    printf '%s~/src/legacy on  main%s\n\n'         "$C_DIM"  "$C_RESET"
    printf '%s›%s audit the legacy auth middleware for raw bearer tokens\n\n' "$C_GREEN" "$C_RESET"
    printf '  %s⚙%s ran  rg -n "bearer" src/           %s18 matches%s\n' "$C_DIM" "$C_RESET" "$C_DIM" "$C_RESET"
    printf '  %s⚙%s ran  cat src/auth/middleware.rs\n\n' "$C_DIM" "$C_RESET"
    printf '  %s⏸  Approval required%s\n'          "$C_YELLOW" "$C_RESET"
    printf '     %s$ rg -n "token" --type rust -g '\''!tests/'\''%s\n\n' "$C_BOLD" "$C_RESET"
    printf '     %s[y]%s yes   %s[n]%s no   %s[a]%s yes, and don'\''t ask again\n' \
      "$C_GREEN" "$C_RESET" "$C_YELLOW" "$C_RESET" "$C_DIM" "$C_RESET"
    ;;
  gemini)
    printf '%s● gemini%s %s· review%s\n'          "$C_BLUE" "$C_RESET" "$C_DIM" "$C_RESET"
    printf '%s~/src/review on  main%s\n\n'         "$C_DIM"  "$C_RESET"
    printf '%s›%s review PR #482 — new sorting knob in muxa watch\n\n' "$C_GREEN" "$C_RESET"
    printf '  %s?%s Which default ordering should I evaluate against —\n' "$C_YELLOW" "$C_RESET"
    printf '    [Session, Activity] or [Activity, Session]?\n\n'
    printf '  %s▶ waiting for your reply%s\n' "$C_BLUE" "$C_RESET"
    ;;
  *)
    printf 'usage: demo-paint.sh {claude|codex|gemini}\n' >&2
    exit 2
    ;;
esac

# Hold the pane. `cat` keeps the frame on screen and the pane alive with a
# quiet stdin, exactly like the old bare-`cat` panes.
exec cat
