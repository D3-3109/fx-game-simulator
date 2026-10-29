//! SQLite ownership and complete business transactions; callers never assemble SQL.
#[path = "backup.rs"]
mod backup;
#[path = "storage_batch.rs"]
mod batch;
#[cfg(test)]
#[path = "nstar_migration_tests.rs"]
mod nstar_migration_tests;
#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
#[path = "storage_writer.rs"]
mod writer;
use crate::{
    limits::Quota,
    symbols,
    trading::{self, Account, Trade},
};
use batch::{Batch, exposed_to, sweep};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{
    Row, SqliteConnection, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::sync::Mutex;
use writer::{Mutation, PreparedAccount, Publication, Work, Writer};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("database operation failed")]
    Database(#[from] sqlx::Error),
    #[error("{0}")]
    Unavailable(String),
    #[error("stored account data is invalid")]
    Json(#[from] serde_json::Error),
    #[error("database backup failed: {0}")]
    Backup(#[from] std::io::Error),
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

#[derive(Debug, Default, serde::Serialize)]
pub struct NstarMigration {
    pub backup: Option<PathBuf>,
    pub accounts: usize,
    pub positions: usize,
    pub orders: usize,
    pub spot_assets: usize,
}

pub struct Store {
    pool: SqlitePool,
    nstar_only: bool,
    // The writer owns this gate through COMMIT and synchronous cache publication.
    gate: Arc<Mutex<()>>,
    next_id: Arc<AtomicU64>,
    /// NSTAR units (player, forced) settled but not yet applied to the book.
    nstar_flow: Arc<std::sync::Mutex<(f64, f64)>>,
    login_aliases: Arc<std::sync::Mutex<BTreeMap<String, String>>>,
    /// Committed copies only; hot paths never re-read and re-parse SQLite.
    cache: Arc<std::sync::RwLock<Cache>>,
    revision: Arc<AtomicU64>,
    writer: Writer,
}

#[derive(Default)]
struct Cache {
    users: BTreeMap<String, User>,
    tokens: std::collections::HashMap<String, String>,
    deposits: BTreeMap<String, f64>,
}

#[derive(Clone, Copy)]
enum AccountLoad {
    Validated,
    ForMigration,
}

impl Store {
    pub async fn open(path: &Path) -> Result<Self> {
        Self::open_mode(path, true, AccountLoad::Validated).await
    }
    /// Explicit compatibility mode for legacy fixtures; account validation stays enabled.
    /// Retains WAL/NORMAL for compatibility: unlike production WAL/FULL, acknowledged
    /// commits may be lost on power failure before the next checkpoint.
    pub async fn open_legacy(path: &Path) -> Result<Self> {
        Self::open_mode(path, false, AccountLoad::Validated).await
    }
    /// Offline CLI only: load typed legacy data before filtering and validating it.
    /// Never use this store to serve requests. The database must already exist.
    pub async fn open_for_migration(path: &Path) -> Result<Self> {
        Self::open_mode(path, true, AccountLoad::ForMigration).await
    }
    pub fn is_nstar_only(&self) -> bool {
        self.nstar_only
    }
    async fn open_mode(path: &Path, nstar_only: bool, load: AccountLoad) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(matches!(load, AccountLoad::Validated))
            .busy_timeout(Duration::from_secs(10))
            // Reliable production acknowledgement requires WAL/FULL. Only explicitly
            // selected legacy fixture mode retains the old NORMAL durability policy.
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(if nstar_only {
                sqlx::sqlite::SqliteSynchronous::Full
            } else {
                sqlx::sqlite::SqliteSynchronous::Normal
            });
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
            let user = load_user_mode(&mut conn, &name, load).await?;
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
        cache.deposits =
            sqlx::query("SELECT name,SUM(amount) AS total FROM deposits GROUP BY name")
                .fetch_all(&mut *conn)
                .await?
                .into_iter()
                .map(|row| (row.get("name"), row.get("total")))
                .collect();
        drop(conn);
        let gate = Arc::new(Mutex::new(()));
        let next_id = Arc::new(AtomicU64::new(next_id));
        let nstar_flow = Arc::new(std::sync::Mutex::new((0.0, 0.0)));
        let login_aliases = Arc::new(std::sync::Mutex::new(BTreeMap::new()));
        let cache = Arc::new(std::sync::RwLock::new(cache));
        let revision = Arc::new(AtomicU64::new(0));
        let writer = Writer::start(writer::Shared {
            pool: pool.clone(),
            nstar_only,
            gate: gate.clone(),
            cache: cache.clone(),
            next_id: next_id.clone(),
            flow: nstar_flow.clone(),
            aliases: login_aliases.clone(),
            revision: revision.clone(),
        });
        Ok(Self {
            pool,
            nstar_only,
            gate,
            next_id,
            nstar_flow,
            login_aliases,
            cache,
            revision,
            writer,
        })
    }
    pub async fn ensure_nstar_only(&self) -> Result<()> {
        if self.nstar_only
            && self
                .cache
                .read()
                .unwrap()
                .users
                .values()
                .any(has_legacy_exposure)
        {
            return Err(invalid(
                "数据库含非 NSTAR 选中币种、持仓或挂单；停服后使用 --migrate-nstar-only --migration-backup <新备份路径> 迁移",
            ));
        }
        Ok(())
    }
    /// Run only while the service is stopped. VACUUM INTO includes committed WAL content.
    pub async fn migrate_nstar_only(&self, backup: &Path) -> Result<NstarMigration> {
        let mutation = self.writer.admit(&self.gate).await?;
        let mut conn = self.pool.acquire().await?;
        let names: Vec<String> = sqlx::query_scalar("SELECT name FROM users ORDER BY name")
            .fetch_all(&mut *conn)
            .await?;
        let mut users = Vec::new();
        for name in names {
            let user = load_user_mode(&mut conn, &name, AccountLoad::ForMigration).await?;
            if has_legacy_exposure(&user) {
                users.push(user);
            } else {
                // A no-op migration must not report success for invalid retained data.
                user.account.validate()?;
            }
        }
        if users.is_empty() {
            return Ok(NstarMigration::default());
        }
        let backup_path = backup
            .to_str()
            .ok_or_else(|| invalid("备份路径必须是有效文本"))?;
        // Permissions are restricted before any secrets are written. Keep the handle
        // through VACUUM and sync the same file; Windows also denies delete/rename.
        let reserved = backup::reserve(backup)?;
        sqlx::query("VACUUM INTO ?")
            .bind(backup_path)
            .execute(&mut *conn)
            .await?;
        reserved.sync_all()?;
        drop(reserved);
        drop(conn);
        let mut report = NstarMigration {
            backup: Some(backup.to_path_buf()),
            ..NstarMigration::default()
        };
        let mut tx = self.begin().await?;
        let now = now_ms();
        let mut committed = Vec::new();
        for before in users {
            let mut user = load_user_mode(&mut tx, &before.name, AccountLoad::ForMigration).await?;
            if user.account != before.account || user.symbol != before.symbol {
                return Err(invalid(
                    "迁移期间账户发生变化；请停止所有访问该数据库的服务后重试",
                ));
            }
            let positions = user.account.positions.len();
            let orders = user.account.orders.len();
            let spot = user.account.spot.len();
            user.account
                .positions
                .retain(|p| p.symbol == symbols::NSTAR);
            user.account.orders.retain(|o| o.symbol == symbols::NSTAR);
            user.account
                .spot
                .retain(|symbol, _| symbol == symbols::NSTAR);
            let removed_positions = positions - user.account.positions.len();
            let removed_orders = orders - user.account.orders.len();
            let removed_spot = spot - user.account.spot.len();
            let detail = json!({
                "actor":"NSTAR迁移", "backup":backup, "previous_symbol":user.symbol,
                "positions_removed":removed_positions, "orders_removed":removed_orders,
                "spot_assets_removed":removed_spot, "balance":user.account.balance,
                "settled":false
            });
            user.symbol = symbols::NSTAR.into();
            user.account.validate()?;
            persist(&mut tx, &user).await?;
            if removed_positions > 0 || removed_spot > 0 {
                sqlx::query("DELETE FROM equity_samples WHERE name=?")
                    .bind(&user.name)
                    .execute(&mut *tx)
                    .await?;
            }
            event(&mut tx, "nstar_migration", &user.name, now, None, &detail).await?;
            report.accounts += 1;
            report.positions += removed_positions;
            report.orders += removed_orders;
            report.spot_assets += removed_spot;
            committed.push(user);
        }
        self.commit(
            mutation,
            tx,
            Publication {
                users: committed,
                revision: true,
                ..Publication::default()
            },
        )
        .await?;
        Ok(report)
    }
    /// Stops admission, drains accepted commits and publications, then closes SQLite.
    /// Kept infallible for existing callers; failure is explicit in stats and stderr.
    pub async fn close(&self) {
        if let Err(error) = self.writer.close().await {
            eprintln!("storage close failed: {error}");
            self.pool.close().await;
        }
    }
    pub async fn flush_pending(&self) -> Result<()> {
        let _mutation = self.writer.admit(&self.gate).await?;
        Ok(())
    }
    pub fn persistence_stats(&self) -> Value {
        self.writer.snapshot()
    }
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }
    /// Committed accounts and deposit totals. One publication updates both under the same
    /// cache write lock, so a read lock alone gives a consistent pair without waiting for
    /// the writer to finish an fsync.
    pub async fn leaderboard_snapshot(&self) -> Result<(Vec<User>, BTreeMap<String, f64>)> {
        let cache = self.cache.read().unwrap();
        Ok((
            cache.users.values().cloned().collect(),
            cache.deposits.clone(),
        ))
    }
    /// Names of committed accounts matching `filter`, without cloning any account.
    pub fn names_where(&self, filter: impl Fn(&User) -> bool) -> Vec<String> {
        self.cache
            .read()
            .unwrap()
            .users
            .values()
            .filter(|user| filter(user))
            .map(|user| user.name.clone())
            .collect()
    }
    pub fn risk_user_names(&self, symbol: &str) -> Vec<String> {
        self.cache
            .read()
            .unwrap()
            .users
            .values()
            .filter(|user| exposed_to(user, symbol))
            .map(|user| user.name.clone())
            .collect()
    }
    async fn commit(
        &self,
        mutation: Mutation,
        tx: sqlx::Transaction<'static, sqlx::Sqlite>,
        publication: Publication,
    ) -> Result<()> {
        self.writer
            .submit(mutation, Work::Commit { tx, publication })
            .await
    }
    async fn begin(&self) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>> {
        self.writer
            .stats
            .transactions
            .fetch_add(1, Ordering::Relaxed);
        Ok(self.pool.begin().await?)
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
    pub async fn authenticate(&self, tok: &str) -> Result<Option<String>> {
        if tok.is_empty() {
            return Ok(None);
        }
        if let Some(name) = self.cache.read().unwrap().tokens.get(tok) {
            return Ok(Some(name.clone()));
        }
        Ok(self.login_aliases.lock().unwrap().get(tok).cloned())
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
        let mutation = self.writer.admit(&self.gate).await?;
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
        let mut tx = self.begin().await?;
        let symbol = if self.nstar_only {
            symbols::NSTAR
        } else {
            "USDJPY"
        };
        sqlx::query(
            "INSERT INTO users(name,salt,hash,token,symbol,account,created) VALUES(?,?,?,?,?,?,?)",
        )
        .bind(name)
        .bind(&salt)
        .bind(password_hash(&salt, pwd))
        .bind(&tok)
        .bind(symbol)
        .bind(serde_json::to_string(&account)?)
        .bind(now / 1000)
        .execute(&mut *tx)
        .await?;
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
        self.commit(
            mutation,
            tx,
            Publication {
                users: vec![User {
                    name: name.into(),
                    symbol: symbol.into(),
                    account,
                    last_trade: None,
                }],
                tokens: vec![(tok.clone(), name.into())],
                revision: true,
                ..Publication::default()
            },
        )
        .await?;
        Ok(tok)
    }
    pub async fn login(&self, name: &str, pwd: &str) -> Result<String> {
        let mutation = self.writer.admit(&self.gate).await?;
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
        let mut tx = self.begin().await?;
        sqlx::query("UPDATE users SET token=? WHERE name=?")
            .bind(&tok)
            .bind(name)
            .execute(&mut *tx)
            .await?;
        self.commit(
            mutation,
            tx,
            Publication {
                tokens: vec![(tok.clone(), name.into())],
                aliases: vec![(old, name.into())],
                revision: true,
                ..Publication::default()
            },
        )
        .await?;
        Ok(tok)
    }
    pub async fn transaction<F>(&self, name: &str, operation: F) -> Result<()>
    where
        F: FnOnce(&mut Account, &mut String, u64) -> std::result::Result<Vec<Trade>, String> + Send,
    {
        let mut operation = Some(operation);
        self.transaction_batch(&[name.to_owned()], |_, account, symbol, id| {
            operation.take().expect("single account operation")(account, symbol, id)
        })
        .await
    }
    /// Executes names in order on committed cache copies under one mutation gate.
    /// Any closure/validation/SQL error aborts the WHOLE batch. Callers that skip an
    /// invalid operation must restore that account's local copy inside their closure.
    /// No changes (full Account + symbol equality, and no trades) means zero SQL.
    pub async fn transaction_batch<F>(&self, names: &[String], mut operation: F) -> Result<()>
    where
        F: FnMut(&str, &mut Account, &mut String, u64) -> std::result::Result<Vec<Trade>, String>
            + Send,
    {
        let mutation = self.writer.admit(&self.gate).await?;
        let mut batch = Batch::new(self.next_id.load(Ordering::SeqCst), self.nstar_only);
        for name in names {
            let before = match batch.current(name) {
                Some(user) => user.clone(),
                None => self
                    .cache
                    .read()
                    .unwrap()
                    .users
                    .get(name)
                    .cloned()
                    .ok_or_else(|| invalid("账号不存在"))?,
            };
            let mut user = before.clone();
            let id = batch.next_id.max(user.account.next_id);
            let trades = operation(name, &mut user.account, &mut user.symbol, id)?;
            batch.stage(&before, user, trades)?;
        }
        self.submit_batch(mutation, batch).await
    }
    /// Risk sweep for one quote of `symbol`: every exposed account in name order, as if each
    /// were settled one after another against `live` prices that every fill moves.
    /// Accounts are evaluated in parallel on the CPU pool, and a result is accepted only
    /// while the prices and position IDs it saw are still current, so the outcome equals a
    /// sequential sweep. Most accounts trigger nothing, so one pass usually covers all.
    pub async fn risk_batch(&self, symbol: &str, live: trading::Prices, now: i64) -> Result<()> {
        let mutation = self.writer.admit_reserved(&self.gate).await?;
        let cache = self.cache.clone();
        let batch = Batch::new(self.next_id.load(Ordering::SeqCst), self.nstar_only);
        let symbol = symbol.to_owned();
        let batch = tokio::task::spawn_blocking(move || {
            let cache = cache.read().unwrap();
            let users: Vec<&User> = cache
                .users
                .values()
                .filter(|user| exposed_to(user, &symbol))
                .collect();
            sweep(batch, &users, live, &symbol, now)
        })
        .await
        .map_err(|_| invalid("风险计算任务失败"))??;
        self.submit_batch(mutation, batch).await
    }
    async fn submit_batch(&self, mutation: Mutation, batch: Batch) -> Result<()> {
        let stats = &self.writer.stats;
        stats
            .noop_accounts
            .fetch_add(batch.noop_accounts, Ordering::Relaxed);
        let Batch {
            mut rows,
            next_id,
            flow,
            ..
        } = batch;
        // Repeated names are allowed; a later operation can undo an earlier edit.
        // Compare the final full account to the original committed copy, not just
        // a per-operation dirty flag, before deciding whether any SQL is needed.
        {
            let cache = self.cache.read().unwrap();
            rows.retain_mut(|row| {
                let Some(original) = cache.users.get(&row.user.name) else {
                    return false;
                };
                row.changed =
                    row.user.account != original.account || row.user.symbol != original.symbol;
                row.changed || !row.trades.is_empty()
            });
        }
        if rows.is_empty() {
            stats.noops.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        self.writer
            .submit(
                mutation,
                Work::Accounts {
                    rows,
                    next_id,
                    flow,
                },
            )
            .await
    }
    pub fn take_nstar_flow(&self) -> (f64, f64) {
        std::mem::take(&mut *self.nstar_flow.lock().unwrap())
    }
    pub fn restore_nstar_flow(&self, player: f64, forced: f64) {
        let mut flow = self.nstar_flow.lock().unwrap();
        flow.0 += player;
        flow.1 += forced;
    }
    pub async fn select_symbol(&self, name: &str, symbol: &str) -> Result<()> {
        let mutation = self.writer.admit(&self.gate).await?;
        let mut tx = self.begin().await?;
        require_tradable(&mut tx, symbol, self.nstar_only).await?;
        let mut user = self.user(name).await?;
        user.symbol = symbol.into();
        persist(&mut tx, &user).await?;
        self.commit(
            mutation,
            tx,
            Publication {
                users: vec![user],
                revision: true,
                ..Publication::default()
            },
        )
        .await
    }
    pub async fn require_tradable(&self, symbol: &str) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        require_tradable(&mut conn, symbol, self.nstar_only).await
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
        let mutation = self.writer.admit(&self.gate).await?;
        let mut tx = self.begin().await?;
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
        self.commit(mutation, tx, Publication::default()).await?;
        Ok(json!({"id":id,"name":name,"time":now,"text":text}))
    }
    pub async fn deposit_totals(&self) -> Result<BTreeMap<String, f64>> {
        Ok(self.cache.read().unwrap().deposits.clone())
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
        let mutation = self.writer.admit(&self.gate).await?;
        let mut tx = self.begin().await?;
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
        self.commit(
            mutation,
            tx,
            Publication {
                users: vec![user],
                deposits: vec![(name.into(), amount)],
                revision: true,
                ..Publication::default()
            },
        )
        .await?;
        Ok(row)
    }
    pub async fn delete(&self, name: &str, confirmation: &str, now: i64) -> Result<Value> {
        if name != confirmation {
            return Err(invalid("请准确输入要删除的账号名"));
        }
        let mutation = self.writer.admit(&self.gate).await?;
        let mut tx = self.begin().await?;
        let user = load_user(&mut tx, name).await?;
        let row = json!({"name":name,"time":now,"actor":"PSK管理员","balance":user.account.balance,"positions":user.account.positions.len()});
        event(&mut tx, "delete", name, now, None, &row).await?;
        for table in ["users", "trades", "chat", "deposits", "equity_samples"] {
            sqlx::query(&format!("DELETE FROM {table} WHERE name=?"))
                .bind(name)
                .execute(&mut *tx)
                .await?;
        }
        // A deleted account's NSTAR book is unwound into the market like a stopout.
        self.commit(
            mutation,
            tx,
            Publication {
                deleted: vec![name.into()],
                flow: (0.0, -nstar_units(&user.account)),
                revision: true,
                ..Publication::default()
            },
        )
        .await?;
        Ok(row)
    }
    pub async fn settings(&self) -> Result<Value> {
        let mut conn = self.pool.acquire().await?;
        settings(&mut conn, self.nstar_only).await
    }
    pub async fn set_symbol(&self, symbol: &str, enabled: bool, now: i64) -> Result<Value> {
        if symbols::provider(symbol).is_none() || (self.nstar_only && symbol != symbols::NSTAR) {
            return Err(invalid("币种对或启用状态无效"));
        }
        let mutation = self.writer.admit(&self.gate).await?;
        let mut tx = self.begin().await?;
        let before = settings(&mut tx, self.nstar_only).await?;
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
        let after = settings(&mut tx, self.nstar_only).await?;
        self.commit(
            mutation,
            tx,
            Publication {
                revision: true,
                ..Publication::default()
            },
        )
        .await?;
        Ok(after)
    }
    pub async fn five_minute_returns(
        &self,
        rows: &mut [Value],
        totals: &BTreeMap<String, f64>,
        now: i64,
    ) -> Result<()> {
        let mutation = self.writer.admit(&self.gate).await?;
        let mut tx = self.begin().await?;
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
        self.commit(mutation, tx, Publication::default()).await
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
        let mutation = self.writer.admit_reserved(&self.gate).await?;
        if bars.is_empty() {
            self.writer.stats.noops.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        self.writer
            .submit(
                mutation,
                Work::Bars {
                    symbol: symbol.into(),
                    bars: bars.to_vec(),
                },
            )
            .await
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
    load_user_mode(conn, name, AccountLoad::Validated).await
}
async fn load_user_mode(
    conn: &mut SqliteConnection,
    name: &str,
    load: AccountLoad,
) -> Result<User> {
    let row = sqlx::query("SELECT symbol,account FROM users WHERE name=?")
        .bind(name)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| invalid("账号不存在"))?;
    let mut account: Account = serde_json::from_str(row.get("account"))?;
    // Leverage reductions are safe migrations: existing positions stay open, while future
    // margin checks and API edits immediately use the new ceiling.
    account.leverage = account.leverage.clamp(1, trading::MAX_LEVERAGE);
    if matches!(load, AccountLoad::Validated) {
        account.validate()?;
    }
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
fn has_legacy_exposure(user: &User) -> bool {
    user.symbol != symbols::NSTAR
        || user
            .account
            .positions
            .iter()
            .any(|p| p.symbol != symbols::NSTAR)
        || user
            .account
            .orders
            .iter()
            .any(|o| o.symbol != symbols::NSTAR)
        || user
            .account
            .spot
            .keys()
            .any(|symbol| symbol != symbols::NSTAR)
}

async fn settings(conn: &mut SqliteConnection, nstar_only: bool) -> Result<Value> {
    let flags: BTreeMap<String, (bool, i64)> =
        sqlx::query("SELECT symbol,enabled,version FROM symbol_settings")
            .fetch_all(conn)
            .await?
            .into_iter()
            .map(|r| (r.get("symbol"), (r.get("enabled"), r.get("version"))))
            .collect();
    Ok(
        json!({"version":flags.values().map(|(_,v)|*v).max().unwrap_or(0),"rows":symbols::all().iter().filter(|s| !nstar_only || **s == symbols::NSTAR).map(|s|json!({"symbol":s,"provider":symbols::provider(s),"enabled":flags.get(*s).map(|(e,_)|*e).unwrap_or(true)})).collect::<Vec<_>>()}),
    )
}
async fn require_tradable(
    conn: &mut SqliteConnection,
    symbol: &str,
    nstar_only: bool,
) -> Result<()> {
    if symbols::provider(symbol).is_none() || (nstar_only && symbol != symbols::NSTAR) {
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
