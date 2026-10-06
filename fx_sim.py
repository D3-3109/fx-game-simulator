#!/usr/bin/env python3
"""外汇保证金交易模拟器 (FX margin trading sim). 价格来自 Binance 公共 API, 无需 key.

Usage:
  python3 fx_sim.py                     # EURUSDT 默认
  python3 fx_sim.py GBPUSDT            # 任意 Binance 符号
  python3 fx_sim.py --leverage 200     # 杠杆倍数, 默认 100
  python3 fx_sim.py --interval 2       # 轮询秒数

REPL 命令:
  buy 0.1        做多 0.1 手 (1 手 = 100000 基础货币)
  sell 0.1       做空 0.1 手
  close          平掉全部持仓 / close 2 平第 2 笔
  pos            持仓明细
  margin         账户保证金状态
 leverage 50     改杠杆 (无持仓时)
  price          当前价
  cash           余额
  quit           退出并结算

保证金规则:
  强平线: margin level (equity/used margin) < 50% 时按亏损最大持仓强平
  点值: 1 pip = 0.0001 (EURUSD 类), JPY 对 0.01
"""
from __future__ import annotations
import json
import math
import os
import sys
import time
import urllib.request

ENDPOINTS = [
    "https://data-api.binance.vision/api/v3/ticker/price?symbol={}",
    "https://api.binance.com/api/v3/ticker/price?symbol={}",
    "https://api1.binance.com/api/v3/ticker/price?symbol={}",
]
CONTRACT = 100_000      # 1 标准手
STOPOUT = 0.50          # 强平线: margin level 50%
SPREAD = 0.0002         # 简化固定点差 2 pips


PROXY: str | None = None
for _k in ("FX_PROXY", "HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"):
    if _v := os.environ.get(_k):
        PROXY = _v
        break


def pip_size(symbol: str) -> float:
    return 0.01 if "JPY" in symbol else 0.0001


def fetch_price(symbol: str, proxy: str | None = None) -> float:
    proxy = PROXY if proxy is None else proxy
    handlers = [urllib.request.ProxyHandler({"http": proxy, "https": proxy})] if proxy else []
    opener = urllib.request.build_opener(*handlers)
    last: Exception = RuntimeError("no endpoints")
    for api in ENDPOINTS:
        try:
            with opener.open(api.format(symbol), timeout=5) as r:
                return float(json.load(r)["price"])
        except Exception as e:
            last = e
    raise last


def market_price(price: float | dict[str, float], symbol: str = "") -> float:
    """兼容 CLI/GUI 单价；多币对账户必须提供完整的币对价格快照。"""
    if isinstance(price, dict):
        if not symbol:
            raise ValueError("旧持仓缺少币对，无法安全结算")
        price = price.get(symbol)
    if price is None or not math.isfinite(price) or price <= 0:
        raise ValueError(f"行情不可用: {symbol}")
    return price


USD_CONVERSION = {'JPY': 'USDJPY', 'CHF': 'USDCHF', 'CAD': 'USDCAD',
                  'CNY': 'USDCNY', 'CNH': 'USDCNH'}


def quote_currency(symbol: str) -> str:
    if not symbol or symbol == 'FXSIM' or symbol.endswith(('USD', 'USDT')):
        return 'USD'  # 模拟器把USDT按USD等值计价。
    for currency in USD_CONVERSION:
        if symbol.endswith(currency):
            return currency
    raise ValueError(f'不支持的结算币种: {symbol}')


def price_symbols(symbols) -> set[str]:
    symbols = set(symbols)
    return symbols | {USD_CONVERSION[quote_currency(s)] for s in symbols
                      if quote_currency(s) != 'USD'}


def usd_rate(prices: float | dict[str, float], symbol: str) -> float:
    currency = quote_currency(symbol)
    if currency == 'USD':
        return 1.
    conversion = USD_CONVERSION[currency]
    if not isinstance(prices, dict) and conversion != symbol:
        raise ValueError(f'美元换算缺少行情: {conversion}')
    return 1. / market_price(prices, conversion)


