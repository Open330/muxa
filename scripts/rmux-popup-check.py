#!/usr/bin/env python3
"""Drive prefix+s/S on a disposable rmux PTY with native tmux first in PATH.

Run after cargo build -p muxa-cli. Never connects to the user's rmux server.
"""
import fcntl
import os
from pathlib import Path
import pty
import select
import shutil
import struct
import subprocess
import tempfile
import termios
import time

repo = Path(__file__).resolve().parents[1]
binary = repo / 'target/debug/muxa'
rmux = shutil.which('rmux')
assert rmux and binary.exists(), 'requires rmux and cargo build -p muxa-cli'
assert Path('/usr/bin/tmux').exists(), 'requires native tmux at /usr/bin/tmux'
with tempfile.TemporaryDirectory(prefix='muxa-popup-check-') as tmp:
    env = dict(os.environ, TERM='xterm-256color')
    for key in ('RMUX', 'RMUX_PANE', 'TMUX', 'TMUX_PANE'):
        env.pop(key, None)
    base = [rmux, '-S', tmp + '/socket']
    def run(*args):
        return subprocess.run(base + list(args), env=env, capture_output=True,
                              text=True, timeout=10, check=True).stdout.strip()
    pid = fd = None
    try:
        run('-f', '/dev/null', 'new-session', '-d', '-s', 'popup-check', '/bin/sh')
        pid, fd = pty.fork()
        if pid == 0:
            os.execve(rmux, base + ['attach', '-t', 'popup-check'], env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 140, 0, 0))
        def collect(seconds):
            data = b''
            end = time.monotonic() + seconds
            while time.monotonic() < end:
                if select.select([fd], [], [], .05)[0]:
                    data += os.read(fd, 65536)
            return data
        for _ in range(50):
            if run('list-clients', '-F', '#{client_name}'):
                break
            collect(.1)
        else:
            raise AssertionError('client did not attach')
        collect(.2)
        # Simulate a server whose run-shell PATH lost rmux's tmux shim.
        # Keep rmux discoverable, but put native /usr/bin/tmux first.
        path = '/usr/bin:/bin:' + str(Path(rmux).parent)
        run('bind-key', 's', 'run-shell', '-b',
            f'PATH={path} tmux display-popup -c \'#{{client_name}}\' -E "{binary} watch"')
        os.write(fd, b'\x02s')
        assert b'muxa watch' not in collect(1), 'old binding unexpectedly worked'
        source = (repo / 'crates/muxa-cli/src/init/files/tmux.rs').read_text()
        body = source.split('pub const POPUP_BODY: &str = r#"', 1)[1].split('"#;', 1)[0]
        body = body.replace('"muxa ', f'"PATH={path} {binary} ')
        config = Path(tmp) / 'popup.conf'
        config.write_text(body)
        run('source-file', str(config))
        subprocess.run([str(binary), 'sidebar', 'show', '--host', 'rmux',
                        '--mux-socket', tmp + '/socket', '--window', '@0'],
                       env=env, check=True, capture_output=True, timeout=10)
        sidebar = run('show-options', '-wqv', '-t', '@0', '@muxa_sidebar_pane')
        run('set-option', '-g', 'mouse', 'on')
        for key in (b's', b'S'):
            run('select-pane', '-t', sidebar)
            collect(.2)
            os.write(fd, b'\x02' + key)
            output = collect(3)
            assert b'muxa watch' in output or b'muxa fleet' in output, output[-2000:]
            os.write(fd, b'q')
            collect(.5)
            assert run('show-options', '-wqv', '-t', '@0', '@muxa_sidebar_pane') == sidebar
            print(f'prefix+{key.decode()}: watch over sidebar and back, despite native tmux in PATH', flush=True)
        os.write(fd, b'\x02e')
        collect(1)
        assert not run('show-options', '-wqv', '-t', '@0', '@muxa_sidebar_pane')
        os.write(fd, b'\x02e')
        collect(1)
        sidebar = run('show-options', '-wqv', '-t', '@0', '@muxa_sidebar_pane')
        assert sidebar and run('display-message', '-p', '-t', sidebar, '#{pane_width}') == '36'
        print('prefix+e: hide/show with saved geometry', flush=True)
        run('select-pane', '-t', sidebar)
        collect(.3)
        os.write(fd, b's')
        output = collect(2)
        assert b'muxa watch' in output, ('sidebar s must expand to watch', run('capture-pane', '-p', '-t', sidebar), run('list-clients', '-F', '#{client_name}|#{pane_id}|#{session_id}'), output[-1200:])
        os.write(fd, b'q')
        collect(.5)
        os.write(fd, b'\x1b[<0;5;1M\x1b[<0;5;1m')
        assert b'muxa watch' in collect(2), 'sidebar header click must expand to watch'
        os.write(fd, b'q')
        collect(.5)
        assert run('show-options', '-wqv', '-t', '@0', '@muxa_sidebar_pane') == sidebar
        print('sidebar s/header click: watch transition preserves explorer pane', flush=True)
    finally:
        subprocess.run(base + ['kill-server'], env=env, capture_output=True, timeout=10)
        if pid:
            os.close(fd)
            os.waitpid(pid, 0)
