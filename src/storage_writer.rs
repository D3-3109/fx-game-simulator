//! One bounded, cancellation-independent commit owner. Counters describe software work,
//! not disk IOPS. Production uses WAL/FULL; legacy fixtures deliberately retain NORMAL.
use super::*;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{OwnedMutexGuard, OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};

const CAPACITY: usize = 32;
const GATE_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) struct Mutation {
    _gate: OwnedMutexGuard<()>,
    _permit: Option<OwnedSemaphorePermit>,
}

#[derive(Default)]
pub(super) struct Stats {
    pub transactions: AtomicU64,
    pub commits: AtomicU64,
    pub noops: AtomicU64,
    pub noop_accounts: AtomicU64,
    pub batch_rows: AtomicU64,
    pub account_updates: AtomicU64,
    pub bar_rows: AtomicU64,
    pub bar_cleanups: AtomicU64,
    pub failures: AtomicU64,
    pub rejected: AtomicU64,
    pub queue_depth: AtomicU64,
    pub close_failures: AtomicU64,
}

#[derive(Default)]
pub(super) struct Publication {
    pub users: Vec<User>,
    pub deleted: Vec<String>,
    pub tokens: Vec<(String, String)>,
    pub aliases: Vec<(String, String)>,
    pub deposits: Vec<(String, f64)>,
    pub next_id: Option<u64>,
    pub flow: (f64, f64),
    pub revision: bool,
}

pub(super) struct PreparedAccount {
    pub user: User,
    pub old_symbol: String,
    pub changed: bool,
    pub trades: Vec<Trade>,
}

pub(super) enum Work {
    Accounts {
        rows: Vec<PreparedAccount>,
        next_id: u64,
        flow: (f64, f64),
    },
    Commit {
        tx: sqlx::Transaction<'static, sqlx::Sqlite>,
        publication: Publication,
    },
    Bars {
        symbol: String,
        bars: Vec<(i64, [f64; 5], i64)>,
    },
}

struct Request {
    work: Work,
    mutation: Mutation,
    reply: oneshot::Sender<Result<()>>,
}

pub(super) struct Writer {
    sender: StdMutex<Option<mpsc::Sender<Request>>>,
    permits: Arc<Semaphore>,
    pub accepting: std::sync::atomic::AtomicBool,
    done: watch::Receiver<bool>,
    pub stats: Arc<Stats>,
}

pub(super) struct Shared {
    pub pool: SqlitePool,
    pub nstar_only: bool,
    pub gate: Arc<Mutex<()>>,
    pub cache: Arc<std::sync::RwLock<Cache>>,
    pub next_id: Arc<AtomicU64>,
    pub flow: Arc<StdMutex<(f64, f64)>>,
    pub aliases: Arc<StdMutex<BTreeMap<String, String>>>,
    pub revision: Arc<AtomicU64>,
}

impl Writer {
    pub fn start(shared: Shared) -> Self {
        let (sender, receiver) = mpsc::channel(CAPACITY);
        let (done_tx, done) = watch::channel(false);
        let stats = Arc::new(Stats::default());
        // Exactly one task per store, never one task per request.
        tokio::spawn(run(shared, receiver, done_tx, stats.clone()));
        Self {
            sender: StdMutex::new(Some(sender)),
            permits: Arc::new(Semaphore::new(CAPACITY)),
            accepting: std::sync::atomic::AtomicBool::new(true),
            done,
            stats,
        }
    }

    fn unavailable(&self, message: &str) -> Error {
        self.stats.rejected.fetch_add(1, Ordering::Relaxed);
        Error::Unavailable(message.into())
    }

