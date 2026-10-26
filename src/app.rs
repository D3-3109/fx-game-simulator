#[path = "leaderboard.rs"]
mod leaderboard;

use crate::{
    limits::{Limiter, Limits, Net},
    market::Market,
    storage::{self, Store},
    symbols, trading,
};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use subtle::ConstantTimeEq;
use tokio::sync::broadcast;

#[derive(Clone)]
pub(crate) struct StatePacket {
    pub value: Value,
    pub text: Arc<str>,
    pub key: Arc<str>,
}
struct StateCache {
    revision: (u64, u64, u64),
    expires: Instant,
    packet: Arc<StatePacket>,
}
struct QuotePacket {
    sequence: u64,
    full: Arc<str>,
    intervals: BTreeMap<String, Arc<str>>,
}

#[derive(Clone, Debug)]
pub enum Event {
    /// Pre-serialized JSON, shared by every socket instead of re-encoded per connection.
    /// Quote ticks carry their symbol so sockets can skip pairs nobody on them is viewing.
    Message(Arc<str>, Option<Arc<str>>),
    Deleted(String),
    Shutdown,
}

pub struct App {
    pub store: Arc<Store>,
    pub market: Arc<Market>,
    pub events: broadcast::Sender<Event>,
    // Quote traffic never occupies the reliable/control subscriber's event ring.
    controls: broadcast::Sender<Event>,
    quotes: Mutex<BTreeMap<String, QuotePacket>>,
    quote_seq: AtomicU64,
    leaderboard: leaderboard::Leaderboard,
    states: Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<Option<StateCache>>>>>,
    pub admin_key: PathBuf,
    admin_sessions: Mutex<BTreeMap<String, i64>>,
    admin_attempts: Mutex<VecDeque<i64>>,
    presence: Arc<Mutex<BTreeMap<u64, String>>>,
    connection_seq: AtomicU64,
    pub shutting_down: AtomicBool,
    pub limits: Limits,
    pub limiter: Limiter,
    /// Proxy hops whose `X-Forwarded-For` is trusted; empty means use the socket peer.
    pub trusted_proxies: Vec<Net>,
}
pub struct Presence {
    id: u64,
    map: Arc<Mutex<BTreeMap<u64, String>>>,
}
impl Drop for Presence {
    fn drop(&mut self) {
        self.map.lock().unwrap().remove(&self.id);
    }
}

