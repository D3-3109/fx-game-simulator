#[cfg(test)]
#[path = "market_tests.rs"]
mod tests;

use crate::{
    storage::{Store, now_ms},
    symbols,
    trading::{self, Prices},
};
use rand::Rng;
use rand_distr::{Distribution, StandardNormal};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, RwLock};

pub type Result<T> = std::result::Result<T, String>;
#[derive(Debug, Clone, Copy)]
pub struct Quote {
    pub price: f64,
    pub at: i64,
    // Receipt time controls HTTP caching, never whether a quote is safe to trade.
    pub received_at: i64,
    pub bid_ask: Option<(f64, f64)>,
    pub open: bool,
}
impl Quote {
    fn single(price: f64, at: i64) -> Self {
        Self {
            price,
            at,
            received_at: now_ms(),
            bid_ask: None,
            open: true,
        }
    }
    pub fn fresh(self, now: i64) -> bool {
        self.open && (-5000..=30000).contains(&(now - self.at))
    }
    pub fn packet(self, symbol: &str, now: i64) -> Value {
        let (bid, ask) = self.bid_ask.unwrap_or_else(|| {
            let half = trading::spread(symbol, self.price) / 2.0;
            (self.price - half, self.price + half)
        });
        json!({"sym":symbol,"price":self.price,"bid":bid,"ask":ask,
            "quote_at":self.at,"quote_valid":self.fresh(now)})
    }
}

pub struct Market {
    pub store: Arc<Store>,
    client: reqwest::Client,
    cache: RwLock<BTreeMap<String, Quote>>,
    sim: Mutex<SimFeed>,
    nstar: Mutex<SimFeed>,
    offline: bool,
    quote_gate: Mutex<()>,
    biquote_retry_at: AtomicI64,
}