    pub async fn admit(&self, gate: &Arc<Mutex<()>>) -> Result<Mutation> {
        if !self.accepting.load(Ordering::Acquire) {
            return Err(self.unavailable("storage is closing"));
        }
        // Bound waiters BEFORE acquiring the account gate. There is no unbounded
        // send-await queue hidden behind the channel's capacity.
        let permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| self.unavailable("storage writer admission is full"))?;
        let guard = tokio::time::timeout(GATE_TIMEOUT, gate.clone().lock_owned())
            .await
            .map_err(|_| self.unavailable("storage mutation wait timed out"))?;
        if !self.accepting.load(Ordering::Acquire) {
            return Err(self.unavailable("storage is closing"));
        }
        Ok(Mutation {
            _gate: guard,
            _permit: Some(permit),
        })
    }

    /// Admission for the fixed set of background producers (risk sweeps, candle flushes).
    /// They wait for the gate like everyone else but never take a request permit, so a
    /// burst of client orders cannot make a liquidation sweep fail and retry.
    pub async fn admit_reserved(&self, gate: &Arc<Mutex<()>>) -> Result<Mutation> {
        if !self.accepting.load(Ordering::Acquire) {
            return Err(self.unavailable("storage is closing"));
        }
        let guard = tokio::time::timeout(GATE_TIMEOUT, gate.clone().lock_owned())
            .await
            .map_err(|_| self.unavailable("storage mutation wait timed out"))?;
        if !self.accepting.load(Ordering::Acquire) {
            return Err(self.unavailable("storage is closing"));
        }
        Ok(Mutation {
            _gate: guard,
            _permit: None,
        })
    }

    pub async fn submit(&self, mutation: Mutation, work: Work) -> Result<()> {
        let (reply, result) = oneshot::channel();
        {
            let sender = self.sender.lock().unwrap();
            let sender = sender
                .as_ref()
                .ok_or_else(|| self.unavailable("storage is closing"))?;
            self.stats.queue_depth.fetch_add(1, Ordering::Relaxed);
            if let Err(error) = sender.try_send(Request {
                work,
                mutation,
                reply,
            }) {
                self.stats.queue_depth.fetch_sub(1, Ordering::Relaxed);
                return Err(self.unavailable(match error {
                    mpsc::error::TrySendError::Full(_) => "storage writer queue is full",
                    mpsc::error::TrySendError::Closed(_) => "storage writer has stopped",
                }));
            }
        }
        // Cancellation here drops only the reply receiver, NOT the mutation guard
        // or transaction. The worker must finish commit AND publication regardless.
        result.await.map_err(|_| {
            self.stats.failures.fetch_add(1, Ordering::Relaxed);
            Error::Unavailable("storage writer stopped before acknowledgement".into())
        })?
    }

    pub async fn close(&self) -> Result<()> {
        self.accepting.store(false, Ordering::Release);
        self.permits.close();
        self.sender.lock().unwrap().take();
        let mut done = self.done.clone();
        loop {
            if *done.borrow() {
                return Ok(());
            }
            if done.changed().await.is_err() {
                self.stats.close_failures.fetch_add(1, Ordering::Relaxed);
                return Err(Error::Unavailable(
                    "storage writer failed during close".into(),
                ));
            }
        }
    }

    pub fn snapshot(&self) -> Value {
        let s = &self.stats;
        let get = |n: &AtomicU64| n.load(Ordering::Relaxed);
        json!({
            "kind": "software_counters_not_disk_iops", "capacity": CAPACITY,
            "admitted": CAPACITY - self.permits.available_permits(),
            "accepting": self.accepting.load(Ordering::Acquire),
            "queue_depth": get(&s.queue_depth), "transactions": get(&s.transactions),
            "commits": get(&s.commits), "noop": get(&s.noops), "noops": get(&s.noops),
            "noop_accounts": get(&s.noop_accounts), "batch_rows": get(&s.batch_rows),
            "rows": get(&s.batch_rows), "account_updates": get(&s.account_updates),
            "bar_rows": get(&s.bar_rows), "bar_cleanups": get(&s.bar_cleanups),
            "failures": get(&s.failures), "rejected": get(&s.rejected),
            "close_failures": get(&s.close_failures)
        })
    }
}

async fn run(
    shared: Shared,
    mut receiver: mpsc::Receiver<Request>,
    done: watch::Sender<bool>,
    stats: Arc<Stats>,
) {
    let mut cleanup = BTreeMap::<(String, i64), i64>::new();
    while let Some(Request {
        work,
        mutation,
        reply,
    }) = receiver.recv().await
    {
        let result = execute(&shared, &stats, &mut cleanup, work).await;
        if result.is_err() {
            stats.failures.fetch_add(1, Ordering::Relaxed);
        }
        stats.queue_depth.fetch_sub(1, Ordering::Relaxed);
        // Publish precedes gate release; even a cancelled caller cannot expose an
        // uncommitted account to the next operation or lose a committed update.
        drop(mutation);
        let _ = reply.send(result);
    }
    // Direct-SQL preparation may still hold the gate. Closing admission ensures it
    // cannot submit new work; its local transaction rolls back before we close.
    let _guard = shared.gate.lock().await;
    shared.pool.close().await;
    let _ = done.send(true);
}

