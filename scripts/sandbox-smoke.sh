#!/usr/bin/env bash
# Lifecycle test for scripts/muxa-sandbox.sh.
#
#   scripts/sandbox-smoke.sh
#
# The property worth testing is not that `up` works — a broken `up` is loud.
# It is that `down` returns the machine to clean from *any* state the sandbox
# can be left in, including the states a crash produces, because a sandbox that
# leaks a daemon or a tmux server is worse than no sandbox at all.
#
# Runs against its own name so it can never collide with a real sandbox.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
SANDBOX="$SCRIPT_DIR/muxa-sandbox.sh"
NAME=muxa-smoke

# The suite deliberately runs inside whatever the caller is sitting in, so the
# nesting refusal has to be waived for every call except the one testing it.
# `--extra-path` puts the freshly built `muxa` on the sandbox PATH. Without it
# the isolation checks below reach for whatever `muxa` the host happens to have
# installed, which passes on a developer machine and fails on a clean runner —
# and a suite that only works where muxa is already installed is testing the
# wrong thing.
sb() { # <command> [args…]
  local command=$1
  shift
  bash "$SANDBOX" "$command" \
    --name "$NAME" \
    --allow-inside-tmux \
    --extra-path "$REPO_DIR/target/debug" \
    "$@"
}

passed=0
failed=0

ok() { printf '  ok    %s\n' "$1"; passed=$((passed + 1)); }
no() { printf '  FAIL  %s\n' "$1"; [ "$#" -ge 2 ] && printf '        %s\n' "$2"; failed=$((failed + 1)); }

check() { # <label> <expected-exit> <actual-exit>
  if [ "$2" -eq "$3" ]; then ok "$1"; else no "$1" "expected exit $2, got $3"; fi
}

cleanup() { sb down >/dev/null 2>&1 || true; }
trap cleanup EXIT INT TERM HUP

echo "sandbox smoke — name '$NAME'"
cleanup

# --------------------------------------------------------------------------
echo "lifecycle"

sb status >/dev/null 2>&1
check "status is 'absent' before anything exists" 4 $?

sb up >/dev/null 2>&1
check "up succeeds" 0 $?

sb status >/dev/null 2>&1
check "status is 'partial' with no daemon yet" 3 $?

sb daemon >/dev/null 2>&1
check "daemon starts and its socket appears" 0 $?

sb status >/dev/null 2>&1
check "status is 'healthy' once the daemon is up" 0 $?

# --------------------------------------------------------------------------
echo "isolation"

env_output=$(sb env 2>/dev/null)

# Fail closed. `eval ""` is not a no-op here — it leaves the caller's real
# MUXA_SOCKET and real tmux in place, so an isolation suite that evaluated an
# empty `env` would go on to inspect the host fleet and report it as isolated.
if [ -z "$env_output" ]; then
  no "env prints exports" "nothing to evaluate; skipping the isolation checks"
  no "MUXA_SOCKET points at the sandbox" "no env"
  no "the tmux shim sees only the sandbox server" "no env"
  no "the sandbox daemon answers, with an empty fleet" "no env"
else
  ok "env prints exports"

  # The whole point of the sandbox: a consumer that evaluates `env` must not be
  # able to reach the caller's daemon or the caller's tmux server.
  isolation=$(
    eval "$env_output"
    printf 'socket=%s\n' "$MUXA_SOCKET"
    printf 'sessions=%s\n' "$(tmux list-sessions -F '#{session_name}' 2>/dev/null | tr '\n' ',')"
    printf 'agents=%s\n' "$(muxa status 2>&1 | head -1)"
  )

  case "$isolation" in
    *"socket=/tmp/$NAME.sock"*) ok "MUXA_SOCKET points at the sandbox" ;;
    *) no "MUXA_SOCKET points at the sandbox" "$isolation" ;;
  esac

  case "$isolation" in
    *'sessions=_sandbox,'*) ok "the tmux shim sees only the sandbox server" ;;
    *) no "the tmux shim sees only the sandbox server" "$isolation" ;;
  esac

  case "$isolation" in
    *'agents=no active agents'*) ok "the sandbox daemon answers, with an empty fleet" ;;
    *) no "the sandbox daemon answers, with an empty fleet" "$isolation" ;;
  esac
fi

# --------------------------------------------------------------------------
echo "crash recovery"

# A consumer killed before its cleanup ran leaves a live daemon that the
# pidfile no longer names. That is the state two daemons end up fighting over
# the same socket from, so it has to be both detected and cleanable.
rm -f "/tmp/$NAME.pid"
sb status >/dev/null 2>&1
check "an untracked daemon is reported as a stray" 3 $?

sb down >/dev/null 2>&1
check "down reaps the stray" 0 $?

sb status >/dev/null 2>&1
check "status is 'absent' after down" 4 $?

# --------------------------------------------------------------------------
echo "nothing left behind"

leftovers=$(find /tmp -maxdepth 1 -name "$NAME*" 2>/dev/null | tr '\n' ' ')
if [ -z "${leftovers// /}" ]; then ok "no files under /tmp"; else no "no files under /tmp" "$leftovers"; fi

procs=$(pgrep -f "$NAME-config" 2>/dev/null | tr '\n' ' ')
if [ -z "${procs// /}" ]; then ok "no daemon processes"; else no "no daemon processes" "$procs"; fi

if tmux -L "$NAME" list-sessions >/dev/null 2>&1; then
  no "no tmux server"
else
  ok "no tmux server"
fi

# --------------------------------------------------------------------------
echo "down is safe from any state"

# Never set up at all.
sb down >/dev/null 2>&1
check "down on a sandbox that never existed" 0 $?

# Half-built: a server and a config, but no daemon and no data directory —
# roughly what an interrupted `up` leaves if its trap never fires.
tmux -u -L "$NAME" new-session -d -s _sandbox cat 2>/dev/null
: > "/tmp/$NAME-config.toml"
sb down >/dev/null 2>&1
check "down on a half-built sandbox" 0 $?

sb status >/dev/null 2>&1
check "status is 'absent' afterwards" 4 $?

# --------------------------------------------------------------------------
echo "guards"

bash "$SANDBOX" status --name '../escape' >/dev/null 2>&1
check "a name that could escape /tmp is rejected" 1 $?

TMUX='/tmp/fake-tmux-socket,1,0' bash "$SANDBOX" up --name "$NAME" >/dev/null 2>&1
check "up refuses to nest inside tmux" 2 $?

sb status >/dev/null 2>&1
check "the refused up created nothing" 4 $?

# --------------------------------------------------------------------------
printf '\n%d passed, %d failed\n' "$passed" "$failed"
[ "$failed" -eq 0 ]