impl Market {
    pub async fn new(store: Arc<Store>) -> Result<Self> {
        Self::build(store, false, Prices::new()).await
    }
    /// Deterministic offline fixture mode; not enabled by the production CLI.
    pub async fn offline(store: Arc<Store>, prices: Prices) -> Result<Self> {
        Self::build(store, true, prices).await
    }
    async fn build(store: Arc<Store>, offline: bool, prices: Prices) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent("Mozilla/5.0");
        if offline {
            builder = builder.no_proxy();
        } else if let Some(proxy) = [
            "FX_PROXY",
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
        ]
        .iter()
        .find_map(|key| std::env::var(key).ok().filter(|s| !s.is_empty()))
        {
            builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|_| "代理设置无效")?);
        }
        let client = builder.build().map_err(|_| "无法创建行情客户端")?;
        let sim = SimFeed::load(&store, symbols::SIM, 1.1, now_ms()).await?;
        let nstar = SimFeed::load(&store, symbols::NSTAR, 100.0, now_ms()).await?;
        let now = now_ms();
        Ok(Self {
            store,
            client,
            cache: RwLock::new(
                prices
                    .mid
                    .into_iter()
                    .map(|(s, price)| {
                        let mut q = Quote::single(price, now);
                        q.bid_ask = prices.bid_ask.get(&s).copied();
                        (s, q)
                    })
                    .collect(),
            ),
            sim: Mutex::new(sim),
            nstar: Mutex::new(nstar),
            offline,
            quote_gate: Mutex::new(()),
            biquote_retry_at: AtomicI64::new(0),
        })
    }
    pub fn is_offline(&self) -> bool {
        self.offline
    }
    pub async fn snapshot(&self) -> BTreeMap<String, Quote> {
        self.cache.read().await.clone()
    }
    pub async fn price(&self, symbol: &str) -> Result<f64> {
        Ok(self.quote(symbol).await?.price)
    }
    pub async fn quote(&self, symbol: &str) -> Result<Quote> {
        let start = now_ms();
        let hit = self.cache.read().await.get(symbol).copied();
        if let Some(q) = hit
            && (self.offline || (0..8000).contains(&(start - q.received_at)))
        {
            return Ok(q);
        }
        if self.offline {
            return Err(format!("行情不可用: {symbol}"));
        }
        let result = if symbol == symbols::SIM {
            Ok(Quote::single(self.sim.lock().await.price, start))
        } else if symbol == symbols::NSTAR {
            Ok(self.nstar.lock().await.quote())
        } else if symbols::FX.contains(&symbol) {
            self.biquote_quote(symbol).await
        } else if symbols::STOCK.contains(&symbol) {
            self.yahoo_price(symbol)
                .await
                .map(|price| Quote::single(price, start))
        } else {
            self.fetch_price(symbol)
                .await
                .map(|price| Quote::single(price, start))
        };
        match result {
            Ok(q) => {
                let mut cache = self.cache.write().await;
                if let Some(latest) = cache.get(symbol)
                    && latest.at > q.at
                {
                    return Ok(*latest);
                }
                cache.insert(symbol.into(), q);
                Ok(q)
            }
            // Retain the last price for display; trading_prices refuses stale/closed quotes.
            Err(e) => hit.ok_or(e),
        }
    }
    pub async fn prices<'a>(&self, symbols: impl IntoIterator<Item = &'a str>) -> Result<Prices> {
        let mut out = Prices::new();
        for symbol in trading::price_symbols(symbols)? {
            let q = self.quote(&symbol).await?;
            out.insert(symbol.clone(), valid(q.price)?);
            if let Some(book) = q.bid_ask {
                out.bid_ask.insert(symbol, book);
            }
        }
        Ok(out)
    }
    pub async fn trading_prices<'a>(
        &self,
        symbols: impl IntoIterator<Item = &'a str>,
    ) -> Result<Prices> {
        let needed = trading::price_symbols(symbols)?;
        let prices = self.prices(needed.iter().map(String::as_str)).await?;
        let quotes = self.cache.read().await;
        for symbol in needed {
            if quotes.get(&symbol).is_none_or(|q| {
                !q.open
                    || (!self.offline
                        && (!q.fresh(now_ms())
                            || (symbols::FX.contains(&symbol.as_str()) && q.bid_ask.is_none())))
            }) {
                return Err(format!("报价已过期或市场已关闭: {symbol}"));
            }
        }
        Ok(prices)
    }
    async fn get(&self, url: &str) -> Result<Value> {
        let biquote = url.starts_with("https://biquote.io/");
        if biquote && now_ms() < self.biquote_retry_at.load(Ordering::Relaxed) {
            return Err("行情源退避中".into());
        }
        let response = match self.client.get(url).send().await {
            Ok(response) => response,
            Err(_) => {
                if biquote {
                    self.biquote_retry_at
                        .fetch_max(now_ms() + 2000, Ordering::Relaxed);
                }
                return Err("行情请求失败".into());
            }
        };
        if biquote && matches!(response.status().as_u16(), 429 | 503) {
            self.biquote_retry_at.fetch_max(
                now_ms().saturating_add(retry_delay(response.headers(), now_ms())),
                Ordering::Relaxed,
            );
        }
        response
            .error_for_status()
            .map_err(|_| "行情源返回错误")?
            .json()
            .await
            .map_err(|_| "行情数据格式无效".into())
    }
    pub async fn biquote_price(&self, symbol: &str) -> Result<f64> {
        Ok(self.biquote_quote(symbol).await?.price)
    }
    pub async fn biquote_quote(&self, symbol: &str) -> Result<Quote> {
        let data = self
            .get(&format!("https://biquote.io/api/{symbol}"))
            .await?;
        biquote_quote(&data, symbol, now_ms())
    }
    pub async fn biquote_quotes(&self, symbols: &[String]) -> Result<BTreeMap<String, Quote>> {
        let query = symbols
            .iter()
            .map(|s| format!("symbols={s}"))
            .collect::<Vec<_>>()
            .join("&");
        let data = self
            .get(&format!("https://biquote.io/api/latest?{query}"))
            .await?;
        data.as_object().ok_or("批量报价无效")?;
        let now = now_ms();
        Ok(symbols
            .iter()
            .filter_map(|s| biquote_quote(&data[s], s, now).ok().map(|q| (s.clone(), q)))
            .collect())
    }
    async fn yahoo_chart(&self, symbol: &str, interval: &str) -> Result<Value> {
        let ticker = symbols::yahoo(symbol).ok_or("不支持的币种对")?;
        let (iv, range) = match interval {
            "1m" => ("1m", "3d"),
            "5m" => ("5m", "1mo"),
            "15m" => ("15m", "1mo"),
            "30m" => ("30m", "1mo"),
            "1h" | "4h" => ("60m", "3mo"),
            "1d" => ("1d", "1y"),
            "1w" => ("1wk", "2y"),
            _ => return Err("不支持的周期".into()),
        };
        self.get(&format!("https://query1.finance.yahoo.com/v8/finance/chart/{ticker}?interval={iv}&range={range}&includePrePost=false")).await
    }
    pub async fn yahoo_price(&self, symbol: &str) -> Result<f64> {
        let data = self.yahoo_chart(symbol, "1h").await?;
        valid(
            data["chart"]["result"][0]["meta"]["regularMarketPrice"]
                .as_f64()
                .ok_or("行情不可用")?,
        )
    }
    async fn fetch_price(&self, symbol: &str) -> Result<f64> {
        for host in [
            "data-api.binance.vision",
            "api.binance.com",
            "api1.binance.com",
        ] {
            if let Ok(data) = self
                .get(&format!(
                    "https://{host}/api/v3/ticker/price?symbol={symbol}"
                ))
                .await
                && let Ok(px) = number(&data["price"]).and_then(valid)
            {
                return Ok(px);
            }
        }
        Err(format!("行情不可用: {symbol}"))
    }
    pub async fn klines(&self, symbol: &str, interval: &str, limit: usize) -> Result<Value> {
        if symbols::provider(symbol).is_none() {
            return Err("不支持的币种对".into());
        }
        let sec = symbols::INTERVALS
            .iter()
            .find(|(i, _)| *i == interval)
            .map(|(_, s)| *s)
            .ok_or("不支持的周期")?;
        if sec == 1 && symbol != symbols::SIM && symbol != symbols::NSTAR {
            return Err("仅模拟币支持1秒周期".into());
        }
        if !(1..=1000).contains(&limit) {
            return Err("K线数量无效".into());
        }
        if symbol == symbols::SIM || symbol == symbols::NSTAR {
            let feed = if symbol == symbols::SIM {
                &self.sim
            } else {
                &self.nstar
            };
            let sim = feed.lock().await;
            return Ok(
                json!({"ok":true,"klines":sim.klines(sec,limit),"updated_at":sim.updated_at}),
            );
        }
        if self.offline {
            return Err("离线行情无历史".into());
        }
        let bars = if symbols::FX.contains(&symbol) {
            // The provider has no 1w endpoint: aggregate its daily bars, not another feed.
            let (iv, count) = if interval == "1w" {
                ("1d", (limit * 7 + 7).min(1000))
            } else {
                (interval, limit)
            };
            let data = self
                .get(&format!(
                    "https://biquote.io/api/{symbol}/ohlc?interval={iv}&limit={count}"
                ))
                .await?;
            if data["symbol"].as_str() != Some(symbol) {
                return Err("K线币对不匹配".into());
            }
            let bars = biquote_bars(&data)?;
            if interval == "1w" {
                weekly_bars(bars, limit)
            } else {
                bars
            }
        } else if symbols::STOCK.contains(&symbol) {
            let data = self.yahoo_chart(symbol, interval).await?;
            let result = &data["chart"]["result"][0];
            let quote = &result["indicators"]["quote"][0];
            let times = result["timestamp"].as_array().ok_or("K线无效")?;
            let mut bars = Vec::new();
            for (i, t) in times.iter().enumerate() {
                let (Some(time), Some(o), Some(h), Some(l), Some(c)) = (
                    t.as_i64(),
                    quote["open"][i].as_f64(),
                    quote["high"][i].as_f64(),
                    quote["low"][i].as_f64(),
                    quote["close"][i].as_f64(),
                ) else {
                    continue;
                };
                bars.push([(time * 1000) as f64, o, h, l, c]);
            }
            let skip = bars.len().saturating_sub(limit);
            bars.into_iter().skip(skip).collect()
        } else {
            let raw=self.get(&format!("https://data-api.binance.vision/api/v3/klines?symbol={symbol}&interval={interval}&limit={limit}")).await?;
            raw.as_array()
                .ok_or("K线无效")?
                .iter()
                .map(|r| {
                    Ok([
                        number(&r[0])?,
                        number(&r[1])?,
                        number(&r[2])?,
                        number(&r[3])?,
                        number(&r[4])?,
                    ])
                })
                .collect::<Result<Vec<_>>>()?
        };
        Ok(json!({"ok":true,"klines":bars}))
    }
    /// Update the shared cache then settle every affected account, even with no browsers.
    pub async fn on_quote(&self, symbol: &str, price: f64, at: i64) -> Result<bool> {
        self.on_market_quote(symbol, Quote::single(price, at)).await
    }
    pub async fn on_market_quote(&self, symbol: &str, quote: Quote) -> Result<bool> {
        valid(quote.price)?;
        if !self.offline && symbols::FX.contains(&symbol) && quote.bid_ask.is_none() {
            return Err("FX缺少源买卖报价".into());
        }
        if let Some((bid, ask)) = quote.bid_ask {
            valid(bid)?;
            valid(ask)?;
            if ask < bid {
                return Err("买卖报价无效".into());
            }
        }
        // Keep accepted quote order and risk settlement order identical across upstream tasks.
        let _guard = self.quote_gate.lock().await;
        let quotes = {
            let mut cache = self.cache.write().await;
            if cache.get(symbol).is_some_and(|q| q.at > quote.at) {
                return Ok(false);
            }
            cache.insert(symbol.into(), quote);
            cache.clone()
        };
        self.check_risk(symbol, &quotes, now_ms()).await?;
        Ok(true)
    }
    async fn check_risk(
        &self,
        symbol: &str,
        quotes: &BTreeMap<String, Quote>,
        now: i64,
    ) -> Result<()> {
        let users = self.store.users().await.map_err(|_| "风险账户读取失败")?;
        for user in users {
            let needed = match trading::price_symbols(
                user.account.positions.iter().map(|p| p.symbol.as_str()),
            ) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if !needed.contains(symbol) {
                continue;
            }
            let mut prices = Prices::new();
            for (s, q) in quotes.iter().filter(|(_, q)| q.fresh(now)) {
                if !self.offline && symbols::FX.contains(&s.as_str()) && q.bid_ask.is_none() {
                    continue;
                }
                prices.insert(s.clone(), q.price);
                if let Some(book) = q.bid_ask {
                    prices.bid_ask.insert(s.clone(), book);
                }
            }
            if needed.iter().any(|s| !prices.contains_key(s)) {
                continue;
            }
            // Re-load current holdings under Store's mutation lock; stale account snapshots cannot double-settle.
            if let Err(e) = self
                .store
                .transaction(&user.name, |a, _, _| a.check_stopout(&prices, now))
                .await
            {
                match e {
                    crate::storage::Error::Invalid(_) => {}
                    _ => return Err("强平保存失败，账户未改动".into()),
                }
            }
        }
        Ok(())
    }
    pub async fn record_sim(&self, price: f64, now: i64) -> Result<Value> {
        let mut sim = self.sim.lock().await;
        let mut next = sim.clone();
        next.record(valid(price)?, now);
        self.store
            .save_bars(symbols::SIM, &next.pending())
            .await
            .map_err(|_| "模拟行情保存失败")?;
        let packet = next.packet();
        *sim = next;
        drop(sim);
        self.on_quote(symbols::SIM, price, now).await?;
        Ok(packet)
    }
    pub async fn tick_sim(&self) -> Result<Value> {
        Ok(self.tick_feed(&self.sim).await?.0)
    }
    /// One NSTAR tick: the packet, plus market events (rumors, news, halts, whales) for everyone.
    pub async fn tick_nstar(&self) -> Result<(Value, Vec<Value>)> {
        self.tick_feed(&self.nstar).await
    }
    async fn tick_feed(&self, feed: &Mutex<SimFeed>) -> Result<(Value, Vec<Value>)> {
        let mut sim = feed.lock().await;
        let mut next = sim.clone();
        if next.symbol == symbols::NSTAR {
            // Liquidations and resets settled outside the desk land on the next tick.
            let (player, forced) = self.store.take_nstar_flow();
            next.flow(player, forced);
        }
        let price = next.next_price();
        let symbol = next.symbol;
        let now = now_ms();
        next.record(price, now);
        self.store
            .save_bars(symbol, &next.pending())
            .await
            .map_err(|_| "模拟行情保存失败")?;
        let events = std::mem::take(&mut next.book.events);
        let packet = next.packet();
        let quote = next.quote();
        *sim = next;
        drop(sim);
        self.on_market_quote(symbol, quote).await?;
        Ok((packet, events))
    }
    /// Lock the NSTAR book for one account operation. Orders are priced and applied one at a
    /// time, so splitting an order across requests cannot reuse a mid it already moved.
    pub async fn nstar_desk(&self) -> NstarDesk<'_> {
        NstarDesk {
            market: self,
            feed: self.nstar.lock().await,
        }
    }
}

