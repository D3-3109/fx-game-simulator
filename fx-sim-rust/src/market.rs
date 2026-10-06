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
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, RwLock};

pub type Result<T> = std::result::Result<T, String>;
#[derive(Debug, Clone, Copy)]
pub struct Quote {
    pub price: f64,
    pub at: i64,
}

pub struct Market {
    pub store: Arc<Store>,
    client: reqwest::Client,
    cache: RwLock<BTreeMap<String, Quote>>,
    sim: Mutex<SimFeed>,
    offline: bool,
    quote_gate: Mutex<()>,
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
        let sim = SimFeed::load(&store, now_ms()).await?;
        let now = now_ms();
        Ok(Self {
            store,
            client,
            cache: RwLock::new(
                prices
                    .into_iter()
                    .map(|(s, price)| (s, Quote { price, at: now }))
                    .collect(),
            ),
            sim: Mutex::new(sim),
            offline,
            quote_gate: Mutex::new(()),
        })
    }
    pub fn is_offline(&self) -> bool {
        self.offline
    }
    pub async fn snapshot(&self) -> BTreeMap<String, Quote> {
        self.cache.read().await.clone()
    }
    pub async fn price(&self, symbol: &str) -> Result<f64> {
        let start = now_ms();
        let hit = self.cache.read().await.get(symbol).copied();
        if let Some(q) = hit
            && (self.offline || start - q.at < 8000)
        {
            return Ok(q.price);
        }
        if self.offline {
            return Err(format!("行情不可用: {symbol}"));
        }
        if symbol == symbols::SIM {
            return Ok(self.sim.lock().await.price);
        }
        let result = self.fetch_price(symbol).await;
        match result {
            Ok(price) => {
                valid(price)?;
                let mut cache = self.cache.write().await;
                if let Some(latest) = cache.get(symbol)
                    && latest.at > start
                {
                    return Ok(latest.price);
                }
                cache.insert(symbol.into(), Quote { price, at: start });
                Ok(price)
            }
            Err(e) => hit.map(|q| q.price).ok_or(e),
        }
    }
    pub async fn prices<'a>(&self, symbols: impl IntoIterator<Item = &'a str>) -> Result<Prices> {
        let mut out = Prices::new();
        for symbol in trading::price_symbols(symbols)? {
            out.insert(symbol.clone(), valid(self.price(&symbol).await?)?);
        }
        Ok(out)
    }
    async fn get(&self, url: &str) -> Result<Value> {
        self.client
            .get(url)
            .send()
            .await
            .map_err(|_| "行情请求失败")?
            .error_for_status()
            .map_err(|_| "行情源返回错误")?
            .json()
            .await
            .map_err(|_| "行情数据格式无效".into())
    }
    pub async fn biquote_price(&self, symbol: &str) -> Result<f64> {
        let data = self
            .get(&format!(
                "https://biquote.io/api/{symbol}/ohlc?interval=1m&limit=1"
            ))
            .await?;
        latest_biquote(&data)
    }
    async fn yahoo_chart(&self, symbol: &str, interval: &str) -> Result<Value> {
        let ticker = symbols::yahoo(symbol).ok_or("不支持的币种对")?;
        let (iv, range) = match interval {
            "1m" => ("1m", "3d"),
            "5m" => ("5m", "1mo"),
            "15m" => ("15m", "1mo"),
            "30m" => ("30m", "2mo"),
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
        if symbols::FX.contains(&symbol) {
            if let Ok(px) = self.biquote_price(symbol).await {
                return Ok(px);
            }
            if let Ok(px) = self.yahoo_price(symbol).await {
                return Ok(px);
            }
        }
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
        if !(1..=1000).contains(&limit) {
            return Err("K线数量无效".into());
        }
        if symbol == symbols::SIM {
            let sim = self.sim.lock().await;
            return Ok(
                json!({"ok":true,"klines":sim.klines(sec,limit),"updated_at":sim.updated_at}),
            );
        }
        if self.offline {
            return Err("离线行情无历史".into());
        }
        let bars = if symbols::FX.contains(&symbol) {
            let bi = self
                .get(&format!(
                    "https://biquote.io/api/{symbol}/ohlc?interval={interval}&limit={limit}"
                ))
                .await
                .and_then(|d| biquote_bars(&d));
            match bi {
                Ok(b) => b,
                Err(_) => yahoo_bars(&self.yahoo_chart(symbol, interval).await?, interval, limit)?,
            }
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
        valid(price)?;
        // Keep accepted quote order and risk settlement order identical across upstream tasks.
        let _guard = self.quote_gate.lock().await;
        let quotes = {
            let mut cache = self.cache.write().await;
            if cache.get(symbol).is_some_and(|q| q.at > at) {
                return Ok(false);
            }
            cache.insert(symbol.into(), Quote { price, at });
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
            let prices: Prices = quotes
                .iter()
                .filter(|(_, q)| now - q.at <= 30000)
                .map(|(s, q)| (s.clone(), q.price))
                .collect();
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
            .save_bars(&next.pending())
            .await
            .map_err(|_| "模拟行情保存失败")?;
        let packet = next.packet();
        *sim = next;
        drop(sim);
        self.on_quote(symbols::SIM, price, now).await?;
        Ok(packet)
    }
    pub async fn tick_sim(&self) -> Result<Value> {
        let mut sim = self.sim.lock().await;
        let mut next = sim.clone();
        let price = next.next_price();
        let now = now_ms();
        next.record(price, now);
        self.store
            .save_bars(&next.pending())
            .await
            .map_err(|_| "模拟行情保存失败")?;
        let packet = next.packet();
        *sim = next;
        drop(sim);
        self.on_quote(symbols::SIM, price, now).await?;
        Ok(packet)
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
fn yahoo_bars(data: &Value, interval: &str, limit: usize) -> Result<Vec<[f64; 5]>> {
    let r = &data["chart"]["result"][0];
    let ts = r["timestamp"].as_array().ok_or("K线无效")?;
    let q = &r["indicators"]["quote"][0];
    let mut out = Vec::new();
    for (i, t) in ts.iter().enumerate() {
        let fields = [&q["open"][i], &q["high"][i], &q["low"][i], &q["close"][i]];
        if fields.iter().any(|v| v.is_null()) {
            continue;
        }
        let rounded = |v: &Value| number(v).map(|x| (x * 1e6).round() / 1e6);
        out.push([
            number(t)? * 1000.0,
            rounded(fields[0])?,
            rounded(fields[1])?,
            rounded(fields[2])?,
            rounded(fields[3])?,
        ]);
    }
    if interval == "4h" {
        out = out
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| {
                [
                    c[0][0],
                    c[0][1],
                    c.iter().map(|b| b[2]).fold(f64::NEG_INFINITY, f64::max),
                    c.iter().map(|b| b[3]).fold(f64::INFINITY, f64::min),
                    c[3][4],
                ]
            })
            .collect();
    }
    Ok(out
        .into_iter()
        .rev()
        .take(limit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect())
}

#[derive(Clone)]
struct SimFeed {
    price: f64,
    trend: f64,
    kind: usize,
    left: u32,
    updated_at: i64,
    bars: BTreeMap<i64, VecDeque<[f64; 5]>>,
}
impl SimFeed {
    fn new(now: i64) -> Self {
        Self {
            price: 1.1,
            trend: 0.0,
            kind: 0,
            left: 0,
            updated_at: now,
            bars: symbols::INTERVALS
                .iter()
                .map(|(_, s)| {
                    (
                        *s,
                        VecDeque::from([[((now / 1000) / s * s) as f64, 1.1, 1.1, 1.1, 1.1]]),
                    )
                })
                .collect(),
        }
    }
    async fn load(store: &Store, now: i64) -> Result<Self> {
        let mut sim = Self::new(now);
        let mut loaded = Vec::new();
        for (_, sec) in symbols::INTERVALS {
            let bars = store
                .load_bars(*sec)
                .await
                .map_err(|_| "模拟历史读取失败")?;
            if !bars.is_empty() {
                sim.bars.insert(*sec, bars.into());
                loaded.push(*sec);
            }
        }
        sim.price = sim.bars[&60].back().unwrap()[4];
        for (sec, bars) in &mut sim.bars {
            if !loaded.contains(sec) {
                let t = ((now / 1000) / sec * sec) as f64;
                *bars = VecDeque::from([[t, sim.price, sim.price, sim.price, sim.price]]);
            }
        }
        sim.record(sim.price, now);
        store
            .save_bars(&sim.pending())
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
    fn packet(&self) -> Value {
        json!({"sym":symbols::SIM,"price":self.price,"time":self.updated_at,"klines":symbols::INTERVALS.iter().map(|(itv,sec)|(*itv,json!(self.klines(*sec,2)))).collect::<BTreeMap<_,_>>()})
    }
    fn next_price(&mut self) -> f64 {
        let mut rng = rand::rng();
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
        ((self.price + 0.003 * (1.1 - self.price) + shock + self.trend).max(0.5) * 1e5).round()
            / 1e5
    }
}
