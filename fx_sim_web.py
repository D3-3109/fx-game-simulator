#!/usr/bin/python3
"""FX 保证金模拟器 - Web 版. 后端零依赖; 图表 lightweight-charts, 数据全部来自我们的 Binance 源.

Run:
  ./fx_sim_web.py            # http://localhost:8000
  ./fx_sim_web.py 8001
"""
from __future__ import annotations
import json
import threading
import time
import random
import math
import hmac
from pathlib import Path

STATIC_DIR = Path(__file__).resolve().parent / 'static'
STATIC_ROUTES = {
    '/': ('index.html', 'text/html; charset=utf-8'),
    '/admin': ('admin.html', 'text/html; charset=utf-8'),
    **{f'/{name}.{ext}': (f'{name}.{ext}', mime)
       for name in ('app', 'admin')
       for ext, mime in (('css', 'text/css; charset=utf-8'), ('js', 'text/javascript'))},
    '/charts.js': ('charts.js', 'text/javascript'),
    '/assets/lightweight-charts-5.2.1.js': ('assets/lightweight-charts-5.2.1.js', 'text/javascript'),
    '/assets/noto-sans-latin.woff2': ('assets/noto-sans-latin.woff2', 'font/woff2'),
    **{f'/assets/{name}': (f'assets/{name}', 'text/plain; charset=utf-8') for name in
       ('Lightweight-Charts-LICENSE.txt', 'Lightweight-Charts-NOTICE.txt', 'OFL-NotoSans.txt')},
}
from collections import deque
import urllib.request
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import fx_sim
from fx_sim import Account, fetch_price

KLINE_API = "https://data-api.binance.vision/api/v3/klines?symbol={}&interval={}&limit={}"

# 非加密货币对: 走 Yahoo Finance (Binance 无 CNY 等)
YAHOO_MAP = {"USDCNH": "CNY=X", "USDCNY": "CNY=X", "EURCNH": "EURCNY=X",
             "EURUSD": "EURUSD=X", "GBPUSD": "GBPUSD=X", "USDJPY": "JPY=X",
             "AUDUSD": "AUDUSD=X", "EURJPY": "EURJPY=X", "GBPJPY": "GBPJPY=X",
             "USDCHF": "CHF=X", "USDCAD": "CAD=X", "NZDUSD": "NZDUSD=X"}
YAHOO_IV = {"1m": "1m", "5m": "5m", "15m": "15m", "30m": "30m",
            "1h": "60m", "4h": "60m", "1d": "1d", "1w": "1wk"}
YAHOO_RANGE = {"1m": "3d", "5m": "1mo", "15m": "1mo", "30m": "2mo",
               "60m": "3mo", "1d": "1y", "1wk": "2y"}


def _opener():
    import urllib.request
    handlers = ([urllib.request.ProxyHandler({"http": fx_sim.PROXY, "https": fx_sim.PROXY})]
                if fx_sim.PROXY else [])
    return urllib.request.build_opener(*handlers)


def _yahoo_chart(symbol, interval):
    iv = YAHOO_IV[interval]
    url = (f"https://query1.finance.yahoo.com/v8/finance/chart/{YAHOO_MAP[symbol]}"
           f"?interval={iv}&range={YAHOO_RANGE[iv]}&includePrePost=false")
    req = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0"})
    with _opener().open(req, timeout=10) as r:
        return json.load(r)


def yahoo_klines(symbol, interval, limit):
    res = _yahoo_chart(symbol, interval)["chart"]["result"][0]
    ts = res["timestamp"]
    q = res["indicators"]["quote"][0]
    out = []
    for i, t in enumerate(ts):
        o, h, l, c = (q[k][i] for k in ("open", "high", "low", "close"))
        if None in (o, h, l, c):
            continue
        out.append([t * 1000, round(o, 6), round(h, 6), round(l, 6), round(c, 6)])
    if interval == "4h":  # 1h x4 聚合
        out = [[ch[0][0], ch[0][1], max(x[2] for x in ch), min(x[3] for x in ch), ch[-1][4]]
               for ch in (out[i:i + 4] for i in range(0, len(out) - 3, 4))]
    return out[-limit:]


def yahoo_price(symbol):
    res = _yahoo_chart(symbol, "1h")["chart"]["result"][0]
    return float(res["meta"]["regularMarketPrice"])

# ---- biquote.io: 匿名免费外汇 OHLC, tick密度高; 外汇对主源 ----
CRYPTO_SET = {"BTCUSDT", "ETHUSDT", "EURUSDT"}  # 币安现货
SIMSYM = "FXSIM"  # 模拟币对: 高波动随机游走+事件脉冲, 0.2s tick
SIM_INTERVALS = {"1m": 60, "5m": 300, "15m": 900, "30m": 1800,
                 "1h": 3600, "4h": 14400, "1d": 86400, "1w": 604800}