pub struct NstarDesk<'a> {
    market: &'a Market,
    feed: tokio::sync::MutexGuard<'a, SimFeed>,
}
impl NstarDesk<'_> {
    /// Price NSTAR from the live book; refuses while the circuit breaker holds trading.
    pub fn price(&self, prices: &mut Prices) -> Result<()> {
        let mid = self.feed.live_mid().ok_or("NSTAR 熔断中，暂停交易")?;
        prices.insert(symbols::NSTAR.into(), mid);
        Ok(())
    }
    /// Move the book by the committed flow and publish the new price before unlocking.
    pub async fn commit(self) -> Result<(Value, Vec<Value>)> {
        let Self { market, mut feed } = self;
        let (player, forced) = market.store.take_nstar_flow();
        feed.flow(player, forced);
        let price = feed.book.published();
        feed.record(price, now_ms());
        if market
            .store
            .save_bars(feed.symbol, &feed.pending())
            .await
            .is_err()
        {
            // The book already moved in memory; the next tick persists the candle.
            eprintln!("[nstar] order candle persistence failed; will retry on tick");
        }
        let events = std::mem::take(&mut feed.book.events);
        let packet = feed.packet();
        let quote = feed.quote();
        drop(feed);
        market.on_market_quote(symbols::NSTAR, quote).await?;
        Ok((packet, events))
    }
}

