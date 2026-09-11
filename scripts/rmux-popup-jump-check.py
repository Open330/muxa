#!/usr/bin/env python3
"""Test the actual Rust jump inside an rmux popup on a disposable server.

Requires rmux, Cargo and Unix PTYs. No user server or config is touched.
"""
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import shlex
import shutil
import struct
import subprocess
import tempfile
import termios
import threading
import time

repo = Path(__file__).resolve().parents[1]
rmux = shutil.which("rmux")
if not rmux:
    raise SystemExit("rmux is required")
build = subprocess.run(
    ["cargo", "test", "-p", "muxa-cli", "--no-run", "--message-format=json"],
    cwd=repo, text=True, stdout=subprocess.PIPE, check=True,
)
artifacts = [json.loads(line) for line in build.stdout.splitlines()]
binary = next(a["executable"] for a in artifacts
              if a.get("reason") == "compiler-artifact" and a.get("executable")
              and a["profile"]["test"] and a["target"]["name"] == "muxa")

with tempfile.TemporaryDirectory(prefix="muxa-popup-jump-") as tmp:
    socket = str(Path(tmp) / "server.sock")
    env = dict(os.environ, TERM="xterm-256color")
    for key in ("RMUX", "RMUX_PANE", "TMUX", "TMUX_PANE"):
        env.pop(key, None)
    base = [rmux, "-S", socket, "-f", "/dev/null"]
    clients = []
    stop = threading.Event()

    def run(*args):
        return subprocess.run(base + list(args), env=env, text=True,
                              capture_output=True, timeout=10, check=True).stdout.strip()

    def eventually(check):
        for _ in range(100):
            value = check()
            if value:
                return value
            time.sleep(.05)
        raise AssertionError("server state did not reach the expected value")

    def listing():
        return dict(line.split("\t") for line in run(
            "list-clients", "-F", "#{client_name}\t#{session_id}").splitlines())

    def attach(session):
        before = set(listing())
        pid, fd = pty.fork()
        if pid == 0:
            os.execve(rmux, base + ["attach-session", "-t", session], env)
        clients.append((pid, fd))
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))

        def drain():
            while not stop.is_set():
                if select.select([fd], [], [], .1)[0]:
                    try:
                        os.read(fd, 65536)
                    except OSError:
                        break
        threading.Thread(target=drain, daemon=True).start()
        return eventually(lambda: next(iter(set(listing()) - before), None))

    def position(session):
        return run("display-message", "-p", "-t", session, "#{window_id}.#{pane_id}")

    def jump(client, pane):
        session = listing()[client].lstrip("$")
        command = shlex.join([
            "env", f"RMUX={socket},0,{session}",
            f"MUXA_RMUX_TEST_ENDPOINT={socket}", f"MUXA_RMUX_TEST_PANE={pane}",
            f"MUXA_RMUX_TEST_CLIENT={client}", binary,
            "--exact", "tests::popup_rmux_jump", "--ignored", "--nocapture",
        ])
        run("display-popup", "-c", client, "-E", command)

    try:
        for session in ("origin", "destination"):
            run("new-session", "-d", "-s", session, "sleep 300")
            run("new-window", "-d", "-t", session, "-n", "target", "sleep 300")
            run("split-window", "-d", "-t", session + ":target", "sleep 300")
        caller = attach("origin")
        origin_id = listing()[caller]
        target = run("list-panes", "-t", "destination:target", "-F", "#{pane_id}").splitlines()[-1]
        target_position = run("display-message", "-p", "-t", target, "#{window_id}.#{pane_id}")

        # A session-only switch kills the popup before select-window can run.
        jump(caller, target)
        eventually(lambda: position(listing()[caller]) == target_position)
        destination_id = listing()[caller]
        print("PASS cross-session popup jump", flush=True)

        run("select-window", "-t", "destination:0")
        jump(caller, target)
        eventually(lambda: position(listing()[caller]) == target_position)
        print("PASS same-session popup jump", flush=True)

        run("switch-client", "-c", caller, "-t", origin_id)
        other = attach("destination:0")
        original_position = position(destination_id)
        jump(caller, target)
        view = eventually(lambda: listing()[caller] if listing()[caller] not in
                          (origin_id, destination_id) else None)
        eventually(lambda: position(view) == target_position)
        assert listing()[other] == destination_id
        assert position(destination_id) == original_position
        eventually(lambda: run("show-options", "-v", "-t", view, "destroy-unattached") == "on")
        run("detach-client", "-t", caller)
        eventually(lambda: view not in run("list-sessions", "-F", "#{session_id}").splitlines())
        print("PASS private view, other client preserved, view reaped on detach", flush=True)
        caller = attach("origin")

        run("set-option", "-t", destination_id, "@no_auto_view", "1")
        jump(caller, target)
        eventually(lambda: listing()[caller] == destination_id and
                   position(destination_id) == target_position)
        print("PASS explicit shared-session opt-out", flush=True)
    finally:
        subprocess.run(base + ["kill-server"], env=env, capture_output=True, timeout=10)
        stop.set()
        for pid, fd in clients:
            os.close(fd)
            os.waitpid(pid, 0)
