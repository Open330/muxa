#!/usr/bin/env bash
# A throwaway muxa that cannot touch the one you actually use.
#
#   scripts/muxa-sandbox.sh up --name muxa-demo --config /tmp/demo.toml
#   eval "$(scripts/muxa-sandbox.sh env --name muxa-demo)"
#   …seed whatever the consumer needs…
#   scripts/muxa-sandbox.sh daemon --name muxa-demo
#   scripts/muxa-sandbox.sh down   --name muxa-demo
#
# Every muxa surface is redirected at once, because redirecting only some of
# them is worse than redirecting none: a daemon on a private socket that still
# scans every tmux server on the host will happily register the caller's real
# agents into a fixture registry.
#
#   MUXA_SOCKET        private daemon socket
#   MUXA_CONFIG        private config
#   XDG_DATA_HOME      private state, history, activity, mailbox
#   MUXA_TMUX_SOCKET   pins the pane scan *and* hook ingest to one tmux server
#   PATH               a `tmux` shim so child processes land on that server too
#
# `up` never starts the daemon. Consumers that seed a store muxad reads once at
# boot — the ask history, for one — need a point between "the data directory
# exists" and "the daemon is running", and a `--seed-hook` flag would only be a
# worse spelling of running the two commands in order.
#
# Consumed by `docs/demo-setup.sh`; `scripts/sandbox-smoke.sh` is the test.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

NAME=muxa-sandbox
CONFIG_SRC=
MUXAD_BIN=
MUXA_BIN=
TMUX_BIN=
ALLOW_INSIDE_TMUX=0
EXTRA_PATHS=()

HOLDER_SESSION=_sandbox

usage() {
  cat <<'USAGE'
Usage: muxa-sandbox.sh <up|daemon|env|status|down> [options]

Commands:
  up       Preflight, clear leftovers, then create the sandbox: config, shims,
           and an isolated tmux server. Does not start the daemon.
  daemon   Start muxad against the sandbox and wait for its socket.
  env      Print the `export` lines a consumer needs. Use with `eval`.
  status   Report what exists. Exit 0 healthy, 3 partial, 4 absent.
  down     Destroy everything, verify it is gone, and report what survived.

Options:
  --name <slug>         Sandbox name; namespaces every path (default: muxa-sandbox)
  --config <file>       Config to install (default: a built-in isolated profile)
  --muxad <path>        muxad binary (default: target/debug, then $PATH)
  --muxa <path>         muxa binary (default: target/debug, then $PATH)
  --tmux <path>         Real tmux binary, resolved before the shim shadows it
  --extra-path <dir>    Prepend to the sandbox PATH; repeatable
  --allow-inside-tmux   Permit `up` while $TMUX is set
  -h, --help            Show this help
USAGE
}

die() { printf 'muxa-sandbox: %s\n' "$*" >&2; exit 1; }
note() { printf 'muxa-sandbox: %s\n' "$*" >&2; }

# --------------------------------------------------------------------------
# Arguments and derived paths
# --------------------------------------------------------------------------

[ "$#" -ge 1 ] || { usage >&2; exit 1; }
COMMAND=$1
shift

case "$COMMAND" in
  up | daemon | env | status | down) ;;
  -h | --help) usage; exit 0 ;;
  *) usage >&2; die "unknown command: $COMMAND" ;;
esac

while [ "$#" -gt 0 ]; do
  case "$1" in
    --name) [ "$#" -ge 2 ] || die '--name needs a value'; NAME=$2; shift 2 ;;
    --name=*) NAME=${1#--name=}; shift ;;
    --config) [ "$#" -ge 2 ] || die '--config needs a path'; CONFIG_SRC=$2; shift 2 ;;
    --config=*) CONFIG_SRC=${1#--config=}; shift ;;
    --muxad) [ "$#" -ge 2 ] || die '--muxad needs a path'; MUXAD_BIN=$2; shift 2 ;;
    --muxad=*) MUXAD_BIN=${1#--muxad=}; shift ;;
    --muxa) [ "$#" -ge 2 ] || die '--muxa needs a path'; MUXA_BIN=$2; shift 2 ;;
    --muxa=*) MUXA_BIN=${1#--muxa=}; shift ;;
    --tmux) [ "$#" -ge 2 ] || die '--tmux needs a path'; TMUX_BIN=$2; shift 2 ;;
    --tmux=*) TMUX_BIN=${1#--tmux=}; shift ;;
    --extra-path) [ "$#" -ge 2 ] || die '--extra-path needs a directory'; EXTRA_PATHS+=("$2"); shift 2 ;;
    --extra-path=*) EXTRA_PATHS+=("${1#--extra-path=}"); shift ;;
    --allow-inside-tmux) ALLOW_INSIDE_TMUX=1; shift ;;
    -h | --help) usage; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