fn valid(price: f64) -> Result<f64> {
    if price.is_finite() && price > 0.0 {
        Ok(price)
    } else {
        Err("行情不可用".into())
    }
}
fn number(v: &Value) -> Result<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .filter(|v| v.is_finite())
        .ok_or("行情数据格式无效".into())
}
fn stamp(v: &Value) -> Result<i64> {
    chrono::DateTime::parse_from_rfc3339(v.as_str().ok_or("行情时间无效")?)
        .map(|d| d.timestamp_millis())
        .map_err(|_| "行情时间无效".into())
}

fn retry_delay(headers: &reqwest::header::HeaderMap, now: i64) -> i64 {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.parse::<i64>()
                .ok()
                .filter(|s| *s >= 0)
                .map(|s| s.max(1).saturating_mul(1000))
                .or_else(|| {
                    chrono::DateTime::parse_from_rfc2822(v)
                        .ok()
                        .map(|date| date.timestamp_millis().saturating_sub(now).max(1000))
                })
        })
        .unwrap_or(60000)
}

pub fn biquote_quote(data: &Value, symbol: &str, now: i64) -> Result<Quote> {
    if data["symbol"].as_str() != Some(symbol) {
        return Err("行情币对不匹配".into());
    }
    let bid = valid(number(&data["bid"])?)?;
    let ask = valid(number(&data["ask"])?)?;
    let mid = valid(number(&data["mid"])?)?;
    let at = stamp(&data["timestamp"])?;
    if ask < bid || at > now + 5000 || (mid - (bid + ask) / 2.0).abs() > mid * 1e-10 {
        return Err("买卖报价无效".into());
    }
    let state = data["marketState"].as_str().ok_or("行情市场状态缺失")?;
    let stale = data["stale"].as_bool().ok_or("行情时效状态缺失")?;
    Ok(Quote {
        price: mid,
        at,
        received_at: now,
        bid_ask: Some((bid, ask)),
        open: state == "open" && !stale,
    })
}

