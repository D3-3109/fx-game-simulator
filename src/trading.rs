//! Pure USD accounting. No storage, network, or wall-clock access.
use crate::symbols;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const CONTRACT: f64 = 100_000.0;
pub const STOPOUT: f64 = 0.5;
pub const MAX_LEVERAGE: u64 = 50_000;
pub const MAX_LOTS: f64 = 100_000.0;
pub const LOAN_RATE: f64 = 0.10;
/// Spot taker fee (0.1%, OKX's base tier) and order size bounds in units.
pub const SPOT_FEE: f64 = 0.001;
pub const SPOT_MIN: f64 = 0.0001;
pub const SPOT_MAX: f64 = 10_000.0;
/// Slowest the book anchor may be ignored: displacements beyond this many widths are
/// solved on the clamped value so `sinh`/`exp` stay finite for absurd inputs.
const AMM_MAX_C: f64 = 1e12;
fn cents(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}
#[derive(Debug, Clone, Default)]
pub struct Prices {
    pub mid: BTreeMap<String, f64>,
    pub bid_ask: BTreeMap<String, (f64, f64)>,
    /// Log-price anchor of each elastic book; absent means "centred on the current mid".
    pub anchor: BTreeMap<String, f64>,
}
impl Prices {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn remove(&mut self, symbol: &str) -> Option<f64> {
        self.bid_ask.remove(symbol);
        self.anchor.remove(symbol);
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
            ..Self::default()
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
    if symbol.is_empty()
        || symbol == "FXSIM"
        || symbol == "NSTAR"
        || symbol.ends_with("USD")
        || symbol.ends_with("USDT")
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

/// Units that move a simulated book by one log unit; 0 for externally quoted symbols.
pub fn depth(symbol: &str) -> f64 {
    symbols::spec(symbol).map_or(0.0, |s| s.depth)
}

/// Leverage applied to one symbol: the account setting, never above the symbol's ceiling.
pub fn leverage_for(symbol: &str, leverage: u64) -> u64 {
    match symbols::spec(symbol) {
        Some(s) if s.max_leverage > 0 => leverage.min(s.max_leverage),
        _ => leverage,
    }
}

/// Elastic AMM width of a book symbol (0: constant depth).
pub fn amm_width(symbol: &str) -> f64 {
    symbols::spec(symbol).map_or(0.0, |s| s.amm_width)
}

/// Book inventory curve: liquidity at displacement `x` (log price minus anchor) is
/// `depth · (1 + x²/w²)`, so the units absorbed between the anchor and `x` are
/// `depth · (x + x³ / 3w²)`. This returns the displacement after trading `units`
/// (positive buys) from `x0`. The curve is a pure function of inventory, so a fixed anchor
/// keeps every fill path-independent: split orders cost the same and a round trip lands
/// back where it started.
pub fn amm_shift(x0: f64, units: f64, depth: f64, width: f64) -> f64 {
    if width <= 0.0 {
        return x0 + units / depth;
    }
    let c = (x0 + x0.powi(3) / (3.0 * width * width) + units / depth).clamp(-AMM_MAX_C, AMM_MAX_C);
    // Unique real root of x³/3w² + x = c (trigonometric/hyperbolic Cardano, no cancellation).
    let x = 2.0 * width * ((3.0 * c / (2.0 * width)).asinh() / 3.0).sinh();
    // One Newton step polishes the last ulps.
    let w2 = width * width;
    x - (x + x.powi(3) / (3.0 * w2) - c) / (1.0 + x * x / w2)
}

/// Average fill price over mid (the price at `x0`) for moving the book from `x0` to `x1`.
/// Closed form of ∫e^x(1 + x²/w²)dx over the units absorbed; equals (e^Δ − 1)/Δ on a flat book.
pub fn amm_avg(x0: f64, x1: f64, width: f64) -> f64 {
    let d = x1 - x0;
    let growth = if d.abs() < 1e-12 { 1.0 } else { d.exp_m1() / d };
    if width <= 0.0 {
        return growth;
    }
    let w2 = width * width;
    let num = growth * (1.0 + (x1 * x1 - 2.0 * x1 + 2.0) / w2) + (x1 + x0 - 2.0) / w2;
    let den = 1.0 + (x1 * x1 + x1 * x0 + x0 * x0) / (3.0 * w2);
    num / den
}

/// Current displacement of a book symbol from its anchor; 0 without an anchor.
fn displacement(prices: &Prices, symbol: &str, mid: f64) -> f64 {
    match prices.anchor.get(symbol) {
        Some(a) if a.is_finite() && amm_width(symbol) > 0.0 => mid.ln() - a,
        _ => 0.0,
    }
}

/// Sweep `units` (positive buys) through a book symbol: (average fill, mid afterwards).
pub fn book_fill(prices: &Prices, symbol: &str, mid: f64, units: f64) -> Result<(f64, f64)> {
    let depth = depth(symbol);
    if depth <= 0.0 {
        return Err("该币种没有模拟盘口".into());
    }
    let width = amm_width(symbol);
    let x0 = displacement(prices, symbol, mid);
    let x1 = amm_shift(x0, units, depth, width);
    Ok((
        finite(mid * amm_avg(x0, x1, width))?,
        finite(mid * (x1 - x0).exp())?,
    ))
}

/// Move a book symbol's local mid by a fill, so later checks in the same operation see it.
pub fn apply_fill(prices: &mut Prices, trade: &Trade) -> Result<()> {
    let depth = depth(&trade.symbol);
    if depth <= 0.0 {
        return Ok(());
    }
    let buy = matches!(
        (trade.kind.as_str(), trade.side.as_str()),
        ("open", "long") | ("close" | "stopout" | "tp" | "sl", "short") | ("spot", "buy")
    );
    if trade.kind == "funding" {
        return Ok(());
    }
    let units = trade.lots * contract(&trade.symbol) * if buy { 1.0 } else { -1.0 };
    let mid = market_price(prices, &trade.symbol)?;
    // The curve must stay put for the rest of the operation: pin a missing anchor to the
    // pre-fill mid, or every later fill would re-centre on the moved price and stacked
    // orders could mark themselves up.
    if amm_width(&trade.symbol) > 0.0 {
        prices
            .anchor
            .entry(trade.symbol.clone())
            .or_insert(mid.ln());
    }
    let (_, after) = book_fill(prices, &trade.symbol, mid, units)?;
    prices.mid.insert(trade.symbol.clone(), after);
    Ok(())
}

/// Fill price for trading `lots` of a book symbol, including its spread.
fn book_price(prices: &Prices, symbol: &str, lots: f64, buy: bool) -> Result<f64> {
    let mid = market_price(prices, symbol)?;
    let units = lots * contract(symbol) * if buy { 1.0 } else { -1.0 };
    let half = spread(symbol, mid) / 2.0;
    let px = book_fill(prices, symbol, mid, units)?.0 + if buy { half } else { -half };
    if !px.is_finite() || px <= 0.0 {
        return Err("成交价无效".into());
    }
    Ok(px)
}

pub fn lots_valid(symbol: &str, lots: f64) -> bool {
    let (min, whole) = symbols::spec(symbol).map_or((0.01, false), |s| (s.min_lots, s.whole_lots));
    lots.is_finite() && lots >= min && lots <= MAX_LOTS && (!whole || lots.fract() == 0.0)
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
    /// Take-profit and stop-loss trigger prices (mid); 0 means unset.
    #[serde(default)]
    pub tp: f64,
    #[serde(default)]
    pub sl: f64,
}

/// Spot holding of a book symbol, owned outright: no leverage, no liquidation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Holding {
    pub units: f64,
    /// Average cost per unit in USD, fees included.
    pub cost: f64,
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
    /// Price this position would close at now; book symbols are marked at liquidation value.
    pub fn exit_price(&self, prices: &Prices) -> Result<f64> {
        if depth(&self.symbol) > 0.0 {
            book_price(prices, &self.symbol, self.lots, self.side == "short")
        } else {
            execution_price(prices, &self.symbol, self.side == "short")
        }
    }
    pub fn pnl(&self, prices: &Prices) -> Result<f64> {
        let quote = if depth(&self.symbol) > 0.0 || prices.bid_ask.contains_key(&self.symbol) {
            self.exit_price(prices)?
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
                / leverage_for(&self.symbol, leverage) as f64,
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
    #[serde(default)]
    pub spot: BTreeMap<String, Holding>,
    /// Last funding settlement time; 0 until the first perpetual position.
    #[serde(default)]
    pub funding_at: i64,
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
            spot: BTreeMap::new(),
            funding_at: 0,
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
        for (symbol, h) in &self.spot {
            if depth(symbol) <= 0.0 || !h.units.is_finite() || h.units < 0.0 || !h.cost.is_finite()
            {
                return Err("现货记录无效".into());
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
    /// Mark price and floating P&L per position. Positions in a book symbol are marked
    /// together at the average price unwinding their net size would fetch, so stacking
    /// orders cannot inflate each other's marks.
    pub fn marks(&self, prices: &Prices) -> Result<Vec<(f64, f64)>> {
        let mut net = BTreeMap::<&str, f64>::new();
        for p in self.positions.iter().filter(|p| depth(&p.symbol) > 0.0) {
            let lots = if p.side == "long" { p.lots } else { -p.lots };
            *net.entry(p.symbol.as_str()).or_default() += lots;
        }
        let mut exits = BTreeMap::new();
        for (symbol, lots) in net {
            exits.insert(symbol, book_price(prices, symbol, lots.abs(), lots < 0.0)?);
        }
        self.positions
            .iter()
            .map(|p| match exits.get(p.symbol.as_str()) {
                Some(&px) => Ok((px, p.pnl_at(px, prices)?)),
                None => {
                    let px = if prices.bid_ask.contains_key(&p.symbol) {
                        execution_price(prices, &p.symbol, p.side == "short")?
                    } else {
                        market_price(prices, &p.symbol)?
                    };
                    Ok((px, p.pnl_at(px, prices)?))
                }
            })
            .collect()
    }
    pub fn equity(&self, prices: &Prices) -> Result<f64> {
        finite(
            self.balance - self.loan
                + self.spot_value(prices)?
                + self.marks(prices)?.iter().map(|(_, pnl)| pnl).sum::<f64>(),
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
        let cap = symbols::spec(symbol).map_or(0.0, |s| s.max_open_lots);
        if cap > 0.0 {
            let held: f64 = self
                .positions
                .iter()
                .filter(|p| p.symbol == symbol)
                .map(|p| p.lots)
                .sum();
            if held + lots > cap {
                return Err(format!(
                    "{symbol} 单账户持仓上限 {cap} 手，当前已持 {held} 手"
                ));
            }
        }
        let px = if depth(symbol) > 0.0 {
            book_price(prices, symbol, lots, side == "long")?
        } else {
            execution_price(prices, symbol, side == "long")?
        };
        let need = finite(
            lots * contract(symbol) * px * usd_rate(prices, symbol)?
                / leverage_for(symbol, self.leverage) as f64,
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
            tp: 0.0,
            sl: 0.0,
        };
        let trade = self.trade("open", &p, px, None, None, now);
        if depth(symbol) > 0.0 {
            // Book positions are marked at what unwinding them would fetch; the order must
            // leave margin free after its own round-trip impact, not just its entry margin.
            let mut next = self.clone();
            next.positions.push(p.clone());
            let mut after = prices.clone();
            apply_fill(&mut after, &trade)?;
            let free = next.equity(&after)? - next.used_margin(&after)?;
            if free < 0.0 {
                return Err(format!(
                    "保证金不足（含 {symbol} 冲击成本）: 缺 ${:.2}（USD），请减小手数",
                    -free
                ));
            }
        }
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
        prices: &mut Prices,
        kind: &str,
        ml: Option<f64>,
        now: i64,
    ) -> Result<Trade> {
        let p = &self.positions[index];
        let px = p.exit_price(prices)?;
        let pnl = p.pnl_at(px, prices)?;
        self.balance = finite(self.balance + pnl)?;
        let p = self.positions.remove(index);
        let trade = self.trade(kind, &p, px, Some(pnl), ml, now);
        apply_fill(prices, &trade)?;
        Ok(trade)
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
        let mut prices = prices.clone();
        let mut trades = Vec::new();
        for id in targets {
            let index = next.positions.iter().position(|p| p.id == id).unwrap();
            trades.push(next.settle(index, &mut prices, "close", None, now)?);
        }
        *self = next;
        Ok(trades)
    }
    /// Every symbol whose price this account's equity depends on.
    pub fn symbols(&self) -> impl Iterator<Item = &str> {
        self.positions
            .iter()
            .map(|p| p.symbol.as_str())
            .chain(self.spot.keys().map(String::as_str))
    }
    /// Liquidation value of spot holdings at the live book.
    pub fn spot_value(&self, prices: &Prices) -> Result<f64> {
        let mut total = 0.0;
        for (symbol, h) in self.spot.iter().filter(|(_, h)| h.units > 0.0) {
            let mid = market_price(prices, symbol)?;
            let px = book_fill(prices, symbol, mid, -h.units)?.0 * (1.0 - SPOT_FEE);
            total += px * h.units;
        }
        finite(total)
    }
    /// Buy (`units > 0`) or sell spot units of a book symbol at its swept price, paying
    /// the spot fee. Spot is fully paid: cash must cover the buy, units must cover the sale.
    pub fn spot(&mut self, symbol: &str, units: f64, prices: &Prices, now: i64) -> Result<Trade> {
        let depth = depth(symbol);
        if depth <= 0.0 {
            return Err("该币种不支持现货".into());
        }
        if !units.is_finite() || units.abs() < SPOT_MIN || units.abs() > SPOT_MAX {
            return Err(format!("现货数量需在 {SPOT_MIN} 至 {SPOT_MAX} 之间"));
        }
        self.validate()?;
        let cap = symbols::spec(symbol).map_or(0.0, |s| s.max_spot_units);
        let held = self.spot.get(symbol).map_or(0.0, |h| h.units);
        if cap > 0.0 && units > 0.0 && held + units > cap + 1e-9 {
            return Err(format!(
                "{symbol} 现货单账户持有上限 {cap}，当前已持 {held:.4}"
            ));
        }
        let mid = market_price(prices, symbol)?;
        let (avg, _) = book_fill(prices, symbol, mid, units)?;
        let holding = self.spot.entry(symbol.into()).or_default();
        let (side, pnl) = if units > 0.0 {
            let cost = finite(avg * units * (1.0 + SPOT_FEE))?;
            // Cash only: borrowed balance can fund spot, open-position margin cannot.
            if cost > self.balance {
                return Err(format!(
                    "可用 USDT 不足: 需 {cost:.2}，余额 {:.2}",
                    self.balance
                ));
            }
            self.balance -= cost;
            holding.cost = (holding.cost * holding.units + cost) / (holding.units + units);
            holding.units += units;
            ("buy", None)
        } else {
            let sell = -units;
            if sell > holding.units + 1e-9 {
                return Err(format!("{symbol} 可用余额不足: 持有 {:.4}", holding.units));
            }
            let proceeds = finite(avg * sell * (1.0 - SPOT_FEE))?;
            let pnl = proceeds - holding.cost * sell;
            self.balance += proceeds;
            holding.units -= sell;
            if holding.units < 1e-9 {
                self.spot.remove(symbol);
            }
            ("sell", Some(pnl))
        };
        Ok(Trade {
            time: now,
            kind: "spot".into(),
            position_id: 0,
            symbol: symbol.into(),
            side: side.into(),
            lots: units.abs() / contract(symbol),
            entry: 0.0,
            price: avg,
            pnl,
            balance: self.balance,
            margin_level: None,
            currency: "USD".into(),
        })
    }
    /// Set or clear (0) take-profit and stop-loss trigger prices on one position.
    pub fn set_triggers(&mut self, id: u64, tp: f64, sl: f64, prices: &Prices) -> Result<()> {
        let p = self
            .positions
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| format!("无 # {id} 持仓"))?;
        for v in [tp, sl] {
            if !v.is_finite() || v < 0.0 {
                return Err("止盈止损价格无效".into());
            }
        }
        let mid = market_price(prices, &p.symbol)?;
        let long = p.side == "long";
        if tp > 0.0 && (tp > mid) != long {
            return Err(format!(
                "{}止盈价需{}当前价",
                if long { "多单" } else { "空单" },
                if long { "高于" } else { "低于" }
            ));
        }
        if sl > 0.0 && (sl < mid) != long {
            return Err(format!(
                "{}止损价需{}当前价",
                if long { "多单" } else { "空单" },
                if long { "低于" } else { "高于" }
            ));
        }
        p.tp = tp;
        p.sl = sl;
        Ok(())
    }
    /// Close every position whose take-profit or stop-loss the mid has crossed.
    pub fn check_triggers(&mut self, prices: &Prices, now: i64) -> Result<Vec<Trade>> {
        let hit: Vec<(u64, &'static str)> = self
            .positions
            .iter()
            .filter_map(|p| {
                let mid = prices.get(&p.symbol).copied()?;
                let long = p.side == "long";
                if p.tp > 0.0 && (if long { mid >= p.tp } else { mid <= p.tp }) {
                    Some((p.id, "tp"))
                } else if p.sl > 0.0 && (if long { mid <= p.sl } else { mid >= p.sl }) {
                    Some((p.id, "sl"))
                } else {
                    None
                }
            })
            .collect();
        let mut next = self.clone();
        let mut prices = prices.clone();
        let mut trades = Vec::new();
        for (id, kind) in hit {
            let index = next.positions.iter().position(|p| p.id == id).unwrap();
            trades.push(next.settle(index, &mut prices, kind, None, now)?);
        }
        *self = next;
        Ok(trades)
    }
    /// Mid at which the account would hit stop-out if only this symbol moved, holding
    /// everything else fixed; None when no move in range would liquidate it.
    pub fn liquidation_price(&self, symbol: &str, prices: &Prices) -> Result<Option<f64>> {
        if !self.positions.iter().any(|p| p.symbol == symbol) {
            return Ok(None);
        }
        let mid = market_price(prices, symbol)?;
        let at = |px: f64| -> Result<bool> {
            let mut q = prices.clone();
            q.insert(symbol.into(), px);
            Ok(self.margin_level(&q)?.is_some_and(|ml| ml < STOPOUT))
        };
        if at(mid)? {
            return Ok(Some(mid));
        }
        let mut found = None;
        for (lo, hi) in [(mid * 1e-3, mid), (mid, mid * 1e3)] {
            let (bad, good) = if lo < mid { (lo, hi) } else { (hi, lo) };
            if !at(bad)? {
                continue;
            }
            let (mut bad, mut good) = (bad, good);
            // 40 geometric halvings over a 1000x range resolve to ~1e-9 relative.
            for _ in 0..40 {
                let m = (bad * good).sqrt();
                if at(m)? { bad = m } else { good = m }
            }
            let px = (bad * good).sqrt();
            if found.is_none_or(|f: f64| (px - mid).abs() < (f - mid).abs()) {
                found = Some(px);
            }
        }
        Ok(found)
    }
    /// Settle hourly funding on book-symbol positions: longs pay shorts when the price is
    /// above fair value and vice versa. `rates` holds the per-hour rate for each symbol.
    pub fn settle_funding(
        &mut self,
        rates: &BTreeMap<String, f64>,
        prices: &Prices,
        now: i64,
    ) -> Result<Vec<Trade>> {
        const HOUR: i64 = 3_600_000;
        let held = self.positions.iter().any(|p| rates.contains_key(&p.symbol));
        if !held {
            self.funding_at = now;
            return Ok(vec![]);
        }
        if self.funding_at == 0 {
            self.funding_at = now;
        }
        let hours = ((now - self.funding_at) / HOUR).min(24);
        if hours <= 0 {
            return Ok(vec![]);
        }
        self.funding_at += hours * HOUR;
        let mut trades = Vec::new();
        for p in self.positions.clone() {
            let Some(rate) = rates.get(&p.symbol) else {
                continue;
            };
            let notional = p.lots * contract(&p.symbol) * market_price(prices, &p.symbol)?;
            let sign = if p.side == "long" { -1.0 } else { 1.0 };
            let fee = finite(cents(sign * rate * notional * hours as f64))?;
            if fee == 0.0 {
                continue;
            }
            self.balance += fee;
            let mut t = self.trade(
                "funding",
                &p,
                market_price(prices, &p.symbol)?,
                Some(fee),
                None,
                now,
            );
            t.balance = self.balance;
            trades.push(t);
        }
        Ok(trades)
    }
    pub fn check_stopout(&mut self, prices: &Prices, now: i64) -> Result<Vec<Trade>> {
        let mut next = self.clone();
        let mut prices = prices.clone();
        let prices = &mut prices;
        let mut trades = Vec::new();
        while !next.positions.is_empty() {
            let ml = next.margin_level(prices)?;
            if ml.is_none_or(|v| v >= STOPOUT) {
                break;
            }
            let pnls: Vec<f64> = next.marks(prices)?.into_iter().map(|m| m.1).collect();
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
                tp: 0.0,
                sl: 0.0,
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
            tp: 0.0,
            sl: 0.0,
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
    fn nstar_margin_and_pnl_are_usd_at_its_price_level() {
        let mut a = Account::default();
        let q = quotes(&[("NSTAR", 100.0)]);
        assert_eq!(conversion("NSTAR").unwrap(), None);
        a.open("long", 0.1, "NSTAR", &q, 1, 0).unwrap();
        let p = &a.positions[0];
        // 0.001 NSTAR barely touches the book: spread dominates.
        assert!((p.entry - 100.025).abs() < 1e-5, "{}", p.entry);
        assert!((p.margin(&q, a.leverage).unwrap() - 0.001).abs() < 1e-6);
        let up = quotes(&[("NSTAR", 101.0)]);
        assert!((p.pnl_at(101.0, &up).unwrap() - 0.000975).abs() < 1e-5);
    }
    #[test]
    fn nstar_round_trip_pays_impact_twice_and_cannot_pump_for_profit() {
        // The old model let $1k of margin pump 100k lots and walk away with millions.
        let q = quotes(&[("NSTAR", 100.0)]);
        for lots in [1_000.0, 10_000.0, MAX_LOTS] {
            let mut a = Account {
                balance: 1e9,
                ..Account::default()
            };
            let open = a.open("long", lots, "NSTAR", &q, 1, 0).unwrap();
            let mut after = q.clone();
            apply_fill(&mut after, &open).unwrap();
            assert!(after["NSTAR"] > 100.0, "buy lifts the book");
            let close = a.close(None, &after, 1).unwrap();
            let pnl = close[0].pnl.unwrap();
            assert!(pnl < 0.0, "{lots} lots round trip made {pnl}");
            apply_fill(&mut after, &close[0]).unwrap();
            assert!(
                (after["NSTAR"] - 100.0).abs() < 1e-6,
                "book returns to start"
            );
        }
    }
    #[test]
    fn nstar_split_orders_cost_the_same_as_one_sweep() {
        let q = quotes(&[("NSTAR", 100.0)]);
        let mut one = Account::default();
        let big = one.open("long", 2_000.0, "NSTAR", &q, 1, 0).unwrap();
        let mut many = Account::default();
        let mut live = q.clone();
        let mut cost = 0.0;
        for i in 0..20 {
            let t = many.open("long", 100.0, "NSTAR", &live, i + 1, 0).unwrap();
            apply_fill(&mut live, &t).unwrap();
            cost += t.price * 100.0;
        }
        // Spread is charged per order on each slice's own price, so only that differs.
        let rel = (cost / 2_000.0 - big.price) / big.price;
        assert!(
            rel.abs() < 1e-3,
            "split {} vs sweep {}",
            cost / 2_000.0,
            big.price
        );
    }
    #[test]
    fn nstar_is_marked_at_liquidation_value_and_caps_size() {
        let q = quotes(&[("NSTAR", 100.0)]);
        let mut a = Account {
            balance: 1e9,
            leverage: MAX_LEVERAGE,
            ..Account::default()
        };
        a.open("long", MAX_LOTS, "NSTAR", &q, 1, 0).unwrap();
        // Unwinding 1,000 NSTAR into a 50,000 book costs ~1%: the mark says so.
        let pnl = a.positions[0].pnl(&q).unwrap();
        assert!(pnl < -400.0, "{pnl}");
        // Account leverage 1:50000 still margins NSTAR at 1:100.
        let margin = a.positions[0].margin(&q, a.leverage).unwrap();
        assert!((margin - MAX_LOTS * 0.01 * 100.0 / 100.0).abs() < 1e-6);
        for id in 2..=20 {
            a.open("long", MAX_LOTS, "NSTAR", &q, id, 0).unwrap();
        }
        // Gross cap: 20,000 NSTAR per account, longs and shorts alike.
        assert!(a.open("short", 0.01, "NSTAR", &q, 6, 0).is_err());
    }
    #[test]
    fn stacked_nstar_orders_are_marked_jointly_and_cannot_inflate_equity() {
        let q = quotes(&[("NSTAR", 100.0)]);
        let mut a = Account {
            balance: 1e9,
            ..Account::default()
        };
        let mut live = q.clone();
        for id in 1..=14 {
            let t = a.open("long", MAX_LOTS, "NSTAR", &live, id, 0).unwrap();
            apply_fill(&mut live, &t).unwrap();
        }
        // Each order lifted the mid for the next; marked one by one they look like a
        // big winner, but unwinding all 14,000 NSTAR together gives the gain back.
        let paper: f64 = a.positions.iter().map(|p| p.pnl(&live).unwrap()).sum();
        let joint = a.equity(&live).unwrap() - a.balance;
        assert!(paper > 0.0, "{paper}");
        assert!(joint < 0.0, "{joint}");
        let mut b = a.clone();
        let realised: f64 = b
            .close(None, &live, 1)
            .unwrap()
            .iter()
            .map(|t| t.pnl.unwrap())
            .sum();
        // Joint marking charges spread at the starting mid, slices at ever lower mids: the
        // mark is the slightly more conservative of the two.
        assert!(
            joint <= realised && realised - joint < joint.abs() * 0.1,
            "{realised} vs {joint}"
        );
        let mut thin = Account::default();
        let mut live = q.clone();
        while let Ok(t) = thin.open("long", MAX_LOTS, "NSTAR", &live, thin.next_id, 0) {
            apply_fill(&mut live, &t).unwrap();
        }
        assert!(
            thin.positions.len() < 10,
            "$10k stacked {} orders",
            thin.positions.len()
        );
        assert!(thin.equity(&live).unwrap() > 0.0);
    }
    #[test]
    fn nstar_order_must_survive_its_own_impact() {
        let q = quotes(&[("NSTAR", 100.0)]);
        // $1,050 passes the plain entry check (~$1,005 margin), but after its own fill the
        // mark drops ~$52 and margin rises to ~$1,010 at the lifted mid: refused.
        let mut thin = Account {
            balance: 1_050.0,
            ..Account::default()
        };
        assert!(thin.open("long", MAX_LOTS, "NSTAR", &q, 1, 0).is_err());
        assert!(thin.positions.is_empty());
        let mut ok = Account {
            balance: 1_200.0,
            ..Account::default()
        };
        assert!(ok.open("long", MAX_LOTS, "NSTAR", &q, 1, 0).is_ok());
    }
    #[test]
    fn nstar_cascade_stopout_pays_its_own_impact() {
        let q = quotes(&[("NSTAR", 100.0)]);
        let mut a = Account {
            balance: 2_000.0,
            ..Account::default()
        };
        let mut live = q.clone();
        for id in 1..=3 {
            let t = a.open("long", 10_000.0, "NSTAR", &live, id, 0).unwrap();
            apply_fill(&mut live, &t).unwrap();
        }
        let crash = quotes(&[("NSTAR", 85.0)]);
        let trades = a.check_stopout(&crash, 1).unwrap();
        assert_eq!(trades.len(), 3);
        // Each forced sale fills lower than the one before.
        assert!(trades[0].price > trades[1].price && trades[1].price > trades[2].price);
    }
    #[test]
    fn amm_curve_inverts_its_inventory_and_matches_flat_book_without_width() {
        let (depth, w) = (50_000.0, 0.08);
        let inv = |x: f64| depth * (x + x.powi(3) / (3.0 * w * w));
        for x0 in [-0.6, -0.1, 0.0, 0.03, 0.4] {
            for units in [-1e6, -20_000.0, -1.0, 1e-6, 750.0, 20_000.0, 1e6] {
                let x1 = amm_shift(x0, units, depth, w);
                let err = (inv(x1) - inv(x0) - units).abs();
                assert!(err <= 1e-6 * units.abs().max(1.0), "{x0} {units}: {err}");
                if units.abs() < 1.0 {
                    // x1 - x0 is ~1e-13 here: a numeric reference would only measure rounding.
                    assert!((amm_avg(x0, x1, w) - 1.0).abs() < 1e-9);
                    continue;
                }
                // Average fill equals the integral of price over inventory (Simpson).
                let n = 2000;
                let h = (x1 - x0) / n as f64;
                let f = |x: f64| (x - x0).exp() * (1.0 + x * x / (w * w));
                let mut sum = f(x0) + f(x1);
                for i in 1..n {
                    sum += f(x0 + i as f64 * h) * if i % 2 == 1 { 4.0 } else { 2.0 };
                }
                let avg = sum * h / 3.0 * depth / units;
                let got = amm_avg(x0, x1, w);
                assert!(
                    (got - avg).abs() < 1e-6 * avg,
                    "{x0} {units}: {got} vs {avg}"
                );
            }
        }
        // width 0 is exactly the old constant-depth sweep.
        let d = 20_000.0 / depth;
        assert_eq!(amm_shift(0.1, 20_000.0, depth, 0.0), 0.1 + d);
        assert!((amm_avg(0.1, 0.1 + d, 0.0) - d.exp_m1() / d).abs() < 1e-15);
        assert!((amm_avg(0.2, 0.2, w) - 1.0).abs() < 1e-12);
    }
    #[test]
    fn amm_curve_damps_big_pushes_but_not_small_trades() {
        let q = quotes(&[("NSTAR", 100.0)]);
        let lift = |units: f64| book_fill(&q, "NSTAR", 100.0, units).unwrap().1 / 100.0 - 1.0;
        // 1,000 NSTAR: ~2%, same as the flat book.
        assert!((lift(1_000.0) - 0.02).abs() < 0.001, "{}", lift(1_000.0));
        // 20,000 NSTAR: ~18% (flat book +49%); 100,000: ~37% (flat book 7x).
        assert!(lift(20_000.0) < 0.19, "{}", lift(20_000.0));
        assert!(lift(100_000.0) < 0.38, "{}", lift(100_000.0));
        assert!(lift(-100_000.0) > -0.28, "{}", lift(-100_000.0));
        // Already pushed 20% above the anchor, the next 1,000 NSTAR barely moves it.
        let mut far = q.clone();
        far.anchor.insert("NSTAR".into(), (100.0f64 / 1.2).ln());
        let next = book_fill(&far, "NSTAR", 100.0, 1_000.0).unwrap().1 / 100.0 - 1.0;
        assert!(next < 0.005, "{next}");
    }
    #[test]
    fn amm_fills_stay_path_independent_off_anchor() {
        let mut q = quotes(&[("NSTAR", 130.0)]);
        q.anchor.insert("NSTAR".into(), 100.0f64.ln());
        let mut one = Account {
            balance: 1e9,
            ..Account::default()
        };
        let big = one.open("long", 1_000_000.0, "NSTAR", &q, 1, 0);
        assert!(big.is_err(), "per-order lot cap");
        let big = one.open("long", 100_000.0, "NSTAR", &q, 1, 0).unwrap();
        let mut many = Account {
            balance: 1e9,
            ..Account::default()
        };
        let mut live = q.clone();
        let mut cost = 0.0;
        for i in 0..20 {
            let t = many
                .open("long", 5_000.0, "NSTAR", &live, i + 1, 0)
                .unwrap();
            apply_fill(&mut live, &t).unwrap();
            cost += t.price * 5_000.0;
        }
        let rel = (cost / 100_000.0 - big.price) / big.price;
        assert!(
            rel.abs() < 1e-3,
            "split {} vs sweep {}",
            cost / 1e5,
            big.price
        );
        // Unwinding everything lands back on the starting mid: no free lunch from the curve.
        let closed = many.close(None, &live, 1).unwrap();
        for t in &closed {
            apply_fill(&mut live, t).unwrap();
        }
        assert!((live["NSTAR"] - 130.0).abs() < 1e-6, "{}", live["NSTAR"]);
        assert!(closed.iter().map(|t| t.pnl.unwrap()).sum::<f64>() < 0.0);
    }
    #[test]
    fn nstar_spot_holding_is_capped_per_account() {
        let q = quotes(&[("NSTAR", 100.0)]);
        let mut a = Account {
            balance: 1e9,
            ..Account::default()
        };
        a.spot("NSTAR", 10_000.0, &q, 1).unwrap();
        a.spot("NSTAR", 10_000.0, &q, 2).unwrap();
        let before = a.clone();
        assert!(a.spot("NSTAR", 0.01, &q, 3).is_err());
        assert_eq!(a, before, "refused buy leaves the account untouched");
        // Selling is always allowed, and frees room to buy again.
        a.spot("NSTAR", -5_000.0, &q, 4).unwrap();
        a.spot("NSTAR", 5_000.0, &q, 5).unwrap();
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

#[cfg(test)]
mod cap_tests {
    use super::*;
    #[test]
    fn leverage_and_lot_caps_reject_inflation() {
        let mut a = Account {
            leverage: MAX_LEVERAGE,
            ..Account::default()
        };
        assert!(a.validate().is_ok());
        a.leverage = MAX_LEVERAGE + 1;
        assert!(a.validate().is_err());
        assert!(lots_valid("EURUSD", MAX_LOTS));
        assert!(!lots_valid("EURUSD", MAX_LOTS + 1.0));
        assert!(!lots_valid("NSTAR", 1e106));
    }
}

#[cfg(test)]
mod exchange_tests {
    use super::*;
    fn q(price: f64) -> Prices {
        [("NSTAR".to_string(), price)].into()
    }
    #[test]
    fn spot_buy_sell_pays_fee_and_impact_and_counts_in_equity() {
        let mut a = Account::default();
        let t = a.spot("NSTAR", 10.0, &q(100.0), 1).unwrap();
        assert_eq!((t.kind.as_str(), t.side.as_str()), ("spot", "buy"));
        assert!(t.price > 100.0, "buy sweeps up: {}", t.price);
        let cost = 10_000.0 - a.balance;
        assert!((cost - t.price * 10.0 * 1.001).abs() < 1e-6);
        let h = &a.spot["NSTAR"];
        assert!((h.cost - cost / 10.0).abs() < 1e-9);
        let eq = a.equity(&q(100.0)).unwrap();
        assert!(eq < 10_000.0 && eq > 9_990.0, "{eq}");
        assert!(a.spot("NSTAR", -10.5, &q(100.0), 2).is_err(), "oversell");
        assert!(a.spot("NSTAR", 1_000.0, &q(100.0), 2).is_err(), "cash only");
        let t = a.spot("NSTAR", -10.0, &q(120.0), 3).unwrap();
        assert!(t.pnl.unwrap() > 190.0, "{:?}", t.pnl);
        assert!(!a.spot.contains_key("NSTAR"));
        assert!(a.spot("BTCUSDT", 1.0, &q(100.0), 4).is_err());
    }
    #[test]
    fn spot_round_trip_loses_and_split_buys_match_one() {
        let mut a = Account::default();
        let mut live = q(100.0);
        let t = a.spot("NSTAR", 50.0, &live, 1).unwrap();
        apply_fill(&mut live, &t).unwrap();
        assert!(live["NSTAR"] > 100.0);
        let t = a.spot("NSTAR", -50.0, &live, 2).unwrap();
        apply_fill(&mut live, &t).unwrap();
        assert!((live["NSTAR"] - 100.0).abs() < 1e-6);
        assert!(
            a.balance < 10_000.0,
            "pump and dump on spot must lose: {}",
            a.balance
        );
    }
    #[test]
    fn triggers_validate_side_and_fire_once() {
        let mut a = Account::default();
        a.open("long", 1_000.0, "NSTAR", &q(100.0), 1, 0).unwrap();
        assert!(
            a.set_triggers(1, 90.0, 0.0, &q(100.0)).is_err(),
            "tp below long"
        );
        assert!(
            a.set_triggers(1, 0.0, 110.0, &q(100.0)).is_err(),
            "sl above long"
        );
        a.set_triggers(1, 110.0, 95.0, &q(100.0)).unwrap();
        assert!(a.check_triggers(&q(105.0), 1).unwrap().is_empty());
        let t = a.check_triggers(&q(111.0), 2).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].kind, "tp");
        assert!(t[0].pnl.unwrap() > 0.0);
        assert!(a.positions.is_empty());
        a.open("short", 1_000.0, "NSTAR", &q(100.0), 2, 0).unwrap();
        a.set_triggers(2, 0.0, 104.0, &q(100.0)).unwrap();
        assert_eq!(a.check_triggers(&q(104.5), 3).unwrap()[0].kind, "sl");
        assert!(a.set_triggers(9, 1.0, 0.0, &q(100.0)).is_err());
    }
    #[test]
    fn liquidation_price_is_where_stopout_starts() {
        let mut a = Account {
            balance: 1_000.0,
            ..Account::default()
        };
        a.open("long", 50_000.0, "NSTAR", &q(100.0), 1, 0).unwrap();
        let liq = a.liquidation_price("NSTAR", &q(100.0)).unwrap().unwrap();
        assert!(liq < 100.0 && liq > 90.0, "{liq}");
        let mut below = a.clone();
        assert!(!below.check_stopout(&q(liq * 0.999), 1).unwrap().is_empty());
        let mut above = a.clone();
        assert!(above.check_stopout(&q(liq * 1.001), 1).unwrap().is_empty());
        let mut short = Account {
            balance: 1_000.0,
            ..Account::default()
        };
        short
            .open("short", 50_000.0, "NSTAR", &q(100.0), 1, 0)
            .unwrap();
        let liq = short
            .liquidation_price("NSTAR", &q(100.0))
            .unwrap()
            .unwrap();
        assert!(liq > 100.0 && liq < 110.0, "{liq}");
        assert_eq!(
            Account::default()
                .liquidation_price("NSTAR", &q(100.0))
                .unwrap(),
            None
        );
    }
    #[test]
    fn funding_moves_cash_from_crowded_side_hourly() {
        let rates: BTreeMap<String, f64> = [("NSTAR".to_string(), 0.0005)].into();
        let mut a = Account::default();
        a.open("long", 10_000.0, "NSTAR", &q(100.0), 1, 0).unwrap();
        assert!(
            a.settle_funding(&rates, &q(100.0), 1_000)
                .unwrap()
                .is_empty()
        );
        let hour = 3_600_000;
        let t = a
            .settle_funding(&rates, &q(100.0), 1_000 + 2 * hour + 5)
            .unwrap();
        // 100 NSTAR * $100 * 0.05% * 2 h = $10 paid by the long.
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].kind, "funding");
        assert!((t[0].pnl.unwrap() + 10.0).abs() < 1e-9);
        assert!(
            a.settle_funding(&rates, &q(100.0), 1_000 + 2 * hour + 9)
                .unwrap()
                .is_empty()
        );
        let mut s = Account::default();
        s.open("short", 10_000.0, "NSTAR", &q(100.0), 1, 0).unwrap();
        s.settle_funding(&rates, &q(100.0), 7).unwrap();
        let t = s.settle_funding(&rates, &q(100.0), 7 + hour).unwrap();
        assert!((t[0].pnl.unwrap() - 5.0).abs() < 1e-9);
    }
    #[test]
    fn legacy_account_json_without_new_fields_still_loads() {
        let old = r#"{"balance":5.0,"leverage":100,"positions":[{"id":1,"side":"long","lots":1.0,"entry":1.1,"symbol":"FXSIM"}]}"#;
        let a: Account = serde_json::from_str(old).unwrap();
        assert!(a.spot.is_empty());
        assert_eq!((a.positions[0].tp, a.positions[0].sl), (0.0, 0.0));
    }
}
