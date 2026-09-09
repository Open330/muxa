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
    other_pid = None
    other_fd = None
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
        other_pid, other_fd = pty.fork()
        if other_pid == 0:
            os.execve(base[0], base + ['attach-session', '-t', 'destination:0'], env)
        fcntl.ioctl(other_fd, termios.TIOCSWINSZ, struct.pack('HHHH', 30, 120, 0, 0))
        def drain_other():
            while not stop.is_set():
                if select.select([other_fd], [], [], 0.1)[0]:
                    try: os.read(other_fd, 65536)
                    except OSError: break
        threading.Thread(target=drain_other, daemon=True).start()
        for _ in range(50):
            others = [name for name in run('list-clients', '-F', '#{client_name}').splitlines() if name != client]
            if others: break
            time.sleep(.1)
        assert len(others) == 1, 'second client did not attach'
        print('before:', run('list-clients', '-F', '#{client_name} #{session_name} #{window_name}'), flush=True)
        testenv = dict(env, RMUX=socket+',0,0', RMUX_PANE='%0',
            MUXA_RMUX_TEST_ENDPOINT=socket, MUXA_RMUX_TEST_PANE=target, MUXA_RMUX_TEST_CLIENT=client, MUXA_RMUX_TEST_OTHER_CLIENT=others[0])
        result = subprocess.run(['cargo', 'test', '-p', 'muxa-cli', 'live_rmux_', '--', '--ignored', '--nocapture'], env=testenv, cwd=repo, timeout=300)
        print('after:', run('list-clients', '-F', '#{client_name} #{session_name} #{window_name}'), flush=True)
        print('clients-detail:', run('list-clients', '-F', '#{client_name} #{session_id} #{session_name}'), flush=True)
        print('destination:', run('display-message', '-p', '-t', 'destination', '#{session_id}:#{window_id}.#{pane_id}'), flush=True)
        print('panes-detail:', run('list-panes', '-a', '-F', '#{session_id}:#{window_id}.#{pane_id} window_active=#{window_active} pane_active=#{pane_active}'), flush=True)
        if result.returncode: raise SystemExit(result.returncode)
        binary = str(repo / 'target/debug/muxa')
        def muxa(*args):
            p = subprocess.run([binary, *args], env=testenv, cwd=repo, text=True, capture_output=True, timeout=15)
            if p.returncode: raise RuntimeError((args, p.stdout, p.stderr))
            return p.stdout.strip()
        window = run('display-message', '-p', '-t', 'destination:target', '#{window_id}')
        run('set-buffer', '-b', 'muxa-window-name-check', 'cli renamed')
        renamed = muxa('window', 'rename', '--window', window, '--buffer', 'muxa-window-name-check', '--json')
        assert 'cli-renamed' in renamed, renamed
        names = [row.split('\t')[1] for row in run('list-windows', '-a', '-F', '#{window_id}\t#{window_name}').splitlines() if row.split('\t')[0] == window]
        assert names and all(name == 'cli-renamed' for name in names), names
        # The hook command must work independently of watch and keep a sole
        # client's existing view without spawning another one.
        muxa('workspace', 'view', '--client', client, '--json')
        plain = muxa('peek', '--plain')
        assert plain, 'peek returned no panes'
        print('CLI buffer rename, workspace view, and peek passed', flush=True)
        subprocess.run(['cargo', 'test', '-p', 'muxa', 'live_backend_smoke_against_explicit_endpoint', '--', '--ignored'], env=testenv, cwd=repo, timeout=300, check=True)
        # Exercise the actual installed hook body, including rmux's run-shell
        # environment, rather than only calling workspace view ourselves.
        source = (repo / 'crates/muxa-cli/src/init/files/tmux.rs').read_text()
        hook = source.split('pub const AUTO_VIEW_BODY: &str = r#"', 1)[1].split('"#;', 1)[0]
        hook = hook.replace('muxa workspace view', binary + ' workspace view')
        hook_file = Path(tmp) / 'auto-view.conf'
        hook_file.write_text(hook)
        run('source-file', str(hook_file))
        original_session = run('display-message', '-p', '-t', 'destination', '#{session_id}')
        run('switch-client', '-c', client, '-t', original_session)
        for _ in range(50):
            rows = dict(line.split('\t') for line in run('list-clients', '-F', '#{client_name}\t#{session_id}').splitlines())
            if rows.get(client) != original_session: break
            time.sleep(.1)
        assert rows[client] != original_session, rows
        assert rows[others[0]] == original_session, rows
        print('Native backend transport and auto-view hook passed', flush=True)


    finally:
        subprocess.run(base + ['kill-server'], env=env, capture_output=True, timeout=10)
        if other_pid:
            stop.set()
            os.close(other_fd)
            os.waitpid(other_pid, 0)
        if pid:
            stop.set()
            os.close(fd)
            os.waitpid(pid, 0)