pub fn latest_biquote(data: &Value) -> Result<f64> {
    let bars = data["bars"].as_array().ok_or("行情不可用")?;
    // Do not rely on limit=1 or undocumented array ordering: the newest active candle updates intra-minute.
    let mut latest = None;
    for bar in bars {
        let t = stamp(&bar["openTime"])?;
        if latest.is_none_or(|(old, _)| t > old) {
            latest = Some((t, valid(number(&bar["close"])?)?));
        }
    }
    latest.map(|(_, p)| p).ok_or("行情不可用".into())
}
fn biquote_bars(data: &Value) -> Result<Vec<[f64; 5]>> {
    let mut out = data["bars"]
        .as_array()
        .ok_or("K线无效")?
        .iter()
        .map(|b| {
            Ok([
                stamp(&b["openTime"])? as f64,
                number(&b["open"])?,
                number(&b["high"])?,
                number(&b["low"])?,
                number(&b["close"])?,
            ])
        })
        .collect::<Result<Vec<_>>>()?;
    out.sort_by(|a, b| a[0].total_cmp(&b[0]));
    out.dedup_by(|a, b| a[0] == b[0]);
    Ok(out)
}
fn weekly_bars(bars: Vec<[f64; 5]>, limit: usize) -> Vec<[f64; 5]> {
    const DAY: i64 = 86_400_000;
    let mut weeks = BTreeMap::<i64, [f64; 5]>::new();
    for bar in bars {
        // UTC Monday boundary; retain the current incomplete week.
        let start = ((bar[0] as i64 + 3 * DAY).div_euclid(7 * DAY)) * 7 * DAY - 3 * DAY;
        let week = weeks
            .entry(start)
            .or_insert([start as f64, bar[1], bar[2], bar[3], bar[4]]);
        week[2] = week[2].max(bar[2]);
        week[3] = week[3].min(bar[3]);
        week[4] = bar[4];
    }
    let skip = weeks.len().saturating_sub(limit);
    weeks.into_values().skip(skip).collect()
}

