pub const SIM: &str = "FXSIM";
pub const NSTAR: &str = "NSTAR";
pub const CRYPTO: &[&str] = &[
    "BTCUSDT", "ETHUSDT", "EURUSDT", "SOLUSDT", "XRPUSDT", "BNBUSDT", "DOGEUSDT", "ADAUSDT",
    "TRXUSDT", "AVAXUSDT", "LINKUSDT",
];
pub const STOCK: &[&str] = &[
    "AAPL", "TSLA", "NVDA", "MSFT", "AMZN", "GOOGL", "META", "AMD",
];
pub const FX: &[&str] = &[
    "EURUSD", "GBPUSD", "USDJPY", "USDCHF", "USDCAD", "AUDUSD", "NZDUSD", "EURJPY", "GBPJPY",
    "USDCNH", "USDCNY", "EURCNH",
];
pub const INTERVALS: &[(&str, i64)] = &[
    ("1s", 1),
    ("1m", 60),
    ("5m", 300),
    ("15m", 900),
    ("30m", 1800),
    ("1h", 3600),
    ("4h", 14400),
    ("1d", 86400),
    ("1w", 604800),
];

pub struct Spec {
    pub symbol: &'static str,
    /// Units traded per lot.
    pub contract: f64,
    pub min_lots: f64,
    pub whole_lots: bool,
    /// Absolute spread, used when `pct` is zero.
    pub spread: f64,
    /// Spread as a fraction of price.
    pub pct: f64,
    /// Settlement pair for the account currency; None means USD.
    pub settle: Option<&'static str>,
    /// Effective leverage ceiling for this symbol; 0 means the account leverage applies.
    pub max_leverage: u64,
    /// Gross open lots one account may hold in this symbol; 0 means only the per-order cap.
    pub max_open_lots: f64,
    /// Largest single order in lots; 0 means the global `MAX_LOTS`.
    pub max_order_lots: f64,
    /// Risk-limit tiers on the account's gross units in this symbol: each `(up_to, lev)`
    /// slice is margined at `min(account leverage, lev)`, like income-tax brackets, so the
    /// margin grows smoothly and only the extra size pays the lower leverage. Empty: none.
    pub leverage_tiers: &'static [(f64, u64)],
    /// Units that move the price by one log unit (e≈2.72x) near the anchor; 0 means no
    /// simulated order book.
    pub depth: f64,
    /// Elastic AMM width (log units). Liquidity at displacement `x` from the book's slow
    /// anchor is `depth · (1 + (x / width)²)`, so small trades feel the plain `depth` while
    /// pushing the price far needs ever more size. 0 keeps the constant-depth book.
    pub amm_width: f64,
    /// Spot units one account may hold in this symbol; 0 means no holding cap.
    pub max_spot_units: f64,
}
const TIGHT: f64 = 0.0002;
const WIDE: f64 = 0.02;
const fn lot(symbol: &'static str, spread: f64, settle: Option<&'static str>) -> Spec {
    Spec {
        symbol,
        contract: 100_000.0,
        min_lots: 0.01,
        whole_lots: false,
        spread,
        pct: 0.0,
        settle,
        max_leverage: 0,
        max_open_lots: 0.0,
        max_order_lots: 0.0,
        leverage_tiers: &[],
        depth: 0.0,
        amm_width: 0.0,
        max_spot_units: 0.0,
    }
}
const fn coin(symbol: &'static str) -> Spec {
    Spec {
        symbol,
        contract: 1.0,
        min_lots: 0.01,
        whole_lots: false,
        spread: 0.0,
        pct: 0.0005,
        settle: None,
        max_leverage: 0,
        max_open_lots: 0.0,
        max_order_lots: 0.0,
        leverage_tiers: &[],
        depth: 0.0,
        amm_width: 0.0,
        max_spot_units: 0.0,
    }
}
const fn stock(symbol: &'static str) -> Spec {
    Spec {
        symbol,
        contract: 1.0,
        min_lots: 1.0,
        whole_lots: true,
        spread: 0.0,
        pct: 0.0005,
        settle: None,
        max_leverage: 0,
        max_open_lots: 0.0,
        max_order_lots: 0.0,
        leverage_tiers: &[],
        depth: 0.0,
        amm_width: 0.0,
        max_spot_units: 0.0,
    }
}
const SPECS: &[Spec] = &[
    lot(SIM, TIGHT, None),
    // 1 lot = 0.01 NSTAR. ~500 NSTAR (~$50k) moves the price ~1% near the anchor. The
    // elastic curve thickens away from it: 20,000 NSTAR lifts ~18% (49% on a flat book),
    // 100,000 ~37% (flat: 7x). Perpetual size is limited by leverage tiers rather than a
    // wall: the first 10,000 NSTAR gross get 100x, the next slices 50x/20x/10x, up to a
    // 100,000 NSTAR safety ceiling (effective ~15x when full); spot also tops out at
    // 100,000. One order may use the whole ceiling, since splitting costs the same. Fills
    // pay for every step of a move both ways, so a pump only pays off for whoever sells
    // into someone else's buying.
    Spec {
        contract: 0.01,
        max_leverage: 100,
        max_open_lots: 10_000_000.0,
        max_order_lots: 10_000_000.0,
        leverage_tiers: &[
            (10_000.0, 100),
            (25_000.0, 50),
            (50_000.0, 20),
            (100_000.0, 10),
        ],
        depth: 50_000.0,
        amm_width: 0.08,
        max_spot_units: 100_000.0,
        ..coin(NSTAR)
    },
    lot("BTCUSDT", TIGHT, None),
    lot("ETHUSDT", TIGHT, None),
    lot("EURUSDT", TIGHT, None),
    coin("SOLUSDT"),
    coin("XRPUSDT"),
    coin("BNBUSDT"),
    coin("DOGEUSDT"),
    coin("ADAUSDT"),
    coin("TRXUSDT"),
    coin("AVAXUSDT"),
    coin("LINKUSDT"),
    lot("EURUSD", TIGHT, None),
    lot("GBPUSD", TIGHT, None),
    lot("AUDUSD", TIGHT, None),
    lot("NZDUSD", TIGHT, None),
    lot("USDJPY", WIDE, Some("USDJPY")),
    lot("USDCHF", TIGHT, Some("USDCHF")),
    lot("USDCAD", TIGHT, Some("USDCAD")),
    lot("USDCNH", TIGHT, Some("USDCNH")),
    lot("USDCNY", TIGHT, Some("USDCNY")),
    lot("EURJPY", WIDE, Some("USDJPY")),
    lot("GBPJPY", WIDE, Some("USDJPY")),
    lot("EURCNH", TIGHT, Some("USDCNH")),
    stock("AAPL"),
    stock("TSLA"),
    stock("NVDA"),
    stock("MSFT"),
    stock("AMZN"),
    stock("GOOGL"),
    stock("META"),
    stock("AMD"),
];
pub fn spec(symbol: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.symbol == symbol)
}
pub fn all() -> Vec<&'static str> {
    let mut out: Vec<_> = CRYPTO
        .iter()
        .chain(FX)
        .chain(STOCK)
        .copied()
        .chain([SIM, NSTAR])
        .collect();
    out.sort();
    out
}
pub fn provider(symbol: &str) -> Option<&'static str> {
    if symbol == SIM || symbol == NSTAR {
        Some("本地模拟")
    } else if CRYPTO.contains(&symbol) {
        Some("Binance")
    } else if FX.contains(&symbol) {
        Some("biquote.io")
    } else if STOCK.contains(&symbol) {
        Some("Yahoo")
    } else {
        None
    }
}
pub fn yahoo(symbol: &str) -> Option<&'static str> {
    if let Some(stock) = STOCK.iter().copied().find(|s| *s == symbol) {
        return Some(stock);
    }
    Some(match symbol {
        "USDCNY" => "CNY=X",
        "EURUSD" => "EURUSD=X",
        "GBPUSD" => "GBPUSD=X",
        "USDJPY" => "JPY=X",
        "AUDUSD" => "AUDUSD=X",
        "EURJPY" => "EURJPY=X",
        "GBPJPY" => "GBPJPY=X",
        "USDCHF" => "CHF=X",
        "USDCAD" => "CAD=X",
        "NZDUSD" => "NZDUSD=X",
        _ => return None,
    })
}
