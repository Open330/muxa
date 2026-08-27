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
ROOT=/tmp/$NAME-sandbox
PIDFILE=$ROOT/muxad.pid
SANDBOX_TMUX_SOCKET=$ROOT/tmux.sock
CUSTOM_MUXAD=/tmp/$NAME-muxad-review

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
innocent_pid=
foreign_root_created=0

ok() { printf '  ok    %s\n' "$1"; passed=$((passed + 1)); }
no() { printf '  FAIL  %s\n' "$1"; [ "$#" -ge 2 ] && printf '        %s\n' "$2"; failed=$((failed + 1)); }

check() { # <label> <expected-exit> <actual-exit>
  if [ "$2" -eq "$3" ]; then ok "$1"; else no "$1" "expected exit $2, got $3"; fi
}

cleanup() {
  if [ -n "$innocent_pid" ]; then
    kill "$innocent_pid" 2>/dev/null || true
    wait "$innocent_pid" 2>/dev/null || true
  fi
  sb down >/dev/null 2>&1 || true
  rm -f "$CUSTOM_MUXAD"
  if [ "$foreign_root_created" -eq 1 ]; then
    rm -f "$ROOT/sentinel"
    rmdir "$ROOT" 2>/dev/null || true
  fi
}
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

first_pid=$(cat "$PIDFILE")
sb daemon >/dev/null 2>&1
check "starting the daemon twice is safe" 0 $?
second_pid=$(cat "$PIDFILE")
if [ "$first_pid" = "$second_pid" ]; then
  ok "starting the daemon twice reuses the original process"
else
  no "starting the daemon twice reuses the original process" "first $first_pid, second $second_pid"
fi

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
    *"socket=$ROOT/muxad.sock"*) ok "MUXA_SOCKET points at the sandbox" ;;
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
rm -f "$PIDFILE"
sb status >/dev/null 2>&1
check "an untracked daemon is reported as a stray" 3 $?

sb down >/dev/null 2>&1
check "down reaps the stray" 0 $?

sb status >/dev/null 2>&1
check "status is 'absent' after down" 4 $?

cp "$REPO_DIR/target/debug/muxad" "$CUSTOM_MUXAD"
chmod +x "$CUSTOM_MUXAD"
sb up >/dev/null 2>&1
sb daemon --muxad "$CUSTOM_MUXAD" >/dev/null 2>&1
custom_pid=$(cat "$PIDFILE")
rm -f "$PIDFILE"
sb down >/dev/null 2>&1
check "down finds a custom-named daemon after its pidfile is lost" 0 $?
if kill -0 "$custom_pid" 2>/dev/null; then
  no "the custom-named daemon is gone" "pid $custom_pid survived"
else
  ok "the custom-named daemon is gone"
fi
rm -f "$CUSTOM_MUXAD"

# --------------------------------------------------------------------------
echo "nothing left behind"

leftovers=$(find /tmp -maxdepth 1 -name "$NAME*" 2>/dev/null | tr '\n' ' ')
if [ -z "${leftovers// /}" ]; then ok "no files under /tmp"; else no "no files under /tmp" "$leftovers"; fi

procs=$(pgrep -f "$ROOT/config\.toml" 2>/dev/null | tr '\n' ' ')
if [ -z "${procs// /}" ]; then ok "no daemon processes"; else no "no daemon processes" "$procs"; fi

if tmux -S "$SANDBOX_TMUX_SOCKET" list-sessions >/dev/null 2>&1; then
  no "no tmux server"
else
  ok "no tmux server"
fi

# --------------------------------------------------------------------------
echo "down is safe from any state"

# Never set up at all.
sb down >/dev/null 2>&1
check "down on a sandbox that never existed" 0 $?

# Half-built: a server and a config, but no daemon and no data or shim — roughly
# what an interrupted consumer can leave after `up`.
sb up >/dev/null 2>&1
rm -rf "$ROOT/data" "$ROOT/shim"
sb down >/dev/null 2>&1
check "down on a half-built sandbox" 0 $?

sb status >/dev/null 2>&1
check "status is 'absent' afterwards" 4 $?

# --------------------------------------------------------------------------
echo "guards"

sb up >/dev/null 2>&1
sleep 30 & innocent_pid=$!
printf '%s\n' "$innocent_pid" > "$PIDFILE"
sb down >/dev/null 2>&1
if kill -0 "$innocent_pid" 2>/dev/null; then
  ok "a stale pidfile cannot kill an unrelated process"
else
  no "a stale pidfile cannot kill an unrelated process" "pid $innocent_pid was terminated"
fi
kill "$innocent_pid" 2>/dev/null || true
wait "$innocent_pid" 2>/dev/null || true
innocent_pid=

bash "$SANDBOX" status --name '../escape' >/dev/null 2>&1
check "a name that could escape /tmp is rejected" 1 $?

TMUX='/tmp/fake-tmux-socket,1,0' bash "$SANDBOX" up --name "$NAME" >/dev/null 2>&1
check "up refuses to nest inside tmux" 2 $?

sb status >/dev/null 2>&1
check "the refused up created nothing" 4 $?

mkdir -m 700 "$ROOT"
foreign_root_created=1
printf '%s\n' 'keep me' > "$ROOT/sentinel"
sb down >/dev/null 2>&1
check "down refuses an unowned path collision" 1 $?
if [ -f "$ROOT/sentinel" ]; then
  ok "refused teardown preserves unrelated data"
else
  no "refused teardown preserves unrelated data"
fi
rm -f "$ROOT/sentinel"
rmdir "$ROOT"
foreign_root_created=0

tmux_a=$(mktemp -d)
tmux_b=$(mktemp -d)
TMUX_TMPDIR=$tmux_a sb up >/dev/null 2>&1
check "up succeeds under a custom TMUX_TMPDIR" 0 $?
TMUX_TMPDIR=$tmux_b sb down >/dev/null 2>&1
check "down reaches the same server under a different TMUX_TMPDIR" 0 $?
if tmux -S "$SANDBOX_TMUX_SOCKET" list-sessions >/dev/null 2>&1; then
  no "the explicit tmux socket is gone"
else
  ok "the explicit tmux socket is gone"
fi
rmdir "$tmux_a" "$tmux_b"

# --------------------------------------------------------------------------
printf '\n%d passed, %d failed\n' "$passed" "$failed"
[ "$failed" -eq 0 ]