#[derive(Clone)]
struct SimFeed {
    symbol: &'static str,
    base: f64,
    price: f64,
    trend: f64,
    /// Order-book state; only used when the symbol has depth (NSTAR).
    book: Book,
    kind: usize,
    left: u32,
    updated_at: i64,
    bars: BTreeMap<i64, VecDeque<[f64; 5]>>,
}
impl SimFeed {
    fn new(symbol: &'static str, base: f64, now: i64) -> Self {
        Self {
            symbol,
            base,
            price: base,
            trend: 0.0,
            book: Book::new(base, base),
            kind: 0,
            left: 0,
            updated_at: now,
            bars: symbols::INTERVALS
                .iter()
                .map(|(_, s)| {
                    (
                        *s,
                        VecDeque::from([[((now / 1000) / s * s) as f64, base, base, base, base]]),
                    )
                })
                .collect(),
        }
    }
    async fn load(store: &Store, symbol: &'static str, base: f64, now: i64) -> Result<Self> {
        let mut sim = Self::new(symbol, base, now);
        let mut loaded = Vec::new();
        for (_, sec) in symbols::INTERVALS {
            let bars = store
                .load_bars(symbol, *sec)
                .await
                .map_err(|_| "模拟历史读取失败")?;
            if !bars.is_empty() {
                sim.bars.insert(*sec, bars.into());
                loaded.push(*sec);
            }
        }
        sim.price = sim.bars[&60].back().unwrap()[4];
        sim.book = Book::new(base, sim.price);
        for (sec, bars) in &mut sim.bars {
            if !loaded.contains(sec) {
                let t = ((now / 1000) / sec * sec) as f64;
                *bars = VecDeque::from([[t, sim.price, sim.price, sim.price, sim.price]]);
            }
        }
        sim.record(sim.price, now);
        store
            .save_bars(symbol, &sim.pending())
            .await
            .map_err(|_| "模拟历史保存失败")?;
        Ok(sim)
    }
    fn record(&mut self, price: f64, now: i64) {
        self.price = price;
        self.updated_at = now;
        for (sec, bars) in &mut self.bars {
            let t = ((now / 1000) / sec * sec) as f64;
            if t < bars.back().unwrap()[0] {
                continue;
            }
            if t != bars.back().unwrap()[0] {
                bars.push_back([t, price, price, price, price]);
                if bars.len() > 1000 {
                    bars.pop_front();
                }
            } else {
                let bar = bars.back_mut().unwrap();
                bar[2] = bar[2].max(price);
                bar[3] = bar[3].min(price);
                bar[4] = price;
            }
        }
    }
    fn pending(&self) -> Vec<(i64, [f64; 5], i64)> {
        self.bars
            .iter()
            .map(|(sec, bars)| (*sec, *bars.back().unwrap(), bars.front().unwrap()[0] as i64))
            .collect()
    }
    fn klines(&self, sec: i64, limit: usize) -> Vec<[f64; 5]> {
        self.bars[&sec]
            .iter()
            .skip(self.bars[&sec].len().saturating_sub(limit))
            .map(|b| [b[0] * 1000.0, b[1], b[2], b[3], b[4]])
            .collect()
    }
    fn quote(&self) -> Quote {
        Quote {
            open: self.book.halt == 0,
            ..Quote::single(self.price, self.updated_at)
        }
    }
    fn packet(&self) -> Value {
        let mut packet = self.quote().packet(self.symbol, self.updated_at);
        packet["time"] = json!(self.updated_at);
        packet["klines"] = json!(
            symbols::INTERVALS
                .iter()
                .map(|(itv, sec)| (*itv, json!(self.klines(*sec, 2))))
                .collect::<BTreeMap<_, _>>()
        );
        packet
    }
    fn regime(&mut self, rng: &mut impl Rng) {
        if self.left == 0 {
            let r = rng.random::<f64>() * 10.0;
            let mut sum = 0.0;
            for (i, w) in [6.0, 1.2, 1.2, 0.6, 0.6].iter().enumerate() {
                sum += w;
                if r <= sum {
                    self.kind = i;
                    break;
                }
            }
            self.left = [20, 6, 40, 15, 15][self.kind];
        }
        self.left -= 1;
    }
    fn next_price(&mut self) -> f64 {
        if trading::depth(self.symbol) > 0.0 {
            return self.next_book_price();
        }
        let mut rng = rand::rng();
        self.regime(&mut rng);
        self.trend *= 0.95;
        let mut shock: f64 = StandardNormal.sample(&mut rng);
        shock *= 0.0004;
        match self.kind {
            1 => shock *= if rng.random::<bool>() { 6.0 } else { -6.0 },
            2 => {
                self.trend += if rng.random::<bool>() {
                    0.00012
                } else {
                    -0.00012
                }
            }
            3 => shock -= 0.0006,
            4 => shock += 0.0006,
            _ => {}
        }
        let scale = self.base / 1.1;
        ((self.price + 0.003 * (self.base - self.price) + (shock + self.trend) * scale)
            .max(0.5 * scale)
            * 1e5)
            .round()
            / 1e5
    }
    /// Log-price walk around a drifting fair value, with news shocks and a circuit breaker.
    /// While halted the book keeps moving unpublished, so trading resumes with a gap.
    fn next_book_price(&mut self) -> f64 {
        let mut rng = rand::rng();
        self.regime(&mut rng);
        self.trend *= 0.95;
        let mut shock: f64 = StandardNormal.sample(&mut rng);
        shock *= TICK_VOL;
        match self.kind {
            1 => shock *= 6.0,
            2 => self.trend += if rng.random::<bool>() { 0.3 } else { -0.3 } * TICK_VOL,
            3 => shock -= 1.5 * TICK_VOL,
            4 => shock += 1.5 * TICK_VOL,
            _ => {}
        }
        let drift: f64 = StandardNormal.sample(&mut rng);
        let book = &mut self.book;
        book.fair += FAIR_REVERT * (book.base - book.fair) + drift * FAIR_VOL;
        book.news(&mut rng);
        book.log += REVERT * (book.fair - book.log) + shock + self.trend;
        book.clamp();
        if book.breaker(self.price) {
            self.price
        } else {
            book.published()
        }
    }
    /// Apply player and liquidation flow (signed units) to the book right away.
    fn flow(&mut self, units: f64, forced: f64) {
        let depth = trading::depth(self.symbol);
        if depth > 0.0 {
            self.book.flow(units, depth, false);
            self.book.flow(forced, depth, true);
        }
    }
    /// Unrounded mid the next fill is priced from; None while the breaker holds trading.
    fn live_mid(&self) -> Option<f64> {
        (self.book.halt == 0).then(|| self.book.log.exp())
    }
}

