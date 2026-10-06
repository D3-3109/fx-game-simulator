"""All monetary account metrics settle in USD, never raw JPY/CAD/CHF/CNY."""
import contextlib
import io
import unittest
from unittest.mock import patch

import fx_sim as core
import fx_sim_web as web


class CurrencyTests(unittest.TestCase):
    def setUp(self):
        self.output=contextlib.redirect_stdout(io.StringIO());self.output.__enter__()
        self.addCleanup(self.output.__exit__,None,None,None)

    def test_usdjpy_margin_is_dollars_and_two_pip_spread(self):
        acc=core.Account(10000,100)
        acc.open('long',.1,{'USDJPY':150},symbol='USDJPY')
        self.assertAlmostEqual(acc.positions[0].entry,150.01)
        self.assertAlmostEqual(acc.used_margin({'USDJPY':150}),100)
        self.assertAlmostEqual(acc.equity({'USDJPY':150}),10000-100/150)
        acc.close(None,{'USDJPY':151})
        self.assertAlmostEqual(acc.balance,10000+9800/151)
        self.assertAlmostEqual(acc.pending_trades[-1]['pnl'],9800/151)

    def test_dollar_base_pairs_all_have_same_margin_at_same_lots_leverage(self):
        for symbol,quote in [('USDJPY',150),('USDCHF',.9),('USDCAD',1.35),('USDCNY',7.1),('USDCNH',7.2)]:
            with self.subTest(symbol=symbol):
                pos=core.Position('long',.1,quote,symbol)
                self.assertAlmostEqual(pos.margin({symbol:quote},100),100)
                self.assertAlmostEqual(pos.pnl({symbol:quote*1.01}),10000*(quote*.01)/(quote*1.01))

    def test_cross_jpy_needs_usdjpy_and_uses_dollar_equity(self):
        pos=core.Position('long',.1,165,'EURJPY')
        prices={'EURJPY':166,'USDJPY':150}
        self.assertAlmostEqual(pos.margin(prices,100),100*166/150)
        self.assertAlmostEqual(pos.pnl(prices),10000/150)
        self.assertEqual(core.price_symbols({'EURJPY','GBPJPY'}),{'EURJPY','GBPJPY','USDJPY'})
        with patch.object(web,'_price',side_effect=prices.get):
            self.assertEqual(web._prices({'EURJPY'}),prices)

    def test_missing_conversion_prevents_partial_close_or_balance_mutation(self):
        acc=core.Account(10000,100)
        eur=core.Position('long',.1,1.1,'EURUSD')
        jpy=core.Position('long',.1,165,'EURJPY');acc.positions=[eur,jpy]
        for usd in (None,0,float('nan')):
            prices={'EURUSD':1.2,'EURJPY':166,'USDJPY':usd}
            with self.assertRaises(ValueError):acc.close(None,prices)
            self.assertEqual(acc.positions,[eur,jpy]);self.assertEqual(acc.balance,10000)
        with self.assertRaises(ValueError):jpy.pnl(166)

    def test_worst_position_compared_in_dollars_not_raw_quote_currency(self):
        acc=core.Account(270,100)
        jpy=core.Position('long',.1,151,'USDJPY')
        eur=core.Position('long',.1,1.11,'EURUSD');acc.positions=[jpy,eur]
        prices={'USDJPY':150,'EURUSD':1.10}
        self.assertTrue(acc.check_stopout(prices))
        self.assertEqual(acc.positions,[jpy])
        self.assertEqual(acc.pending_trades[-1]['symbol'],'EURUSD')
        self.assertAlmostEqual(acc.balance,169)
        self.assertGreaterEqual(acc.margin_level(prices),.5)

    def test_single_price_usdjpy_works_but_cross_pair_cannot_guess_conversion(self):
        pos=core.Position('short',.1,151,'USDJPY')
        self.assertAlmostEqual(pos.pnl(150),10000/150)
        self.assertAlmostEqual(pos.margin(150,100),100)
        with self.assertRaises(ValueError):core.usd_rate(165,'EURJPY')


if __name__=='__main__':unittest.main()
