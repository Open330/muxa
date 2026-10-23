#!/usr/bin/env python3
"""Exercise Global Ask through a private daemon and a fake, unbilled provider.

Run after cargo build -p muxa-cli -p muxad. No live daemon/config is modified.
"""
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time

repo = Path(__file__).resolve().parents[1]
with tempfile.TemporaryDirectory(prefix='muxa-ask-stream-') as directory:
    root = Path(directory)
    endpoint = root / 'daemon.sock'
    provider = root / 'codex'
    provider.write_text('''#!/bin/sh
printf '%s\\n' '{"type":"thread.started","thread_id":"test-thread"}'
printf '%s\\n' '{"type":"item.completed","item":{"type":"agent_message","text":"Checking"}}'
while ! [ -f "$(dirname "$0")/continue" ]; do sleep 0.01; done
printf '%s\\n' '{"type":"item.completed","item":{"type":"agent_message","text":"Done"}}'
''')
    provider.chmod(0o700)
    config = root / 'config.toml'
    config.write_text('[ask]\nenabled = true\nagent = "codex"\ncwd = '
                      + json.dumps(str(root)) + '\n[ask.providers.codex]\nexecutable = '
                      + json.dumps(str(provider)) + '\n')
    history = root / 'data' / 'muxa' / 'ask.json'
    history.parent.mkdir(parents=True)
    history.write_text(json.dumps({'entries': [{
        'id': 'archived', 'conversation_id': 'archived-conversation', 'prompt': 'old',
        'answer': 'old history ' * 10000, 'status': 'answered', 'agent': 'claude',
        'cwd': str(root), 'asked_at': '2026-01-01T00:00:00Z', 'answered_at': '2026-01-01T00:00:01Z'
    }]}))
    env = dict(os.environ, XDG_CONFIG_HOME=str(root / 'config'),
               XDG_DATA_HOME=str(root / 'data'), XDG_STATE_HOME=str(root / 'state'),
               MUXA_HOST='rmux', MUXA_HOSTS='rmux',
               RMUX=str(root / 'absent-mux') + ',1,0')
    for key in ('TMUX', 'TMUX_PANE', 'RMUX_PANE'):
        env.pop(key, None)
    cli = [str(repo / 'target/debug/muxa'), '--socket', str(endpoint), '--config', str(config)]

    def call(kind, **filters):
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(3)
            connection.connect(str(endpoint))
            connection.sendall(json.dumps({'kind': kind, **filters}).encode() + b'\n')
            with connection.makefile('rb') as reader:
                response = json.loads(reader.readline())
            assert response.get('ok'), response
            return response

    with (root / 'daemon.log').open('wb') as log:
        daemon = subprocess.Popen([str(repo / 'target/debug/muxad'), '--socket', str(endpoint),
                                   '--config', str(config), '--no-dashboard'],
                                  env=env, stdout=log, stderr=log)
        ask = None
        try:
            deadline = time.monotonic() + 10
            while not endpoint.exists():
                assert daemon.poll() is None, (root / 'daemon.log').read_text()
                assert time.monotonic() < deadline, 'daemon did not start'
                time.sleep(.05)
            started = time.monotonic()
            ask = subprocess.Popen(cli + ['ask', '--json', 'hello'], env=env,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            while True:
                entries = call('ask_list').get('ask_entries', [])
                if entries and entries[-1]['answer'] == 'Checking':
                    entry = entries[-1]
                    assert entry['status'] == 'running' and entry['answered_at'] is None
                    assert ask.poll() is None, 'CLI finished on progress'
                    assert entry['prompt'] == 'hello'
                    one = call('ask_list', id=entry['id'])['ask_entries']
                    conversation = call('ask_list', conversation_id=entry['conversation_id'])['ask_entries']
                    assert len(one) == len(conversation) == 1
                    assert one[0]['id'] == conversation[0]['id'] == entry['id']
                    assert call('ask_list', id='missing')['ask_entries'] == []
                    assert len(entries) == 2, 'unfiltered history remains backward compatible'
                    assert len(json.dumps(one)) < len(json.dumps(entries)) / 20
                    break
                assert time.monotonic() - started < 10, 'no provisional response'
                time.sleep(.05)
            print(f'PASS: provisional response after {time.monotonic() - started:.2f}s; turn still running')
            print('PASS: single-entry and conversation filters exclude unrelated history')
            (root / 'continue').touch()
            stdout, stderr = ask.communicate(timeout=10)
            assert ask.returncode == 0, stderr.decode()
            final = json.loads(stdout)
            assert final['status'] == 'answered' and final['answer'] == 'Done', final
            assert final['agent_session_id'] == 'test-thread'
            assert stderr == b'', stderr
            print('PASS: CLI subscription delivers final-only JSON and preserves the provider session')
        finally:
            (root / 'continue').touch()
            if ask is not None and ask.poll() is None:
                ask.kill()
                ask.wait(timeout=5)
            if daemon.poll() is None:
                daemon.terminate()
                daemon.wait(timeout=10)
