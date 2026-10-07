"""Run one HTTP contract against both servers, with independent temporary data.
Usage: python3 -B tests/blackbox_contract.py
Builds/starts the Rust offline example; never contacts the service on port 8000.
"""
import asyncio
import hashlib
import json
import math
import subprocess
import sys
import tempfile
import threading
import urllib.request
import urllib.error
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
import fx_sim_web as web

OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def request(base, path, body=None, token=''):
    req = urllib.request.Request(base + path,
        data=None if body is None else json.dumps(body).encode(),
        headers={'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'})
    try:
        response = OPENER.open(req, timeout=10)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        return response.status, json.load(response)


def normalize(value, position_ids):
    if isinstance(value, dict):
        out = {k: normalize(v, position_ids) for k, v in value.items()
               if k not in ('time', 'version', 'token', 'err')}
        # New position IDs are opaque and may contain allocator gaps. Preserve stable
        # references across state/history, not Python's incidental lazy-load offsets.
        for key in ('position_id', 'id'):
            if key in out and (key == 'position_id' or ('sym' in out and 'entry' in out)):
                ident = out[key]
                out[key] = position_ids.setdefault(ident, len(position_ids) + 1)
        return out
    if isinstance(value, list):
        return [normalize(v, position_ids) for v in value]
    return value


def same(a, b, path=''):
    if isinstance(a, dict) and isinstance(b, dict):
        assert a.keys() == b.keys(), (path, a, b)
        for key in a:
            same(a[key], b[key], path + '/' + key)
    elif isinstance(a, (list, tuple)) and isinstance(b, (list, tuple)):
        assert len(a) == len(b), (path, a, b)
        for i, (x, y) in enumerate(zip(a, b)):
            same(x, y, path + '/' + str(i))
    elif isinstance(a, (int, float)) and isinstance(b, (int, float)):
        assert math.isclose(a, b, rel_tol=1e-10, abs_tol=1e-7), (path, a, b)
    else:
        assert a == b, (path, a, b)


def exercise(base):
    transcript = []
    position_ids = {}
    def call(path, body=None, token=''):
        result = request(base, path, body, token)
        transcript.append((path, normalize(result[1], position_ids), result[0]))
        return result
    _, auth = call('/api/auth', dict(action='register', name='contract', pwd='pw'))
    token = auth['token']
    call('/api/me', token=token)
    call('/api/state', token=token)
    call('/api/history', token=token)
    call('/api/trade', dict(side='buy', lots=.1), token)
    call('/api/state', token=token)
    call('/api/history', token=token)
    call('/api/symbol', dict(symbol='FXSIM'), token)
    call('/api/close', {}, token)
    call('/api/state', token=token)
    call('/api/history', token=token)
    call('/api/leverage', dict(leverage=200), token)
    call('/api/leverage', dict(leverage=0), token)
    call('/api/chat', dict(text=' <script>safe</script> '), token)
    call('/api/chat', token=token)
    call('/api/chat', dict(text='too quick'), token)
    call('/api/admin/users', token=token)
    _, auth = call('/api/admin/login', dict(psk='test-key'))
    admin = auth['token']
    call('/api/admin/users', token=admin)
    body = dict(name='contract', amount=123.45, request_id='same-request-0001')
    call('/api/admin/deposit', body, admin)
    call('/api/admin/deposit', body, admin)
    call('/api/admin/users', token=admin)
    call('/api/admin/symbols', dict(symbol='USDJPY', enabled=False), admin)
    call('/api/symbols')
    call('/api/symbol', dict(symbol='USDJPY'), token)
    call('/api/symbol', dict(symbol='EURJPY'), token)
    call('/api/trade', dict(side='sell', lots=.1), token)
    call('/api/state', token=token)
    call('/api/close', {}, token)
    call('/api/leaderboard')
    call('/api/admin/deposits', token=admin)
    call('/api/admin/delete', dict(name='contract', confirmation='wrong'), admin)
    call('/api/admin/delete', dict(name='contract', confirmation='contract'), admin)
    call('/api/me', token=token)
    call('/api/admin/deposits', token=admin)
    call('/api/admin/logout', {}, admin)
    call('/api/admin/users', token=admin)
    return transcript