done

# `down` recursively removes two directories below an owned root, so the name
# that root is built from is validated rather than trusted. Anything outside
# this alphabet — a slash, a `..`, an empty string — would aim cleanup somewhere
# it has no business being.
[[ "$NAME" =~ ^[a-z0-9][a-z0-9-]{0,30}$ ]] \
  || die "invalid --name '$NAME' (expected lowercase letters, digits and dashes)"

SB_ROOT=/tmp/$NAME-sandbox
SB_OWNER=$SB_ROOT/.owner
SB_SOCKET=$SB_ROOT/muxad.sock
SB_CONFIG=$SB_ROOT/config.toml
SB_DATA=$SB_ROOT/data
SB_SHIM=$SB_ROOT/shim
SB_PID=$SB_ROOT/muxad.pid
SB_LOG=$SB_ROOT/muxad.log
SB_TMUX_SOCKET=$SB_ROOT/tmux.sock
# Recorded at `up` so `env` reproduces the same PATH without the caller
# having to repeat --extra-path on every invocation.
SB_EXTRA=$SB_SHIM/.extra-path
OWNER_MARK="muxa-sandbox-v1:$NAME"

resolve_bins() {
  if [ -z "$MUXAD_BIN" ]; then
    MUXAD_BIN=$REPO_DIR/target/debug/muxad
    [ -x "$MUXAD_BIN" ] || MUXAD_BIN=$(command -v muxad || true)
  fi
  if [ -z "$MUXA_BIN" ]; then
    MUXA_BIN=$REPO_DIR/target/debug/muxa
    [ -x "$MUXA_BIN" ] || MUXA_BIN=$(command -v muxa || true)
  fi
  # Resolved before the shim can shadow it, and kept absolute: the shim's whole
  # job is to make `tmux` mean the sandbox server, which would recurse if the
  # shim resolved `tmux` through PATH.
  if [ -z "$TMUX_BIN" ]; then
    TMUX_BIN=$(command -v tmux || true)
  fi
}

tm() { "$TMUX_BIN" -u -S "$SB_TMUX_SOCKET" "$@"; }

# --------------------------------------------------------------------------
# status / down
# --------------------------------------------------------------------------

owned_sandbox() {
  [ -d "$SB_ROOT" ] && [ ! -L "$SB_ROOT" ] \
    && [ -f "$SB_OWNER" ] && [ ! -L "$SB_OWNER" ] \
    && [ "$(cat "$SB_OWNER" 2>/dev/null)" = "$OWNER_MARK" ]
}

daemon_belongs_to_sandbox() {
  local pid=$1 process_command
  [[ "$pid" =~ ^[0-9]+$ ]] || return 1
  kill -0 "$pid" 2>/dev/null || return 1
  process_command=$(ps -o command= -p "$pid" 2>/dev/null) || return 1
  case " $process_command " in
    *" --config $SB_CONFIG "* | *" --config=$SB_CONFIG "*) return 0 ;;
    *) return 1 ;;
  esac
}

daemon_pid() {
  [ -f "$SB_PID" ] || return 1
  local pid
  pid=$(cat "$SB_PID" 2>/dev/null) || return 1
  daemon_belongs_to_sandbox "$pid" || return 1
  printf '%s' "$pid"
}

server_running() {
  [ -n "$TMUX_BIN" ] && tm has-session -t "$HOLDER_SESSION" >/dev/null 2>&1 && return 0
  [ -n "$TMUX_BIN" ] && tm list-sessions >/dev/null 2>&1
}

# Every daemon belonging to this sandbox. The initial pgrep is only a candidate
# set; each result is checked for the exact config argument before it can be
# killed. No executable-name check: --muxad deliberately supports renamed
# same-version binaries.
all_daemons() {
  local pid
  for pid in $(pgrep -f "$SB_ROOT/config\\.toml" 2>/dev/null || true); do
    daemon_belongs_to_sandbox "$pid" && printf '%s\n' "$pid"
  done
}

# The ones the pidfile lost track of: an interrupted `up`, or a consumer killed
# before its own cleanup ran. The daemon this sandbox started is *not* a stray,
# so health checks must not count it as one.
stray_daemons() {
  local tracked pid
  tracked=$(daemon_pid || true)
  for pid in $(all_daemons); do
    [ "$pid" = "$tracked" ] && continue
    printf '%s\n' "$pid"
  done
}

