#!/usr/bin/env python3
"""Isolated pane-sharing browser smoke test. Requires cryptography and Playwright."""
import os, sys, json, pathlib, tempfile, subprocess, shutil, time, socket, threading, base64, hashlib, signal, atexit
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
from urllib.parse import urlparse, parse_qs, urlencode
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives import serialization
REPO = pathlib.Path(__file__).resolve().parent.parent
root = pathlib.Path(tempfile.mkdtemp(prefix='muxa-share-browser-'))
# Windows has no tmux: a private rmux label stands in for the private tmux socket.
WINDOWS = os.name == 'nt'

def port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]

def b64(b):
    return base64.urlsafe_b64encode(b).rstrip(b'=').decode()
webport = port()
oidcport = port()
origin = f'http://127.0.0.1:{webport}'
issuer = f'http://127.0.0.1:{oidcport}'
key = Ed25519PrivateKey.generate()
codes = {}

class Provider(BaseHTTPRequestHandler):

    def log_message(self, *a):
        pass

    def reply(self, obj):
        data = json.dumps(obj).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        path = urlparse(self.path)
        if path.path == '/.well-known/openid-configuration':
            self.reply(dict(issuer=issuer, authorization_endpoint=issuer + '/authorize', token_endpoint=issuer + '/token', jwks_uri=issuer + '/jwks', response_types_supported=['code'], subject_types_supported=['public'], id_token_signing_alg_values_supported=['EdDSA']))
        elif path.path == '/jwks':
            self.reply({'keys': [dict(kty='OKP', crv='Ed25519', kid='test', use='sig', alg='EdDSA', x=b64(key.public_key().public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)))]})
        elif path.path == '/authorize':
            q = {k: v[0] for k, v in parse_qs(path.query).items()}
            code = os.urandom(16).hex()
            codes[code] = q
            self.send_response(302)
            self.send_header('Location', q['redirect_uri'] + '?' + urlencode({'code': code, 'state': q['state']}))
            self.end_headers()
        else:
            self.send_error(404)

    def do_POST(self):
        if self.path == '/test/add-pane':
            subprocess.run(mux + ['split-window', '-d', '-t', 'share-test:0', pane_cmd], env=env, check=True)
            self.reply({'ok': True})
            return
        if self.path == '/test/restart':
            restart_daemon()
            self.reply({'ok': True})
            return
        q = {k: v[0] for k, v in parse_qs(self.rfile.read(int(self.headers['Content-Length'])).decode()).items()}
        flow = codes.pop(q['code'])
        assert b64(hashlib.sha256(q['code_verifier'].encode()).digest()) == flow['code_challenge']
        now = int(time.time())
        claims = dict(iss=issuer, sub='browser-guest', aud='muxa', iat=now, exp=now + 300, nonce=flow['nonce'], email='guest@example.com', email_verified=True)
        signing = (b64(json.dumps(dict(alg='EdDSA', kid='test')).encode()) + '.' + b64(json.dumps(claims).encode())).encode()
        jwt = signing.decode() + '.' + b64(key.sign(signing))
        self.reply(dict(access_token='test', token_type='Bearer', id_token=jwt))
server = ThreadingHTTPServer(('127.0.0.1', oidcport), Provider)
threading.Thread(target=server.serve_forever, daemon=True).start()
env = dict(os.environ)
for name in list(env):
    if name.startswith('MUXA_') and name not in {'MUXA_PLAYWRIGHT_PACKAGE', 'MUXA_TEST_CHROMIUM'}:
        env.pop(name)
env['NO_PROXY'] = '127.0.0.1,localhost'
for k in ['TMUX', 'TMUX_PANE', 'MUXA_CONFIG', 'MUXA_SOCKET', 'MUXA_TMUX_SOCKET', 'RMUX', 'RMUX_PANE']:
    env.pop(k, None)
