#!/usr/bin/env python3
"""Exercise watch's rmux jump against a disposable server and PTY client.

Run from any directory: python3 scripts/rmux-jump-check.py
Requires rmux, cargo, and Unix PTYs. Never connects to a user server.
"""
import os, pty, subprocess, tempfile, time, select, threading, fcntl, termios, struct, shutil
from pathlib import Path
repo = Path(__file__).resolve().parents[1]
rmux = shutil.which('rmux')
if not rmux:
    raise SystemExit('rmux is required')
with tempfile.TemporaryDirectory(prefix='muxa-rmux-check-') as tmp:
    socket = tmp + '/rmux.sock'
    env = dict(os.environ, TERM='xterm-256color')
    for key in ('RMUX', 'RMUX_PANE', 'TMUX', 'TMUX_PANE'):
        env.pop(key, None)
    base = [rmux, '-S', socket]
    def run(*args):
        p = subprocess.run(base + list(args), env=env, text=True, capture_output=True, timeout=10)
        if p.returncode: raise RuntimeError((args, p.stdout, p.stderr))
        return p.stdout.strip()
    pid = None
    stop = threading.Event()
    try:
        run('-f', '/dev/null', 'new-session', '-d', '-s', 'origin', '/bin/sh')
        run('new-session', '-d', '-s', 'destination', '/bin/sh')
        run('new-window', '-d', '-t', 'destination', '-n', 'target', '/bin/sh')
        run('split-window', '-d', '-t', 'destination:target', '/bin/sh')
        target = run('list-panes', '-t', 'destination:target', '-F', '#{pane_id}').splitlines()[-1]
        pid, fd = pty.fork()
        if pid == 0:
            os.execve(base[0], base + ['attach-session', '-t', 'origin'], env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', 30, 120, 0, 0))
        def drain():
            while not stop.is_set():
                if select.select([fd], [], [], 0.1)[0]:
                    try: os.read(fd, 65536)
                    except OSError: break
        thread = threading.Thread(target=drain, daemon=True); thread.start()
        client = ''
        for _ in range(50):
            client = run('list-clients', '-F', '#{client_name}')
            if client: break
            time.sleep(.1)
        assert client, 'client did not attach'
        print('before:', run('list-clients', '-F', '#{client_name} #{session_name} #{window_name}'), flush=True)
        testenv = dict(env, RMUX=socket+',0,0', RMUX_PANE='%0',
            MUXA_RMUX_TEST_ENDPOINT=socket, MUXA_RMUX_TEST_PANE=target, MUXA_RMUX_TEST_CLIENT=client)
        result = subprocess.run(['cargo', 'test', '-p', 'muxa-cli', 'live_rmux_jump_switches_client_session_and_window', '--', '--ignored', '--nocapture'], env=testenv, cwd=repo, timeout=300)
        print('after:', run('list-clients', '-F', '#{client_name} #{session_name} #{window_name}'), flush=True)
        print('clients-detail:', run('list-clients', '-F', '#{client_name} #{session_id} #{session_name}'), flush=True)
        print('destination:', run('display-message', '-p', '-t', 'destination', '#{session_id}:#{window_id}.#{pane_id}'), flush=True)
        print('panes-detail:', run('list-panes', '-a', '-F', '#{session_id}:#{window_id}.#{pane_id} window_active=#{window_active} pane_active=#{pane_active}'), flush=True)
        if result.returncode: raise SystemExit(result.returncode)
    finally:
        subprocess.run(base + ['kill-server'], env=env, capture_output=True, timeout=10)
        if pid:
            stop.set()
            os.close(fd)
            os.waitpid(pid, 0)