present_artifacts() {
  local found=()
  server_running && found+=("tmux server ($NAME)")
  daemon_pid >/dev/null && found+=("muxad pid $(daemon_pid)")
  [ -S "$SB_SOCKET" ] && found+=("socket $SB_SOCKET")
  [ -S "$SB_TMUX_SOCKET" ] && found+=("tmux socket $SB_TMUX_SOCKET")
  [ -f "$SB_CONFIG" ] && found+=("config $SB_CONFIG")
  [ -d "$SB_DATA" ] && found+=("data $SB_DATA")
  [ -d "$SB_SHIM" ] && found+=("shims $SB_SHIM")
  [ -f "$SB_PID" ] && found+=("pidfile $SB_PID")
  local stray
  stray=$(stray_daemons | tr '\n' ' ')
  [ -n "${stray// /}" ] && found+=("stray muxad: ${stray% }")
  printf '%s\n' "${found[@]:-}"
}

cmd_status() {
  resolve_bins
  if [ ! -e "$SB_ROOT" ] && [ ! -L "$SB_ROOT" ]; then
    echo "absent — nothing named '$NAME' exists"
    return 4
  fi
  if ! owned_sandbox; then
    echo "partial — $SB_ROOT exists but is not owned by muxa-sandbox; refusing to inspect it"
    return 3
  fi
  local artifacts=() line
  while IFS= read -r line; do
    [ -n "$line" ] && artifacts+=("$line")
  done < <(present_artifacts)

  if [ "${#artifacts[@]}" -eq 0 ]; then
    printf 'present:\n  owned root %s\n' "$SB_ROOT"
    echo "partial — sandbox '$NAME' has no live components; run 'down'"
    return 3
  fi

  printf 'present:\n'
  printf '  %s\n' "${artifacts[@]}"

  local healthy=1
  server_running || healthy=0
  daemon_pid >/dev/null || healthy=0
  [ -S "$SB_SOCKET" ] || healthy=0
  [ -n "$(stray_daemons)" ] && healthy=0

  if [ "$healthy" -eq 1 ]; then
    echo "healthy — sandbox '$NAME' is up"
    return 0
  fi
  echo "partial — sandbox '$NAME' is incomplete or has strays; run 'down'"
  return 3
}

cmd_down() {
  resolve_bins

  if [ ! -e "$SB_ROOT" ] && [ ! -L "$SB_ROOT" ]; then
    return 0
  fi
  owned_sandbox \
    || die "$SB_ROOT exists but is not owned by muxa-sandbox; refusing to delete it"

  if [ -n "$TMUX_BIN" ]; then
    tm kill-server 2>/dev/null || true
  fi

  local pid
  if pid=$(daemon_pid); then
    kill "$pid" 2>/dev/null || true
  fi
  for pid in $(all_daemons); do
    kill "$pid" 2>/dev/null || true
  done

  # SIGTERM is asynchronous; without the wait, `down` can report success while
  # a daemon is still holding its socket, and the next `up` races it.
  local waited=0
  while [ "$waited" -lt 50 ]; do
    [ -z "$(all_daemons)" ] && break
    sleep 0.1
    waited=$((waited + 1))
  done
  for pid in $(all_daemons); do
    kill -9 "$pid" 2>/dev/null || true
  done

  rm -f "$SB_SOCKET" "$SB_TMUX_SOCKET" "$SB_CONFIG" "$SB_PID" "$SB_LOG"
  # Belt and braces on top of the ownership marker: recurse only into the two
  # exact child directories created by this script.
  case "$SB_DATA" in "$SB_ROOT"/data) rm -rf "$SB_DATA" ;; esac
  case "$SB_SHIM" in "$SB_ROOT"/shim) rm -rf "$SB_SHIM" ;; esac

  local survivors=() line
  while IFS= read -r line; do
    [ -n "$line" ] && survivors+=("$line")
  done < <(present_artifacts)
  if [ "${#survivors[@]}" -gt 0 ]; then
    note "these survived teardown:"
    printf '  %s\n' "${survivors[@]}" >&2
    return 1
  fi
  local unknown
  unknown=$(find "$SB_ROOT" -mindepth 1 -maxdepth 1 ! -name .owner -print 2>/dev/null | head -1)
  if [ -n "$unknown" ]; then
    note "refusing to remove unrecognized sandbox artifact: $unknown"
    return 1
  fi
  rm -f "$SB_OWNER"
  rmdir "$SB_ROOT" 2>/dev/null \
    || { note "could not remove sandbox root $SB_ROOT"; return 1; }
  return 0
}