const TICK_VOL: f64 = 0.0003;
const FAIR_VOL: f64 = 0.00012;
/// Fair value drifts back toward base with a ~1 h half-life.
const FAIR_REVERT: f64 = 4e-5;
/// Fair value stays within base/4 ..= base*4 (25..=400 for NSTAR).
const FAIR_BAND: f64 = 2.0 * std::f64::consts::LN_2;
/// Pull of price toward fair value per tick: a displacement halves in ~46 s.
const REVERT: f64 = 0.003;
/// Price stays within fair/2 ..= fair*2 whatever the flow.
const STRETCH: f64 = std::f64::consts::LN_2;
/// Share of flow impact that becomes fair value instead of reverting.
const CONVICTION: f64 = 0.2;
const BREAKER_WINDOW: usize = 300;
/// ln(1.10): a 10% move from any print in the last minute halts trading.
const BREAKER_MOVE: f64 = 0.0953;
const HALT_TICKS: u32 = 50;
const RUMOR_LEAD: u32 = 25;
const RUMOR_ACCURACY: f64 = 0.65;
const NEWS_MIN_GAP: u32 = 1500;
/// Flow that moves price by 1.5% or more (~1,500 NSTAR) is announced.
const BIG_FLOW: f64 = 0.015;

#[derive(Clone)]
struct Book {
    /// True log price; while halted it keeps moving but is not published.
    log: f64,
    fair: f64,
    base: f64,
    halt: u32,
    window: VecDeque<f64>,
    news_in: u32,
    pending: Option<f64>,
    events: Vec<Value>,
}
impl Book {
    fn new(base: f64, price: f64) -> Self {
        let lb = base.ln();
        let log = price.ln();
        let mut book = Self {
            log,
            fair: log,
            base: lb,
            halt: 0,
            window: VecDeque::from([log]),
            news_in: NEWS_MIN_GAP,
            pending: None,
            events: Vec::new(),
        };
        book.clamp();
        book
    }
    fn clamp(&mut self) {
        self.fair = self
            .fair
            .clamp(self.base - FAIR_BAND, self.base + FAIR_BAND);
        self.log = self.log.clamp(self.fair - STRETCH, self.fair + STRETCH);
    }
    fn published(&self) -> f64 {
        (self.log.exp() * 1e5).round() / 1e5
    }
    fn event(&mut self, kind: &str, text: String) {
        self.events
            .push(json!({"type":"nstar","kind":kind,"text":text,"time":now_ms()}));
    }
    /// A rumor, right 65% of the time, precedes each news shock by five seconds.
    fn news(&mut self, rng: &mut impl Rng) {
        const GOOD: &[&str] = &[
            "传闻：北极星基金会即将公布重大合作",
            "传闻：头部交易所准备上线 NSTAR 永续合约",
            "传闻：神秘巨鲸正在场外吸筹",
        ];
        const BAD: &[&str] = &[
            "传闻：北极星核心钱包疑似异动",
            "传闻：监管机构点名调查 NSTAR 项目方",
            "传闻：早期投资人解锁筹码准备出货",
        ];
        self.news_in = self.news_in.saturating_sub(1);
        if self.news_in == RUMOR_LEAD {
            let up = rng.random::<bool>();
            let size = rng.random_range(0.02..0.07);
            self.pending = Some(if up { size } else { -size });
            let hint = if rng.random::<f64>() < RUMOR_ACCURACY {
                up
            } else {
                !up
            };
            let pool = if hint { GOOD } else { BAD };
            let text = pool[rng.random_range(0..pool.len())];
            self.event("rumor", format!("{text}（5 秒后见分晓，消息未经证实）"));
        }
        if self.news_in == 0 {
            if let Some(jump) = self.pending.take() {
                self.fair += jump;
                self.log += jump * 0.5;
                let pct = jump.exp_m1() * 100.0;
                let text = if jump > 0.0 {
                    format!("利好落地：NSTAR 公允价值 {pct:+.1}%")
                } else {
                    format!("利空坐实：NSTAR 公允价值 {pct:+.1}%")
                };
                self.event("news", text);
            }
            let mean = NEWS_MIN_GAP as f64;
            let gap: f64 = rand_distr::Exp::new(1.0 / mean).unwrap().sample(rng);
            self.news_in = NEWS_MIN_GAP + gap.min(mean * 6.0) as u32;
        }
    }
    /// True while halted. Trips when price moves 10% from any print in the last minute.
    fn breaker(&mut self, last: f64) -> bool {
        if self.halt > 0 {
            self.halt -= 1;
            if self.halt > 0 {
                return true;
            }
            self.window = VecDeque::from([self.log]);
            let gap = (self.log.exp() / last - 1.0) * 100.0;
            self.event("resume", format!("NSTAR 恢复交易，跳空 {gap:+.1}%"));
            return false;
        }
        self.window.push_back(self.log);
        if self.window.len() > BREAKER_WINDOW {
            self.window.pop_front();
        }
        let (lo, hi) = self
            .window
            .iter()
            .fold((f64::MAX, f64::MIN), |(lo, hi), x| (lo.min(*x), hi.max(*x)));
        let from = if self.log - lo > BREAKER_MOVE {
            lo
        } else if hi - self.log > BREAKER_MOVE {
            hi
        } else {
            return false;
        };
        self.halt = HALT_TICKS;
        let pct = (self.log - from).exp_m1() * 100.0;
        self.event(
            "halt",
            format!("NSTAR 一分钟内波动 {pct:+.1}%，触发熔断，暂停交易 10 秒"),
        );
        true
    }
    /// Signed units bought (+) or sold (-) move the book immediately.
    fn flow(&mut self, units: f64, depth: f64, forced: bool) {
        if units == 0.0 {
            return;
        }
        let d = units / depth;
        self.log += d;
        self.fair += CONVICTION * d;
        self.clamp();
        if d.abs() >= BIG_FLOW {
            let side = if units > 0.0 { "买入" } else { "卖出" };
            let pct = d.exp_m1() * 100.0;
            let size = units.abs();
            let text = if forced {
                format!("强平潮：{size:.0} NSTAR 被强制{side}，价格 {pct:+.1}%")
            } else {
                format!("巨鲸出没：有人一口气{side} {size:.0} NSTAR，价格 {pct:+.1}%")
            };
            self.event("whale", text);
        }
    }
}
