#!/usr/bin/env bash
# Emit a believable agent frame for one demo pane, on stdout.
#
#   demo-paint.sh <agent> <state> <prompt> [tool line]... > frame.txt
#
#   agent   claude | codex | gemini
#   state   working | approval | choice | error | rate-limit | done
#
# Why this exists: `muxa watch`'s inspector and preview show the selected
# pane's *live screen*. With the demo panes parked on bare `cat`, every one
# of those surfaces renders an empty box — the recording ends up
# advertising the feature by showing nothing. Painting each pane fixes
# that, and painting it from the same prompt the pane was seeded with keeps
# the screen and the row agreeing with each other.
#
# Colors are raw ANSI so this has no dependency on the agent binaries —
# the frames are fixtures, matched closely enough to each CLI's look to
# read as genuine at GIF resolution.
#
# Writing to stdout rather than painting in-pane is deliberate: the caller
# stages a frame file and the pane runs `cat <file>; exec cat`, so no
# prompt text ever has to survive tmux's shell quoting.

set -euo pipefail

agent="${1:-}"
state="${2:-}"
prompt="${3:-}"
shift 3 2>/dev/null || { echo "usage: demo-paint.sh <agent> <state> <prompt> [tool]..." >&2; exit 2; }

C_RESET=$'\033[0m'
C_DIM=$'\033[2m'
C_BOLD=$'\033[1m'
C_GREEN=$'\033[1;32m'
C_YELLOW=$'\033[1;33m'
C_BLUE=$'\033[1;34m'
C_CYAN=$'\033[1;36m'
C_MAG=$'\033[1;35m'
C_RED=$'\033[1;31m'

case "$agent" in
  claude) dot="$C_MAG"  label=claude ;;
  codex)  dot="$C_CYAN" label=codex  ;;
  gemini) dot="$C_BLUE" label=gemini ;;
  *) echo "demo-paint.sh: unknown agent '$agent'" >&2; exit 2 ;;
esac

printf '%s●%s %s%s%s\n\n' "$dot" "$C_RESET" "$C_BOLD" "$label" "$C_RESET"
printf '%s›%s %s\n\n' "$C_GREEN" "$C_RESET" "$prompt"

for line in "$@"; do
  printf '  %s⚙%s %s\n' "$C_DIM" "$C_RESET" "$line"
done
[ "$#" -gt 0 ] && printf '\n'

case "$state" in
  working)
    printf '  %s▶ working…%s\n' "$C_BLUE" "$C_RESET"
    ;;
  approval)
    printf '  %s⏸  Approval required%s\n' "$C_YELLOW" "$C_RESET"
    printf '     %s$ rg -n "token" --type rust -g '\''!tests/'\''%s\n\n' "$C_BOLD" "$C_RESET"
    printf '     %s[y]%s yes   %s[n]%s no   %s[a]%s yes, and don'\''t ask again\n' \
      "$C_GREEN" "$C_RESET" "$C_YELLOW" "$C_RESET" "$C_DIM" "$C_RESET"
    ;;
  choice)
    printf '  %s?%s Which environments should rotate first?\n\n' "$C_YELLOW" "$C_RESET"
    printf '     %s❯%s staging, then prod on approval\n' "$C_GREEN" "$C_RESET"
    printf '       all environments at once\n'
    printf '       prod only — staging already rotated\n'
    ;;
  error)
    printf '  %s✗ 500 from the codegen service after 3 retries%s\n' "$C_RED" "$C_RESET"
    printf '    %sspec fetch succeeded, generation did not%s\n' "$C_DIM" "$C_RESET"
    ;;
  rate-limit)
    printf '  %s⏳ 5-hour limit reached%s\n' "$C_YELLOW" "$C_RESET"
    printf '    %spicking back up from the importer once it resets%s\n' "$C_DIM" "$C_RESET"
    ;;
  done)
    printf '  %s✓ done%s\n' "$C_GREEN" "$C_RESET"
    ;;
  *)
    echo "demo-paint.sh: unknown state '$state'" >&2; exit 2 ;;
esac