def spread_for(symbol: str) -> float:
    return SPREAD * (100 if symbol.endswith('JPY') else 1)


class Position:
    _seq = 0

    def __init__(self, side: str, lots: float, entry: float, symbol: str = ""):
        Position._seq += 1
        self.id = Position._seq
        self.side = side          # long / short
        self.lots = lots
        self.entry = entry
        self.symbol = symbol      # 所属币对, 跨对持仓各按各的价算

    def pnl(self, price: float | dict[str, float], prices=None) -> float:
        quote = market_price(price, self.symbol)
        diff = quote - self.entry if self.side == 'long' else self.entry - quote
        return diff * self.lots * CONTRACT * usd_rate(price if prices is None else prices, self.symbol)

    def margin(self, price: float | dict[str, float], leverage: int) -> float:
        return self.lots * CONTRACT * market_price(price, self.symbol) * usd_rate(price, self.symbol) / leverage


class Account:
    def __init__(self, cash: float, leverage: int):
        self.balance = cash       # 已实现余额 (USD)
        self.leverage = leverage
        self.positions: list[Position] = []
        self.pending_trades: list[dict] = []  # Web 与账户余额同事务写入，成功后清空。

    # ---- 账户指标 ----
    def equity(self, price: float | dict[str, float]) -> float:
        return self.balance + sum(p.pnl(price) for p in self.positions)

    def used_margin(self, price: float | dict[str, float]) -> float:
        return sum(p.margin(price, self.leverage) for p in self.positions)

    def free_margin(self, price: float | dict[str, float]) -> float:
        return self.equity(price) - self.used_margin(price)

    def margin_level(self, price: float | dict[str, float]) -> float | None:
        used = self.used_margin(price)
        return self.equity(price) / used if used else None

    # ---- 操作 ----
    def open(self, side: str, lots: float, price: float | dict[str, float], symbol: str = ""):
        if not math.isfinite(lots) or lots < 0.01:
            raise ValueError("手数必须为有限数且不少于0.01")
        quote = market_price(price, symbol)
        spread = spread_for(symbol)
        px = quote + spread / 2 if side == 'long' else quote - spread / 2
        need = lots * CONTRACT * px * usd_rate(price, symbol) / self.leverage  # USD；不造临时Position
        free = self.free_margin(price)
        if need > free:
            raise ValueError(f"保证金不足: 需 ${need:.2f}，可用 ${free:.2f}（USD）")
        position = Position(side, lots, px, symbol=symbol)
        self.positions.append(position)
        self._record_trade("open", position, px, None)
        print(f"OPEN {side.upper():5} {lots} lot @ {px:.5f}  margin {need:.2f}")

    def _record_trade(self, kind, position, price, pnl, margin_level=None):
        self.pending_trades.append({"time": int(time.time() * 1000), "kind": kind,
                                    "position_id": position.id, "symbol": position.symbol,
                                    "side": position.side, "lots": position.lots,
                                    "entry": position.entry, "price": price, "pnl": pnl,
                                    "balance": self.balance, "margin_level": margin_level, "currency": "USD"})

    def _settle(self, position, quote, prices, kind="close", margin_level=None):
        spread = spread_for(position.symbol)
        px = quote - spread / 2 if position.side == 'long' else quote + spread / 2
        pnl = position.pnl(px, prices)
        self.balance += pnl
        self.positions.remove(position)
        self._record_trade(kind, position, px, pnl, margin_level)
        prefix = "*** STOPOUT" if kind == "stopout" else "CLOSE"
        print(f"{prefix} #{position.id} {position.side:5} {position.lots} lot "
              f"{position.entry:.5f} -> {px:.5f}  pnl {pnl:+.2f}")

    def close(self, idx: int | None, price: float | dict[str, float]):
        if not self.positions:
            raise ValueError("无持仓")
        if idx is None:
            targets = list(self.positions)
        else:
            targets = [p for p in self.positions if p.id == idx]
            if not targets:
                raise ValueError(f"无 # {idx} 持仓")
        # 先验证全部目标行情，缺价时不允许部分平仓。
        quotes = [(p, market_price(price, p.symbol)) for p in targets]
        for p, _ in quotes:
            usd_rate(price, p.symbol)  # 缺换算行情时，也不能部分平仓。
        for p, quote in quotes:
            self._settle(p, quote, price)

    def check_stopout(self, price: float | dict[str, float]) -> bool:
        """强平: 亏最多的一笔, 直到 margin level >= 强平线或无仓. 返回是否触发."""
        fired = False
        while self.positions:
            ml = self.margin_level(price)
            if ml is not None and ml >= STOPOUT:
                break
            worst = min(self.positions, key=lambda p: p.pnl(price))
            quote = market_price(price, worst.symbol)
            self._settle(worst, quote, price, 'stopout', ml)
            fired = True
        return fired

    def status(self, price: float | dict[str, float]):
        eq = self.equity(price)
        used = self.used_margin(price)
        ml = self.margin_level(price)
        print(f"余额 {self.balance:.2f} | 净值 {eq:.2f} | 已用保证金 {used:.2f} | "
              f"可用 {eq - used:.2f} | margin level {f'{ml*100:.0f}%' if ml else '-'} | "
              f"杠杆 1:{self.leverage}")
        for p in self.positions:
            print(f"  #{p.id} {p.side:5} {p.lots} lot @ {p.entry:.5f}  pnl {p.pnl(price):+.2f}")


