"""Real binary restart/CWD/WS shutdown check against a temporary database only.
Run from repo root after cargo build. Requires Python websockets for the WS probe.
"""
import asyncio
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))
import fx_sim_web as web
spec = importlib.util.spec_from_file_location('contract', Path(__file__).with_name('blackbox_contract.py'))
contract = importlib.util.module_from_spec(spec)
spec.loader.exec_module(contract)


def launch(binary, folder, port):
    log = (folder / 'server.log').open('ab')
    process = subprocess.Popen([str(binary), '--listen', f'127.0.0.1:{port}',
                               '--db', str(folder/'test.db'), '--static-dir', str(ROOT/'static'),
                               '--admin-key', str(folder/'admin.json')], cwd=folder,
                              stdin=subprocess.DEVNULL, stdout=log, stderr=log)
    log.close()
    for _ in range(100):
        if process.poll() is not None:
            raise AssertionError('server failed to start: ' + (folder/'server.log').read_text())
        try:
            with socket.create_connection(('127.0.0.1', port), timeout=.1):
                return process
        except OSError:
            time.sleep(.05)
    process.terminate()
    raise AssertionError('server did not start')


async def stop_with_socket_open(process, base, token):
    import websockets
    async with websockets.connect(base.replace('http:', 'ws:')+'/ws?token='+token, proxy=None) as ws:
        async with asyncio.timeout(8):
            while json.loads(await ws.recv()).get('type') != 'state':
                pass
        process.send_signal(signal.SIGINT)
        try:
            async with asyncio.timeout(8):
                while True:
                    await ws.recv()
        except websockets.ConnectionClosed:
            pass
    await asyncio.to_thread(process.wait, 8)
    assert process.returncode == 0, process.returncode


def main():
    binary = ROOT/'fx-sim-rust/target/debug/fx-sim-rust'
    with tempfile.TemporaryDirectory(prefix='fx-process-') as tmp:
        folder = Path(tmp)
        salt = bytes.fromhex('cd'*16)
        (folder/'admin.json').write_text(json.dumps(dict(salt=salt.hex(), hash=hashlib.pbkdf2_hmac('sha256', b'test-key', salt, 200000).hex())))
        os.chmod(folder/'admin.json', 0o600)
        # Seed actual recorded historical ticks through the Python writer, before Rust starts.
        sim = web.SimFeed()
        old = int(time.time())//60*60-180
        for sec in sim.bars:
            sim.bars[sec].clear()
            sim.bars[sec].append([old//sec*sec, 1.1, 1.1, 1.1, 1.1])
        sim._history_db = sqlite3.connect(folder/'test.db')
        sim._history_db.execute('CREATE TABLE sim_bars(sec INTEGER,t INTEGER,o REAL,h REAL,l REAL,c REAL,PRIMARY KEY(sec,t))')
        sim._record(.9, old+.2)
        sim._record(1.09, old+.4)
        sim._record(1.1, old+60)
        sim._history_db.close()
        with socket.socket() as listener:
            listener.bind(('127.0.0.1',0))
            port=listener.getsockname()[1]
        base=f'http://127.0.0.1:{port}'
        process = launch(binary, folder, port)
        try:
            request=lambda path, body=None, token='': contract.request(base,path,body,token)
            def ok(path, body=None, token=''):
                status, value=request(path,body,token)
                assert status==200 and value['ok'], (path,status,value)
                return value
            token=ok('/api/auth',dict(action='register',name='restart',pwd='pw'))['token']
            ok('/api/symbol',dict(symbol='FXSIM'),token)
            ok('/api/trade',dict(side='buy',lots=.01),token)
            ok('/api/chat',dict(text='persists across restart'),token)
            admin=ok('/api/admin/login',dict(psk='test-key'))['token']
            ok('/api/admin/deposit',dict(name='restart',amount=123.45,request_id='restart-00000001'),admin)
            ok('/api/admin/symbols',dict(symbol='EURUSD',enabled=False),admin)
            before=ok('/api/state',token=token)
            bars=ok('/api/klines?symbol=FXSIM&interval=1m&limit=1000')['klines'][:-1]
            assert bars and any(row[3]==.9 for row in bars)
            asyncio.run(stop_with_socket_open(process,base,token))
            with socket.socket() as check:
                assert check.connect_ex(('127.0.0.1',port))!=0, 'listener not released'
            process=launch(binary,folder,port)
            after=ok('/api/state',token=token)
            assert before['balance']==after['balance']
            for key in ('id','side','lots','sym','entry'):
                assert before['positions'][0][key]==after['positions'][0][key]
            assert len(ok('/api/history',token=token)['rows'])==1
            assert ok('/api/chat',token=token)['rows'][0]['text']=='persists across restart'
            assert not next(row for row in ok('/api/symbols')['rows'] if row['symbol']=='EURUSD')['enabled']
            assert request('/api/admin/users',token=admin)[0]==401
            admin=ok('/api/admin/login',dict(psk='test-key'))['token']
            assert len(ok('/api/admin/deposits',token=admin)['rows'])==2
            current={row[0]:row for row in ok('/api/klines?symbol=FXSIM&interval=1m&limit=1000')['klines']}
            assert all(current[row[0]]==row for row in bars)
            asyncio.run(stop_with_socket_open(process,base,token))
            # Rust schema and persisted portfolio remain readable by the Python reference.
            web.DB_FILE=str(folder/'test.db')
            web.load_users()
            assert web.user_by_token(token)=='restart'
            restored=web.user_state('restart')['account']
            assert restored.positions[0].symbol=='FXSIM' and restored.balance==after['balance']
            print('PASS: actual binary start from foreign CWD; SIGINT with WS; port release; restart preserves account/token/position/history/chat/deposit/audit/settings/closed OHLC; Python reload')
        finally:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=8)


if __name__=='__main__':
    main()
