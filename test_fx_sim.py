"""Offline cross-symbol regressions. Run: /usr/bin/python3 -B -m unittest -v"""
import contextlib
import io
import json
import math
import shutil
import subprocess
import tempfile
import threading
import unittest
import urllib.error
import urllib.request
from pathlib import Path
from unittest.mock import patch

import fx_sim as core
import fx_sim_web as web


class CrossSymbolTests(unittest.TestCase):
    def setUp(self):
        self.output = contextlib.redirect_stdout(io.StringIO())
        self.output.__enter__()
        self.addCleanup(self.output.__exit__, None, None, None)
        self.account = core.Account(10000, 100)
        self.eur = core.Position('long', .1, 1.1001, 'EURUSD')
        self.btc = core.Position('short', .01, 60000.0001, 'BTCUSDT')
        self.account.positions = [self.eur, self.btc]
        self.prices = {'EURUSD': 1.11, 'BTCUSDT': 59999.0}

    def test_single_symbol_api_unchanged(self):
        account = core.Account(10000, 100)
        account.open('long', .1, 1.1)
        account.close(None, 1.1)
        self.assertAlmostEqual(account.balance, 9998)

    def test_metrics_use_each_position_symbol(self):
        self.assertAlmostEqual(self.account.equity(self.prices), 11099.1)
        self.assertAlmostEqual(self.account.used_margin(self.prices), 600101.0)
        self.assertAlmostEqual(self.account.free_margin(self.prices), -589001.9)
        self.assertAlmostEqual(self.account.margin_level(self.prices), 11099.1 / 600101)

    def test_open_checks_existing_positions_at_their_own_prices(self):
        self.account.positions = [self.eur]
        prices = {'EURUSD': 1.11, 'BTCUSDT': 60000.0}
        # Wrongly valuing EURUSD at BTC price creates enough fictitious equity to open this.
        with self.assertRaises(ValueError):
            self.account.open('long', .01, prices, symbol='BTCUSDT')
        self.assertEqual(self.account.positions, [self.eur])
        self.account.balance = 1000000
        self.account.open('long', .01, prices, symbol='BTCUSDT')
        self.assertAlmostEqual(self.account.positions[-1].entry, 60000.0001)

    def test_close_one_only_requires_its_own_quote(self):
        self.account.close(self.eur.id, {'EURUSD': 1.11})
        self.assertAlmostEqual(self.account.balance, 10098)
        self.assertEqual(self.account.positions, [self.btc])

    def test_close_all_and_restore_preserve_symbol_and_pnl(self):
        restored = web.acc_from_dict(web.acc_to_dict(self.account))
        self.assertEqual([p.symbol for p in restored.positions], ['EURUSD', 'BTCUSDT'])
        self.assertEqual([p.id for p in restored.positions], [self.eur.id, self.btc.id])
        restored.close(None, self.prices)
        self.assertAlmostEqual(restored.balance, 11098)
        self.assertEqual(restored.positions, [])

    def test_missing_quote_does_not_partially_close(self):
        for prices in ({'EURUSD': 1.11}, {'EURUSD': 1.11, 'BTCUSDT': None},
                       {'EURUSD': 1.11, 'BTCUSDT': float('nan')},
                       {'EURUSD': 1.11, 'BTCUSDT': 0}):
            with self.subTest(prices=prices), self.assertRaises(ValueError):
                self.account.close(None, prices)
            self.assertEqual(self.account.balance, 10000)
            self.assertEqual(self.account.positions, [self.eur, self.btc])

    def test_stopout_uses_own_quotes_and_closes_worst_first(self):
        self.account.balance = 270
        self.account.positions = [self.eur, core.Position('long', .1, 1.5, 'GBPUSD')]
        self.assertTrue(self.account.check_stopout({'EURUSD': 1.1, 'GBPUSD': 1.48}))
        self.assertEqual(self.account.positions, [self.eur])
        self.assertAlmostEqual(self.account.balance, 69)

    def test_unknown_legacy_symbol_is_not_guessed(self):
        data = web.acc_to_dict(self.account)
        for position in data['positions']:
            position.pop('symbol', None)
        restored = web.acc_from_dict(data)
        self.assertTrue(all(p.symbol == '' for p in restored.positions))
        with self.assertRaises(ValueError):
            restored.close(None, self.prices)
        self.assertEqual(restored.balance, 10000)

    def test_legacy_state_file_roundtrip(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(web, 'STATE_FILE', str(Path(directory) / 'state.json')), \
                 patch.object(web, 'STATE', {'account': self.account, 'symbol': 'USDJPY'}):
                web.save_state()
                restored = web.load_state()
        self.assertEqual(restored['symbol'], 'USDJPY')
        self.assertEqual([p.symbol for p in restored['account'].positions], ['EURUSD', 'BTCUSDT'])

    def test_web_routes_persistence_and_held_symbol_subscription(self):
        # Real HTTP handlers and SQLite, isolated from running server and user data.
        with tempfile.TemporaryDirectory() as directory, \
             patch.object(web, 'DB_FILE', str(Path(directory) / 'test.db')), \
             patch.dict(web.USERS, {}, clear=True), patch.dict(web.TOKENS, {}, clear=True), \
             patch.dict(web.USER_STATE, {}, clear=True), \
             patch.dict(web._lb_cache, {'t': 0, 'rows': []}, clear=True), \
             patch.object(web, '_price', side_effect=lambda symbol: self.prices.get(symbol)):
            token = web.register_user('test', 'test-password')
            self.assertEqual(web.user_state('test')['symbol'], 'USDJPY')
            web.user_state('test')['symbol'] = 'EURUSD'
            server = web.ThreadingHTTPServer(('127.0.0.1', 0), web.Handler)
            worker = threading.Thread(target=server.serve_forever, daemon=True)
            worker.start()
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

            def request(path, body=None):
                req = urllib.request.Request(
                    'http://127.0.0.1:%d/api/%s' % (server.server_port, path),
                    data=json.dumps(body).encode() if body is not None else None,
                    headers={'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'})
                try:
                    response = opener.open(req, timeout=3)
                except urllib.error.HTTPError as exc:
                    response = exc
                with response:
                    return response.status, json.load(response)

            try:
                self.prices['EURUSD'] = 1.1
                self.assertEqual(request('trade', {'side': 'buy', 'lots': .1})[0], 200)
                eur_id = web.user_state('test')['account'].positions[0].id
                self.assertEqual(request('symbol', {'symbol': 'BTCUSDT'})[0], 200)
                self.assertTrue({'EURUSD', 'BTCUSDT'} <= web.active_symbols())
                # No fake windfall available to pay BTC margin.
                self.assertEqual(request('trade', {'side': 'buy', 'lots': .01})[0], 400)
                # Reload from SQLite with BTC selected; EUR position must remain EUR.
                web.USER_STATE.clear()
                web.USERS.clear()
                web.load_users()
                self.assertEqual(web.user_state('test')['account'].positions[0].symbol, 'EURUSD')
                self.prices['EURUSD'] = 1.11
                status, state = request('state')
                self.assertEqual(status, 200)
                self.assertAlmostEqual(state['equity'], 10099)
                self.assertAlmostEqual(state['positions'][0]['now'], 1.11)
                self.assertAlmostEqual(request('leaderboard')[1]['rows'][0]['equity'], 10099)
                # Unavailable held quote must never fall back to selected BTC quote.
                self.prices['EURUSD'] = None
                self.assertEqual(request('state')[0], 502)
                self.assertEqual(request('close', {'id': eur_id})[0], 400)
                self.assertEqual(len(web.user_state('test')['account'].positions), 1)
                # Unavailable selected quote must not prevent closing available EUR.
                self.prices.update(EURUSD=1.11, BTCUSDT=None)
                self.assertEqual(request('close', {'id': eur_id})[0], 200)
                self.assertAlmostEqual(web.user_state('test')['account'].balance, 10098)
                # Bulk close across two symbols uses each quote.
                web.user_state('test')['account'].positions = [self.eur, self.btc]
                web.user_state('test')['symbol'] = 'USDJPY'
                self.prices['BTCUSDT'] = 59999.0
                self.assertEqual(request('close', {})[0], 200)
                self.assertAlmostEqual(web.user_state('test')['account'].balance, 11196)
            finally:
                server.shutdown()
                server.server_close()
                worker.join(timeout=3)


class ChartTests(unittest.TestCase):
    @unittest.skipUnless(shutil.which('node'), 'Node required for browser-script regressions')
    def test_switch_resets_scale_without_resetting_periodic_zoom(self):
        script = web.INDEX.split('<script>')[1].split('</script>')[0]
        result = subprocess.run(['node', str(Path(__file__).with_name('test_chart.js'))],
                                input=script, text=True, capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


class SimFeedTests(unittest.TestCase):
    def test_selectable_sim_symbol(self):
        options = next(line for line in web.INDEX.splitlines() if 'symSel.add(new Option' in line)
        self.assertIn('FXSIM', options.split("'")[1].split())
        self.assertIn('模拟币', options)

    def test_candles_are_real_ordered_milliseconds_and_roll_over(self):
        for sec in (60, 300, 900, 1800, 3600, 14400, 86400, 604800):
            with self.subTest(sec=sec), patch.object(web.time, 'time', return_value=1800000000):
                feed = web.SimFeed()
                self.assertEqual(len(feed.klines(sec, 500)), 1, 'invented pre-start history')
                for i, px in enumerate((1.1, 1.12, 1.05)):
                    feed._record(px, 1800000000 + sec * i)
                bars = feed.klines(sec, 500)
                self.assertEqual(len(bars), 3)
                times = [bar[0] for bar in bars]
                self.assertTrue(all(b - a == sec * 1000 for a, b in zip(times, times[1:])))
                self.assertTrue(all(math.isfinite(value) and value > 0 for bar in bars for value in bar[1:]))
                self.assertTrue(all(low <= min(open_, close) <= max(open_, close) <= high
                                    for _, open_, high, low, close in bars))
                self.assertEqual(feed.klines(sec, 2), bars[-2:])

    def test_crash_rebound_and_refresh_preserve_closed_wick(self):
        t = 1800000000
        with patch.object(web.time, 'time', return_value=t):
            feed = web.SimFeed()
        feed._record(.9, t + .2)
        feed._record(1.09, t + .4)
        expected = [t * 1000, 1.1, 1.1, .9, 1.09]
        self.assertEqual(feed.klines(60, 500), [expected])
        feed._record(1.11, t + 60)
        for i in range(1, 5):
            feed._record(1.11 + i * .001, t + 60 + i)
            self.assertEqual(feed.klines(60, 500)[0], expected)
        bars = feed.klines(60, 500)
        bars[0][3] = 999
        self.assertEqual(feed.klines(60, 500)[0], expected, 'API exposed mutable internal bars')

    def test_history_persists_across_restart_without_touching_users(self):
        t = 1800000000
        with tempfile.TemporaryDirectory() as directory, patch.object(web.time, 'time', return_value=t):
            path = str(Path(directory) / 'history.db')
            with web.sqlite3.connect(path) as db:
                db.execute('CREATE TABLE users (marker TEXT)')
                db.execute('INSERT INTO users VALUES (?)', ('unchanged',))
            feed = web.SimFeed()
            feed.load_history(path)
            self.addCleanup(feed._history_db.close)
            feed._record(.9, t + .2)
            feed._record(1.09, t + .4)
            feed._record(1.11, t + 60)
            expected = feed.klines(60, 500)
            restored = web.SimFeed()
            restored.load_history(path)
            self.addCleanup(restored._history_db.close)
            self.assertEqual(restored.klines(60, 500), expected)
            self.assertEqual(restored.price_now(), 1.11)
            restored._record(1.12, t + 61)
            self.assertEqual(restored.klines(60, 500)[0], expected[0])
            self.assertEqual(restored._history_db.execute('SELECT * FROM users').fetchall(), [('unchanged',)])

    def test_history_bounded_and_no_fake_gap_bars(self):
        t = 1800000000
        with patch.object(web.time, 'time', return_value=t):
            feed = web.SimFeed()
        for i in range(1005):
            feed._record(1.1, t + i * 60)
        self.assertEqual(len(feed.klines(60, 2000)), 1000)
        previous = feed.klines(60, 1)[0][0]
        feed._record(1.11, t + 1010 * 60)
        self.assertEqual(feed.klines(60, 2)[0][0], previous)
        self.assertEqual(feed.klines(60, 2)[1][0] - previous, 6 * 60000)

    def test_ws_push_contains_intrasecond_extrema_and_final_previous_bar(self):
        t = 1800000000
        with patch.object(web.time, 'time', return_value=t), patch.object(web, '_broadcast') as send:
            feed = web.SimFeed()
            feed.MEAN_TAU = 0
            feed.mode = ('none', 100)
            for dt, shock in ((.1, -.2), (.3, .19), (60.1, .01)):
                with patch.object(web.time, 'time', return_value=t + dt), \
                     patch.object(web.random, 'gauss', return_value=shock):
                    feed._tick()
            self.assertEqual(send.call_count, 3, 'integer-second throttle drops intrasecond quotes')
            packets = [c.args[0] for c in send.call_args_list]
            self.assertEqual(packets[1]['klines']['1m'][-1][3], .9)
            self.assertEqual(packets[2]['klines']['1m'][0], [t*1000, 1.1, 1.1, .9, 1.09])

    def test_sim_http_intervals_and_trading(self):
        with tempfile.TemporaryDirectory() as directory, \
             patch.object(web, 'DB_FILE', str(Path(directory) / 'sim.db')), \
             patch.dict(web.USERS, {}, clear=True), patch.dict(web.TOKENS, {}, clear=True), \
             patch.dict(web.USER_STATE, {}, clear=True), patch.object(web, 'SIM', web.SimFeed()):
            token = web.register_user('sim-test', 'password')
            server = web.ThreadingHTTPServer(('127.0.0.1', 0), web.Handler)
            worker = threading.Thread(target=server.serve_forever, daemon=True)
            worker.start()
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

            def request(path, body=None):
                req = urllib.request.Request(
                    'http://127.0.0.1:%d/api/%s' % (server.server_port, path),
                    data=json.dumps(body).encode() if body is not None else None,
                    headers={'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'})
                with opener.open(req, timeout=3) as response:
                    return json.load(response)

            try:
                self.assertTrue(request('symbol', {'symbol': 'FXSIM'})['ok'])
                for interval, sec in {'1m': 60, '5m': 300, '15m': 900, '30m': 1800,
                                      '1h': 3600, '4h': 14400, '1d': 86400, '1w': 604800}.items():
                    with self.subTest(interval=interval):
                        data = request('klines?symbol=FXSIM&interval=%s&limit=3' % interval)
                        self.assertTrue(data['ok'])
                        bars = data['klines']
                        self.assertEqual(len(bars), 1, 'unobserved historical bars fabricated')
                        self.assertEqual(bars[0][0] % (sec * 1000), 0)
                with contextlib.redirect_stdout(io.StringIO()):
                    self.assertTrue(request('trade', {'side': 'buy', 'lots': .1})['ok'])
                    state = request('state')
                    self.assertEqual(state['symbol'], 'FXSIM')
                    self.assertEqual(state['positions'][0]['sym'], 'FXSIM')
                    self.assertTrue(request('close', {})['ok'])
                self.assertAlmostEqual(web.user_state('sim-test')['account'].balance, 9998)
                web.USER_STATE.clear()
                web.USERS.clear()
                web.load_users()
                self.assertEqual(web.user_state('sim-test')['symbol'], 'FXSIM')
            finally:
                server.shutdown()
                server.server_close()
                worker.join(timeout=3)


if __name__ == '__main__':
    unittest.main()
