#!/usr/bin/env python3
"""Verify peek on a linked view while popup env names a detached base.

Private rmux socket and shell panes only. Run after cargo build -p muxa-cli.
"""
import fcntl
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
import time

binary = Path(__file__).resolve().parents[1] / 'target/debug/muxa'
rmux = shutil.which('rmux')
assert rmux and binary.exists()
with tempfile.TemporaryDirectory(prefix='muxa-peek-view-') as directory:
    root = Path(directory)
    env = dict(os.environ, TERM='xterm-256color', MUXA_HOST='rmux', MUXA_HOSTS='rmux')
    for key in ('RMUX', 'RMUX_PANE', 'TMUX', 'TMUX_PANE'):
        env.pop(key, None)
    config = root / 'config.toml'
    config.write_text('')
    env['MUXA_CONFIG'] = str(config)
    base = [rmux, '-S', str(root / 'socket')]
    children = []

    def run(*words):
        return subprocess.run(base + list(words), env=env, capture_output=True,
                              text=True, check=True, timeout=10).stdout.strip()

    def collect(seconds):
        end = time.monotonic() + seconds
        result = b''
        while time.monotonic() < end:
            for descriptor in select.select([fd for _, fd in children], [], [], .03)[0]:
                result += os.read(descriptor, 65536)
        return result

    def attach(session):
        before = set(run('list-clients', '-F', '#{client_name}').splitlines())
        pid, fd = pty.fork()
        if pid == 0:
            os.execvpe(rmux, base + ['attach', '-t', session], env)
        children.append((pid, fd))
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 140, 0, 0))
        for _ in range(50):
            collect(.1)
            added = set(run('list-clients', '-F', '#{client_name}').splitlines()) - before
            if added:
                return added.pop(), fd
        raise AssertionError('client did not attach')

    try:
        run('-f', '/dev/null', 'new-session', '-d', '-s', 'original', '/bin/sh')
        run('split-window', '-d', '-t', 'original:0', '/bin/sh')
        main = run('display-message', '-p', '-t', 'original:0', '#{window_id}')
        run('new-window', '-t', 'original', '-n', 'other', '/bin/sh')
        run('new-session', '-d', '-t', 'original', '-s', 'original-view')
        view = run('display-message', '-p', '-t', 'original-view', '#{session_id}')
        run('select-window', '-t', 'original-view:' + main)
        client, fd = attach('original-view')
        assert run('display-message', '-p', '-t', 'original', '#{session_attached}') == '0'
        inherited = str(root / 'socket') + ',1,0'
        stale = ['env', 'RMUX=' + inherited, 'TMUX=' + inherited, str(binary)]
        old_closed = root / 'old-closed'
        old_child = shlex.join(stale + ['peek'])
        old = shlex.join(base + ['display-popup', '-c', client, '-B', '-E', '-w', '100%', '-h', '100%', old_child])
        run('bind-key', 'q', 'run-shell', '-b', old + ' ; touch ' + shlex.quote(str(old_closed)))
        os.write(fd, b'\x02q')
        old_output = collect(2)
        assert b'q/Esc close' not in old_output, 'old peek should not render an interactive overlay'
        assert old_closed.exists(), 'old peek should prematurely close on the detached base'
        print('REPRO: inherited detached base closes old prefix+q', flush=True)

        # Explicit targets must describe the view's two panes, while the base
        # has another current window containing only one pane.
        plain = subprocess.run(stale + ['peek', '--plain', '--caller-session', view,
                                       '--caller-window', main], env=env, capture_output=True,
                               text=True, check=True, timeout=10).stdout
        assert len(plain.strip().splitlines()) == 2, plain
        run('new-session', '-d', '-s', 'unrelated', '/bin/sh')
        other_client, other_fd = attach('unrelated')
        other_session = run('display-message', '-p', '-t', 'unrelated', '#{session_id}')
        fixed = shlex.join(stale + ['peek', '--popup', '--caller-client', '#{client_name}',
                                   '--caller-session', '#{session_id}', '--caller-window', '#{window_id}'])
        run('bind-key', 'q', 'run-shell', '-b', fixed)
        os.write(fd, b'\x02q')
        output = collect(2)
        assert b'q/Esc close' in output, output[-2400:]
        # rmux's display-popup command returns before its child exits. Test
        # a real peek action instead: digit 1 must jump to the second pane.
        os.write(other_fd, b' ')
        collect(1.2)
        os.write(fd, b'1')
        collect(1)
        assert run('display-message', '-p', '-t', 'original-view', '#{pane_id}') == '%1'
        os.write(fd, b'\x02q')
        assert b'q/Esc close' in collect(2)
        os.write(fd, b'q')
        collect(.5)
        assert run('display-message', '-p', '-t', 'original-view', '#{window_id}') == main
        clients = dict(line.split('\t') for line in run('list-clients', '-F', '#{client_name}\t#{session_id}').splitlines())
        assert clients[other_client] == other_session
        print('PASS: prefix+q pins the view/window and pane jumps despite another active client', flush=True)
    finally:
        subprocess.run(base + ['kill-server'], env=env, capture_output=True, timeout=10)
        for pid, fd in children:
            os.close(fd)
            os.waitpid(pid, 0)
