use crate::{
    market::Market,
    storage::{self, Store},
    symbols, trading,
};
use serde_json::{Value, json};
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

#[derive(Clone, Debug)]
pub enum Event {
    Message(Value),
    Deleted(String),
    Shutdown,
}

pub struct App {
    pub store: Arc<Store>,
    pub market: Arc<Market>,
    pub events: broadcast::Sender<Event>,
    pub admin_key: PathBuf,
    admin_sessions: Mutex<BTreeMap<String, i64>>,
    admin_attempts: Mutex<VecDeque<i64>>,
    presence: Arc<Mutex<BTreeMap<u64, String>>>,
    connection_seq: AtomicU64,
    pub shutting_down: AtomicBool,
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
        Self {
            store,
            market,
            events,
            admin_key,
            admin_sessions: Mutex::new(BTreeMap::new()),
            admin_attempts: Mutex::new(VecDeque::new()),
            presence: Arc::new(Mutex::new(BTreeMap::new())),
            connection_seq: AtomicU64::new(1),
            shutting_down: AtomicBool::new(false),
        }
    }
    pub fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        let _ = self.events.send(Event::Shutdown);
    }
    pub fn broadcast(&self, message: Value) {
        let _ = self.events.send(Event::Message(message));
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
        let _ = self.events.send(Event::Deleted(name.into()));
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
        let user = self.store.user(name).await.map_err(|_| "账户读取失败")?;
        let prices = self
            .market
            .prices(
                std::iter::once(user.symbol.as_str())
                    .chain(user.account.positions.iter().map(|p| p.symbol.as_str())),
            )
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
        let positions=acc.positions.iter().zip(marks).map(|(p,(now,pnl))|Ok(json!({"id":p.id,"side":p.side,"lots":p.lots,"sym":p.symbol,"entry":p.entry,"now":now,"pnl":pnl,"margin":p.margin(&prices,acc.leverage)?}))).collect::<Result<Vec<_>,String>>()?;
        Ok(
            json!({"ok":true,"type":"state","name":name,"symbol":user.symbol,"price":prices[&user.symbol],"bid":bid,"ask":ask,"quote_valid":quote_valid,"quote_at":if self.market.is_offline(){None}else{Some(quote.at)},"balance":acc.balance,"loan":acc.loan,"debt":acc.debt(),"debt_since":acc.debt_since,"debt_limit":trading::DEBT_LIMIT,"equity":equity,"used":used,"free":equity-used,"margin_level":if used==0.0 {None} else {Some(equity/used)},"leverage":acc.leverage,"positions":positions,"stopout_level":trading::STOPOUT,"currency":"USD","last_trade":user.last_trade}),
        )
    }
    pub async fn leaderboard(&self) -> Result<Value, String> {
        let totals = self
            .store
            .deposit_totals()
            .await
            .map_err(|_| "充值记录读取失败")?;
        let online = self.online();
        let mut rows = Vec::new();
        for user in self.store.users().await.map_err(|_| "账户读取失败")? {
            let Ok(prices) = self
                .market
                .prices(user.account.positions.iter().map(|p| p.symbol.as_str()))
                .await
            else {
                continue;
            };
            let Ok(eq) = user.account.equity(&prices) else {
                continue;
            };
            let deposited = totals.get(&user.name).copied().unwrap_or(0.0);
            rows.push(json!({"name":user.name,"equity":(eq*100.0).round()/100.0,"ret":((eq/(10000.0+deposited)-1.0)*1e4).round()/1e4,"pos":user.account.positions.len(),"sym":user.symbol,"online":online.contains(&user.name)}));
        }
        self.store
            .five_minute_returns(&mut rows, &totals, storage::now_ms() / 1000)
            .await
            .map_err(|_| "收益记录保存失败")?;
        rows.sort_by(|a, b| {
            b["ret"]
                .as_f64()
                .unwrap()
                .total_cmp(&a["ret"].as_f64().unwrap())
        });
        Ok(json!({"ok":true,"rows":rows}))
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
        Ok(out.into_iter().collect())
    }
    pub async fn risk_symbols(&self) -> Result<Vec<String>, String> {
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
