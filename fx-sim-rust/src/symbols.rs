pub const SIM: &str = "FXSIM";
pub const CRYPTO: &[&str] = &["BTCUSDT", "ETHUSDT", "EURUSDT"];
pub const FX: &[&str] = &[
    "EURUSD", "GBPUSD", "USDJPY", "USDCHF", "USDCAD", "AUDUSD", "NZDUSD", "EURJPY", "GBPJPY",
    "USDCNH", "USDCNY", "EURCNH",
];
pub const INTERVALS: &[(&str, i64)] = &[
    ("1m", 60),
    ("5m", 300),
    ("15m", 900),
    ("30m", 1800),
    ("1h", 3600),
    ("4h", 14400),
    ("1d", 86400),
    ("1w", 604800),
];
pub fn all() -> Vec<&'static str> {
    let mut out: Vec<_> = CRYPTO.iter().chain(FX).copied().chain([SIM]).collect();
    out.sort();
    out
}
pub fn provider(symbol: &str) -> Option<&'static str> {
    if symbol == SIM {
        Some("本地模拟")
    } else if CRYPTO.contains(&symbol) {
        Some("Binance")
    } else if FX.contains(&symbol) {
        Some("biquote.io → Yahoo Finance（备用）")
    } else {
        None
    }
}
pub fn yahoo(symbol: &str) -> Option<&'static str> {
    Some(match symbol {
        "USDCNH" | "USDCNY" => "CNY=X",
        "EURCNH" => "EURCNY=X",
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
