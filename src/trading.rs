//! Pure USD accounting. No storage, network, or wall-clock access.
use crate::symbols;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const CONTRACT: f64 = 100_000.0;
pub const STOPOUT: f64 = 0.5;
pub const MAX_LEVERAGE: u64 = 9_007_199_254_740_991;
pub const LOAN_RATE: f64 = 0.10;
fn cents(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}
#[derive(Debug, Clone, Default)]
pub struct Prices {
    pub mid: BTreeMap<String, f64>,
    pub bid_ask: BTreeMap<String, (f64, f64)>,
}
impl Prices {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn remove(&mut self, symbol: &str) -> Option<f64> {
        self.bid_ask.remove(symbol);
        self.mid.remove(symbol)
    }
    pub fn insert(&mut self, symbol: String, price: f64) -> Option<f64> {
        self.bid_ask.remove(&symbol);
        self.mid.insert(symbol, price)
    }
}
impl std::ops::Deref for Prices {
    type Target = BTreeMap<String, f64>;
    fn deref(&self) -> &Self::Target {
        &self.mid
    }
}
impl FromIterator<(String, f64)> for Prices {
    fn from_iter<T: IntoIterator<Item = (String, f64)>>(iter: T) -> Self {
        Self {
            mid: iter.into_iter().collect(),
            bid_ask: BTreeMap::new(),
        }
    }
}
impl<const N: usize> From<[(String, f64); N]> for Prices {
    fn from(pairs: [(String, f64); N]) -> Self {
        pairs.into_iter().collect()
    }
}
pub type Result<T> = std::result::Result<T, String>;

pub fn market_price(prices: &Prices, symbol: &str) -> Result<f64> {
    if symbol.is_empty() {
        return Err("旧持仓缺少币对，无法安全结算".into());
    }
    prices
        .get(symbol)
        .copied()
        .filter(|p| p.is_finite() && *p > 0.0)
        .ok_or_else(|| format!("行情不可用: {symbol}"))
}

pub fn conversion(symbol: &str) -> Result<Option<&'static str>> {
    if symbol.is_empty() || symbol == "FXSIM" || symbol.ends_with("USD") || symbol.ends_with("USDT")
    {
        return Ok(None);
    }
    if let Some(spec) = symbols::spec(symbol) {
        return Ok(spec.settle);
    }
    for (currency, pair) in [
        ("JPY", "USDJPY"),
        ("CHF", "USDCHF"),
        ("CAD", "USDCAD"),
        ("CNY", "USDCNY"),
        ("CNH", "USDCNH"),
    ] {
        if symbol.ends_with(currency) {
            return Ok(Some(pair));
        }
    }
    Err(format!("不支持的结算币种: {symbol}"))
}

pub fn price_symbols<'a>(symbols: impl IntoIterator<Item = &'a str>) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for symbol in symbols {
        if symbol.is_empty() {
            return Err("旧持仓缺少币对，无法安全结算".into());
        }
        out.insert(symbol.to_owned());
        if let Some(pair) = conversion(symbol)? {
            out.insert(pair.to_owned());
        }
    }
    Ok(out)
}

pub fn usd_rate(prices: &Prices, symbol: &str) -> Result<f64> {
    Ok(match conversion(symbol)? {
        None => 1.0,
        Some(pair) => 1.0 / market_price(prices, pair)?,
    })
}

pub fn contract(symbol: &str) -> f64 {
    symbols::spec(symbol).map_or(CONTRACT, |s| s.contract)
}

pub fn lots_valid(symbol: &str, lots: f64) -> bool {
    let (min, whole) = symbols::spec(symbol).map_or((0.01, false), |s| (s.min_lots, s.whole_lots));
    lots.is_finite() && lots >= min && (!whole || lots.fract() == 0.0)
}

