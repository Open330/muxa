#!/usr/bin/env python3
"""Exercise real prefix+s / Enter inside watch, including private view cleanup.

Uses disposable sockets and PTYs, never live sessions. --caller-pane seeds the
cursor to an exact test pane; Enter still runs the normal watch action path.
"""
import argparse
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

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--rmux', default=shutil.which('rmux'))
parser.add_argument('--muxa', type=Path, default=Path(__file__).resolve().parents[1] / 'target/debug/muxa')
args = parser.parse_args()
assert args.rmux and args.muxa.exists()

with tempfile.TemporaryDirectory(prefix='muxa-popup-jump-') as tmp:
    root = Path(tmp)
    env = dict(os.environ, TERM='xterm-256color')
    for key in ('RMUX', 'RMUX_PANE', 'TMUX', 'TMUX_PANE'):
        env.pop(key, None)
    # Pin all subprocesses, including watch's rmux backend, to the candidate.
    env['PATH'] = str(Path(args.rmux).resolve().parent) + os.pathsep + env['PATH']
    base = [str(Path(args.rmux).resolve()), '-S', str(root / 'socket')]
    (root / 'config.toml').write_text('')
    children = []

    def run(*words):
        return subprocess.run(base + list(words), env=env, text=True,
                              capture_output=True, check=True, timeout=10).stdout.strip()

    def collect(seconds):
        data = b''
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            ready, _, _ = select.select([fd for _, fd in children], [], [], .03)
            for fd in ready:
                data += os.read(fd, 65536)
        return data

    def clients():
        return dict(line.split('\t') for line in
                    run('list-clients', '-F', '#{client_name}\t#{session_id}').splitlines())

    def attach(target):
        before = clients()
        pid, fd = pty.fork()
        if pid == 0:
            os.execvpe(base[0], base + ['attach', '-t', target], env)
        children.append((pid, fd))
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 140, 0, 0))
        for _ in range(50):
            collect(.1)
            added = set(clients()) - set(before)
            if added:
                return added.pop(), fd
        raise AssertionError('client did not attach')

    def location(session):
        return run('display-message', '-p', '-t', session, '#{window_id}.#{pane_id}')

    def jump(client, fd, target):
        pane = target.split('.')[-1]
        command = shlex.join([
            'env', 'MUXA_SOCKET=' + str(root / 'muxa.sock'),
            'MUXA_CONFIG=' + str(root / 'config.toml'), str(args.muxa.resolve()),
            'watch', '--popup', '--view', 'pane', '--caller-client', client,
            '--caller-pane', pane,
        ])
        run('bind-key', 's', 'run-shell', '-b', command)
        os.write(fd, b'\x02s')
        output = collect(3)
        assert b'muxa watch' in output, 'prefix+s must open watch'
        os.write(fd, b'\r')
        expected = target.split(':', 1)[1]
        for _ in range(50):
            collect(.1)
            session = clients()[client]
            if location(session) == expected:
                # Detect delayed hooks that undo a successful selection.
                collect(.5)
                assert location(clients()[client]) == expected
                return clients()[client]
        raise AssertionError(f'Enter wanted {expected}, got {location(session)} in {session}')

    try:
        run('-f', '/dev/null', 'new-session', '-d', '-s', 'origin', '/bin/sh')
        run('new-session', '-d', '-s', 'destination', '/bin/sh')
        run('new-window', '-d', '-t', 'destination', '-n', 'wanted', '/bin/sh')
        run('split-window', '-d', '-t', 'destination:wanted', '/bin/sh')
        target = run('list-panes', '-t', 'destination:wanted', '-F',
                     '#{session_id}:#{window_id}.#{pane_id}').splitlines()[-1]
        # Include the production auto-view hooks, which must not undo the
        # explicit destination or spawn a second view after the final switch.
        source = (Path(__file__).resolve().parents[1] /
                  'crates/muxa-cli/src/init/files/tmux.rs').read_text()
        hooks = source.split('pub const AUTO_VIEW_BODY: &str = r#"', 1)[1].split('"#;', 1)[0]
        hooks = hooks.replace('muxa workspace view', str(args.muxa.resolve()) + ' workspace view')
        (root / 'hooks.conf').write_text(hooks)
        run('source-file', str(root / 'hooks.conf'))
        client, fd = attach('origin')
        jump(client, fd, target)
        print('PASS popup Enter across sessions to exact split pane', flush=True)
        home = run('display-message', '-p', '-t', 'destination:0',
                   '#{session_id}:#{window_id}.#{pane_id}')
        jump(client, fd, home)
        print('PASS popup Enter within the same session', flush=True)
        run('switch-client', '-c', client, '-t', 'origin')
        other, _ = attach('destination:0')
        original = clients()[other]
        before = location(original)
        view = jump(client, fd, target)
        assert view != original, 'busy destination must get a private view'
        assert clients()[other] == original and location(original) == before
        assert run('show-options', '-v', '-t', view, 'destroy-unattached') == 'on'
        repeated = jump(client, fd, home)
        assert repeated == view, 'subsequent jump must reuse the private view'
        print('PASS private view, other-client isolation, reuse, cleanup hook', flush=True)
        run('switch-client', '-c', client, '-t', 'origin')
        collect(.5)
        assert view not in run('list-sessions', '-F', '#{session_id}').splitlines()
        print('PASS private view reaped after leaving', flush=True)
    finally:
        subprocess.run(base + ['kill-server'], env=env, capture_output=True, timeout=10)
        for pid, fd in children:
            os.close(fd)
            os.waitpid(pid, 0)
