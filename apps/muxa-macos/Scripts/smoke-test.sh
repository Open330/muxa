#!/bin/bash

set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
APP_DIR=$(cd "$SCRIPT_DIR/.." && pwd)
CONFIGURATION=${CONFIGURATION:-Debug}
APP_PATH=${1:-"$APP_DIR/.build/DerivedData/Build/Products/$CONFIGURATION/Muxa.app"}
APP_EXECUTABLE="$APP_PATH/Contents/MacOS/Muxa"
MUXA_EXECUTABLE="$APP_PATH/Contents/Helpers/muxa"

if [ ! -x "$APP_EXECUTABLE" ]; then
    echo "Muxa.app is not built: $APP_PATH" >&2
    exit 1
fi
if [ ! -x "$MUXA_EXECUTABLE" ]; then
    echo "Muxa.app is missing its bundled muxa Work runtime: $MUXA_EXECUTABLE" >&2
    exit 1
fi
"$MUXA_EXECUTABLE" --version >/dev/null

SMOKE_DIR=$(mktemp -d)
SOCKET_PATH="$SMOKE_DIR/muxad.sock"
APP_LOG="$SMOKE_DIR/muxa.log"
APP_PID=

cleanup() {
    if [ -n "$APP_PID" ] && kill -0 "$APP_PID" 2>/dev/null; then
        kill "$APP_PID" 2>/dev/null || true
        wait "$APP_PID" 2>/dev/null || true
    fi
    if [ -S "$SOCKET_PATH" ]; then
        while IFS= read -r daemon_pid; do
            [ -n "$daemon_pid" ] && kill "$daemon_pid" 2>/dev/null || true
        done < <(lsof -t "$SOCKET_PATH" 2>/dev/null || true)
    fi
    rm -rf "$SMOKE_DIR"
}
trap cleanup EXIT

ipc() {
    python3 - "$SOCKET_PATH" "$1" <<'PY'
import json
import socket
import sys

socket_path, request = sys.argv[1:]
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
    client.settimeout(3)
    client.connect(socket_path)
    client.sendall(request.encode() + b"\n")
    chunks = bytearray()
    while b"\n" not in chunks:
        chunk = client.recv(65536)
        if not chunk:
            break
        chunks.extend(chunk)

response = json.loads(bytes(chunks).split(b"\n", 1)[0])
if not response.get("ok"):
    raise SystemExit(response.get("error", "muxad rejected smoke request"))
print(json.dumps(response, separators=(",", ":")))
PY
}

MUXA_SOCKET="$SOCKET_PATH" "$APP_EXECUTABLE" >"$APP_LOG" 2>&1 &
APP_PID=$!

for _ in $(seq 1 50); do
    [ -S "$SOCKET_PATH" ] && break
    if ! kill -0 "$APP_PID" 2>/dev/null; then
        cat "$APP_LOG" >&2
        exit 1
    fi
    sleep 0.1
done
test -S "$SOCKET_PATH"

hello=$(ipc '{"protocol":6,"kind":"hello","client":"muxa-macos-smoke"}')
python3 - "$hello" <<'PY'
import json
import sys

response = json.loads(sys.argv[1])
required = {"session_bytes_v1", "session_attachment_identity_v1", "work_control_v1"}
missing = required.difference(response.get("capabilities", []))
if missing:
    raise SystemExit(f"muxad did not advertise required capabilities: {sorted(missing)}")
PY

spawned=$(ipc '{"protocol":6,"kind":"spawn_session","command":"/bin/sh","args":["-c","/usr/bin/yes muxa-macos-smoke | /usr/bin/head -c 524288; sleep 5"],"env":[],"cwd":"/tmp","name":"macOS smoke","cols":80,"rows":24}')
session_id=$(python3 - "$spawned" <<'PY'
import json
import sys
print(json.loads(sys.argv[1])["session"]["id"])
PY
)

# Give the SwiftUI list time to select the session, construct a Ghostty surface,
# and render a burst large enough to exercise output truncation and replay.
sleep 3
kill -0 "$APP_PID"
ipc "{\"protocol\":6,\"kind\":\"terminate_session\",\"session_id\":\"$session_id\"}" >/dev/null

echo "Muxa.app smoke test passed"