pub fn spread(symbol: &str, mid: f64) -> f64 {
    match symbols::spec(symbol) {
        Some(spec) if spec.pct > 0.0 => mid * spec.pct,
        Some(spec) => spec.spread,
        None if symbol.ends_with("JPY") => 0.02,
        None => 0.0002,
    }
}

/// Source bid/ask when present; keep the original simulated spread for offline/non-FX feeds.
pub fn execution_price(prices: &Prices, symbol: &str, buy: bool) -> Result<f64> {
    let mid = market_price(prices, symbol)?;
    let px = if let Some(&(bid, ask)) = prices.bid_ask.get(symbol) {
        if !bid.is_finite() || !ask.is_finite() || bid <= 0.0 || ask < bid {
            return Err("买卖报价无效".into());
        }
        if buy { ask } else { bid }
    } else {
        mid + if buy {
            spread(symbol, mid) / 2.0
        } else {
            -spread(symbol, mid) / 2.0
        }
    };
    if !px.is_finite() || px <= 0.0 {
        return Err("成交价无效".into());
    }
    Ok(px)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Position {
    pub id: u64,
    pub side: String,
    pub lots: f64,
    pub entry: f64,
    #[serde(default)]
    pub symbol: String,
}

impl Position {
    pub fn pnl_at(&self, quote: f64, prices: &Prices) -> Result<f64> {
        let diff = if self.side == "long" {
            quote - self.entry
        } else {
            self.entry - quote
        };
        finite(diff * self.lots * contract(&self.symbol) * usd_rate(prices, &self.symbol)?)
    }
    pub fn pnl(&self, prices: &Prices) -> Result<f64> {
        let quote = if prices.bid_ask.contains_key(&self.symbol) {
            execution_price(prices, &self.symbol, self.side == "short")?
        } else {
            market_price(prices, &self.symbol)?
        };
        self.pnl_at(quote, prices)
    }
    pub fn margin(&self, prices: &Prices, leverage: u64) -> Result<f64> {
        finite(
            self.lots
                * contract(&self.symbol)
                * market_price(prices, &self.symbol)?
                * usd_rate(prices, &self.symbol)?
                / leverage as f64,
        )
    }
}

fn one() -> u64 {
    1
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Account {
    pub balance: f64,
    pub leverage: u64,
    #[serde(default = "one")]
    pub next_id: u64,
    pub positions: Vec<Position>,
    #[serde(default)]
    pub loan: f64,
    #[serde(default)]
    pub interest_at: i64,
    #[serde(default)]
    pub debt_since: i64,
}

pub const DEBT_LIMIT: f64 = 100_000.0;
pub const DEBT_GRACE_MS: i64 = 30 * 60 * 1000;

impl Default for Account {
    fn default() -> Self {
        Self {
            balance: 10000.0,
            leverage: 100,
            next_id: 1,
            positions: vec![],
            loan: 0.0,
            interest_at: 0,
            debt_since: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Trade {
    pub time: i64,
    pub kind: String,
    pub position_id: u64,
    pub symbol: String,
    pub side: String,
    pub lots: f64,
    pub entry: f64,
    pub price: f64,
    pub pnl: Option<f64>,
    pub balance: f64,
    pub margin_level: Option<f64>,
    pub currency: String,
}

fn finite(value: f64) -> Result<f64> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err("计算结果超出有效范围".into())
    }
}

impl Account {
    pub fn validate(&self) -> Result<()> {
        finite(self.balance)?;
        if !(1..=MAX_LEVERAGE).contains(&self.leverage) {
            return Err("杠杆必须为有效正整数".into());
        }
        for p in &self.positions {
            if !matches!(p.side.as_str(), "long" | "short")
                || !p.lots.is_finite()
                || !lots_valid(&p.symbol, p.lots)
                || !p.entry.is_finite()
                || p.entry <= 0.0
            {
                return Err("持仓记录无效".into());
            }
        }
        Ok(())
    }
    pub fn accrue(&mut self, now: i64) {
        if self.loan <= 0.0 {
            self.interest_at = now;
            return;
        }
        let hours = (now - self.interest_at) / 3_600_000;
        if hours <= 0 {
            return;
        }
        let growth = (1.0 + LOAN_RATE / 8760.0).powf(hours.min(100_000) as f64);
        self.loan = cents(self.loan * growth);
        self.interest_at += hours * 3_600_000;
    }
    pub fn borrow(&mut self, amount: f64, now: i64) -> Result<()> {
        if !amount.is_finite() || amount <= 0.0 {
            return Err("借款金额必须为正数".into());
        }
        self.accrue(now);
        self.loan = cents(self.loan + amount);
        self.balance += amount;
        self.validate()
    }
    pub fn repay(&mut self, amount: f64, now: i64) -> Result<()> {
        if !amount.is_finite() || amount <= 0.0 {
            return Err("还款金额必须为正数".into());
        }
        self.accrue(now);
        let pay = amount.min(self.loan).min(self.balance.max(0.0));
        if pay <= 0.0 {
            return Err("余额不足，无法还款".into());
        }
        self.loan = cents(self.loan - pay);
        self.balance -= pay;
        self.validate()
    }
    pub fn debt(&self) -> f64 {
        self.loan + (-self.balance).max(0.0)
    }
    /// Starts, clears, or reports expiry of the 30-minute debt grace period.
    pub fn debt_expired(&mut self, now: i64) -> bool {
        if self.debt() <= DEBT_LIMIT {
            self.debt_since = 0;
            return false;
        }
        if self.debt_since == 0 {
            self.debt_since = now;
            return false;
        }
        now - self.debt_since >= DEBT_GRACE_MS
    }
    pub fn reset(&mut self, now: i64) -> Trade {
        let balance = Account::default().balance;
        let trade = Trade {
            time: now,
            kind: "reset".into(),
            position_id: 0,
            symbol: String::new(),
            side: String::new(),
            lots: 0.0,
            entry: 0.0,
            price: 0.0,
            pnl: None,
            balance,
            margin_level: None,
            currency: "USD".into(),
        };
        let next_id = self.next_id;
        *self = Account {
            next_id,
            ..Account::default()
        };
        trade
    }
    pub fn loan_trade(&self, kind: &str, amount: f64, now: i64) -> Trade {
        Trade {
            time: now,
            kind: kind.into(),
            position_id: 0,
            symbol: String::new(),
            side: String::new(),
            lots: 0.0,
            entry: 0.0,
            price: amount,
            pnl: None,
            balance: self.balance,
            margin_level: None,
            currency: "USD".into(),
        }
    }
    pub fn equity(&self, prices: &Prices) -> Result<f64> {
        finite(
            self.balance - self.loan
                + self
                    .positions
                    .iter()
                    .map(|p| p.pnl(prices))
                    .collect::<Result<Vec<_>>>()?
                    .iter()
                    .sum::<f64>(),
        )
    }
    pub fn used_margin(&self, prices: &Prices) -> Result<f64> {
        finite(
            self.positions
                .iter()
                .map(|p| p.margin(prices, self.leverage))
                .collect::<Result<Vec<_>>>()?
                .iter()
                .sum::<f64>(),
        )
    }
    pub fn margin_level(&self, prices: &Prices) -> Result<Option<f64>> {
        let used = self.used_margin(prices)?;
        if used == 0.0 {
            Ok(None)
        } else {
            Ok(Some(finite(self.equity(prices)? / used)?))
        }
    }
    pub fn open(
        &mut self,
        side: &str,
        lots: f64,
        symbol: &str,
        prices: &Prices,
        id: u64,
        now: i64,
    ) -> Result<Trade> {
        if !matches!(side, "long" | "short") {
            return Err("交易方向无效".into());
        }
        if !lots_valid(symbol, lots) {
            return Err(if symbols::spec(symbol).is_some_and(|s| s.whole_lots) {
                "美股须为整数股且不少于1股".into()
            } else {
                "手数必须为有限数且不少于0.01".into()
            });
        }
        self.validate()?;
        let px = execution_price(prices, symbol, side == "long")?;
        let need = finite(
            lots * contract(symbol) * px * usd_rate(prices, symbol)? / self.leverage as f64,
        )?;
        let free = self.equity(prices)? - self.used_margin(prices)?;
        if need > free {
            return Err(format!("保证金不足: 需 ${need:.2}，可用 ${free:.2}（USD）"));
        }
        let p = Position {
            id,
            side: side.into(),
            lots,
            entry: px,
            symbol: symbol.into(),
        };
        let trade = self.trade("open", &p, px, None, None, now);
        self.positions.push(p);
        self.next_id = self.next_id.max(id + 1);
        Ok(trade)
    }
    fn trade(
        &self,
        kind: &str,
        p: &Position,
        price: f64,
        pnl: Option<f64>,
        ml: Option<f64>,
        now: i64,
    ) -> Trade {
        Trade {
            time: now,
            kind: kind.into(),
            position_id: p.id,
            symbol: p.symbol.clone(),
            side: p.side.clone(),
            lots: p.lots,
            entry: p.entry,
            price,
            pnl,
            balance: self.balance,
            margin_level: ml,
            currency: "USD".into(),
        }
    }
    fn settle(
        &mut self,
        index: usize,
        prices: &Prices,
        kind: &str,
        ml: Option<f64>,
        now: i64,
    ) -> Result<Trade> {
        let p = &self.positions[index];
        let px = execution_price(prices, &p.symbol, p.side == "short")?;
        let pnl = p.pnl_at(px, prices)?;
        self.balance = finite(self.balance + pnl)?;
        let p = self.positions.remove(index);
        Ok(self.trade(kind, &p, px, Some(pnl), ml, now))
    }
    pub fn close(&mut self, id: Option<u64>, prices: &Prices, now: i64) -> Result<Vec<Trade>> {
        if self.positions.is_empty() {
            return Err("无持仓".into());
        }
        let targets: Vec<_> = self
            .positions
            .iter()
            .filter(|p| id.is_none_or(|id| p.id == id))
            .map(|p| p.id)
            .collect();
        if targets.is_empty() {
            return Err(format!("无 # {} 持仓", id.unwrap_or_default()));
        }
        // Work on a clone: unavailable conversion/overflow cannot partly close a basket.
        let mut next = self.clone();
        let mut trades = Vec::new();
        for id in targets {
            let index = next.positions.iter().position(|p| p.id == id).unwrap();
            trades.push(next.settle(index, prices, "close", None, now)?);
        }
        *self = next;
        Ok(trades)
    }
    pub fn check_stopout(&mut self, prices: &Prices, now: i64) -> Result<Vec<Trade>> {
        let mut next = self.clone();
        let mut trades = Vec::new();
        while !next.positions.is_empty() {
            let ml = next.margin_level(prices)?;
            if ml.is_none_or(|v| v >= STOPOUT) {
                break;
            }
            let pnls = next
                .positions
                .iter()
                .map(|p| p.pnl(prices))
                .collect::<Result<Vec<_>>>()?;
            let worst = pnls
                .iter()
                .enumerate()
                .min_by(|a, b| a.1.total_cmp(b.1))
                .unwrap()
                .0;
            trades.push(next.settle(worst, prices, "stopout", ml, now)?);
        }
        *self = next;
        Ok(trades)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn quotes(pairs: &[(&str, f64)]) -> Prices {
        pairs.iter().map(|(s, p)| (s.to_string(), *p)).collect()
    }
    fn near(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-7, "{a} != {b}");
    }
    #[test]
    fn jpy_margin_and_settlement_in_usd() {
        let mut a = Account::default();
        let mut q = quotes(&[("USDJPY", 150.0)]);
        a.open("long", 0.1, "USDJPY", &q, 1, 123).unwrap();
        near(a.used_margin(&q).unwrap(), 100.0);
        near(a.positions[0].pnl(&q).unwrap(), -100.0 / 150.0);
        q.insert("USDJPY".into(), 151.0);
        let t = a.close(Some(1), &q, 124).unwrap();
        near(t[0].pnl.unwrap(), 9800.0 / 151.0);
        near(a.balance, 10000.0 + 9800.0 / 151.0);
        assert_eq!(t[0].currency, "USD");
    }
    #[test]
    fn missing_conversion_or_legacy_symbol_never_partly_settles() {
        let mut a = Account::default();
        let q = quotes(&[("EURUSD", 1.1), ("EURJPY", 165.0), ("USDJPY", 150.0)]);
        a.open("long", 0.1, "EURUSD", &q, 1, 1).unwrap();
        a.open("short", 0.1, "EURJPY", &q, 2, 1).unwrap();
        let before = a.clone();
        let mut missing = q.clone();
        missing.remove("USDJPY");
        assert!(a.close(None, &missing, 2).is_err());
        assert_eq!(a, before);
        a.positions[1].symbol.clear();
        let before = a.clone();
        assert!(a.check_stopout(&q, 2).is_err());
        assert_eq!(a, before);
    }
    #[test]
    fn loan_borrow_repay_accrue_and_equity() {
        let mut a = Account::default();
        let q = quotes(&[]);
        a.borrow(1000.0, 0).unwrap();
        assert_eq!(a.balance, 11000.0);
        assert_eq!(a.loan, 1000.0);
        assert_eq!(a.equity(&q).unwrap(), 10000.0);
        a.accrue(3_600_000 * 8760);
        assert!((a.loan - 1105.17).abs() < 0.02);
        a.repay(500.0, 3_600_000 * 8760).unwrap();
        assert_eq!(a.balance, 10500.0);
        assert!(a.repay(-1.0, 0).is_err());
        assert!(a.repay(f64::NAN, 0).is_err());
        assert!(a.borrow(0.0, 0).is_err());
    }
    #[test]
    fn debt_grace_starts_clears_and_expires_into_reset() {
        let mut a = Account::default();
        let t0 = 1_000_000;
        a.borrow(DEBT_LIMIT + 1.0, t0).unwrap();
        assert!(!a.debt_expired(t0));
        assert_eq!(a.debt_since, t0);
        assert!(!a.debt_expired(t0 + DEBT_GRACE_MS - 1));
        a.repay(10.0, t0 + DEBT_GRACE_MS - 1).unwrap();
        assert!(!a.debt_expired(t0 + DEBT_GRACE_MS));
        assert_eq!(a.debt_since, 0);
        a.borrow(DEBT_LIMIT, t0 + DEBT_GRACE_MS).unwrap();
        let start = t0 + DEBT_GRACE_MS;
        assert!(!a.debt_expired(start + 1));
        assert!(a.debt_expired(start + 1 + DEBT_GRACE_MS));
        a.next_id = 9;
        let trade = a.reset(start + DEBT_GRACE_MS);
        assert_eq!(trade.kind, "reset");
        assert_eq!(a.balance, 10000.0);
        assert_eq!(a.loan, 0.0);
        assert_eq!(a.debt_since, 0);
        assert_eq!(a.next_id, 9);
    }
    #[test]
    fn stock_whole_shares_and_percent_spread() {
        assert!(lots_valid("AAPL", 1.0));
        assert!(lots_valid("AAPL", 10.0));
        assert!(!lots_valid("AAPL", 0.5));
        assert!(!lots_valid("AAPL", 1.5));
        assert!(lots_valid("BTCUSDT", 0.01));
        assert!(!lots_valid("BTCUSDT", 0.001));
        assert!((spread("AAPL", 200.0) - 0.1).abs() < 1e-9);
        assert_eq!(contract("AAPL"), 1.0);
        assert_eq!(contract("EURUSD"), 100_000.0);
        assert_eq!(conversion("AAPL").unwrap(), None);
    }
    #[test]
    fn cross_margin_and_spread() {
        let mut a = Account::default();
        let q = quotes(&[("EURJPY", 165.0), ("USDJPY", 150.0)]);
        a.open("long", 0.1, "EURJPY", &q, 1, 0).unwrap();
        near(a.used_margin(&q).unwrap(), 110.0);
        near(
            a.close(None, &q, 1).unwrap()[0].pnl.unwrap(),
            -200.0 / 150.0,
        );
    }
    #[test]
    fn stopout_boundary_and_worst_first() {
        let mut a = Account {
            balance: 50.0,
            positions: vec![Position {
                id: 1,
                side: "long".into(),
                lots: 0.1,
                entry: 1.0,
                symbol: "FXSIM".into(),
            }],
            ..Account::default()
        };
        assert!(
            a.check_stopout(&quotes(&[("FXSIM", 1.0)]), 0)
                .unwrap()
                .is_empty()
        );
        let t = a.check_stopout(&quotes(&[("FXSIM", 0.9999)]), 1).unwrap();
        assert_eq!(t.len(), 1);
        near(a.balance, 48.0);
        let p = |id, entry| Position {
            id,
            entry,
            side: "long".into(),
            lots: 0.1,
            symbol: "FXSIM".into(),
        };
        a.balance = 80.0;
        a.positions = vec![p(1, 1.0), p(2, 1.001)];
        let t = a.check_stopout(&quotes(&[("FXSIM", 1.0)]), 2).unwrap();
        assert_eq!(t[0].position_id, 2);
        assert_eq!(a.positions.len(), 1);
    }
    #[test]
    fn source_bid_ask_controls_fills_pnl_close_and_stopout() {
        let mut q = quotes(&[("USDCNH", 6.706195)]);
        q.bid_ask.insert("USDCNH".into(), (6.70493, 6.70746));
        for (side, entry, exit) in [("long", 6.70746, 6.70493), ("short", 6.70493, 6.70746)] {
            let mut a = Account::default();
            let open = a.open(side, 0.1, "USDCNH", &q, 1, 1).unwrap();
            near(open.price, entry);
            let pnl = a.positions[0].pnl(&q).unwrap();
            let close = a.close(None, &q, 2).unwrap();
            near(close[0].price, exit);
            near(close[0].pnl.unwrap(), pnl);
            near(pnl, -0.00253 * 10000.0 / 6.706195);
        }
        let mut a = Account::default();
        a.open("long", 0.1, "USDCNH", &q, 1, 1).unwrap();
        a.balance = 1.0;
        let stop = a.check_stopout(&q, 2).unwrap();
        near(stop[0].price, 6.70493);
        let mut invalid = q.clone();
        invalid.bid_ask.insert("USDCNH".into(), (6.8, 6.7));
        assert!(
            Account::default()
                .open("long", 0.1, "USDCNH", &invalid, 1, 0)
                .is_err()
        );
        q.insert("USDCNH".into(), 6.7);
        assert!(!q.bid_ask.contains_key("USDCNH"));
    }
    #[test]
    fn invalid_numbers_and_directions_do_not_mutate() {
        let mut a = Account::default();
        let q = quotes(&[("FXSIM", 1.1)]);
        for lots in [f64::NAN, f64::INFINITY, -1.0, 0.001, f64::MAX] {
            assert!(a.open("long", lots, "FXSIM", &q, 1, 0).is_err());
        }
        assert!(a.open("oops", 0.1, "FXSIM", &q, 1, 0).is_err());
        assert_eq!(a, Account::default());
        assert!(market_price(&quotes(&[("FXSIM", f64::NAN)]), "FXSIM").is_err());
    }
}