async def exercise_ws(base):
    import websockets
    _, auth = await asyncio.to_thread(request, base, '/api/auth', dict(action='register', name='wsplayer', pwd='pw'))
    token = auth['token']
    _, auth = await asyncio.to_thread(request, base, '/api/auth', dict(action='register', name='wsother', pwd='pw'))
    other = auth['token']
    async def packet(socket, kind):
        async with asyncio.timeout(12):
            while True:
                value = json.loads(await socket.recv())
                if value.get('type') == kind:
                    return value
    transcript = []
    url = base.replace('http:', 'ws:') + '/ws?token='
    async with websockets.connect(url+token, proxy=None) as a, websockets.connect(url+token, proxy=None) as b, websockets.connect(url+other, proxy=None) as c:
        transcript.extend([await packet(a, 'state'), await packet(b, 'state'), await packet(c, 'state')])
        await asyncio.to_thread(request, base, '/api/chat', dict(text='websocket contract'), token)
        transcript.extend([await packet(a, 'chat'), await packet(c, 'chat')])
        _, auth = await asyncio.to_thread(request, base, '/api/admin/login', dict(psk='test-key'))
        await asyncio.to_thread(request, base, '/api/admin/symbols', dict(symbol='FXSIM', enabled=False), auth['token'])
        transcript.append(await packet(c, 'symbols'))
        # Read a fresh shared leaderboard packet after both users have connected.
        leaderboard = await packet(c, 'lb')
        # Tied returns have no ranking preference; compare members deterministically.
        leaderboard['rows'].sort(key=lambda row: (-row['ret'], row['name']))
        transcript.append(leaderboard)
    return normalize(transcript, {})


def main():
    subprocess.run(['cargo', 'build', '--manifest-path', str(ROOT/'Cargo.toml'),
                    '--example', 'fixture_server'], check=True)
    with tempfile.TemporaryDirectory() as folder:
        folder = Path(folder)
        key = dict(salt='ab'*16, hash=hashlib.pbkdf2_hmac('sha256', b'test-key', bytes.fromhex('ab'*16), 200000).hex())
        (folder/'admin.json').write_text(json.dumps(key))
        web.DB_FILE = str(folder/'python.db')
        web.ADMIN_KEY_FILE = str(folder/'admin.json')
        web._price = dict(USDJPY=150, EURJPY=165, EURUSD=1.1, FXSIM=1.1, BTCUSDT=60000).get
        server = web.ThreadingHTTPServer(('127.0.0.1', 0), web.Handler)
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        threading.Thread(target=web._state_push_loop, daemon=True).start()
        threading.Thread(target=web._lb_push_loop, daemon=True).start()
        process = subprocess.Popen([str(ROOT/'target/debug/examples/fixture_server'), str(folder)],
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            rust_url = process.stdout.readline().strip()
            assert rust_url.startswith('http://127.0.0.1:'), rust_url
            old = exercise(f'http://127.0.0.1:{server.server_port}')
            new = exercise(rust_url)
            same(old, new)
            print(f'PASS: {len(old)} paired HTTP operations; independent Python/Rust fixtures')
            old_ws = asyncio.run(exercise_ws(f'http://127.0.0.1:{server.server_port}'))
            new_ws = asyncio.run(exercise_ws(rust_url))
            same(old_ws, new_ws)
            print(f'PASS: {len(old_ws)} paired WS packets (state isolation, multi-tab, chat, symbols, lb)')
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill(); process.wait()
            server.shutdown(); server.server_close(); worker.join(timeout=3)


if __name__ == '__main__':
    main()