class SimFeed:
    """FXSIM 价格引擎: 均值回归随机游走 + 偶发插针/暴走, 5x外汇波动"""
    BASE = 1.1000
    VOL = 0.0004        # 每tick标准差(约4pip, 外汇1m级) → 视觉上跳得凶
    MEAN_TAU = 0.003    # 向BASE回归强度
    EVENTS = [          # (权重, 类型) 事件脉冲让行情刺激
        (6.0, "none"), (1.2, "spike"), (1.2, "trend"), (0.6, "crash"), (0.6, "pump")]

    def __init__(self):
        self.price = self.BASE
        self.trend = 0.0        # 惯性
        self.mode = ("none", 0) # (事件, 剩余tick)
        self.t0 = time.time()
        self.updated_at = int(self.t0 * 1000)
        self.bars = {sec: deque([[int(self.t0) // sec * sec, *([self.BASE] * 4)]], maxlen=1000)
                     for sec in SIM_INTERVALS.values()}
        self.lock = threading.RLock()
        self._history_db = None

    def load_history(self, path):
        """只恢复实际记录；没有旧行情时从当前报价开始，不伪造历史。"""
        with self.lock:
            db = sqlite3.connect(path, check_same_thread=False)
            db.execute('CREATE TABLE IF NOT EXISTS sim_bars ('
                       'sec INTEGER, t INTEGER, o REAL, h REAL, l REAL, c REAL, '
                       'PRIMARY KEY(sec,t))')
            loaded = set()
            for sec in self.bars:
                rows = db.execute('SELECT t,o,h,l,c FROM sim_bars WHERE sec=? '
                                  'ORDER BY t DESC LIMIT 1000', (sec,)).fetchall()
                if rows:
                    self.bars[sec] = deque((list(row) for row in reversed(rows)), maxlen=1000)
                    loaded.add(sec)
            self.price = self.bars[60][-1][4]
            for sec in self.bars.keys() - loaded:
                self.bars[sec] = deque([[int(time.time()) // sec * sec, *([self.price] * 4)]], maxlen=1000)
            self._history_db = db
            self._record(round(self.price, 5), time.time())

    def _record(self, px, now):
        """调用者持有锁；每个tick聚合所有周期，跨周期保留已完成的OHLC。"""
        self.price = px
        self.updated_at = int(now * 1000)
        db = self._history_db
        for sec, history in self.bars.items():
            t = int(now) // sec * sec
            if t < history[-1][0]:
                continue  # 系统时钟回退不能修改已结束K线。
            if history[-1][0] != t:
                history.append([t, px, px, px, px])
                if db:
                    db.execute('DELETE FROM sim_bars WHERE sec=? AND t<?', (sec, history[0][0]))
            else:
                bar = history[-1]
                bar[2] = max(bar[2], px); bar[3] = min(bar[3], px); bar[4] = px
            if db:
                db.execute('INSERT OR REPLACE INTO sim_bars VALUES (?,?,?,?,?,?)',
                           (sec, *history[-1]))
        if db:
            db.commit()

    def _tick(self):
        with self.lock:
            # 事件切换
            kind, left = self.mode
            if left <= 0:
                r = random.random() * 10  # 权重和
                acc = 0
                for w, k in self.EVENTS:
                    acc += w
                    if r <= acc:
                        kind = k; break
                dur = {"none": 20, "spike": 6, "trend": 40, "crash": 15, "pump": 15}[kind]
                self.mode = (kind, dur)
            kind, left = self.mode; self.mode = (kind, left - 1)
            # 演化
            drift = self.MEAN_TAU * (self.BASE - self.price)
            self.trend *= 0.95
            shock = random.gauss(0, self.VOL)
            if kind == "spike":    # 单根插针
                shock *= 6 if random.random() < .5 else -6
            elif kind == "trend":  # 暴走趋势
                self.trend += random.choice([-1, 1]) * self.VOL * .3
            elif kind == "crash":  # 连续暴跌
                shock -= self.VOL * 1.5
            elif kind == "pump":
                shock += self.VOL * 1.5
            self.price = max(0.5, self.price + drift + shock + self.trend)
            px = round(self.price, 5)
            self._record(px, time.time())
            # 发送当前根及已闭合前一根，客户端漏过插针也能收到完整极值。
            packet = {"sym": SIMSYM, "price": px, "time": self.updated_at,
                      "klines": {itv: [[b[0]*1000, *b[1:]] for b in list(self.bars[sec])[-2:]]
                                 for itv, sec in SIM_INTERVALS.items()}}
        # 不持有SIM锁时检查账户，避免与HTTP的users_lock形成锁顺序反转。
        _on_quote(SIMSYM, px)
        _broadcast(packet)
        return px

    def tick_loop(self):
        while True:
            self._tick()
            time.sleep(0.2)

    def klines(self, sec, limit):
        with self.lock:
            return [[b[0] * 1000, *b[1:]] for b in list(self.bars[sec])[-limit:]]

    def price_now(self):
        with self.lock:
            return round(self.price, 5)


SIM = SimFeed()
BIQUOTE_SET = {"EURUSD", "GBPUSD", "USDJPY", "USDCHF", "USDCAD", "AUDUSD",
               "NZDUSD", "EURJPY", "GBPJPY", "USDCNH", "USDCNY", "EURCNH"}
SYMBOL_PROVIDERS = {symbol: ('本地模拟' if symbol == SIMSYM else 'Binance' if symbol in CRYPTO_SET
                            else 'biquote.io → Yahoo Finance（备用）' if symbol in BIQUOTE_SET
                            else 'Yahoo Finance')
                    for symbol in sorted(CRYPTO_SET | BIQUOTE_SET | set(YAHOO_MAP) | {SIMSYM})}
import datetime as _dt


def _biquote_get(path):
    req = urllib.request.Request("https://biquote.io/api/" + path,
                                 headers={"User-Agent": "Mozilla/5.0"})
    with _opener().open(req, timeout=10) as r:
        return json.load(r)


def biquote_klines(symbol, interval, limit):
    env = _biquote_get(f"{symbol}/ohlc?interval={interval}&limit={min(limit,1000)}")
    out = []
    for b in env["bars"]:
        t = _dt.datetime.strptime(b["openTime"], "%Y-%m-%dT%H:%M:%SZ").replace(
            tzinfo=_dt.timezone.utc).timestamp()
        out.append([int(t * 1000), b["open"], b["high"], b["low"], b["close"]])
    out.reverse()  # biquote新->旧, lwc要升序
    return out


def biquote_price(symbol):
    bars = _biquote_get(f"{symbol}/ohlc?interval=1m&limit=1")["bars"]
    # limit=1 may include the previous closed bar; ordering is not a freshness contract.
    latest = max(bars, key=lambda b: _dt.datetime.fromisoformat(b['openTime'].replace('Z', '+00:00')), default=None)
    return float(latest['close']) if latest else None


import os

STATE_FILE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fx_state.json")

state_lock = threading.Lock()
STATE: dict | None = None


def save_state():
    st = STATE
    acc: Account = st["account"]
    data = {"symbol": st["symbol"], **acc_to_dict(acc)}
    tmp = STATE_FILE + ".tmp"
    with open(tmp, "w") as f:
        json.dump(data, f, indent=1)
    os.replace(tmp, STATE_FILE)


def load_state() -> dict | None:
    try:
        with open(STATE_FILE) as f:
            d = json.load(f)
        return {"account": acc_from_dict(d), "symbol": d["symbol"]}
    except Exception:
        return None


def active_symbols() -> set:
    with users_lock:
        syms = {u.get("symbol", "USDJPY") for u in USERS.values()}
        for u in USERS.values():
            syms.update(p.get("symbol", "") for p in u["account"]["positions"])
        for st in USER_STATE.values():
            syms.add(st["symbol"])
            syms.update(p.symbol for p in st["account"].positions)
    return {x for x in syms if x}


_px_cache: dict = {}  # sym -> (ts, price); TTL 见下


def _price(symbol):
    now = time.time()
    hit = _px_cache.get(symbol)
    if hit and now - hit[0] < 8:
        return hit[1]
    if symbol == SIMSYM:
        return SIM.price_now()
    px = None
    try:
        if symbol in BIQUOTE_SET:
            try:
                px = biquote_price(symbol)
            except Exception:
                px = None
        if px is None and symbol in YAHOO_MAP:
            px = yahoo_price(symbol)
        if px is None:
            px = fetch_price(symbol)
        fx_sim.market_price(px, symbol)
    except Exception:
        return hit[1] if hit else None
    with users_lock:
        latest = _px_cache.get(symbol)
        if latest and latest[0] > now:
            return latest[1]  # 慢HTTP响应不能覆盖请求期间收到的新WS报价。
        _px_cache[symbol] = (now, px)
    return px




def _prices(symbols) -> dict[str, float]:
    """一次操作固定一份行情；缺币对或缺价时禁止用其他币对代替。"""
    symbols = set(symbols)
    if "" in symbols:
        raise ValueError("旧持仓缺少币对，无法安全结算")
    return {sym: fx_sim.market_price(_price(sym), sym) for sym in fx_sim.price_symbols(symbols)}


# ---------------- WS 转发: Binance websocket -> 浏览器 ----------------
import base64
import hashlib
import socket
import ssl
from collections import deque
import threading as _th

GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
_front_clients = set()          # 浏览器 ws socket 集合
_front_lock = _th.Lock()
BINANCE_WS_HOST = ("data-stream.binance.vision", 443)


def _ws_send(sock, text: str):
    data = text.encode()
    n = len(data)
    if n < 126:
        head = bytes([0x81, n])
    else:
        head = bytes([0x81, 126]) + n.to_bytes(2, "big")
    with _front_lock:
        dead = None
        try:
            sock.sendall(head + data)
        except OSError:
            dead = sock
    if dead:
        _drop_client(dead)


def _drop_client(sock):
    with _front_lock:
        _front_clients.discard(sock)
        CLIENT_USER.pop(sock, None)
        _lb_cache['t'] = 0
    try:
        sock.close()
    except OSError:
        pass


def _broadcast(obj):
    msg = json.dumps(obj)
    with _front_lock:
        socks = list(_front_clients)
    for s in socks:
        _ws_send(s, msg)


def _recv_exact(sock, n: int) -> bytes:
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise OSError("closed")
        buf += chunk
    return buf


def _ws_read_msg(sock) -> tuple:
    """返回 (opcode, payload); 服务端帧不掩码"""
    b1, b2 = _recv_exact(sock, 2)
    fin_op = b1 & 0x0F
    ln = b2 & 0x7F
    if ln == 126:
        ln = int.from_bytes(_recv_exact(sock, 2), "big")
    elif ln == 127:
        ln = int.from_bytes(_recv_exact(sock, 8), "big")
    payload = _recv_exact(sock, ln) if ln else b""
    return fin_op, payload


def _binance_ws_loop():
    """连 Binance combined stream, 当前交易对 miniTicker -> 广播; 断线重连"""
    cur = None
    sock = None
    while True:
        want = {x.lower() for x in active_symbols() if x not in YAHOO_MAP and x in CRYPTO_SET}
        if not want:
            time.sleep(1); cur=None; sock=None; continue
        key=",".join(sorted(want))
        if key != cur or sock is None:
            if sock:
                try: sock.close()
                except OSError: pass
            try:
                sock = _binance_connect("/stream?streams=" + "/".join(f"{x}@miniTicker" for x in sorted(want)))
                cur = key
                print(f"[ws] binance streams {key}")
            except Exception as e:
                print(f"[ws] connect err {e}")
                time.sleep(3)
                continue
        try:
            op, payload = _ws_read_msg(sock)
            if op == 1:  # text
                d = json.loads(payload)["data"]
                sym = d["s"].upper()
                px = float(d["c"])
                _on_quote(sym, px)
                _broadcast({"sym": sym, "price": px})
            elif op == 9:  # ping -> pong(掩码位必须, key 全零)
                sock.sendall(bytes([0x8A, 0x80]) + b"\x00\x00\x00\x00")
        except Exception:
            try: sock.close()
            except OSError: pass
            sock = None
            cur = None
            time.sleep(2)


def _binance_connect(path: str):
    host, port = BINANCE_WS_HOST
    raw = socket.create_connection((host, port), timeout=10)
    sock = ssl.create_default_context().wrap_socket(raw, server_hostname=host)
    key = base64.b64encode(os.urandom(16)).decode()
    req = (f"GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\n"
           f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n"
           f"Sec-WebSocket-Version: 13\r\nUser-Agent: fx-sim\r\n\r\n")
    sock.sendall(req.encode())
    resp = b""
    while b"\r\n\r\n" not in resp:
        chunk = sock.recv(4096)
        if not chunk:
            raise OSError("handshake eof")
        resp += chunk
    if b"101" not in resp.split(b"\r\n")[0]:
        raise OSError(f"handshake: {resp[:80]!r}")
    sock.settimeout(30)
    return sock


def _yahoo_push_loop():
    last = {}
    while True:
        time.sleep(2)
        with _front_lock:
            names = {CLIENT_USER.get(sk) for sk in _front_clients} - {None}
        try:
            with users_lock:
                syms = set()
                for name in names:
                    st = user_state(name)
                    syms.add(st["symbol"])
                    syms.update(p.symbol for p in st["account"].positions)
        except Exception:
            continue
        for sym in {x for x in syms if x in YAHOO_MAP}:
            try:
                px = yahoo_price(sym)
            except Exception:
                continue
            if px != last.get(sym):
                last[sym] = px
                _on_quote(sym, px)
                _broadcast({"sym": sym, "price": px})


def _state_push_loop():
    while True:
        time.sleep(1)
        with _front_lock:
            by_user = {}
            for sk in _front_clients:
                by_user.setdefault(CLIENT_USER.get(sk), []).append(sk)
        for name, socks in by_user.items():
            if not name:
                continue
            try:
                p = state_payload(name)
            except Exception:
                continue
            if p.get("ok"):
                for sk in socks:
                    _ws_send(sk, json.dumps(p))


def _serve_ws(handler):
    """升级 /ws 连接并常驻转发"""
    req = handler.headers
    key = req.get("Sec-WebSocket-Key", "")
    accept = base64.b64encode(hashlib.sha1((key + GUID).encode()).digest()).decode()
    resp = ("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n"
            f"Connection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n")
    handler.connection.sendall(resp.encode())
    sock = handler.connection
    sock.settimeout(60)
    with _front_lock:
        _front_clients.add(sock)
    try:
        while True:
            op, _ = _ws_read_msg_client(sock)
            if op in (8, None):
                break
    except OSError:
        pass
    finally:
        _drop_client(sock)


def _ws_read_msg_client(sock):
    """浏览器帧带掩码"""
    try:
        b1, b2 = _recv_exact(sock, 2)
    except OSError:
        return None, b""
    op = b1 & 0x0F
    masked = bool(b2 & 0x80)
    ln = b2 & 0x7F
    if ln == 126:
        ln = int.from_bytes(_recv_exact(sock, 2), "big")
    elif ln == 127:
        ln = int.from_bytes(_recv_exact(sock, 8), "big")
    mask = _recv_exact(sock, 4) if masked else b"\x00\x00\x00\x00"
    data = _recv_exact(sock, ln) if ln else b""
    if masked:
        data = bytes(c ^ mask[i % 4] for i, c in enumerate(data))
    return op, data


# ================= 账号系统 (sqlite) =================
import sqlite3
DB_FILE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fx_sim.db")
users_lock = threading.RLock()
USERS: dict = {}          # name -> {salt,hash,token,symbol,account:{...}}
TOKENS: dict = {}         # token -> name
USER_STATE: dict = {}     # name -> {"account":Account,"symbol":str} 内存缓存


def _db():
    conn = sqlite3.connect(DB_FILE, check_same_thread=False)
    conn.execute("""CREATE TABLE IF NOT EXISTS users(
        name TEXT PRIMARY KEY, salt TEXT, hash TEXT, token TEXT,
        symbol TEXT DEFAULT 'USDJPY', account TEXT, created INTEGER)""")
    conn.execute("""CREATE TABLE IF NOT EXISTS trades(
        id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL,
        time INTEGER NOT NULL, kind TEXT NOT NULL, position_id INTEGER,
        symbol TEXT, side TEXT, lots REAL, entry REAL, price REAL,
        pnl REAL, balance REAL, margin_level REAL, currency TEXT DEFAULT 'LEGACY')""")
    if 'currency' not in {column[1] for column in conn.execute('PRAGMA table_info(trades)')}:
        conn.execute("ALTER TABLE trades ADD COLUMN currency TEXT DEFAULT 'LEGACY'")
    conn.execute('CREATE INDEX IF NOT EXISTS trades_by_user ON trades(name,id DESC)')
    conn.execute('CREATE TABLE IF NOT EXISTS chat ('
                 'id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, time INTEGER NOT NULL, text TEXT NOT NULL)')
    conn.execute('CREATE TABLE IF NOT EXISTS deposits ('
                 'request_id TEXT PRIMARY KEY, name TEXT NOT NULL, time INTEGER NOT NULL, '
                 'amount REAL NOT NULL, balance REAL NOT NULL, actor TEXT NOT NULL)')
    conn.execute('CREATE TABLE IF NOT EXISTS symbol_settings ('
                 'symbol TEXT PRIMARY KEY, enabled INTEGER NOT NULL CHECK(enabled IN (0,1)), '
                 'version INTEGER NOT NULL)')
    conn.execute('CREATE TABLE IF NOT EXISTS equity_samples ('
                 'name TEXT NOT NULL, time INTEGER NOT NULL, equity REAL NOT NULL, '
                 'deposited REAL NOT NULL, PRIMARY KEY(name,time))')
    conn.execute('CREATE TABLE IF NOT EXISTS admin_events ('
                 'id INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT NOT NULL, name TEXT NOT NULL, '
                 'time INTEGER NOT NULL, request_id TEXT UNIQUE, detail TEXT NOT NULL)')
    return conn


def _hash_pwd(pwd, salt):
    return hashlib.sha256((salt + pwd).encode()).hexdigest()


def load_users():
    global USERS, TOKENS
    try:
        conn = _db()
        for name, salt, h, tok, sym, acc, cr in conn.execute(
                "SELECT name,salt,hash,token,symbol,account,created FROM users"):
            USERS[name] = {"salt": salt, "hash": h, "token": tok, "symbol": sym,
                           "account": json.loads(acc), "created": cr}
        conn.close()
    except Exception as e:
        print("[db] load err", e); USERS = {}
    TOKENS = {u.get("token"): n for n, u in USERS.items() if u.get("token")}


def save_users():
    with users_lock:
        conn = _db()
        for n, u in USERS.items():
            conn.execute("INSERT OR REPLACE INTO users VALUES(?,?,?,?,?,?,?)",
                         (n, u["salt"], u["hash"], u.get("token", ""),
                          u.get("symbol", "USDJPY"),
                          json.dumps(u["account"], ensure_ascii=False),
                          u.get("created", 0)))
        conn.commit(); conn.close()


def acc_to_dict(acc: Account):
    return {"balance": acc.balance, "leverage": acc.leverage,
            "next_id": fx_sim.Position._seq + 1,
            "positions": [{"id": p.id, "side": p.side, "lots": p.lots, "entry": p.entry,
                           "symbol": p.symbol} for p in acc.positions]}


def acc_from_dict(d) -> Account:
    acc = Account(cash=d["balance"], leverage=d["leverage"])
    for p in d["positions"]:
        # 旧记录无法可靠还原币对；保留空值，结算时明确报错，不猜。
        acc.positions.append(fx_sim.Position(p["side"], p["lots"], p["entry"], p.get("symbol", "")))
        acc.positions[-1].id = p["id"]
    fx_sim.Position._seq = max(fx_sim.Position._seq,
                               d.get("next_id", 1),
                               max((p["id"] for p in d["positions"]), default=0))
    return acc


def user_state(name) -> dict:
    with users_lock:
        if name not in USER_STATE:
            u = USERS[name]
            USER_STATE[name] = {"account": acc_from_dict(u["account"]),
                                "symbol": u.get("symbol", "USDJPY"),
                                "last_trade": next(iter(trade_history(name, 1)), None)}
        return USER_STATE[name]


TRADE_FIELDS = ('time', 'kind', 'position_id', 'symbol', 'side', 'lots',
                'entry', 'price', 'pnl', 'balance', 'margin_level', 'currency')


def trade_history(name, limit=100):
    conn = _db()
    try:
        conn.row_factory = sqlite3.Row
        return [dict(row) for row in conn.execute(
            'SELECT id,' + ','.join(TRADE_FIELDS) + ' FROM trades WHERE name=? '
            'ORDER BY id DESC LIMIT ?', (name, limit))]
    finally:
        conn.close()


def chat_history():
    conn = _db()
    try:
        conn.row_factory = sqlite3.Row
        return [dict(row) for row in conn.execute('SELECT * FROM chat ORDER BY id DESC LIMIT 100')][::-1]
    finally:
        conn.close()


def post_chat(name, text):
    if not isinstance(text, str) or not text.strip() or len(text.strip()) > 300:
        raise ValueError('聊天内容需为1至300字')
    with users_lock:
        now = int(time.time() * 1000)
        conn = _db()
        try:
            with conn:
                previous = conn.execute('SELECT time FROM chat WHERE name=? ORDER BY id DESC LIMIT 1', (name,)).fetchone()
                if previous and now - previous[0] < 2000:
                    raise ValueError('发送太快，请等2秒')
                cursor = conn.execute('INSERT INTO chat(name,time,text) VALUES (?,?,?)', (name, now, text.strip()))
                row = {'id': cursor.lastrowid, 'name': name, 'time': now, 'text': text.strip()}
                conn.execute('DELETE FROM chat WHERE id <= (SELECT id FROM chat ORDER BY id DESC LIMIT 1 OFFSET 100)')
        finally:
            conn.close()
    _broadcast({'type': 'chat', 'row': row})
    return row


ADMIN_KEY_FILE = os.path.join(os.path.dirname(__file__), '.pi', 'admin-key.json')
ADMIN_SESSIONS = {}
ADMIN_ATTEMPTS = deque(maxlen=10)


def admin_login(psk):
    with users_lock:
        now = time.time()
        if len(ADMIN_ATTEMPTS) == 10 and now - ADMIN_ATTEMPTS[0] < 60:
            raise ValueError('登录尝试过多，请一分钟后重试')
        ADMIN_ATTEMPTS.append(now)
        if not isinstance(psk, str) or not 1 <= len(psk) <= 256:
            raise ValueError('PSK错误')
        try:
            with open(ADMIN_KEY_FILE) as f:
                config = json.load(f)
        except FileNotFoundError:
            raise ValueError('管理员PSK尚未配置')
        digest = hashlib.pbkdf2_hmac('sha256', psk.encode(), bytes.fromhex(config['salt']), 200000).hex()
        if not hmac.compare_digest(digest, config['hash']):
            raise ValueError('PSK错误')
        for token, expiry in list(ADMIN_SESSIONS.items()):
            if expiry <= now:
                del ADMIN_SESSIONS[token]
        token = secrets.token_hex(32)
        ADMIN_SESSIONS[token] = now + 3600
        return token


def admin_authorized(token):
    with users_lock:
        return ADMIN_SESSIONS.get(token, 0) > time.time()


def deposit_totals():
    conn = _db()
    try:
        return dict(conn.execute('SELECT name,SUM(amount) FROM deposits GROUP BY name'))
    finally:
        conn.close()


def admin_deposits():
    conn = _db()
    try:
        conn.row_factory = sqlite3.Row
        return [{'id': row['id'], 'kind': row['kind'], **json.loads(row['detail'])}
                for row in conn.execute('SELECT * FROM admin_events ORDER BY id DESC LIMIT 100')]
    finally:
        conn.close()


def admin_deposit(name, amount, request_id):
    if isinstance(amount, bool):
        raise ValueError('金额无效')
    amount = float(amount)
    if not math.isfinite(amount) or not 0 < amount <= 1000000000 or amount != round(amount, 2):
        raise ValueError('充值金额须为大于0的美元金额，最多两位小数、单次不超过10亿美元')
    if not isinstance(request_id, str) or not 16 <= len(request_id) <= 64:
        raise ValueError('充值请求编号无效')
    with users_lock:
        if name not in USERS:
            raise ValueError('账号不存在')
        conn = _db()
        try:
            conn.row_factory = sqlite3.Row
            existing = conn.execute('SELECT * FROM deposits WHERE request_id=?', (request_id,)).fetchone()
            if existing:
                if existing['name'] != name or existing['amount'] != amount:
                    raise ValueError('充值请求编号已使用')
                return dict(existing)
            if conn.execute('SELECT 1 FROM admin_events WHERE request_id=?', (request_id,)).fetchone():
                raise ValueError('原账号已删除，旧充值请求不可重放')
        finally:
            conn.close()
        balance = user_state(name)['account'].balance + amount
        if not math.isfinite(balance):
            raise ValueError('充值后余额无效')
        row = {'request_id': request_id, 'name': name, 'time': int(time.time() * 1000),
               'amount': amount, 'balance': balance, 'actor': 'PSK管理员'}
        account_transaction(name, lambda acc: setattr(acc, 'balance', balance), funding=row)
        return row


def symbol_settings():
    with users_lock:
        conn = _db()
        try:
            flags = {row[0]: row[1:] for row in conn.execute('SELECT symbol,enabled,version FROM symbol_settings')}
        finally:
            conn.close()
        return {'version': max((row[1] for row in flags.values()), default=0),
                'rows': [{'symbol': symbol, 'provider': provider, 'enabled': bool(flags.get(symbol, (1, 0))[0])}
                         for symbol, provider in SYMBOL_PROVIDERS.items()]}


def require_tradable_symbol(symbol):
    if symbol not in SYMBOL_PROVIDERS:
        raise ValueError('不支持的币种对')
    if not next(row['enabled'] for row in symbol_settings()['rows'] if row['symbol'] == symbol):
        raise ValueError(symbol + ' 已禁用新开仓；已有持仓仍可平仓')


def admin_set_symbol(symbol, enabled):
    if not isinstance(symbol, str) or symbol.upper() not in SYMBOL_PROVIDERS or not isinstance(enabled, bool):
        raise ValueError('币种对或启用状态无效')
    symbol = symbol.upper()
    with users_lock:
        settings = symbol_settings()
        current = next(row for row in settings['rows'] if row['symbol'] == symbol)
        if current['enabled'] == enabled:
            return settings
        version = max(int(time.time() * 1000), settings['version'] + 1)
        event = {'name': symbol, 'time': version, 'actor': 'PSK管理员', 'enabled': enabled,
                 'provider': SYMBOL_PROVIDERS[symbol]}
        conn = _db()
        try:
            with conn:
                conn.execute('INSERT OR REPLACE INTO symbol_settings VALUES (?,?,?)', (symbol, int(enabled), version))
                conn.execute('INSERT INTO admin_events(kind,name,time,detail) VALUES (?,?,?,?)',
                             ('symbol', symbol, version, json.dumps(event, ensure_ascii=False)))
        finally:
            conn.close()
        settings = symbol_settings()
    _broadcast({'type': 'symbols', **settings})
    return settings


def admin_delete(name, confirmation):
    if not isinstance(name, str) or confirmation != name:
        raise ValueError('请准确输入要删除的账号名')
    with users_lock:
        if name not in USERS:
            raise ValueError('账号不存在')
        acc = user_state(name)['account']
        row = {'name': name, 'time': int(time.time() * 1000), 'actor': 'PSK管理员',
               'balance': acc.balance, 'positions': len(acc.positions)}
        conn = _db()
        try:
            with conn:
                conn.execute('INSERT INTO admin_events(kind,name,time,detail) VALUES (?,?,?,?)',
                             ('delete', name, row['time'], json.dumps(row, ensure_ascii=False)))
                for table in ('users', 'trades', 'chat', 'deposits', 'equity_samples'):
                    conn.execute('DELETE FROM ' + table + ' WHERE name=?', (name,))
        finally:
            conn.close()
        USERS.pop(name)
        USER_STATE.pop(name, None)
        for token, owner in list(TOKENS.items()):
            if owner == name:
                TOKENS.pop(token)
        _lb_cache['t'] = 0
    with _front_lock:
        sockets = [sock for sock in _front_clients if CLIENT_USER.get(sock) == name]
    for sock in sockets:
        _drop_client(sock)
    return row


def persist_user(name, funding=None):
    """余额、持仓及本次成交记录在同一个SQLite事务提交。"""
    with users_lock:
        st = USER_STATE[name]
        acc = st['account']
        account = acc_to_dict(acc)
        conn = _db()
        last = st.get('last_trade')
        try:
            with conn:
                changed = conn.execute('UPDATE users SET account=?,symbol=? WHERE name=?',
                                       (json.dumps(account, ensure_ascii=False), st['symbol'], name))
                if changed.rowcount != 1:
                    raise ValueError('账户不存在')
                if funding:
                    conn.execute('INSERT INTO deposits(request_id,name,time,amount,balance,actor) VALUES (?,?,?,?,?,?)',
                                 tuple(funding[key] for key in ('request_id', 'name', 'time', 'amount', 'balance', 'actor')))
                    conn.execute('INSERT INTO admin_events(kind,name,time,request_id,detail) VALUES (?,?,?,?,?)',
                                 ('deposit', name, funding['time'], funding['request_id'], json.dumps(funding, ensure_ascii=False)))
                for trade in acc.pending_trades:
                    cursor = conn.execute('INSERT INTO trades(name,' + ','.join(TRADE_FIELDS) +
                                          ') VALUES (' + ','.join('?' for _ in range(len(TRADE_FIELDS) + 1)) + ')',
                                          (name, *(trade[field] for field in TRADE_FIELDS)))
                    last = {'id': cursor.lastrowid, **trade}
        finally:
            conn.close()
        USERS[name]['account'] = account
        USERS[name]['symbol'] = st['symbol']
        st['last_trade'] = last
        acc.pending_trades.clear()
        _lb_cache['t'] = 0


def account_transaction(name, operation, funding=None):
    """串行修改账户；持久化失败恢复内存，禁止先平仓后丢记录。"""
    with users_lock:
        acc = user_state(name)['account']
        before = (acc.balance, acc.leverage, acc.positions[:], acc.pending_trades[:])
        try:
            result = operation(acc)
            if funding is None:
                persist_user(name)
            else:
                persist_user(name, funding=funding)
            return result
        except Exception:
            acc.balance, acc.leverage, acc.positions, acc.pending_trades = before
            raise


MAX_QUOTE_AGE = 30  # 强平不使用过期行情；外部源轮询/WS继续尝试更新。


def _on_quote(symbol, price, quoted_at=None):
    try:
        fx_sim.market_price(price, symbol)
    except (ValueError, TypeError):
        return
    stamp = time.time() if quoted_at is None else quoted_at
    with users_lock:
        now = time.time()
        latest = _px_cache.get(symbol)
        if latest and latest[0] > stamp:
            return
        _px_cache[symbol] = (stamp, price)
        # ponytail: 全局账户锁串行结算；账户量大时再拆为账户级锁。
        for name in USERS:
            try:
                acc = user_state(name)['account']
                symbols = fx_sim.price_symbols(p.symbol for p in acc.positions)
                if symbol not in symbols:
                    continue
                quotes = {}
                for sym in symbols:
                    hit = _px_cache.get(sym)
                    if not hit or now - hit[0] > MAX_QUOTE_AGE:
                        raise ValueError('强平行情缺失或过期: ' + sym)
                    quotes[sym] = fx_sim.market_price(hit[1], sym)
                ml = acc.margin_level(quotes)
                if ml is not None and math.isfinite(ml) and ml < fx_sim.STOPOUT:
                    account_transaction(name, lambda account: account.check_stopout(quotes))
            except ValueError:
                continue  # 缺价不猜价，等待该币对下一次有效行情。
            except Exception as exc:
                print('[risk] settlement failed:', name, exc)


def _risk_price_loop():
    """无浏览器连接时也为真实币对持仓补充行情；模拟币逐tick检查。"""
    while True:
        with users_lock:
            symbols = fx_sim.price_symbols(p.symbol for name in USERS
                                           for p in user_state(name)['account'].positions if p.symbol) - {SIMSYM}
        for symbol in symbols:
            try:
                price = _price(symbol)
                hit = _px_cache.get(symbol)
                if hit:
                    _on_quote(symbol, hit[1], hit[0])
            except Exception as exc:
                print('[risk] quote failed:', symbol, exc)
        time.sleep(2)


def user_by_token(token):
    if not token:
        return None
    with users_lock:
        return TOKENS.get(token)


def register_user(name, pwd):
    if not (1 <= len(name) <= 16) or not pwd:
        raise ValueError("名字1-16字,密码必填")
    with users_lock:
        if name in USERS:
            raise ValueError("用户已存在")
        salt = os.urandom(4).hex()
        USERS[name] = {"salt": salt, "hash": _hash_pwd(pwd, salt),
                       "token": secrets.token_hex(16),
                       "symbol": "USDJPY",
                       "account": acc_to_dict(Account(cash=10000.0, leverage=100)),
                       "created": int(time.time())}
        tok = USERS[name]["token"]
        TOKENS[tok] = name
        save_users()
        return tok


def login_user(name, pwd):
    with users_lock:
        u = USERS.get(name)
        if not u or u["hash"] != _hash_pwd(pwd, u["salt"]):
            raise ValueError("用户名或密码错")
        u["token"] = secrets.token_hex(16)   # 重登换新token
        tok = u["token"]
        TOKENS[tok] = name
        save_users()
        return tok


import secrets  # noqa: E402

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def _json(self, obj, code=200):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _user(self):
        tok = self.headers.get("Authorization", "").replace("Bearer ", "")
        return user_by_token(tok)

    def do_GET(self):
        u = urllib.parse.urlparse(self.path)
        q = urllib.parse.parse_qs(u.query)
        if u.path == "/ws":
            name = user_by_token(q.get("token", [""])[0])
            if not name:
                self._json({"ok": False, "err": "auth"}, 401); return
            _serve_ws(self, name)
            return
        if u.path == "/api/klines":
            symbol = q.get("symbol", ["USDJPY"])[0].upper()
            interval = q.get("interval", ["15m"])[0]
            limit = min(int(q.get("limit", ["500"])[0]), 1000)
            try:
                if symbol == SIMSYM:
                    with SIM.lock:
                        payload = {"ok": True, "klines": SIM.klines(SIM_INTERVALS[interval], limit),
                                   "updated_at": SIM.updated_at}
                    self._json(payload); return
                elif symbol in BIQUOTE_SET:
                    try:
                        out = biquote_klines(symbol, interval, limit)
                    except Exception:
                        out = yahoo_klines(symbol, interval, limit)
                elif symbol in YAHOO_MAP:
                    out = yahoo_klines(symbol, interval, limit)
                else:
                    with urllib.request.urlopen(
                            KLINE_API.format(symbol, interval, limit), timeout=8) as r:
                        raw = json.load(r)
                    out = [[k[0], float(k[1]), float(k[2]), float(k[3]), float(k[4])] for k in raw]
                self._json({"ok": True, "klines": out})
            except Exception as e:
                self._json({"ok": False, "err": str(e)}, 502)
        elif u.path == "/api/state":
            name = self._user()
            if not name:
                self._json({"ok": False, "err": "auth"}, 401); return
            s = state_payload(name)
            if not s.get("ok"):
                self._json(s, 502); return
            self._json(s)
        elif u.path == "/api/me":
            name = self._user()
            if not name:
                self._json({"ok": False}, 401); return
            with users_lock:
                self._json({"ok": True, "name": name})
        elif u.path.startswith('/api/admin/'):
            token = self.headers.get('Authorization', '').removeprefix('Bearer ')
            if not admin_authorized(token):
                self._json({'ok': False, 'err': '管理员登录已失效'}, 401); return
            if u.path == '/api/admin/users':
                with users_lock:
                    totals = deposit_totals()
                    self._json({'ok': True, 'rows': [{'name': name, 'balance': user_state(name)['account'].balance,
                                'deposited': totals.get(name, 0), 'positions': len(user_state(name)['account'].positions)} for name in sorted(USERS)]})
            elif u.path == '/api/admin/deposits':
                self._json({'ok': True, 'rows': admin_deposits()})
            elif u.path == '/api/admin/symbols':
                self._json({'ok': True, **symbol_settings()})
            else:
                self._json({'ok': False}, 404)
        elif u.path == '/api/symbols':
            self._json({'ok': True, **symbol_settings()})
        elif u.path == '/api/chat':
            if not self._user():
                self._json({'ok': False, 'err': 'auth'}, 401); return
            self._json({'ok': True, 'rows': chat_history()})
        elif u.path == '/api/history':
            name = self._user()
            if not name:
                self._json({'ok': False, 'err': 'auth'}, 401); return
            self._json({'ok': True, 'rows': trade_history(name)})
        elif u.path == "/api/leaderboard":
            self._json({"ok": True, "rows": leaderboard()})
        elif u.path in STATIC_ROUTES:
            filename, mime = STATIC_ROUTES[u.path]
            path = (STATIC_DIR / filename).resolve()
            if not path.is_relative_to(STATIC_DIR) or not path.is_file():
                self._json({'ok': False}, 404); return
            body = path.read_bytes()
            self.send_response(200)
            self.send_header('Content-Type', mime)
            immutable = u.path in ('/assets/lightweight-charts-5.2.1.js', '/assets/noto-sans-latin.woff2')
            self.send_header('Cache-Control', 'public, max-age=31536000, immutable' if immutable else 'no-cache')
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        else:
            self._json({"ok": False}, 404)

    def do_POST(self):
        u = urllib.parse.urlparse(self.path)
        try:
            n = int(self.headers.get('Content-Length', 0))
            if not 0 <= n <= 65536:
                self._json({'ok': False, 'err': '请求过大'}, 413); return
            req = json.loads(self.rfile.read(n) or b'{}')
            if not isinstance(req, dict):
                raise ValueError('请求格式错误')
        except (ValueError, TypeError):
            self._json({'ok': False, 'err': '请求格式错误'}, 400); return
        if u.path.startswith('/api/admin/'):
            try:
                if u.path == '/api/admin/login':
                    self._json({'ok': True, 'token': admin_login(req.get('psk'))}); return
                token = self.headers.get('Authorization', '').removeprefix('Bearer ')
                if not admin_authorized(token):
                    self._json({'ok': False, 'err': '管理员登录已失效'}, 401); return
                if u.path == '/api/admin/deposit':
                    self._json({'ok': True, 'row': admin_deposit(req.get('name'), req.get('amount'), req.get('request_id'))})
                elif u.path == '/api/admin/delete':
                    self._json({'ok': True, 'row': admin_delete(req.get('name'), req.get('confirmation'))})
                elif u.path == '/api/admin/symbols':
                    self._json({'ok': True, **admin_set_symbol(req.get('symbol'), req.get('enabled'))})
                elif u.path == '/api/admin/logout':
                    with users_lock:
                        ADMIN_SESSIONS.pop(token, None)
                    self._json({'ok': True})
                else:
                    self._json({'ok': False}, 404)
            except (ValueError, TypeError, OverflowError) as e:
                self._json({'ok': False, 'err': str(e)}, 400)
            except sqlite3.Error:
                self._json({'ok': False, 'err': '管理员操作保存失败，数据未改动；可重试'}, 503)
            return
        if u.path == "/api/auth":
            try:
                if req.get("action") == "register":
                    tok = register_user(req["name"].strip(), req["pwd"])
                else:
                    tok = login_user(req["name"].strip(), req["pwd"])
                self._json({"ok": True, "token": tok, "name": req["name"].strip()})
            except (ValueError, KeyError) as e:
                self._json({"ok": False, "err": str(e)}, 400)
            return
        name = self._user()
        if not name:
            self._json({"ok": False, "err": "auth"}, 401); return
        st = user_state(name)
        acc = st["account"]
        try:
            if u.path == '/api/chat':
                self._json({'ok': True, 'row': post_chat(name, req.get('text'))}); return
            elif u.path == "/api/trade":
                side = {'buy': 'long', 'long': 'long', 'sell': 'short', 'short': 'short'}[req['side']]
                with users_lock:
                    require_tradable_symbol(st['symbol'])
                    prices = _prices({st["symbol"]} | {p.symbol for p in acc.positions})
                    def open_position(account):
                        account.open(side, float(req['lots']), prices, symbol=st['symbol'])
                        account.check_stopout(prices)
                    account_transaction(name, open_position)
            elif u.path == "/api/close":
                with users_lock:
                    idx = int(req["id"]) if req.get("id") else None
                    prices = _prices(p.symbol for p in acc.positions if idx is None or p.id == idx)
                    account_transaction(name, lambda account: account.close(idx, prices))
            elif u.path == "/api/leverage":
                with users_lock:
                    if acc.positions:
                        raise ValueError("先平仓")
                    leverage = int(req['leverage'])
                    if isinstance(req['leverage'], bool) or leverage != float(req['leverage']) or not 1 <= leverage <= 9007199254740991:
                        raise ValueError('杠杆必须为有效正整数')
                    account_transaction(name, lambda account: setattr(account, 'leverage', leverage))
            elif u.path == "/api/symbol":
                if not isinstance(req.get('symbol'), str):
                    raise ValueError('币种对无效')
                sym = req['symbol'].upper()
                with users_lock:
                    require_tradable_symbol(sym)
                    before = st['symbol']
                    st['symbol'] = sym
                    try:
                        persist_user(name)
                    except Exception:
                        st['symbol'] = before
                        raise
            else:
                self._json({"ok": False, "err": "unknown"}, 404); return
            self._json({"ok": True})
        except (ValueError, KeyError, TypeError, OverflowError) as e:
            self._json({"ok": False, "err": str(e)}, 400)
        except sqlite3.Error:
            self._json({'ok': False, 'err': '成交保存失败，账户已回滚，请重试'}, 503)


def state_payload(name) -> dict:
    st = user_state(name)
    acc = st["account"]
    with users_lock:
        try:
            prices = _prices({st["symbol"]} | {p.symbol for p in acc.positions})
        except ValueError as e:
            return {"ok": False, "err": str(e)}
        price = prices[st["symbol"]]
        used = acc.used_margin(prices)
        poses = []
        for p in acc.positions:
            px = prices[p.symbol]
            poses.append({"id": p.id, "side": p.side, "lots": p.lots,
                          "sym": p.symbol,
                          "entry": p.entry, "now": px, "pnl": p.pnl(prices),
                          "margin": p.margin(prices, acc.leverage)})
        eq = acc.equity(prices)
        return {"ok": True, "type": "state", "name": name, "symbol": st["symbol"], "price": price,
                "balance": acc.balance, "equity": eq, "used": used, "free": eq - used,
                "margin_level": (eq / used if used else None), "leverage": acc.leverage,
                "positions": poses, "stopout_level": fx_sim.STOPOUT, "currency": "USD",
                "last_trade": st.get('last_trade')}


_lb_cache = {"t": 0, "rows": []}


def five_minute_returns(rows, totals, now):
    """实际净值快照，扣除入金；基线须在300至315秒前，停服缺口不冒充5分钟。"""
    conn = _db()
    try:
        with conn:
            for row in rows:
                deposited = totals.get(row['name'], 0)
                baseline = conn.execute('SELECT equity,deposited FROM equity_samples '
                    'WHERE name=? AND time BETWEEN ? AND ? ORDER BY time DESC LIMIT 1',
                    (row['name'], now - 315, now - 300)).fetchone()
                row['pnl_5m'] = row['ret_5m'] = None
                if baseline:
                    pnl = row['equity'] - baseline[0] - (deposited - baseline[1])
                    row['pnl_5m'] = round(pnl, 2)
                    if baseline[0] > 0:
                        row['ret_5m'] = pnl / baseline[0]
                conn.execute('INSERT OR IGNORE INTO equity_samples VALUES (?,?,?,?)',
                             (row['name'], int(now) // 5 * 5, row['equity'], deposited))
            conn.execute('DELETE FROM equity_samples WHERE time<?', (now - 900,))
    finally:
        conn.close()


def leaderboard():
    now = time.time()
    if now - _lb_cache["t"] < 3:
        return _lb_cache["rows"]
    rows = []
    with users_lock:
        totals = deposit_totals()
        with _front_lock:
            online = {CLIENT_USER.get(sock) for sock in _front_clients}
        for name, u in USERS.items():
            try:
                st = user_state(name)
                acc = st["account"]
                prices = _prices(p.symbol for p in acc.positions)
                eq = acc.equity(prices)
                rows.append({"name": name, "equity": round(eq, 2),
                             "ret": round(eq / (10000 + totals.get(name, 0)) - 1, 4),
                             "pos": len(acc.positions), "sym": st["symbol"], "online": name in online})
            except Exception:
                continue
        five_minute_returns(rows, totals, now)
    rows.sort(key=lambda r: -r["ret"])
    _lb_cache["t"] = now
    _lb_cache["rows"] = rows
    return rows


# ---- WS: 每连接绑定用户; state 按用户推 ----
CLIENT_USER: dict = {}  # sock -> name


def _serve_ws(handler, name):
    req = handler.headers
    key = req.get("Sec-WebSocket-Key", "")
    accept = base64.b64encode(hashlib.sha1((key + GUID).encode()).digest()).decode()
    resp = ("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n"
            f"Connection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n")
    handler.connection.sendall(resp.encode())
    sock = handler.connection
    sock.settimeout(60)
    with _front_lock:
        _front_clients.add(sock)
        CLIENT_USER[sock] = name
        _lb_cache['t'] = 0
    try:
        while True:
            op, _ = _ws_read_msg_client(sock)
            if op in (8, None):
                break
    except OSError:
        pass
    finally:
        with _front_lock:
            _front_clients.discard(sock)
            CLIENT_USER.pop(sock, None)
        try:
            sock.close()
        except OSError:
            pass


def _lb_push_loop():
    while True:
        time.sleep(5)
        try:
            rows = leaderboard()
        except Exception:
            continue
        _broadcast({"type": "lb", "rows": rows})


def _state_push_loop():
    while True:
        time.sleep(1)
        with _front_lock:
            by_user = {}
            for s in _front_clients:
                by_user.setdefault(CLIENT_USER.get(s), []).append(s)
        for name, socks in by_user.items():
            if not name:
                continue
            try:
                s = state_payload(name)
            except Exception:
                continue
            if s.get("ok"):
                for sk in socks:
                    _ws_send(sk, json.dumps(s))





def main():
    import sys
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8000
    load_users()
    SIM.load_history(DB_FILE)
    _th.Thread(target=_binance_ws_loop, daemon=True).start()
    _th.Thread(target=_yahoo_push_loop, daemon=True).start()
    _th.Thread(target=_state_push_loop, daemon=True).start()
    _th.Thread(target=_lb_push_loop, daemon=True).start()
    _th.Thread(target=SIM.tick_loop, daemon=True).start()
    _th.Thread(target=_risk_price_loop, daemon=True).start()
    srv = ThreadingHTTPServer(("0.0.0.0", port), Handler)
    print(f"== FX对战 :: http://localhost:{port} ==")
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
