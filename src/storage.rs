//! SQLite ownership and complete business transactions; callers never assemble SQL.
#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
use crate::{
    limits::Quota,
    symbols,
    trading::{self, Account, Trade},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{
    Row, SqliteConnection, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("database operation failed")]
    Database(#[from] sqlx::Error),
    #[error("stored account data is invalid")]
    Json(#[from] serde_json::Error),
}
impl From<String> for Error {
    fn from(e: String) -> Self {
        Self::Invalid(e)
    }
}
pub type Result<T> = std::result::Result<T, Error>;
fn invalid(s: &str) -> Error {
    Error::Invalid(s.into())
}
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
pub fn token(bytes: usize) -> String {
    hex::encode((0..bytes).map(|_| rand::random::<u8>()).collect::<Vec<_>>())
}
fn password_hash(salt: &str, password: &str) -> String {
    hex::encode(Sha256::digest(format!("{salt}{password}")))
}

#[derive(Debug, Clone)]
pub struct User {
    pub name: String,
    pub symbol: String,
    pub account: Account,
    pub last_trade: Option<Value>,
}

pub struct Store {
    pool: SqlitePool,
    // ponytail: one process-wide mutation lock; per-account locks if throughput requires it.
    gate: Mutex<()>,
    next_id: AtomicU64,
    /// NSTAR units (player, forced) settled but not yet applied to the book.
    nstar_flow: std::sync::Mutex<(f64, f64)>,
    login_aliases: Mutex<BTreeMap<String, String>>,
    /// Committed copy of every account, so hot paths (risk sweeps, leaderboard, live state)
    /// never re-read and re-parse SQLite. Updated only after a successful commit.
    cache: std::sync::RwLock<Cache>,
}

#[derive(Default)]
struct Cache {
    users: BTreeMap<String, User>,
    tokens: std::collections::HashMap<String, String>,
}

impl Store {
    pub async fn open(path: &Path) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .busy_timeout(Duration::from_secs(10))
            // WAL + NORMAL: a commit is an append, not a full fsync; a crash can lose at most
            // the last un-checkpointed commits, never corrupt the database.
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        let schema = include_str!("schema.sql");
        sqlx::raw_sql(schema).execute(&pool).await?;
        let fields = sqlx::query("PRAGMA table_info(trades)")
            .fetch_all(&pool)
            .await?;
        if !fields
            .iter()
            .any(|r| r.get::<String, _>("name") == "currency")
        {
            sqlx::query("ALTER TABLE trades ADD COLUMN currency TEXT DEFAULT 'LEGACY'")
                .execute(&pool)
                .await?;
        }
        let sim_fields = sqlx::query("PRAGMA table_info(sim_bars)")
            .fetch_all(&pool)
            .await?;
        if !sim_fields
            .iter()
            .any(|r| r.get::<String, _>("name") == "symbol")
        {
            let mut tx = pool.begin().await?;
            sqlx::query("ALTER TABLE sim_bars RENAME TO sim_bars_old")
                .execute(&mut *tx)
                .await?;
            sqlx::raw_sql(schema).execute(&mut *tx).await?;
            sqlx::query(
                "INSERT INTO sim_bars(symbol,sec,t,o,h,l,c) SELECT 'FXSIM',sec,t,o,h,l,c FROM sim_bars_old",
            )
            .execute(&mut *tx)
            .await?;
            sqlx::query("DROP TABLE sim_bars_old")
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }
        let mut next_id = 1;
        let mut cache = Cache::default();
        let mut conn = pool.acquire().await?;
        for row in sqlx::query("SELECT name,token FROM users ORDER BY name")
            .fetch_all(&mut *conn)
            .await?
        {
            let name: String = row.get("name");
            let user = load_user(&mut conn, &name).await?;
            let token: String = row.get("token");
            if !token.is_empty() {
                cache.tokens.insert(token, name.clone());
            }
            let a = user.account.clone();
            cache.users.insert(name, user);
            // Python restores Position._seq to at least persisted next_id, then increments.
            next_id = next_id.max(a.next_id.saturating_add(1));
            for p in a.positions {
                next_id = next_id.max(p.id.saturating_add(1));
            }
        }
        drop(conn);
        Ok(Self {
            pool,
            gate: Mutex::new(()),
            next_id: AtomicU64::new(next_id),
            nstar_flow: std::sync::Mutex::new((0.0, 0.0)),
            login_aliases: Mutex::new(BTreeMap::new()),
            cache: std::sync::RwLock::new(cache),
        })
    }
    pub async fn close(&self) {
        self.pool.close().await;
    }
    pub async fn user(&self, name: &str) -> Result<User> {
        self.cache
            .read()
            .unwrap()
            .users
            .get(name)
            .cloned()
            .ok_or_else(|| invalid("账号不存在"))
    }
    pub async fn users(&self) -> Result<Vec<User>> {
        Ok(self.cache.read().unwrap().users.values().cloned().collect())
    }
    /// The pair a user is viewing, from the cache; None for an unknown user.
    pub fn selected(&self, name: &str) -> Option<String> {
        self.cache
            .read()
            .unwrap()
            .users
            .get(name)
            .map(|u| u.symbol.clone())
    }
    /// Re-read one account from SQLite into the cache after a committed write.
    async fn refresh(&self, name: &str) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let user = load_user(&mut conn, name).await?;
        self.cache.write().unwrap().users.insert(name.into(), user);
        Ok(())
    }
    fn forget(&self, name: &str) {
        let mut cache = self.cache.write().unwrap();
        cache.users.remove(name);
        cache.tokens.retain(|_, owner| owner != name);
    }
    pub async fn authenticate(&self, tok: &str) -> Result<Option<String>> {
        if tok.is_empty() {
            return Ok(None);
        }
        if let Some(name) = self.cache.read().unwrap().tokens.get(tok) {
            return Ok(Some(name.clone()));
        }
        Ok(self.login_aliases.lock().await.get(tok).cloned())
    }
    /// Trusted in-process registration (tests, tools); bypasses client quotas.
    pub async fn register(&self, name: &str, pwd: &str) -> Result<String> {
        self.register_from(name, pwd, "local", &Quota::NONE, now_ms())
            .await
    }
    /// Registration under per-client and global quotas; callers validate credentials
    /// first with `validate_credentials`. Quota rows outlive account deletion so delete-and-retry gains nothing.
    pub async fn register_from(
        &self,
        name: &str,
        pwd: &str,
        client: &str,
        quota: &Quota,
        now: i64,
    ) -> Result<String> {
        let _guard = self.gate.lock().await;
        let since = |ms: i64| now - ms;
        let count = |sql: &'static str, key: Option<&str>, from: i64| {
            let q = sqlx::query_scalar::<_, i64>(sql);
            let q = match key {
                Some(k) => q.bind(k.to_owned()),
                None => q,
            };
            q.bind(from)
        };
        const HOUR: i64 = 3_600_000;
        let by_client = "SELECT COUNT(*) FROM registrations WHERE client=? AND time>?";
        if count(by_client, Some(client), since(HOUR))
            .fetch_one(&self.pool)
            .await?
            >= quota.per_client_hour
            || count(by_client, Some(client), since(24 * HOUR))
                .fetch_one(&self.pool)
                .await?
                >= quota.per_client_day
        {
            return Err(invalid("该网络注册过于频繁，请稍后再试"));
        }
        if count(
            "SELECT COUNT(*) FROM registrations WHERE time>?",
            None,
            since(HOUR),
        )
        .fetch_one(&self.pool)
        .await?
            >= quota.global_hour
        {
            return Err(invalid("当前注册人数过多，请稍后再试"));
        }
        if sqlx::query("SELECT name FROM users WHERE name=? COLLATE NOCASE")
            .bind(name)
            .fetch_optional(&self.pool)
            .await?
            .is_some()
        {
            return Err(invalid("用户已存在"));
        }
        let salt = token(4);
        let tok = token(16);
        let account = Account {
            next_id: self.next_id.load(Ordering::SeqCst),
            ..Account::default()
        };
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO users(name,salt,hash,token,symbol,account,created) VALUES(?,?,?,?,'USDJPY',?,?)")
            .bind(name).bind(&salt).bind(password_hash(&salt,pwd)).bind(&tok).bind(serde_json::to_string(&account)?).bind(now/1000).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO registrations(client,time,name) VALUES(?,?,?)")
            .bind(client)
            .bind(now)
            .bind(name)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM registrations WHERE time<?")
            .bind(since(7 * 24 * HOUR))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.refresh(name).await?;
        self.cache
            .write()
            .unwrap()
            .tokens
            .insert(tok.clone(), name.into());
        Ok(tok)
    }
    pub async fn login(&self, name: &str, pwd: &str) -> Result<String> {
        let _guard = self.gate.lock().await;
        let row = sqlx::query("SELECT salt,hash,token FROM users WHERE name=?")
            .bind(name)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| invalid("用户名或密码错"))?;
        let actual = password_hash(row.get("salt"), pwd);
        let expected: String = row.get("hash");
        if !bool::from(actual.as_bytes().ct_eq(expected.as_bytes())) {
            return Err(invalid("用户名或密码错"));
        }
        // Python rotates the durable token but keeps older logins in memory until restart.
        let old: String = row.get("token");
        let tok = token(16);
        sqlx::query("UPDATE users SET token=? WHERE name=?")
            .bind(&tok)
            .bind(name)
            .execute(&self.pool)
            .await?;
        {
            let mut cache = self.cache.write().unwrap();
            cache.tokens.remove(&old);
            cache.tokens.insert(tok.clone(), name.into());
        }
        if !old.is_empty() {
            self.login_aliases.lock().await.insert(old, name.into());
        }
        Ok(tok)
    }
    pub async fn transaction<F>(&self, name: &str, operation: F) -> Result<()>
    where
        F: FnOnce(&mut Account, &mut String, u64) -> std::result::Result<Vec<Trade>, String> + Send,
    {
        let _guard = self.gate.lock().await;
        let mut tx = self.pool.begin().await?;
        let mut user = load_user(&mut tx, name).await?;
        let id = self.next_id.load(Ordering::SeqCst);
        let held = nstar_units(&user.account);
        let trades = operation(&mut user.account, &mut user.symbol, id)?;
        let opened = trades.iter().any(|t| t.kind == "open");
        user.account.validate()?;
        // Re-check enabled status inside the same transaction for all new positions/selection changes.
        for t in &trades {
            if t.kind == "open" {
                require_tradable(&mut tx, &t.symbol).await?;
            }
        }
        let old_symbol: String = sqlx::query_scalar("SELECT symbol FROM users WHERE name=?")
            .bind(name)
            .fetch_one(&mut *tx)
            .await?;
        if old_symbol != user.symbol {
            require_tradable(&mut tx, &user.symbol).await?;
        }
        persist(&mut tx, &user).await?;
        // Net change in held NSTAR is exactly the flow, including stopouts and resets that
        // abandon positions. Only voluntary orders count as player flow.
        let total = nstar_units(&user.account) - held;
        let player: f64 = trades
            .iter()
            .filter(|t| t.symbol == symbols::NSTAR)
            .map(|t| {
                let units = t.lots * trading::contract(symbols::NSTAR);
                match (t.kind.as_str(), t.side.as_str()) {
                    ("open", "long") | ("close" | "tp" | "sl", "short") | ("spot", "buy") => units,
                    ("open", "short") | ("close" | "tp" | "sl", "long") | ("spot", "sell") => {
                        -units
                    }
                    _ => 0.0,
                }
            })
            .sum();
        for trade in &trades {
            insert_trade(&mut tx, name, trade).await?;
        }
        if !trades.is_empty() {
            user.last_trade = history(&mut tx, name, 1).await?.into_iter().next();
        }
        tx.commit().await?;
        self.cache
            .write()
            .unwrap()
            .users
            .insert(name.into(), user.clone());
        if total != 0.0 {
            let mut flow = self.nstar_flow.lock().unwrap();
            flow.0 += player;
            flow.1 += total - player;
        }
        if opened {
            self.next_id
                .fetch_max(user.account.next_id, Ordering::SeqCst);
        }
        Ok(())
    }
    pub fn take_nstar_flow(&self) -> (f64, f64) {
        std::mem::take(&mut *self.nstar_flow.lock().unwrap())
    }
    pub async fn select_symbol(&self, name: &str, symbol: &str) -> Result<()> {
        let _guard = self.gate.lock().await;
        let mut tx = self.pool.begin().await?;
        require_tradable(&mut tx, symbol).await?;
        let mut user = load_user(&mut tx, name).await?;
        user.symbol = symbol.into();
        persist(&mut tx, &user).await?;
        tx.commit().await?;
        self.cache.write().unwrap().users.insert(name.into(), user);
        Ok(())
    }
    pub async fn require_tradable(&self, symbol: &str) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        require_tradable(&mut conn, symbol).await
    }
    pub async fn history(&self, name: &str) -> Result<Vec<Value>> {
        let mut conn = self.pool.acquire().await?;
        history(&mut conn, name, 100).await
    }
    pub async fn chat(&self) -> Result<Vec<Value>> {
        let rows = sqlx::query("SELECT id,name,time,text FROM chat ORDER BY id DESC LIMIT 100")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().rev().map(|r|json!({"id":r.get::<i64,_>("id"),"name":r.get::<String,_>("name"),"time":r.get::<i64,_>("time"),"text":r.get::<String,_>("text")})).collect())
    }
    pub async fn post_chat(&self, name: &str, text: &str, now: i64) -> Result<Value> {
        let text = text.trim();
        if !(1..=300).contains(&text.chars().count()) {
            return Err(invalid("聊天内容需为1至300字"));
        }
        let _guard = self.gate.lock().await;
        let mut tx = self.pool.begin().await?;
        load_user(&mut tx, name).await?;
        let previous: Option<i64> =
            sqlx::query_scalar("SELECT time FROM chat WHERE name=? ORDER BY id DESC LIMIT 1")
                .bind(name)
                .fetch_optional(&mut *tx)
                .await?;
        if previous.is_some_and(|t| now - t < 2000) {
            return Err(invalid("发送太快，请等2秒"));
        }
        let id = sqlx::query("INSERT INTO chat(name,time,text) VALUES(?,?,?)")
            .bind(name)
            .bind(now)
            .bind(text)
            .execute(&mut *tx)
            .await?
            .last_insert_rowid();
        sqlx::query("DELETE FROM chat WHERE id <= (SELECT id FROM chat ORDER BY id DESC LIMIT 1 OFFSET 100)").execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(json!({"id":id,"name":name,"time":now,"text":text}))
    }
    pub async fn deposit_totals(&self) -> Result<BTreeMap<String, f64>> {
        Ok(
            sqlx::query("SELECT name,SUM(amount) AS total FROM deposits GROUP BY name")
                .fetch_all(&self.pool)
                .await?
                .into_iter()
                .map(|r| (r.get("name"), r.get("total")))
                .collect(),
        )
    }
    pub async fn events(&self) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        for row in sqlx::query("SELECT id,kind,detail FROM admin_events ORDER BY id DESC LIMIT 100")
            .fetch_all(&self.pool)
            .await?
        {
            let mut detail: Value = serde_json::from_str(row.get("detail"))?;
            detail["id"] = json!(row.get::<i64, _>("id"));
            detail["kind"] = json!(row.get::<String, _>("kind"));
            out.push(detail);
        }
        Ok(out)
    }
    pub async fn deposit(
        &self,
        name: &str,
        amount: f64,
        request_id: &str,
        now: i64,
    ) -> Result<Value> {
        if !amount.is_finite()
            || amount <= 0.0
            || amount > 1e9
            || (amount * 100.0).round() / 100.0 != amount
        {
            return Err(invalid(
                "充值金额须为大于0的美元金额，最多两位小数、单次不超过10亿美元",
            ));
        }
        if !(16..=64).contains(&request_id.chars().count()) {
            return Err(invalid("充值请求编号无效"));
        }
        let _guard = self.gate.lock().await;
        let mut tx = self.pool.begin().await?;
        let mut user = load_user(&mut tx, name).await?;
        if let Some(row) =
            sqlx::query("SELECT name,amount,time,balance,actor FROM deposits WHERE request_id=?")
                .bind(request_id)
                .fetch_optional(&mut *tx)
                .await?
        {
            if row.get::<String, _>("name") != name || row.get::<f64, _>("amount") != amount {
                return Err(invalid("充值请求编号已使用"));
            }
            return Ok(
                json!({"request_id":request_id,"name":name,"amount":amount,"time":row.get::<i64,_>("time"),"balance":row.get::<f64,_>("balance"),"actor":row.get::<String,_>("actor")}),
            );
        }
        if sqlx::query("SELECT 1 FROM admin_events WHERE request_id=?")
            .bind(request_id)
            .fetch_optional(&mut *tx)
            .await?
            .is_some()
        {
            return Err(invalid("原账号已删除，旧充值请求不可重放"));
        }
        user.account.balance += amount;
        user.account.validate()?;
        let row = json!({"request_id":request_id,"name":name,"time":now,"amount":amount,"balance":user.account.balance,"actor":"PSK管理员"});
        persist(&mut tx, &user).await?;
        sqlx::query(
            "INSERT INTO deposits(request_id,name,time,amount,balance,actor) VALUES(?,?,?,?,?,?)",
        )
        .bind(request_id)
        .bind(name)
        .bind(now)
        .bind(amount)
        .bind(user.account.balance)
        .bind("PSK管理员")
        .execute(&mut *tx)
        .await?;
        event(&mut tx, "deposit", name, now, Some(request_id), &row).await?;
        tx.commit().await?;
        self.cache.write().unwrap().users.insert(name.into(), user);
        Ok(row)
    }
    pub async fn delete(&self, name: &str, confirmation: &str, now: i64) -> Result<Value> {
        if name != confirmation {
            return Err(invalid("请准确输入要删除的账号名"));
        }
        let _guard = self.gate.lock().await;
        let mut tx = self.pool.begin().await?;
        let user = load_user(&mut tx, name).await?;
        let row = json!({"name":name,"time":now,"actor":"PSK管理员","balance":user.account.balance,"positions":user.account.positions.len()});
        event(&mut tx, "delete", name, now, None, &row).await?;
        for table in ["users", "trades", "chat", "deposits", "equity_samples"] {
            sqlx::query(&format!("DELETE FROM {table} WHERE name=?"))
                .bind(name)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        self.forget(name);
        // A deleted account's NSTAR book is unwound into the market like a stopout.
        self.nstar_flow.lock().unwrap().1 -= nstar_units(&user.account);
        self.login_aliases
            .lock()
            .await
            .retain(|_, owner| owner != name);
        Ok(row)
    }
    pub async fn settings(&self) -> Result<Value> {
        let mut conn = self.pool.acquire().await?;
        settings(&mut conn).await
    }
    pub async fn set_symbol(&self, symbol: &str, enabled: bool, now: i64) -> Result<Value> {
        if symbols::provider(symbol).is_none() {
            return Err(invalid("币种对或启用状态无效"));
        }
        let _guard = self.gate.lock().await;
        let mut tx = self.pool.begin().await?;
        let before = settings(&mut tx).await?;
        let current = before["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["symbol"] == symbol)
            .unwrap();
        if current["enabled"] == enabled {
            return Ok(before);
        }
        let version = now.max(before["version"].as_i64().unwrap() + 1);
        let row = json!({"name":symbol,"time":version,"actor":"PSK管理员","enabled":enabled,"provider":symbols::provider(symbol)});
        sqlx::query("INSERT OR REPLACE INTO symbol_settings VALUES(?,?,?)")
            .bind(symbol)
            .bind(enabled)
            .bind(version)
            .execute(&mut *tx)
            .await?;
        event(&mut tx, "symbol", symbol, version, None, &row).await?;
        let after = settings(&mut tx).await?;
        tx.commit().await?;
        Ok(after)
    }
    pub async fn five_minute_returns(
        &self,
        rows: &mut [Value],
        totals: &BTreeMap<String, f64>,
        now: i64,
    ) -> Result<()> {
        let _guard = self.gate.lock().await;
        let mut tx = self.pool.begin().await?;
        for row in rows {
            let name = row["name"].as_str().unwrap().to_owned();
            let eq = row["equity"].as_f64().unwrap();
            let deposited = totals.get(&name).copied().unwrap_or(0.0);
            let baseline=sqlx::query("SELECT equity,deposited FROM equity_samples WHERE name=? AND time BETWEEN ? AND ? ORDER BY time DESC LIMIT 1").bind(&name).bind(now-315).bind(now-300).fetch_optional(&mut *tx).await?;
            row["pnl_5m"] = Value::Null;
            row["ret_5m"] = Value::Null;
            if let Some(b) = baseline {
                let equity: f64 = b.get("equity");
                let prior: f64 = b.get("deposited");
                let pnl = eq - equity - (deposited - prior);
                row["pnl_5m"] = json!((pnl * 100.0).round() / 100.0);
                if equity > 0.0 {
                    row["ret_5m"] = json!(pnl / equity);
                }
            }
            // A leaderboard snapshot racing deletion must not resurrect a deleted user's samples.
            sqlx::query("INSERT OR IGNORE INTO equity_samples SELECT ?,?,?,? WHERE EXISTS(SELECT 1 FROM users WHERE name=?)").bind(&name).bind(now/5*5).bind(eq).bind(deposited).bind(&name).execute(&mut *tx).await?;
        }
        sqlx::query("DELETE FROM equity_samples WHERE time<?")
            .bind(now - 900)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn load_bars(&self, symbol: &str, sec: i64) -> Result<Vec<[f64; 5]>> {
        let rows = sqlx::query(
            "SELECT t,o,h,l,c FROM sim_bars WHERE symbol=? AND sec=? ORDER BY t DESC LIMIT 1000",
        )
        .bind(symbol)
        .bind(sec)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .rev()
            .map(|r| {
                [
                    r.get::<i64, _>("t") as f64,
                    r.get("o"),
                    r.get("h"),
                    r.get("l"),
                    r.get("c"),
                ]
            })
            .collect())
    }
    pub async fn save_bars(&self, symbol: &str, bars: &[(i64, [f64; 5], i64)]) -> Result<()> {
        let _guard = self.gate.lock().await;
        let mut tx = self.pool.begin().await?;
        for (sec, b, oldest) in bars {
            sqlx::query(
                "INSERT OR REPLACE INTO sim_bars(symbol,sec,t,o,h,l,c) VALUES(?,?,?,?,?,?,?)",
            )
            .bind(symbol)
            .bind(sec)
            .bind(b[0] as i64)
            .bind(b[1])
            .bind(b[2])
            .bind(b[3])
            .bind(b[4])
            .execute(&mut *tx)
            .await?;
            sqlx::query("DELETE FROM sim_bars WHERE symbol=? AND sec=? AND t<?")
                .bind(symbol)
                .bind(sec)
                .bind(oldest)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

/// Rules for new accounts. Existing accounts keep logging in with whatever they have.
pub fn validate_credentials(name: &str, pwd: &str) -> Result<()> {
    validate_name(name)?;
    validate_password(name, pwd)
}
/// Names: 2-16 characters of letters (any script), digits, `_` or `-`, not all digits.
fn validate_name(name: &str) -> Result<()> {
    let n = name.chars().count();
    if !(2..=16).contains(&n)
        || !name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        || name.chars().all(|c| c.is_ascii_digit())
    {
        return Err(invalid(
            "用户名需2-16字，仅限文字、数字、_或-，且不能全为数字",
        ));
    }
    Ok(())
}
fn validate_password(name: &str, pwd: &str) -> Result<()> {
    let n = pwd.chars().count();
    if !(8..=128).contains(&n) {
        return Err(invalid("密码需8-128位"));
    }
    let letters = pwd.chars().any(|c| c.is_alphabetic());
    let digits = pwd.chars().any(|c| c.is_ascii_digit());
    let distinct = pwd.chars().collect::<std::collections::BTreeSet<_>>().len();
    if !letters || !digits || distinct < 4 || pwd.eq_ignore_ascii_case(name) {
        return Err(invalid("密码需同时包含字母和数字，且不能与用户名相同"));
    }
    Ok(())
}
/// Signed NSTAR units held (long positive), perpetual positions and spot alike.
fn nstar_units(account: &Account) -> f64 {
    let perp: f64 = account
        .positions
        .iter()
        .filter(|p| p.symbol == symbols::NSTAR)
        .map(|p| {
            let units = p.lots * trading::contract(symbols::NSTAR);
            if p.side == "long" { units } else { -units }
        })
        .sum();
    perp + account.spot.get(symbols::NSTAR).map_or(0.0, |h| h.units)
}
async fn load_user(conn: &mut SqliteConnection, name: &str) -> Result<User> {
    let row = sqlx::query("SELECT symbol,account FROM users WHERE name=?")
        .bind(name)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| invalid("账号不存在"))?;
    let account: Account = serde_json::from_str(row.get("account"))?;
    account.validate()?;
    let last_trade = history(conn, name, 1).await?.into_iter().next();
    Ok(User {
        name: name.into(),
        symbol: row.get("symbol"),
        account,
        last_trade,
    })
}
async fn persist(conn: &mut SqliteConnection, user: &User) -> Result<()> {
    let changed = sqlx::query("UPDATE users SET account=?,symbol=? WHERE name=?")
        .bind(serde_json::to_string(&user.account)?)
        .bind(&user.symbol)
        .bind(&user.name)
        .execute(conn)
        .await?;
    if changed.rows_affected() != 1 {
        return Err(invalid("账户不存在"));
    }
    Ok(())
}
async fn insert_trade(conn: &mut SqliteConnection, name: &str, t: &Trade) -> Result<()> {
    sqlx::query("INSERT INTO trades(name,time,kind,position_id,symbol,side,lots,entry,price,pnl,balance,margin_level,currency) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)")
        .bind(name).bind(t.time).bind(&t.kind).bind(t.position_id as i64).bind(&t.symbol).bind(&t.side).bind(t.lots).bind(t.entry).bind(t.price).bind(t.pnl).bind(t.balance).bind(t.margin_level).bind(&t.currency).execute(conn).await?;
    Ok(())
}
async fn history(conn: &mut SqliteConnection, name: &str, limit: i64) -> Result<Vec<Value>> {
    Ok(sqlx::query("SELECT id,time,kind,position_id,symbol,side,lots,entry,price,pnl,balance,margin_level,currency FROM trades WHERE name=? ORDER BY id DESC LIMIT ?")
        .bind(name).bind(limit).fetch_all(conn).await?.into_iter().map(|r|json!({
            "id":r.get::<i64,_>("id"),"time":r.get::<i64,_>("time"),"kind":r.get::<String,_>("kind"),
            "position_id":r.get::<Option<i64>,_>("position_id"),"symbol":r.get::<Option<String>,_>("symbol"),"side":r.get::<Option<String>,_>("side"),
            "lots":r.get::<Option<f64>,_>("lots"),"entry":r.get::<Option<f64>,_>("entry"),"price":r.get::<Option<f64>,_>("price"),
            "pnl":r.get::<Option<f64>,_>("pnl"),"balance":r.get::<Option<f64>,_>("balance"),"margin_level":r.get::<Option<f64>,_>("margin_level"),"currency":r.get::<Option<String>,_>("currency")
        })).collect())
}
async fn event(
    conn: &mut SqliteConnection,
    kind: &str,
    name: &str,
    now: i64,
    request_id: Option<&str>,
    row: &Value,
) -> Result<()> {
    sqlx::query("INSERT INTO admin_events(kind,name,time,request_id,detail) VALUES(?,?,?,?,?)")
        .bind(kind)
        .bind(name)
        .bind(now)
        .bind(request_id)
        .bind(serde_json::to_string(row)?)
        .execute(conn)
        .await?;
    Ok(())
}
async fn settings(conn: &mut SqliteConnection) -> Result<Value> {
    let flags: BTreeMap<String, (bool, i64)> =
        sqlx::query("SELECT symbol,enabled,version FROM symbol_settings")
            .fetch_all(conn)
            .await?
            .into_iter()
            .map(|r| (r.get("symbol"), (r.get("enabled"), r.get("version"))))
            .collect();
    Ok(
        json!({"version":flags.values().map(|(_,v)|*v).max().unwrap_or(0),"rows":symbols::all().iter().map(|s|json!({"symbol":s,"provider":symbols::provider(s),"enabled":flags.get(*s).map(|(e,_)|*e).unwrap_or(true)})).collect::<Vec<_>>()}),
    )
}
async fn require_tradable(conn: &mut SqliteConnection, symbol: &str) -> Result<()> {
    if symbols::provider(symbol).is_none() {
        return Err(invalid("不支持的币种对"));
    }
    let enabled: Option<bool> =
        sqlx::query_scalar("SELECT enabled FROM symbol_settings WHERE symbol=?")
            .bind(symbol)
            .fetch_optional(conn)
            .await?;
    if enabled == Some(false) {
        return Err(Error::Invalid(format!(
            "{symbol} 已禁用新开仓；已有持仓仍可平仓"
        )));
    }
    Ok(())
}
