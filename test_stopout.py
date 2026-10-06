"""Offline risk engine and atomic trade-ledger regressions; never uses live database."""
import contextlib
import io
import json
import tempfile
import threading
import unittest
import urllib.error
import urllib.request
from pathlib import Path
from unittest.mock import patch

import fx_sim as core
import fx_sim_web as web


class StopoutTests(unittest.TestCase):
    def setUp(self):
        self.stack = contextlib.ExitStack()
        self.addCleanup(self.stack.close)
        folder = self.stack.enter_context(tempfile.TemporaryDirectory())
        self.stack.enter_context(patch.object(web, 'DB_FILE', str(Path(folder) / 'risk.db')))
        for mapping in (web.USERS, web.TOKENS, web.USER_STATE, web._px_cache):
            self.stack.enter_context(patch.dict(mapping, {}, clear=True))
        self.stack.enter_context(patch.dict(web._lb_cache, {'t': 0, 'rows': []}, clear=True))
        self.stack.enter_context(patch.object(web, '_front_clients', set()))
        self.stack.enter_context(patch.object(web.time, 'time', return_value=1800000000))
        self.stack.enter_context(contextlib.redirect_stdout(io.StringIO()))
        self.token = web.register_user('risk', 'test-password')
        self.st = web.user_state('risk')
        self.acc = self.st['account']
        self.position = core.Position('long', .1, 1., web.SIMSYM)
        self.acc.balance = 50
        self.acc.positions = [self.position]
        # Selected chart must never determine held-position settlement price.
        self.st['symbol'] = 'BTCUSDT'
        web.persist_user('risk')

    def rows(self):
        return web.trade_history('risk')

    def test_exact_threshold_safe_then_next_quote_forces_without_browser(self):
        web._on_quote(web.SIMSYM, 1.)
        self.assertEqual(self.acc.positions, [self.position])
        self.assertEqual(self.rows(), [])
        web._on_quote(web.SIMSYM, .9999)
        self.assertEqual(self.acc.positions, [])
        self.assertAlmostEqual(self.acc.balance, 48.)
        trade, = self.rows()
        self.assertEqual(trade['kind'], 'stopout')
        self.assertEqual(trade['position_id'], self.position.id)
        self.assertEqual(trade['symbol'], web.SIMSYM)
        self.assertLess(trade['margin_level'], core.STOPOUT)
        self.assertAlmostEqual(trade['price'], .9998)
        self.assertAlmostEqual(trade['pnl'], -2.)
        self.assertAlmostEqual(trade['balance'], self.acc.balance)
        self.assertEqual(self.acc.pending_trades, [])

    def test_worst_first_partial_liquidation_restores_safety(self):
        eur = core.Position('long', .1, 1.1001, 'EURUSD')
        gbp = core.Position('long', .1, 1.5, 'GBPUSD')
        self.acc.balance = 270
        self.acc.positions = [eur, gbp]
        web.persist_user('risk')
        web._px_cache['EURUSD'] = (web.time.time(), 1.1)
        web._on_quote('GBPUSD', 1.48)
        self.assertEqual(self.acc.positions, [eur])
        self.assertAlmostEqual(self.acc.balance, 69)
        self.assertEqual(self.rows()[0]['position_id'], gbp.id)
        self.assertAlmostEqual(self.rows()[0]['pnl'], -201)
        self.assertGreaterEqual(self.acc.margin_level({'EURUSD': 1.1}), core.STOPOUT)

    def test_large_gap_closes_all_at_available_price_not_fake_zero_balance(self):
        self.acc.balance = 20
        second = core.Position('long', .1, 1.1, web.SIMSYM)
        self.acc.positions.append(second)
        web.persist_user('risk')
        web._on_quote(web.SIMSYM, .9)
        self.assertEqual(self.acc.positions, [])
        self.assertEqual([t['position_id'] for t in reversed(self.rows())], [second.id, self.position.id])
        self.assertAlmostEqual(self.acc.balance, -2982)
        self.assertAlmostEqual(sum(t['pnl'] for t in self.rows()) + 20, self.acc.balance)

    def test_missing_stale_invalid_quote_cannot_force_settlement(self):
        for price in (None, 0, float('nan'), float('inf'), 'bad'):
            web._on_quote(web.SIMSYM, price)
        self.assertEqual(self.acc.positions, [self.position])
        web._on_quote(web.SIMSYM, .9, web.time.time() - 31)
        self.assertEqual(self.rows(), [])
        other = core.Position('long', .1, 1., 'EURUSD')
        self.acc.positions.append(other)
        web.persist_user('risk')
        web._on_quote(web.SIMSYM, .9)
        self.assertEqual(len(self.acc.positions), 2, 'mixed account force-closed with missing quote')
        web._px_cache['EURUSD'] = (web.time.time() - 31, 1.)
        web._on_quote(web.SIMSYM, .9)
        self.assertEqual(len(self.acc.positions), 2)
        web._on_quote('EURUSD', 1.)
        self.assertEqual(self.acc.positions, [])
        self.assertEqual(len(self.rows()), 2)

    def test_stale_quote_cannot_overwrite_newer_safe_quote(self):
        web._on_quote(web.SIMSYM, 1.)
        web._on_quote(web.SIMSYM, .9, web.time.time() - 1)
        self.assertEqual(self.acc.positions, [self.position])
        self.assertEqual(web._px_cache[web.SIMSYM][1], 1.)
        self.assertEqual(self.rows(), [])

    def test_slow_http_price_cannot_overwrite_newer_ws_quote(self):
        web._px_cache['EURUSD'] = (web.time.time() - 20, .9)
        def response(symbol):
            web._px_cache[symbol] = (web.time.time() + .1, 1.1)
            return .9
        with patch.object(web, 'biquote_price', side_effect=response):
            self.assertEqual(web._price('EURUSD'), 1.1)
        self.assertEqual(web._px_cache['EURUSD'][1], 1.1)

    def test_concurrent_quotes_never_duplicate_liquidation_or_pnl(self):
        workers = [threading.Thread(target=web._on_quote, args=(web.SIMSYM, .9999)) for _ in range(8)]
        for worker in workers:
            worker.start()
        for worker in workers:
            worker.join(timeout=3)
            self.assertFalse(worker.is_alive())
        self.assertEqual(len(self.rows()), 1)
        self.assertAlmostEqual(self.acc.balance, 48.)

    def test_insert_failure_rolls_back_account_and_retries_once(self):
        conn = web._db()
        with conn:
            conn.execute("CREATE TRIGGER fail_trade BEFORE INSERT ON trades "
                         "BEGIN SELECT RAISE(ABORT, 'test ledger failure'); END")
        conn.close()
        web._on_quote(web.SIMSYM, .9999)
        self.assertEqual(self.acc.positions, [self.position])
        self.assertEqual(self.acc.balance, 50)
        self.assertEqual(self.acc.pending_trades, [])
        self.assertEqual(self.rows(), [])
        conn = web._db()
        saved = json.loads(conn.execute('SELECT account FROM users WHERE name=?', ('risk',)).fetchone()[0])
        self.assertEqual(saved['balance'], 50)
        self.assertEqual(saved['positions'][0]['id'], self.position.id)
        with conn:
            conn.execute('DROP TRIGGER fail_trade')
        conn.close()
        web._on_quote(web.SIMSYM, .9999)
        self.assertEqual(len(self.rows()), 1)
        self.assertAlmostEqual(self.acc.balance, 48.)

    def test_restart_restores_settlement_and_history_without_repeating(self):
        web._on_quote(web.SIMSYM, .9999)
        expected = self.rows()
        web.USER_STATE.clear()
        web.USERS.clear()
        web.load_users()
        restored = web.user_state('risk')
        self.assertEqual(restored['account'].positions, [])
        self.assertAlmostEqual(restored['account'].balance, 48.)
        self.assertEqual(restored['last_trade'], expected[0])
        web._on_quote(web.SIMSYM, .9999)
        self.assertEqual(self.rows(), expected)

    def test_external_quote_poll_checks_offline_held_pairs(self):
        self.acc.positions[0].symbol = 'EURUSD'
        web.persist_user('risk')
        def source(symbol):
            self.assertEqual(symbol, 'EURUSD')
            web._px_cache[symbol] = (web.time.time(), .9999)
            return .9999
        with patch.object(web, '_price', side_effect=source) as get, \
             patch.object(web.time, 'sleep', side_effect=StopIteration):
            with self.assertRaises(StopIteration):
                web._risk_price_loop()
        get.assert_called_once_with('EURUSD')
        self.assertEqual(self.acc.positions, [])
        self.assertEqual(self.rows()[0]['symbol'], 'EURUSD')

    def test_simulated_tick_settles_outside_sim_lock(self):
        with patch.object(web, '_broadcast'), patch.object(web.random, 'gauss', return_value=-.1001):
            feed = web.SimFeed()
            feed.price = 1.1
            feed.MEAN_TAU = 0
            feed.mode = ('none', 100)
            real_quote = web._on_quote
            with patch.object(web, '_on_quote', side_effect=lambda symbol, price:
                              (self.assertFalse(feed.lock._is_owned()), real_quote(symbol, price))):
                feed._tick()
        self.assertEqual(self.acc.positions, [])
        self.assertEqual(self.rows()[0]['kind'], 'stopout')

    def test_high_leverage_restores_without_changing_balances_or_history(self):
        self.acc.positions=[]
        before=self.acc.balance
        web.account_transaction('risk',lambda acc:setattr(acc,'leverage',500))
        self.assertEqual(self.rows(),[])
        web.USER_STATE.clear()
        restored=web.user_state('risk')['account']
        self.assertEqual(restored.leverage,500)
        self.assertEqual(restored.balance,before)
        self.assertEqual(restored.positions,[])
        web.account_transaction('risk',lambda acc:acc.open('long',.01,{web.SIMSYM:1.1},symbol=web.SIMSYM))
        self.assertAlmostEqual(restored.used_margin({web.SIMSYM:1.1}),2.2)
        self.assertEqual(self.rows()[0]['kind'],'open')

    def test_http_manual_history_auth_isolation_validation_and_rollback(self):
        self.acc.balance = 10000
        self.acc.positions = []
        self.st['symbol'] = web.SIMSYM
        web.persist_user('risk')
        other_token = web.register_user('other', 'test-password')
        server = web.ThreadingHTTPServer(('127.0.0.1', 0), web.Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        def request(path, body=None, token=None):
            req = urllib.request.Request('http://127.0.0.1:%d/api/%s' % (server.server_port, path),
                data=json.dumps(body).encode() if body is not None else None,
                headers={'Authorization': 'Bearer ' + (token or self.token), 'Content-Type': 'application/json'})
            try:
                response = opener.open(req, timeout=3)
            except urllib.error.HTTPError as exc:
                response = exc
            with response:
                return response.status, json.load(response)
        try:
            with patch.object(web, '_price', return_value=1.1):
                for lots in (float('nan'), float('inf'), 0, -.1):
                    self.assertEqual(request('trade', {'side': 'buy', 'lots': lots})[0], 400)
                self.assertEqual(self.rows(), [])
                for leverage in (0, -1, 1.5, True, 9007199254740992, float('inf')):
                    self.assertEqual(request('leverage', {'leverage': leverage})[0], 400)
                self.assertEqual(request('trade', {'side': 'invalid', 'lots': .1})[0], 400)
                for leverage in (200, 500, 1000):
                    self.assertEqual(request('leverage', {'leverage': leverage})[0], 200)
                    self.assertEqual(request('state')[1]['leverage'], leverage)
                self.assertEqual(request('trade', {'side': 'buy', 'lots': .1})[0], 200)
                self.assertEqual(request('leverage', {'leverage': 200})[0], 400)
                self.assertEqual(self.acc.leverage, 1000)
                self.assertAlmostEqual(request('state')[1]['used'], 11)
                self.assertEqual(request('close', {})[0], 200)
                self.assertAlmostEqual(self.acc.balance, 9998)
                rows = request('history')[1]['rows']
                self.assertEqual([t['kind'] for t in rows], ['close', 'open'])
                self.assertAlmostEqual(rows[0]['pnl'], -2)
                self.assertIsNone(rows[1]['pnl'])
                self.assertEqual(request('history', token=other_token)[1]['rows'], [])
                self.assertEqual(request('history', token='invalid')[0], 401)
                with patch.object(web, 'persist_user', side_effect=web.sqlite3.OperationalError('test disk failure')):
                    status, body = request('trade', {'side': 'buy', 'lots': .1})
                self.assertEqual(status, 503)
                self.assertFalse(body['ok'])
                self.assertEqual(self.acc.positions, [])
                self.assertAlmostEqual(self.acc.balance, 9998)
                self.assertEqual(request('history')[1]['rows'], rows)
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=3)


if __name__ == '__main__':
    unittest.main()
