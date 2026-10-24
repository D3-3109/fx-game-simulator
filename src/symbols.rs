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
    /// Units that move the price by one log unit (e≈2.72x); 0 means no simulated order book.
    pub depth: f64,
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
        depth: 0.0,
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
        depth: 0.0,
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
        depth: 0.0,
    }
}
const SPECS: &[Spec] = &[
    lot(SIM, TIGHT, None),
    // 1 lot = 0.01 NSTAR. ~1,000 NSTAR (~$100k) moves the price ~1%, so ordinary orders
    // barely notice the book; one account tops out at 20,000 NSTAR gross (~22% impact),
    // and its fills pay for every step of that move both ways.
    Spec {
        contract: 0.01,
        max_leverage: 100,
        max_open_lots: 2_000_000.0,
        depth: 100_000.0,
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