impl App {
    pub fn new(store: Arc<Store>, market: Arc<Market>, admin_key: PathBuf) -> Self {
        let (events, _) = broadcast::channel(256);
        let (controls, _) = broadcast::channel(256);
        Self {
            store,
            market,
            events,
            controls,
            quotes: Mutex::new(BTreeMap::new()),
            quote_seq: AtomicU64::new(1),
            leaderboard: leaderboard::Leaderboard::default(),
            states: Mutex::new(BTreeMap::new()),
            admin_key,
            admin_sessions: Mutex::new(BTreeMap::new()),
            admin_attempts: Mutex::new(VecDeque::new()),
            presence: Arc::new(Mutex::new(BTreeMap::new())),
            connection_seq: AtomicU64::new(1),
            shutting_down: AtomicBool::new(false),
            limits: Limits::default(),
            limiter: Limiter::default(),
            trusted_proxies: Vec::new(),
        }
    }
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }
    pub fn with_trusted_proxies(mut self, proxies: Vec<Net>) -> Self {
        self.trusted_proxies = proxies;
        self
    }
    /// Client address for limits. Forwarded headers count only when the direct peer is a
    /// trusted proxy, and then the right-most hop not itself a trusted proxy is used,
    /// since anything left of it is client-controlled.
    pub fn client_ip(
        &self,
        peer: Option<std::net::IpAddr>,
        headers: &axum::http::HeaderMap,
    ) -> Option<std::net::IpAddr> {
        let peer = peer.map(|ip| ip.to_canonical());
        let trusted = |ip: &std::net::IpAddr| self.trusted_proxies.iter().any(|n| n.contains(*ip));
        if !peer.as_ref().is_some_and(trusted) {
            return peer;
        }
        let hops: Vec<std::net::IpAddr> = headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .filter_map(|s| s.trim().parse::<std::net::IpAddr>().ok())
            .map(|ip| ip.to_canonical())
            .collect();
        hops.into_iter().rev().find(|ip| !trusted(ip)).or(peer)
    }
    pub fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        let _ = self.events.send(Event::Shutdown);
        let _ = self.controls.send(Event::Shutdown);
    }
    pub fn broadcast(&self, message: Value) {
        let symbol: Option<Arc<str>> = (message.get("type").is_none())
            .then(|| message["sym"].as_str().map(Arc::from))
            .flatten();
        let text: Arc<str> = message.to_string().into();
        if let Some(symbol) = &symbol {
            let mut intervals = BTreeMap::new();
            if let Some(bars) = message["klines"].as_object() {
                // Serialize once per shared tick/interval, never parse or recode at each socket.
                let mut variant = message.clone();
                variant["klines"] = json!({});
                for (interval, _) in symbols::INTERVALS {
                    if let Some(rows) = bars.get(*interval) {
                        variant["klines"] = json!({*interval:rows});
                        intervals.insert((*interval).to_owned(), Arc::from(variant.to_string()));
                    }
                }
            }
            let mut quotes = self.quotes.lock().unwrap();
            // Only known market symbols enter this bounded latest-value map.
            if symbols::all().contains(&symbol.as_ref()) {
                quotes.insert(
                    symbol.to_string(),
                    QuotePacket {
                        sequence: self.quote_seq.fetch_add(1, Ordering::Relaxed),
                        full: text.clone(),
                        intervals,
                    },
                );
            }
        } else if message["type"] != "lb" {
            let _ = self.controls.send(Event::Message(text.clone(), None));
        }
        let _ = self.events.send(Event::Message(text, symbol));
    }
    pub(crate) fn control_events(&self) -> broadcast::Receiver<Event> {
        self.controls.subscribe()
    }
    pub(crate) fn latest_quote(
        &self,
        symbol: &str,
        interval: Option<&str>,
    ) -> Option<(u64, Arc<str>)> {
        self.quotes.lock().unwrap().get(symbol).map(|q| {
            (
                q.sequence,
                interval
                    .and_then(|i| q.intervals.get(i))
                    .unwrap_or(&q.full)
                    .clone(),
            )
        })
    }
    pub(crate) fn leaderboard_packet(&self) -> Option<(u64, Arc<str>)> {
        self.leaderboard
            .current()
            .map(|s| (s.generation, s.packet.clone()))
    }
    pub(crate) fn broadcast_leaderboard(&self) {
        if let Some((_, text)) = self.leaderboard_packet() {
            let _ = self.events.send(Event::Message(text, None));
        }
    }
    pub fn connected(&self, name: &str) -> Presence {
        let id = self.connection_seq.fetch_add(1, Ordering::Relaxed);
        self.presence.lock().unwrap().insert(id, name.into());
        Presence {
            id,
            map: self.presence.clone(),
        }
    }
    pub fn online(&self) -> Vec<String> {
        self.presence.lock().unwrap().values().cloned().collect()
    }
    pub fn deleted(&self, name: &str) {
        self.presence
            .lock()
            .unwrap()
            .retain(|_, owner| owner != name);
        self.leaderboard.remove(name);
        self.states.lock().unwrap().remove(name);
        let _ = self.events.send(Event::Deleted(name.into()));
        let _ = self.controls.send(Event::Deleted(name.into()));
    }
    pub fn admin_authorized(&self, token: &str, now: i64) -> bool {
        self.admin_sessions
            .lock()
            .unwrap()
            .get(token)
            .is_some_and(|expiry| *expiry > now)
    }
    pub fn admin_logout(&self, token: &str) {
        self.admin_sessions.lock().unwrap().remove(token);
    }
    pub async fn admin_login(&self, psk: &str, now: i64) -> Result<String, String> {
        {
            let mut attempts = self.admin_attempts.lock().unwrap();
            if attempts.len() == 10 && now - attempts[0] < 60000 {
                return Err("登录尝试过多，请一分钟后重试".into());
            }
            if attempts.len() == 10 {
                attempts.pop_front();
            }
            attempts.push_back(now);
        }
        if !(1..=256).contains(&psk.chars().count()) {
            return Err("PSK错误".into());
        }
        let bytes = tokio::fs::read(&self.admin_key)
            .await
            .map_err(|_| "管理员PSK尚未配置")?;
        let config: Value = serde_json::from_slice(&bytes).map_err(|_| "管理员PSK配置无效")?;
        let salt = hex::decode(config["salt"].as_str().ok_or("管理员PSK配置无效")?)
            .map_err(|_| "管理员PSK配置无效")?;
        let expected = hex::decode(config["hash"].as_str().ok_or("管理员PSK配置无效")?)
            .map_err(|_| "管理员PSK配置无效")?;
        let psk = psk.to_owned();
        let matched = tokio::task::spawn_blocking(move || {
            let mut hash = [0u8; 32];
            pbkdf2::pbkdf2_hmac::<sha2::Sha256>(psk.as_bytes(), &salt, 200000, &mut hash);
            bool::from(hash.as_slice().ct_eq(&expected))
        })
        .await
        .map_err(|_| "管理员验证失败")?;
        if !matched {
            return Err("PSK错误".into());
        }
        let token = storage::token(32);
        let mut sessions = self.admin_sessions.lock().unwrap();
        sessions.retain(|_, expiry| *expiry > now);
        sessions.insert(token.clone(), now + 3600000);
        Ok(token)
    }
    pub async fn state(&self, name: &str) -> Result<Value, String> {
        Ok(self.state_packet(name).await?.value.clone())
    }
    pub(crate) async fn state_packet(&self, name: &str) -> Result<Arc<StatePacket>, String> {
        let slot = {
            let mut states = self.states.lock().unwrap();
            if !states.contains_key(name) && states.len() >= 1024 {
                // One version per account, bounded even under one-shot HTTP readers.
                if let Some(oldest) = states.keys().next().cloned() {
                    states.remove(&oldest);
                }
            }
            states.entry(name.to_owned()).or_default().clone()
        };
        let mut cache = slot.lock().await;
        let board = self.leaderboard.current();
        let revision = (
            self.store.revision(),
            self.market.revision(),
            board.as_ref().map_or(0, |s| s.generation),
        );
        if let Some(hit) = &*cache
            && hit.revision == revision
            && Instant::now() < hit.expires
        {
            return Ok(hit.packet.clone());
        }
        let mut value = self.compute_state(name).await?;
        value["leaderboard_me"] = board.map(|s| s.me(name)).unwrap_or(Value::Null);
        let mut key = value.clone();
        if let Some(object) = key.as_object_mut() {
            for field in [
                "price",
                "bid",
                "ask",
                "quote_at",
                "quote_valid",
                "nstar_price",
            ] {
                object.remove(field);
            }
        }
        // The cache expires at the quote's freshness boundary, even without a market revision.
        let ttl = if value["quote_valid"] == true {
            value["quote_at"]
                .as_i64()
                .map(|at| (at + 30001 - storage::now_ms()).clamp(0, 500) as u64)
                .unwrap_or(500)
        } else {
            500
        };
        let packet = Arc::new(StatePacket {
            text: value.to_string().into(),
            key: key.to_string().into(),
            value,
        });
        *cache = Some(StateCache {
            revision,
            expires: Instant::now() + Duration::from_millis(ttl),
            packet: packet.clone(),
        });
        Ok(packet)
    }
    async fn compute_state(&self, name: &str) -> Result<Value, String> {
        let user = self.store.user(name).await.map_err(|_| "账户读取失败")?;
        let prices = self
            .market
            .prices(std::iter::once(user.symbol.as_str()).chain(user.account.symbols()))
            .await?;
        let quote = self.market.snapshot().await[&user.symbol];
        let bid = trading::execution_price(&prices, &user.symbol, false)?;
        let ask = trading::execution_price(&prices, &user.symbol, true)?;
        let quote_valid = quote.open
            && (self.market.is_offline()
                || (quote.fresh(storage::now_ms())
                    && (!symbols::FX.contains(&user.symbol.as_str()) || quote.bid_ask.is_some())));
        let acc = &user.account;
        let used = acc.used_margin(&prices)?;
        let equity = acc.equity(&prices)?;
        let marks = acc.marks(&prices)?;
        // The liquidation price depends on the symbol, not the position: solve once each.
        let mut liq = std::collections::BTreeMap::new();
        for p in &acc.positions {
            if !liq.contains_key(p.symbol.as_str()) {
                liq.insert(
                    p.symbol.as_str(),
                    // Liquidation is decided at mark, so the shown level is a mark price.
                    acc.liquidation_price(&p.symbol, &prices.marked())?,
                );
            }
        }
        let mut positions = Vec::new();
        let margins = acc.position_margins(&prices)?;
        for ((p, (now, pnl)), margin) in acc.positions.iter().zip(marks).zip(margins) {
            let units = p.lots * trading::contract(&p.symbol);
            let notional = units * p.entry * trading::usd_rate(&prices, &p.symbol)?;
            // Tiered stacks show their effective leverage, e.g. 15x for a full NSTAR book.
            let leverage = if trading::leverage_tiers(&p.symbol).is_empty() || margin <= 0.0 {
                trading::leverage_for(&p.symbol, acc.leverage) as f64
            } else {
                (notional / margin * 10.0).round() / 10.0
            };
            // Whole leverages stay integers on the wire, as before tiers existed.
            let leverage = if leverage.fract() == 0.0 {
                json!(leverage as u64)
            } else {
                json!(leverage)
            };
            positions.push(json!({"id":p.id,"side":p.side,"lots":p.lots,"sym":p.symbol,
                "entry":p.entry,"now":now,"pnl":pnl,"margin":margin,
                "units":units,
                "leverage":leverage,
                "roe":if margin>0.0 {pnl/margin} else {0.0},
                "tp":p.tp,"sl":p.sl,
                "liq":liq[p.symbol.as_str()]}));
        }
        let mut spot = Vec::new();
        for (symbol, h) in &acc.spot {
            let mid = trading::market_price(&prices, symbol)?;
            let value = trading::book_fill(&prices, symbol, mid, -h.units)?.0
                * h.units
                * (1.0 - trading::SPOT_FEE);
            spot.push(
                json!({"sym":symbol,"units":h.units,"cost":h.cost,"price":mid,
                "value":value,"pnl":value-h.cost*h.units}),
            );
        }
        // Shown in the asset panel even with no NSTAR held; absent offline.
        let nstar_prices = if prices.contains_key(symbols::NSTAR) {
            Some(prices.clone())
        } else {
            self.market.prices([symbols::NSTAR]).await.ok()
        };
        let nstar = nstar_prices
            .as_ref()
            .and_then(|q| q.get(symbols::NSTAR).copied());
        // Slider ranges: the largest orders the real checks would accept right now.
        let max_lots = json!({
            "buy": acc.max_open_lots("long", &user.symbol, &prices),
            "sell": acc.max_open_lots("short", &user.symbol, &prices),
        });
        let spot_max_buy = nstar_prices
            .as_ref()
            .map(|q| acc.max_spot_buy(symbols::NSTAR, q));
        let tiers: Vec<_> = trading::leverage_tiers(&user.symbol)
            .iter()
            .map(|(up_to, lev)| json!([up_to, (*lev).min(acc.leverage)]))
            .collect();
        let limits = json!({
            "max_lots": max_lots, "spot_max_buy": spot_max_buy,
            "order_max_lots": trading::max_order_lots(&user.symbol),
            "tiers": tiers, "gross_units": acc.gross_units(&user.symbol),
            "max_leverage": trading::MAX_LEVERAGE,
        });
        Ok(
            json!({"ok":true,"type":"state","name":name,"symbol":user.symbol,"price":prices[&user.symbol],"bid":bid,"ask":ask,"quote_valid":quote_valid,"quote_at":if self.market.is_offline(){None}else{Some(quote.at)},"balance":acc.balance,"loan":acc.loan,"debt":acc.debt(),"debt_since":acc.debt_since,"debt_limit":trading::DEBT_LIMIT,"equity":equity,"used":used,"free":equity-used,"margin_level":if used==0.0 {None} else {Some(equity/used)},"leverage":acc.leverage,"positions":positions,"spot":spot,"nstar_price":nstar,"spot_fee":trading::SPOT_FEE,"contract":trading::contract(&user.symbol),"symbol_leverage":trading::leverage_for(&user.symbol,acc.leverage),"funding_at":acc.funding_at,"stopout_level":trading::STOPOUT,"currency":"USD","last_trade":user.last_trade,"limits":limits,"mark":prices.mark.get(&user.symbol),"orders":acc.orders}),
        )
    }
    /// Default public Top50. HTTP reads do not sample, refresh a warm cache, or write SQL.
    pub async fn leaderboard(&self) -> Result<Value, String> {
        self.leaderboard_page(None, 0, 50).await
    }
    pub async fn leaderboard_page(
        &self,
        name: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<Value, String> {
        Ok(self
            .leaderboard
            .get(self, false)
            .await?
            .page(name, offset, limit))
    }
    /// Background-only producer. Five-minute history is intentionally reset on restart.
    pub async fn refresh_leaderboard(&self) -> Result<Value, String> {
        Ok(self.leaderboard.get(self, true).await?.page(None, 0, 50))
    }
    /// Settle due hourly funding on every account holding a perpetual book position. Each
    /// account is re-read under the store lock, so a settlement is never applied twice.
    pub async fn settle_funding(&self) -> Result<(), String> {
        let rates = self.market.funding_rates().await;
        let now = storage::now_ms();
        for user in self.store.users().await.map_err(|_| "账户读取失败")? {
            let acc = &user.account;
            if !acc.positions.iter().any(|p| rates.contains_key(&p.symbol)) {
                continue;
            }
            let Ok(prices) = self.market.prices(acc.symbols()).await else {
                continue;
            };
            let result = self
                .store
                .transaction(&user.name, |a, _, _| {
                    let mut trades = a.settle_funding(&rates, &prices, now)?;
                    if !trades.is_empty() {
                        trades.extend(a.check_stopout_at(&prices.marked(), &prices, now)?);
                    }
                    Ok(trades)
                })
                .await;
            if let Err(storage::Error::Database(_)) = result {
                return Err("资金费保存失败".into());
            }
        }
        Ok(())
    }
    pub async fn active_symbols(&self) -> Result<Vec<String>, String> {
        let mut out = std::collections::BTreeSet::new();
        for u in self.store.users().await.map_err(|_| "账户读取失败")? {
            out.insert(u.symbol);
            out.extend(
                u.account
                    .positions
                    .into_iter()
                    .map(|p| p.symbol)
                    .filter(|s| !s.is_empty()),
            );
        }
        Ok(out
            .into_iter()
            .filter(|s| !self.store.is_nstar_only() || s == symbols::NSTAR)
            .collect())
    }
    pub async fn risk_symbols(&self) -> Result<Vec<String>, String> {
        // NSTAR ticks already settle risk; external conversion/position refresh is legacy-only.
        if self.store.is_nstar_only() {
            return Ok(Vec::new());
        }
        let users = self.store.users().await.map_err(|_| "账户读取失败")?;
        let needed = trading::price_symbols(
            users
                .iter()
                .flat_map(|u| u.account.positions.iter().map(|p| p.symbol.as_str()))
                .filter(|s| !s.is_empty()),
        )?;
        Ok(needed
            .into_iter()
            .filter(|s| s != symbols::SIM && s != symbols::NSTAR)
            .collect())
    }
}
