"""Deterministic oracle; imports only the old pure core, never the live DB."""
import contextlib
import io
import json
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
import fx_sim

fixtures = []
for symbol, price, changed, conversion in [
    ('USDJPY', 150, 151, {}), ('EURJPY', 165, 164, {'USDJPY': 150}),
    ('EURUSD', 1.1, 1.105, {}), ('BTCUSDT', 60000, 59999, {}),
    ('USDCHF', .85, .86, {}), ('USDCAD', 1.35, 1.34, {}),
    ('USDCNY', 7.2, 7.25, {}), ('EURCNH', 7.8, 7.79, {'USDCNH': 7.2}),
    ('FXSIM', 1.1, .99, {}),
]:
    for side in ('long', 'short'):
        fx_sim.Position._seq = 0
        acc = fx_sim.Account(10000000, 100)
        initial = {symbol: price, **conversion}
        later = {symbol: changed, **conversion}
        with contextlib.redirect_stdout(io.StringIO()):
            acc.open(side, .1, initial, symbol)
            opened = {'equity': acc.equity(initial), 'used': acc.used_margin(initial),
                      'margin_level': acc.margin_level(initial), 'entry': acc.positions[0].entry}
            acc.close(None, later)
        trades = [{**t, 'time': 123} for t in acc.pending_trades]
        fixtures.append(dict(symbol=symbol, side=side, initial=initial, later=later,
                             opened=opened, balance=acc.balance, trades=trades))
print(json.dumps(fixtures))
