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
        body = body.replace('"muxa watch', f'"PATH={path} {binary} watch')
        config = Path(tmp) / 'popup.conf'
        config.write_text(body)
        run('source-file', str(config))
        for key in (b's', b'S'):
            os.write(fd, b'\x02' + key)
            output = collect(3)
            assert b'muxa watch' in output or b'muxa fleet' in output, output[-2000:]
            os.write(fd, b'q')
            collect(.5)
            print(f'prefix+{key.decode()}: watch rendered despite native tmux in PATH', flush=True)
    finally:
        subprocess.run(base + ['kill-server'], env=env, capture_output=True, timeout=10)
        if pid:
            os.close(fd)
            os.waitpid(pid, 0)
