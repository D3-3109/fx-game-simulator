"""Isolated HTTP regressions for chat and administrator funding."""
import contextlib
import json
import hashlib
import base64
from collections import deque
import tempfile
import threading
import unittest
import urllib.error
import urllib.request
from pathlib import Path
from unittest.mock import patch, Mock

import fx_sim_web as web


class WebPanelTests(unittest.TestCase):
    def setUp(self):
        self.stack=contextlib.ExitStack();self.addCleanup(self.stack.close)
        folder=self.stack.enter_context(tempfile.TemporaryDirectory())
        self.stack.enter_context(patch.object(web,'DB_FILE',str(Path(folder)/'panels.db')))
        for data in (web.USERS,web.TOKENS,web.USER_STATE,web._px_cache):
            self.stack.enter_context(patch.dict(data,{},clear=True))
        self.stack.enter_context(patch.dict(web._lb_cache,{'t':0,'rows':[]},clear=True))
        self.stack.enter_context(patch.dict(web.ADMIN_SESSIONS,{},clear=True))
        self.stack.enter_context(patch.object(web,'ADMIN_ATTEMPTS',deque(maxlen=10)))
        key=Path(folder)/'admin-key.json';salt='ab'*16
        key.write_text(json.dumps({'salt':salt,'hash':hashlib.pbkdf2_hmac('sha256',b'test-key',bytes.fromhex(salt),200000).hex()}))
        self.stack.enter_context(patch.object(web,'ADMIN_KEY_FILE',str(key)))
        self.token=web.register_user('player','test-password')
        self.server=web.ThreadingHTTPServer(('127.0.0.1',0),web.Handler)
        self.thread=threading.Thread(target=self.server.serve_forever,daemon=True);self.thread.start()
        self.addCleanup(self.stop_server)
        self.opener=urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def stop_server(self):
        self.server.shutdown();self.server.server_close();self.thread.join(timeout=3)

    def request(self,path,body=None,token=None):
        req=urllib.request.Request('http://127.0.0.1:%d%s'%(self.server.server_port,path),
            data=json.dumps(body).encode() if body is not None else None,
            headers={'Authorization':'Bearer '+(token or self.token),'Content-Type':'application/json'})
        try:r=self.opener.open(req,timeout=3)
        except urllib.error.HTTPError as e:r=e
        with r:
            raw=r.read()
            return r.status,json.loads(raw) if r.headers.get('Content-Type','').startswith('application/json') else raw.decode()

    def test_versioned_local_v5_fallback_sri_cache_and_v4_compatibility(self):
        status,script=self.request('/assets/lightweight-charts-5.2.1.js',token='invalid')
        self.assertEqual(status,200)
        self.assertIn('Lightweight Charts™ v5.2.1',script)
        self.assertEqual(base64.b64encode(hashlib.sha256(script.encode()).digest()).decode(),
                         '4hzFyqAibvML2FScULnvkmYV8qTua05IY1NHelX1mM8=')
        request=urllib.request.Request('http://127.0.0.1:%d/assets/lightweight-charts-5.2.1.js'%self.server.server_port)
        with self.opener.open(request,timeout=3) as response:
            self.assertIn('immutable',response.headers['Cache-Control'])
            self.assertEqual(response.headers['Content-Type'],'text/javascript')
        self.assertIn('v4.2.3',self.request('/charts.js')[1],'old HTML lost compatible chart file')
        html=self.request('/')[1]
        self.assertNotIn('<script src="/charts.js">',html)
        self.assertIn('<script src="/app.js"></script>',html)
        app=self.request('/app.js')[1]
        self.assertIn('chart.addSeries(LightweightCharts.CandlestickSeries',app)
        self.assertIn('https://unpkg.com/lightweight-charts@5.2.1/',app)
        self.assertEqual(self.request('/assets/../.pi/admin-key.json')[0],404)

    def test_static_allowlist_mime_cache_and_working_directory(self):
        import os
        before=os.getcwd()
        try:
            os.chdir(tempfile.gettempdir())
            for url,(_,mime) in web.STATIC_ROUTES.items():
                with self.subTest(url=url):
                    req=urllib.request.Request('http://127.0.0.1:%d%s'%(self.server.server_port,url))
                    with self.opener.open(req,timeout=3) as response:
                        self.assertEqual(response.headers['Content-Type'],mime)
                        self.assertTrue(response.read())
                        if url in ('/','/admin','/app.js','/app.css','/admin.js','/admin.css','/charts.js'):
                            self.assertEqual(response.headers['Cache-Control'],'no-cache')
            for url in ('/assets/../.pi/admin-key.json','/assets/%2e%2e/.pi/admin-key.json',
                        '/%2e%2e/fx_sim.db','/.pi/admin-key.json','/fx_sim.db','/assets/',
                        '/static/index.html','/admin.html','/%252e%252e/fx_sim.db'):
                self.assertEqual(self.request(url)[0],404,url)
            self.assertIn('<link rel="stylesheet" href="/app.css">',self.request('/')[1])
            self.assertIn('<script src="/admin.js"></script>',self.request('/admin')[1])
        finally:
            os.chdir(before)

    def test_symbol_whitelist_auth_providers_validation_persistence_and_audit(self):
        defaults=self.request('/api/symbols',token='invalid')[1]
        self.assertTrue(all(row['enabled'] for row in defaults['rows']))
        self.assertEqual(defaults['version'],0)
        providers={row['symbol']:row['provider'] for row in defaults['rows']}
        self.assertEqual(providers['FXSIM'],'本地模拟');self.assertEqual(providers['BTCUSDT'],'Binance')
        self.assertIn('biquote.io',providers['USDJPY']);self.assertIn('Yahoo',providers['USDJPY'])
        self.assertEqual(self.request('/api/admin/symbols')[0],401)
        self.assertEqual(self.request('/api/admin/symbols',{'symbol':'USDJPY','enabled':False})[0],401)
        token=self.admin_token()
        for symbol,enabled in [('unknown',True),('USDJPY',0),('USDJPY','false'),(None,True)]:
            self.assertEqual(self.request('/api/admin/symbols',{'symbol':symbol,'enabled':enabled},token=token)[0],400)
        with patch.object(web,'_broadcast') as broadcast:
            status,data=self.request('/api/admin/symbols',{'symbol':'usdjpy','enabled':False},token=token)
            self.assertEqual(status,200);self.assertGreater(data['version'],0)
            broadcast.assert_called_once_with({'type':'symbols','version':data['version'],'rows':data['rows']})
        disabled=next(row for row in data['rows'] if row['symbol']=='USDJPY')
        self.assertFalse(disabled['enabled'])
        web.USER_STATE.clear()
        self.assertEqual(self.request('/api/symbols')[1]['rows'],data['rows'])
        self.assertEqual(web.admin_deposits()[0]['kind'],'symbol')
        self.assertEqual(self.request('/api/admin/symbols',{'symbol':'USDJPY','enabled':False},token=token)[1],data)
        self.assertEqual(len(web.admin_deposits()),1,'repeat setting duplicated audit')
        conn=web._db()
        with conn:conn.execute("CREATE TRIGGER fail_symbol BEFORE INSERT ON admin_events BEGIN SELECT RAISE(ABORT,'test symbol failure');END")
        conn.close()
        self.assertEqual(self.request('/api/admin/symbols',{'symbol':'USDJPY','enabled':True},token=token)[0],503)
        self.assertEqual(self.request('/api/symbols')[1]['rows'],data['rows'],'failed audit partly enabled symbol')

    def test_disabled_pair_blocks_new_orders_but_existing_closes_and_risk_work(self):
        token=self.admin_token()
        with patch.object(web,'_price',return_value=150):
            self.assertEqual(self.request('/api/trade',{'side':'buy','lots':.1})[0],200)
            position_id=self.request('/api/state')[1]['positions'][0]['id']
            with patch.object(web,'_broadcast'):
                self.assertEqual(self.request('/api/admin/symbols',{'symbol':'USDJPY','enabled':False},token=token)[0],200)
            before=web.acc_to_dict(web.user_state('player')['account'])
            self.assertEqual(self.request('/api/trade',{'side':'sell','lots':.1})[0],400)
            self.assertEqual(self.request('/api/symbol',{'symbol':'USDJPY'})[0],400)
            self.assertEqual(web.acc_to_dict(web.user_state('player')['account']),before)
            self.assertEqual(self.request('/api/state')[0],200,'disabled selected chart broke account state')
            self.assertEqual(self.request('/api/close',{'id':position_id})[0],200)
            self.assertEqual(web.user_state('player')['account'].positions,[])
        self.assertEqual(self.request('/api/symbol',{'symbol':'fake'})[0],400)
        with patch.object(web,'_broadcast'):web.admin_set_symbol('FXSIM',False)
        acc=web.user_state('player')['account']
        acc.balance=50;acc.positions=[web.fx_sim.Position('long',.1,1.,'FXSIM')];web.persist_user('player')
        web._on_quote('FXSIM',.9999)
        self.assertEqual(acc.positions,[]);self.assertAlmostEqual(acc.balance,48.)
        self.assertEqual(web.trade_history('player')[0]['kind'],'stopout')

    def test_disabled_conversion_pair_remains_available_for_cross_jpy_trades(self):
        with patch.object(web,'_broadcast'):web.admin_set_symbol('USDJPY',False)
        quotes={'EURJPY':165,'USDJPY':150}
        with patch.object(web,'_price',side_effect=quotes.get):
            self.assertEqual(self.request('/api/symbol',{'symbol':'EURJPY'})[0],200)
            self.assertEqual(self.request('/api/trade',{'side':'buy','lots':.1})[0],200)
            self.assertAlmostEqual(self.request('/api/state')[1]['used'],110)
            self.assertEqual(self.request('/api/close',{})[0],200)
            self.assertAlmostEqual(web.trade_history('player')[0]['pnl'],-200/150)

    def test_usdjpy_http_margin_pnl_currency_and_cross_selection_close(self):
        quotes={'USDJPY':150,'FXSIM':1.1}
        with patch.object(web,'_price',side_effect=quotes.get):
            self.assertEqual(self.request('/api/symbol',{'symbol':'USDJPY'})[0],200)
            self.assertEqual(self.request('/api/trade',{'side':'buy','lots':.1})[0],200)
            state=self.request('/api/state')[1]
            self.assertEqual(state['currency'],'USD')
            self.assertAlmostEqual(state['used'],100)
            self.assertAlmostEqual(state['positions'][0]['margin'],100)
            self.assertAlmostEqual(state['positions'][0]['pnl'],-100/150)
            position_id=state['positions'][0]['id']
            self.assertEqual(self.request('/api/symbol',{'symbol':'FXSIM'})[0],200)
            quotes['USDJPY']=151
            self.assertEqual(self.request('/api/close',{'id':position_id})[0],200)
            trade=self.request('/api/history')[1]['rows'][0]
            self.assertEqual(trade['currency'],'USD')
            self.assertAlmostEqual(trade['pnl'],9800/151)
            self.assertAlmostEqual(trade['balance'],10000+9800/151)

    def test_currency_migration_marks_old_trades_without_rewriting_them(self):
        conn=web._db()
        with conn:
            conn.execute('DROP TABLE trades')
            conn.execute('CREATE TABLE trades (id INTEGER PRIMARY KEY,name TEXT,time INTEGER,kind TEXT,position_id INTEGER,symbol TEXT,side TEXT,lots REAL,entry REAL,price REAL,pnl REAL,balance REAL,margin_level REAL)')
            conn.execute("INSERT INTO trades VALUES (1,'player',1,'close',1,'USDJPY','long',.1,150,151,10000,20000,NULL)")
        conn.close()
        row=web.trade_history('player')[0]
        self.assertEqual(row['currency'],'LEGACY')
        self.assertEqual(row['pnl'],10000)
        self.assertEqual(row['balance'],20000)
        self.assertEqual(web.user_state('player')['account'].balance,10000)

    def admin_token(self):
        status,data=self.request('/api/admin/login',{'psk':'test-key'})
        self.assertEqual(status,200);return data['token']

    def test_admin_public_page_private_api_psk_session_expiry_logout(self):
        self.assertEqual(self.request('/admin',token='invalid')[0],200)
        self.assertEqual(self.request('/api/admin/users')[0],401)
        self.assertEqual(self.request('/api/admin/deposit',{'name':'player','amount':100,'request_id':'a'*32})[0],401)
        self.assertEqual(self.request('/api/admin/login',{'psk':'incorrect'})[0],400)
        token=self.admin_token()
        self.assertEqual(self.request('/api/admin/users',token=token)[1]['rows'][0]['name'],'player')
        with patch.object(web.time,'time',return_value=web.time.time()+3601):
            self.assertEqual(self.request('/api/admin/users',token=token)[0],401)
        self.assertEqual(self.request('/api/admin/logout',{},token=token)[0],200)
        self.assertEqual(self.request('/api/admin/users',token=token)[0],401)

    def test_admin_deposit_atomic_idempotent_and_not_trading_return(self):
        token=self.admin_token();body={'name':'player','amount':100.01,'request_id':'b'*32}
        before=web.user_state('player')['account'].balance
        first=self.request('/api/admin/deposit',body,token=token)
        again=self.request('/api/admin/deposit',body,token=token)
        self.assertEqual(first,again);self.assertEqual(first[0],200)
        self.assertEqual(web.user_state('player')['account'].balance,before+100.01)
        self.assertEqual(web.trade_history('player'),[])
        self.assertEqual(web.deposit_totals()['player'],100.01)
        self.assertEqual(len(web.admin_deposits()),1)
        self.assertEqual(web.leaderboard()[0]['ret'],0)
        self.assertEqual(self.request('/api/admin/deposit',{**body,'amount':200},token=token)[0],400)
        web.USER_STATE.clear();self.assertAlmostEqual(web.user_state('player')['account'].balance,before+100.01)
        conn=web._db()
        with conn:conn.execute("CREATE TRIGGER fail_deposit BEFORE INSERT ON admin_events BEGIN SELECT RAISE(ABORT,'test funding failure');END")
        conn.close()
        self.assertEqual(self.request('/api/admin/deposit',{**body,'request_id':'c'*32},token=token)[0],503)
        self.assertEqual(web.user_state('player')['account'].balance,before+100.01)
        self.assertEqual(web.deposit_totals()['player'],100.01)

    def test_admin_funding_validation_and_login_rate_limit(self):
        token=self.admin_token()
        for amount in (None,0,-1,True,'bad',float('nan'),float('inf'),.001,1000000001):
            self.assertEqual(self.request('/api/admin/deposit',{'name':'player','amount':amount,'request_id':'d'*32},token=token)[0],400)
        self.assertEqual(self.request('/api/admin/deposit',{'name':'missing','amount':100,'request_id':'d'*32},token=token)[0],400)
        self.assertEqual(self.request('/api/admin/deposit',{'name':'player','amount':100,'request_id':'short'},token=token)[0],400)
        with patch.object(web.time,'time',return_value=1800000000):
            for i in range(11):self.assertEqual(self.request('/api/admin/login',{'psk':'bad'})[0],400)
        self.assertEqual(web.user_state('player')['account'].balance,10000)

    def test_admin_delete_confirmation_revokes_tokens_sockets_and_audits(self):
        token=self.admin_token();body={'name':'player','amount':10,'request_id':'e'*32}
        self.assertEqual(self.request('/api/admin/deposit',body,token=token)[0],200)
        with patch.object(web,'_broadcast'):web.post_chat('player','hello')
        acc=web.user_state('player')['account'];acc.positions=[web.fx_sim.Position('long',.1,1.1,'FXSIM')]
        web.persist_user('player')
        self.assertEqual(self.request('/api/admin/delete',{'name':'player','confirmation':'wrong'},token=token)[0],400)
        self.assertIn('player',web.USERS)
        with patch.object(web,'_front_clients',{'fake-socket'}),patch.dict(web.CLIENT_USER,{'fake-socket':'player'},clear=True),patch.object(web,'_drop_client') as drop:
            status,row=self.request('/api/admin/delete',{'name':'player','confirmation':'player'},token=token)
            self.assertEqual(status,200);self.assertEqual(row['row']['positions'],1)
            drop.assert_called_once_with('fake-socket')
        self.assertNotIn('player',web.USERS);self.assertNotIn(self.token,web.TOKENS)
        self.assertEqual(self.request('/api/me')[0],401)
        self.assertEqual(web.trade_history('player'),[]);self.assertEqual(web.chat_history(),[])
        audit=web.admin_deposits();self.assertEqual([a['kind'] for a in audit],['delete','deposit'])
        web.register_user('player','new-password')
        self.assertEqual(self.request('/api/admin/deposit',body,token=token)[0],400,'deleted old request replay credited new account')
        self.assertEqual(web.user_state('player')['account'].balance,10000)

    def test_admin_delete_failure_keeps_account_balance_token_and_messages(self):
        token=self.admin_token()
        conn=web._db()
        with conn:conn.execute("CREATE TRIGGER fail_delete BEFORE DELETE ON users BEGIN SELECT RAISE(ABORT,'test delete failure');END")
        conn.close()
        self.assertEqual(self.request('/api/admin/delete',{'name':'player','confirmation':'player'},token=token)[0],503)
        self.assertIn('player',web.USERS);self.assertEqual(self.request('/api/me')[0],200)
        self.assertEqual(web.admin_deposits(),[])

    def test_five_minute_net_return_deducts_deposits_and_survives_state_reload(self):
        with patch.object(web.time,'time',return_value=1800000000):
            first=web.leaderboard()[0];self.assertIsNone(first['pnl_5m'])
        with patch.object(web.time,'time',return_value=1800000150):
            web.admin_deposit('player',1000,'f'*32)
        with patch.object(web.time,'time',return_value=1800000300):
            row=web.leaderboard()[0]
            self.assertEqual(row['pnl_5m'],0);self.assertEqual(row['ret_5m'],0)
            web.account_transaction('player',lambda acc:setattr(acc,'balance',11100))
            web.USER_STATE.clear()
            row=web.leaderboard()[0]
            self.assertEqual(row['pnl_5m'],100);self.assertAlmostEqual(row['ret_5m'],.01)
        with patch.object(web.time,'time',return_value=1800000700):
            self.assertIsNone(web.leaderboard()[0]['pnl_5m'],'old baseline incorrectly presented as 5min')
        with patch.object(web.time,'time',return_value=1800001300):
            web.leaderboard()
            conn=web._db();self.assertGreaterEqual(conn.execute('SELECT MIN(time) FROM equity_samples').fetchone()[0],1800000400);conn.close()

    def test_online_presence_multiple_tabs_and_last_disconnect(self):
        with patch.object(web,'_front_clients',set()),patch.dict(web.CLIENT_USER,{},clear=True):
            self.assertFalse(web.leaderboard()[0]['online'])
            one,two=Mock(),Mock()
            web._front_clients.update({one,two})
            web.CLIENT_USER.update({one:'player',two:'player'});web._lb_cache['t']=0
            self.assertTrue(web.leaderboard()[0]['online'])
            web._drop_client(one);one.close.assert_called_once()
            self.assertTrue(web.leaderboard()[0]['online'])
            web._drop_client(two);two.close.assert_called_once()
            self.assertFalse(web.leaderboard()[0]['online'])
            self.assertEqual(web.CLIENT_USER,{})

    def test_chat_auth_validation_broadcast_and_history(self):
        self.assertEqual(self.request('/api/chat',token='invalid')[0],401)
        self.assertEqual(self.request('/api/chat',{'text':'hi'},token='invalid')[0],401)
        for text in ('',' '*10,None,123,'x'*301):
            self.assertEqual(self.request('/api/chat',{'text':text})[0],400)
        with patch.object(web,'_broadcast') as send:
            status,data=self.request('/api/chat',{'text':'  <script>hello</script>  '})
            self.assertEqual(status,200)
            row=data['row'];self.assertEqual(row['name'],'player')
            self.assertEqual(row['text'],'<script>hello</script>')
            send.assert_called_once_with({'type':'chat','row':row})
            self.assertEqual(self.request('/api/chat',{'text':'too fast'})[0],400)
        self.assertEqual(self.request('/api/chat')[1]['rows'],[row])
        web.USER_STATE.clear()
        self.assertEqual(self.request('/api/chat')[1]['rows'],[row])

    def test_chat_bounded_retention_and_server_not_client_sets_identity(self):
        with patch.object(web,'_broadcast'),patch.object(web.time,'time',return_value=1800000000):
            for i in range(105):
                with patch.object(web.time,'time',return_value=1800000000+i*3):
                    row=web.post_chat('player',str(i))
            rows=web.chat_history()
        self.assertEqual(len(rows),100)
        self.assertEqual(rows[0]['text'],'5');self.assertEqual(rows[-1],row)


if __name__=='__main__':
    unittest.main()