for k in ['XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'XDG_CACHE_HOME', 'XDG_STATE_HOME', 'XDG_RUNTIME_DIR', 'TMUX_TMPDIR']:
    d = root / k
    d.mkdir(mode=448)
    env[k] = str(d)
sock = root / 'pane.sock'
if WINDOWS:
    env.pop('SHELL', None)  # rmux would otherwise run pane commands through Git Bash
    env.update(MUXA_HOST='rmux', MUXA_HOSTS='rmux')
    mux = ['rmux', '-L', os.environ.get('MUXA_TEST_RMUX_LABEL', 'muxa-share-smoke')]
    (root / 'echo.py').write_text('import sys\nfor line in sys.stdin:\n    print(line, end="", flush=True)\n')
    pane_cmd = f"& '{sys.executable}' -u '{root / 'echo.py'}'"
    shell = shutil.which('pwsh') or shutil.which('powershell')
    (root / 'rmux.conf').write_text('set -g default-shell ' + json.dumps(shell) + '\n')
    mux_start = mux + ['-f', str(root / 'rmux.conf')]
else:
    env.update(MUXA_HOST='tmux', MUXA_HOSTS='tmux')
    env['MUXA_TMUX_SOCKET'] = str(sock)
    mux = ['tmux', '-S', str(sock)]
    pane_cmd = 'cat'
    mux_start = ['tmux', '-f', '/dev/null', '-S', str(sock)]
exe = '.exe' if WINDOWS else ''
p = None
node = None

def stop_daemon():
    if WINDOWS:
        p.terminate()
    else:
        p.send_signal(signal.SIGTERM)

def cleanup():
    if node is not None:
        if WINDOWS:
            subprocess.run(['taskkill', '/T', '/F', '/PID', str(node.pid)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        else:
            try:
                os.killpg(node.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        node.wait()
    if p is not None and p.poll() is None:
        stop_daemon()
        try:
            p.wait(timeout=15)
        except subprocess.TimeoutExpired:
            p.kill()
            p.wait()
    server.shutdown()
    subprocess.run(mux + ['kill-server'], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
atexit.register(cleanup)
subprocess.run(mux_start + ['new-session', '-d', '-s', 'share-test', pane_cmd], env=env, check=True)
subprocess.run(mux + ['split-window', '-d', '-t', 'share-test:0', pane_cmd], env=env, check=True)
if WINDOWS:
    env['RMUX'] = subprocess.run(mux + ['display-message', '-p', '-t', 'share-test', '#{socket_path},#{pid},#{session_id}'], env=env, check=True, capture_output=True, text=True).stdout.strip()
config = root / 'config.toml'
config.write_text(f'[discovery]\nenabled=false\n[reconciler]\nenabled=false\n[dashboard]\nenabled=true\nbind="127.0.0.1:{webport}"\nauth="token"\ntoken="operator"\n[dashboard.sharing]\npublic_url="{origin}"\nissuer_url="{issuer}"\nclient_id="muxa"\n')
shutil.copy2(os.environ.get('MUXA_TEST_MUXAD', str(REPO / ('target/debug/muxad' + exe))), root / ('muxad' + exe))
log = (root / 'daemon.log').open('w')
p = subprocess.Popen([str(root / ('muxad' + exe)), '--config', str(config), '--socket', str(root / 'ipc.sock')], env=env, cwd=root, stdout=log, stderr=log)

def restart_daemon():
    global p
    stop_daemon()
    p.wait(timeout=15)
    p = subprocess.Popen([str(root / ('muxad' + exe)), '--config', str(config), '--socket', str(root / 'ipc.sock')], env=env, cwd=root, stdout=log, stderr=log)
    for _ in range(200):
        assert p.poll() is None, (root / 'daemon.log').read_text()
        try:
            with socket.create_connection(('127.0.0.1', webport), timeout=0.1):
                return
        except OSError:
            time.sleep(0.05)
    raise AssertionError('restart not ready')
try:
    for _ in range(200):
        assert p.poll() is None, (root / 'daemon.log').read_text()
        try:
            with socket.create_connection(('127.0.0.1', webport), timeout=0.1):
                break
        except OSError:
            time.sleep(0.05)
    env['MUXA_TEST_PANE_PREFIX'] = 'rmux:' if WINDOWS else ''
    group = dict(creationflags=subprocess.CREATE_NEW_PROCESS_GROUP) if WINDOWS else dict(start_new_session=True)
    node = subprocess.Popen(['node', str(pathlib.Path(__file__).resolve().parent / 'dashboard-sharing-smoke.cjs'), origin, str(root), issuer], env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, **group)
    stdout, stderr = node.communicate(timeout=120)
    print(stdout)
    print(stderr)
    assert node.returncode == 0
    print('ARTIFACTS', root)
finally:
    cleanup()
    atexit.unregister(cleanup)
    log.close()