def main():
    args = sys.argv[1:]
    interval, leverage = 5.0, 100
    global PROXY
    if "--proxy" in args:
        i = args.index("--proxy")
        PROXY = args[i + 1]
        del args[i:i + 2]
    for opt, conv in (("--interval", float), ("--leverage", int)):
        if opt in args:
            i = args.index(opt)
            conv(args[i + 1])
            {("--interval", None): None}
            if opt == "--interval":
                interval = float(args[i + 1])
            else:
                leverage = int(args[i + 1])
            del args[i:i + 2]
    symbol = args[0].upper() if args else "EURUSDT"
    pip = pip_size(symbol)

    acc = Account(cash=10000.0, leverage=leverage)
    price = fetch_price(symbol)
    print(f"== FX 保证金模拟 :: {symbol} :: {price:.5f} :: 1:{leverage} ==")
    print("cmds: buy/sell <lot> | close [id] | pos | margin | leverage <n> | price | quit")

    last_tick = 0.0
    while True:
        try:
            price = fetch_price(symbol)
        except Exception as e:
            print(f"[行情错误] {e}")
            time.sleep(interval)
            continue

        if time.time() - last_tick >= 10:
            print(f"  {symbol} {price:.5f}")
            last_tick = time.time()

        cmd = input(">> ").strip() if sys.stdin.isatty() else ""
        if not cmd:
            time.sleep(0.2)
            continue
        parts = cmd.split()
        try:
            c = parts[0]
            if c == "quit":
                acc.close(None, price)
                acc.status(price)
                break
            elif c in ("buy", "sell"):
                acc.open("long" if c == "buy" else "short", float(parts[1]), price)
            elif c == "close":
                acc.close(int(parts[1]) if len(parts) > 1 else None, price)
            elif c in ("pos", "status"):
                acc.status(price)
            elif c == "margin":
                acc.status(price)
            elif c == "leverage":
                if acc.positions:
                    print("先平仓再改杠杆")
                else:
                    leverage = acc.leverage = int(parts[1])
                    print(f"杠杆 1:{leverage}")
            elif c == "price":
                print(f"{symbol} {price:.5f}")
            elif c == "cash":
                print(f"余额 {acc.balance:.2f}")
            else:
                print("? cmds: buy/sell <lot>, close [id], pos, margin, leverage <n>, price, quit")
            acc.check_stopout(price)
        except (ValueError, IndexError) as e:
            print(f"err: {e}")


if __name__ == "__main__":
    main()