async fn execute(
    shared: &Shared,
    stats: &Stats,
    cleanup: &mut BTreeMap<(String, i64), i64>,
    work: Work,
) -> Result<()> {
    let (tx, publication, account_updates, batch_rows, bar_rows, cleanups) = match work {
        Work::Commit { tx, publication } => (tx, publication, 0, 0, 0, BTreeMap::new()),
        Work::Accounts {
            rows,
            next_id,
            flow,
        } => {
            stats.transactions.fetch_add(1, Ordering::Relaxed);
            let mut tx = shared.pool.begin().await?;
            let mut publication = Publication {
                next_id: Some(next_id),
                flow,
                revision: true,
                ..Publication::default()
            };
            let mut updates = 0;
            let count = rows.len() as u64;
            for PreparedAccount {
                mut user,
                old_symbol,
                changed,
                trades,
            } in rows
            {
                for trade in &trades {
                    if trade.kind == "open" {
                        require_tradable(&mut tx, &trade.symbol, shared.nstar_only).await?;
                    }
                }
                if old_symbol != user.symbol {
                    require_tradable(&mut tx, &user.symbol, shared.nstar_only).await?;
                }
                if changed {
                    persist(&mut tx, &user).await?;
                    updates += 1;
                }
                for trade in &trades {
                    insert_trade(&mut tx, &user.name, trade).await?;
                }
                if !trades.is_empty() {
                    user.last_trade = history(&mut tx, &user.name, 1).await?.into_iter().next();
                }
                publication.users.push(user);
            }
            (tx, publication, updates, count, 0, BTreeMap::new())
        }
        Work::Bars { symbol, bars } => {
            stats.transactions.fetch_add(1, Ordering::Relaxed);
            let mut tx = shared.pool.begin().await?;
            let mut cleanups = BTreeMap::new();
            let count = bars.len() as u64;
            for (sec, b, oldest) in bars {
                sqlx::query(
                    "INSERT OR REPLACE INTO sim_bars(symbol,sec,t,o,h,l,c) VALUES(?,?,?,?,?,?,?)",
                )
                .bind(&symbol)
                .bind(sec)
                .bind(b[0] as i64)
                .bind(b[1])
                .bind(b[2])
                .bind(b[3])
                .bind(b[4])
                .execute(&mut *tx)
                .await?;
                let key = (symbol.clone(), sec);
                if cleanup.get(&key).is_none_or(|previous| oldest > *previous) {
                    cleanups
                        .entry(key)
                        .and_modify(|v: &mut i64| *v = (*v).max(oldest))
                        .or_insert(oldest);
                }
            }
            for ((symbol, sec), oldest) in &cleanups {
                sqlx::query("DELETE FROM sim_bars WHERE symbol=? AND sec=? AND t<?")
                    .bind(symbol)
                    .bind(sec)
                    .bind(oldest)
                    .execute(&mut *tx)
                    .await?;
            }
            (tx, Publication::default(), 0, 0, count, cleanups)
        }
    };
    tx.commit().await?;
    // NO await between COMMIT and publication (nor before metrics/watermarks).
    publish(shared, publication);
    stats.commits.fetch_add(1, Ordering::Relaxed);
    stats
        .account_updates
        .fetch_add(account_updates, Ordering::Relaxed);
    stats.batch_rows.fetch_add(batch_rows, Ordering::Relaxed);
    stats.bar_rows.fetch_add(bar_rows, Ordering::Relaxed);
    stats
        .bar_cleanups
        .fetch_add(cleanups.len() as u64, Ordering::Relaxed);
    cleanup.extend(cleanups);
    Ok(())
}

fn publish(shared: &Shared, publication: Publication) {
    let mut cache = shared.cache.write().unwrap();
    let mut aliases = shared.aliases.lock().unwrap();
    for name in publication.deleted {
        cache.users.remove(&name);
        cache.tokens.retain(|_, owner| owner != &name);
        cache.deposits.remove(&name);
        aliases.retain(|_, owner| owner != &name);
    }
    for user in publication.users {
        cache.users.insert(user.name.clone(), user);
    }
    for (token, name) in publication.aliases {
        cache.tokens.remove(&token);
        if !token.is_empty() {
            aliases.insert(token, name);
        }
    }
    for (token, name) in publication.tokens {
        cache.tokens.insert(token, name);
    }
    for (name, amount) in publication.deposits {
        *cache.deposits.entry(name).or_default() += amount;
    }
    if let Some(id) = publication.next_id {
        shared.next_id.fetch_max(id, Ordering::SeqCst);
    }
    let mut flow = shared.flow.lock().unwrap();
    flow.0 += publication.flow.0;
    flow.1 += publication.flow.1;
    if publication.revision {
        shared.revision.fetch_add(1, Ordering::Release);
    }
}