# --------------------------------------------------------------------------
# up
# --------------------------------------------------------------------------

DEFAULT_CONFIG=$(cat <<'TOML'
# Built-in sandbox profile: observable, but unable to reach out.
#
# `wake = "never"` is the load-bearing line. The default `idle_only` lets the
# daemon type a wake notice into a recipient's pane, which in a sandbox means
# writing into a pane the consumer is painting or a learner is reading.
[discovery]
enabled = false

[collaboration]
enabled = true
wake = 'never'
scope = 'host'
TOML
)

preflight() {
  resolve_bins

  [ -n "$TMUX_BIN" ] || die 'tmux not found; pass --tmux <path>'
  [ -x "${MUXAD_BIN:-}" ] || die 'muxad not found; pass --muxad <path> or run cargo build'
  [ -x "${MUXA_BIN:-}" ] || die 'muxa not found; pass --muxa <path> or run cargo build'

  # A CLI and a daemon from different builds negotiate a protocol version that
  # does not exist, and the symptom — "no active agents" while agents are
  # plainly running — reads as a muxa bug rather than a stale binary.
  local muxa_version muxad_version
  muxa_version=$("$MUXA_BIN" --version 2>/dev/null | awk '{print $2}')
  muxad_version=$("$MUXAD_BIN" --version 2>/dev/null | awk '{print $2}')
  if [ -n "$muxa_version" ] && [ -n "$muxad_version" ] && [ "$muxa_version" != "$muxad_version" ]; then
    die "muxa $muxa_version and muxad $muxad_version are different builds; rebuild both"
  fi

  if [ -n "${TMUX:-}" ] && [ "$ALLOW_INSIDE_TMUX" -eq 0 ]; then
    printf '%s\n' \
      "muxa-sandbox: refusing to build a sandbox from inside tmux." \
      "" \
      "  The sandbox runs its own tmux server. Nested inside yours, the prefix" \
      "  becomes ambiguous and it stops being obvious which server a key reaches." \
      "" \
      "  Detach with your prefix + d, or open a terminal outside tmux, and run" \
      "  this again. Pass --allow-inside-tmux if you know you want the nesting." \
      >&2
    exit 2
  fi
}

cmd_up() {
  local extra
  preflight

  # Required, not defensive: an interrupted previous run leaves a daemon that
  # still holds its own copy of the socket, and two of them trading it back and
  # forth looks exactly like muxa crashing at random.
  if [ -e "$SB_ROOT" ] || [ -L "$SB_ROOT" ]; then
    owned_sandbox \
      || die "$SB_ROOT already exists and is not owned by muxa-sandbox"
    cmd_down >/dev/null
  fi

  mkdir -m 700 "$SB_ROOT" || die "could not create sandbox root $SB_ROOT"
  printf '%s\n' "$OWNER_MARK" > "$SB_OWNER"

  # From here until the sandbox is complete, any failure or interrupt must take
  # the half-built sandbox with it — a partial sandbox is the state that costs
  # an hour to diagnose.
  trap 'cmd_down >/dev/null 2>&1 || true' EXIT INT TERM HUP

  if [ -n "$CONFIG_SRC" ]; then
    [ -f "$CONFIG_SRC" ] || die "config not found: $CONFIG_SRC"
    cp "$CONFIG_SRC" "$SB_CONFIG"
  else
    printf '%s\n' "$DEFAULT_CONFIG" > "$SB_CONFIG"
  fi
  mkdir -p "$SB_DATA"

  mkdir -p "$SB_SHIM"
  cat > "$SB_SHIM/tmux" <<EOF
#!/bin/sh
# Sandbox shim: every tmux call, including ones muxa's children make, lands on
# the sandbox server rather than the caller's.
exec $TMUX_BIN -u -S $SB_TMUX_SOCKET "\$@"
EOF
  chmod +x "$SB_SHIM/tmux"

  : > "$SB_EXTRA"
  for extra in ${EXTRA_PATHS[@]+"${EXTRA_PATHS[@]}"}; do
    printf '%s\n' "$extra" >> "$SB_EXTRA"
  done

  # The server has to exist before MUXA_TMUX_SOCKET can be read off it, and the
  # pin has to be in the server environment before any real session is created:
  # panes inherit their environment at creation time, and an unpinned pane
  # enumerates every tmux server on the host.
  tm new-session -d -s "$HOLDER_SESSION" cat
  local tmux_socket
  tmux_socket=$(tm display-message -p '#{socket_path}')
  [ -n "$tmux_socket" ] || die 'could not read the sandbox tmux socket path'
  tm set-environment -g MUXA_TMUX_SOCKET "$tmux_socket"
  tm set-environment -g MUXA_SOCKET "$SB_SOCKET"
  tm set-environment -g MUXA_CONFIG "$SB_CONFIG"
  tm set-environment -g XDG_DATA_HOME "$SB_DATA"

  # Left running on purpose: killing it before the consumer creates its own
  # sessions would take the server down with it. `env` reports the name so a
  # consumer can drop it once its own sessions exist.

  trap - EXIT INT TERM HUP
  return 0
}

# --------------------------------------------------------------------------
# daemon / env
# --------------------------------------------------------------------------

cmd_daemon() {
  resolve_bins
  owned_sandbox || die "sandbox '$NAME' is not up; run 'up' first"
  [ -f "$SB_CONFIG" ] || die "sandbox '$NAME' is not up; run 'up' first"
  server_running || die "sandbox '$NAME' has no tmux server; run 'up' first"

  local pid existing
  if pid=$(daemon_pid); then
    if [ -S "$SB_SOCKET" ]; then
      echo "already running — sandbox '$NAME' muxad pid $pid"
      return 0
    fi
    die "sandbox '$NAME' muxad pid $pid is running without its socket; run 'down'"
  fi
  existing=$(all_daemons | tr '\n' ' ')
  [ -z "${existing// /}" ] \
    || die "sandbox '$NAME' already has untracked muxad: ${existing% }; run 'down'"

  local tmux_socket
  tmux_socket=$(tm display-message -p '#{socket_path}')

  rm -f "$SB_SOCKET" "$SB_LOG"
  # `nohup` so the daemon outlives this invocation — a consumer that sets up now
  # and runs later would otherwise be left with a fixture and no daemon.
  MUXA_SOCKET="$SB_SOCKET" \
  MUXA_CONFIG="$SB_CONFIG" \
  XDG_DATA_HOME="$SB_DATA" \
  MUXA_TMUX_SOCKET="$tmux_socket" \
    nohup "$MUXAD_BIN" --config "$SB_CONFIG" >"$SB_LOG" 2>&1 &
  echo $! > "$SB_PID"

  local waited=0
  while [ "$waited" -lt 100 ]; do
    [ -S "$SB_SOCKET" ] && return 0
    if ! kill -0 "$(cat "$SB_PID")" 2>/dev/null; then
      note "muxad exited before creating its socket:"
      cat "$SB_LOG" >&2 || true
      return 1
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
  note "muxad did not create $SB_SOCKET within 10 seconds:"
  cat "$SB_LOG" >&2 || true
  return 1
}

cmd_env() {
  resolve_bins
  owned_sandbox || die "sandbox '$NAME' is not up; run 'up' first"
  [ -f "$SB_CONFIG" ] || die "sandbox '$NAME' is not up; run 'up' first"
  server_running || die "sandbox '$NAME' has no tmux server; run 'up' first"

  local tmux_socket path_prefix extra
  tmux_socket=$(tm display-message -p '#{socket_path}')
  path_prefix=$SB_SHIM
  if [ -f "$SB_EXTRA" ]; then
    while IFS= read -r extra; do
      [ -n "$extra" ] && path_prefix=$path_prefix:$extra
    done < "$SB_EXTRA"
  fi
  for extra in ${EXTRA_PATHS[@]+"${EXTRA_PATHS[@]}"}; do
    path_prefix=$path_prefix:$extra
  done

  cat <<EOF
export MUXA_SOCKET='$SB_SOCKET'
export MUXA_CONFIG='$SB_CONFIG'
export XDG_DATA_HOME='$SB_DATA'
export MUXA_TMUX_SOCKET='$tmux_socket'
export PATH='$path_prefix':"\$PATH"
export MUXA_SANDBOX_NAME='$NAME'
export MUXA_SANDBOX_SHIM='$SB_SHIM'
export MUXA_SANDBOX_HOLDER='$HOLDER_SESSION'
export MUXA_SANDBOX_TMUX='$TMUX_BIN'
export MUXA_SANDBOX_LOG='$SB_LOG'
# A hook client stamps events with \$TMUX. Seeding from your own tmux session
# without this makes muxad drop every event as out-of-scope.
export MUXA_SANDBOX_TMUX_ENV='$tmux_socket,0,0'
EOF
}

case "$COMMAND" in
  up) cmd_up ;;
  daemon) cmd_daemon ;;
  env) cmd_env ;;
  status) cmd_status ;;
  down) cmd_down ;;
esac
