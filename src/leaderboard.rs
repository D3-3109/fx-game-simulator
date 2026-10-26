//! Display-only history: bounded in memory, deliberately reset on process restart.
//! Neither a cold HTTP read nor a page read samples equity or touches SQLite.
use super::App;
use crate::{storage, trading::Prices};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex},
};

const HISTORY_SECONDS: i64 = 900;
const MAX_SAMPLES: usize = 181; // Inclusive endpoints of fifteen minutes at five seconds.

#[derive(Clone, Copy)]
struct Sample {
    at: i64,
    equity: f64,
    deposited: f64,
}

pub(super) struct Snapshot {
    rows: Vec<Value>,
    names: BTreeMap<String, usize>,
    pub generation: u64,
    pub packet: Arc<str>,
    as_of: i64,
}
impl Snapshot {
    fn new(mut rows: Vec<Value>, generation: u64, as_of: i64) -> Self {
        let mut names = BTreeMap::new();
        for (index, row) in rows.iter_mut().enumerate() {
            row["rank"] = json!(index + 1);
            names.insert(row["name"].as_str().unwrap().to_owned(), index);
        }
        let packet = json!({"type":"lb","rows":rows.iter().take(50).collect::<Vec<_>>(),
            "total":rows.len(),"as_of":as_of})
        .to_string()
        .into();
        Self {
            rows,
            names,
            generation,
            packet,
            as_of,
        }
    }
    pub fn me(&self, name: &str) -> Value {
        self.names
            .get(name)
            .map(|i| self.rows[*i].clone())
            .unwrap_or(Value::Null)
    }
    pub fn page(&self, name: Option<&str>, offset: usize, limit: usize) -> Value {
        json!({"ok":true,"rows":self.rows.iter().skip(offset).take(limit.min(100)).collect::<Vec<_>>(),
            "me":name.map(|n|self.me(n)),"total":self.rows.len(),"as_of":self.as_of})
    }
}
#[derive(Default)]
struct Data {
    snapshot: Option<Arc<Snapshot>>,
    baselines: BTreeMap<String, VecDeque<Sample>>,
    deletion_epoch: u64,
    generation: u64,
}
#[derive(Default)]
pub(super) struct Leaderboard {
    data: Mutex<Data>,
    refresh: tokio::sync::Mutex<()>,
}
impl Leaderboard {
    pub fn current(&self) -> Option<Arc<Snapshot>> {
        self.data.lock().unwrap().snapshot.clone()
    }
    pub fn remove(&self, name: &str) {
        let mut data = self.data.lock().unwrap();
        data.deletion_epoch += 1;
        data.baselines.remove(name);
        data.generation += 1;
        if let Some(old) = &data.snapshot {
            let rows = old
                .rows
                .iter()
                .filter(|r| r["name"] != name)
                .cloned()
                .collect();
            data.snapshot = Some(Arc::new(Snapshot::new(
                rows,
                data.generation,
                storage::now_ms(),
            )));
        }
    }
    pub async fn get(&self, app: &App, sample: bool) -> Result<Arc<Snapshot>, String> {
        if !sample && let Some(snapshot) = self.current() {
            return Ok(snapshot);
        }
        let _singleflight = self.refresh.lock().await;
        if !sample && let Some(snapshot) = self.current() {
            return Ok(snapshot);
        }
        loop {
            let epoch = self.data.lock().unwrap().deletion_epoch;
            let (users, totals) = app
                .store
                .leaderboard_snapshot()
                .await
                .map_err(|_| "账户读取失败")?;
            // This is the committed in-memory quote set, never quote()/prices() with upstream I/O.
            let mut prices = Prices::new();
            for (symbol, quote) in app.market.snapshot().await {
                prices.insert(symbol.clone(), quote.price);
                if let Some(book) = quote.bid_ask {
                    prices.bid_ask.insert(symbol.clone(), book);
                }
                if let Some(anchor) = quote.anchor {
                    prices.anchor.insert(symbol.clone(), anchor);
                }
                if let Some(mark) = quote.mark {
                    prices.mark.insert(symbol, mark);
                }
            }
            let online: BTreeSet<_> = app.online().into_iter().collect();
            let names: BTreeSet<_> = users.iter().map(|u| u.name.clone()).collect();
            let mut rows = Vec::with_capacity(users.len());
            for user in users {
                let Ok(equity) = user.account.equity(&prices) else {
                    continue;
                };
                let deposited = totals.get(&user.name).copied().unwrap_or(0.0);
                rows.push(json!({"name":user.name,"equity":(equity*100.0).round()/100.0,
                    "ret":((equity/(10000.0+deposited)-1.0)*1e4).round()/1e4,
                    "pos":user.account.positions.len(),"sym":user.symbol,"online":online.contains(&user.name)}));
            }
            let now = storage::now_ms();
            let mut data = self.data.lock().unwrap();
            // A deletion during the async snapshot must not resurrect a row or its baseline.
            if data.deletion_epoch != epoch {
                continue;
            }
            data.baselines.retain(|name, _| names.contains(name));
            for row in &mut rows {
                let name = row["name"].as_str().unwrap().to_owned();
                let deposited = totals.get(&name).copied().unwrap_or(0.0);
                let equity = row["equity"].as_f64().unwrap();
                let history = data.baselines.entry(name).or_default();
                returns(row, history, deposited, now / 1000);
                if sample {
                    record(
                        history,
                        Sample {
                            at: now / 1000 / 5 * 5,
                            equity,
                            deposited,
                        },
                    );
                }
            }
            rows.sort_by(|a, b| {
                b["ret"]
                    .as_f64()
                    .unwrap()
                    .total_cmp(&a["ret"].as_f64().unwrap())
                    .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
            });
            data.generation += 1;
            let snapshot = Arc::new(Snapshot::new(rows, data.generation, now));
            data.snapshot = Some(snapshot.clone());
            return Ok(snapshot);
        }
    }
}
fn returns(row: &mut Value, history: &VecDeque<Sample>, deposited: f64, now: i64) {
    row["pnl_5m"] = Value::Null;
    row["ret_5m"] = Value::Null;
    if let Some(base) = history
        .iter()
        .rev()
        .find(|s| (now - 315..=now - 300).contains(&s.at))
    {
        let pnl = row["equity"].as_f64().unwrap() - base.equity - (deposited - base.deposited);
        row["pnl_5m"] = json!((pnl * 100.0).round() / 100.0);
        if base.equity > 0.0 {
            row["ret_5m"] = json!(pnl / base.equity);
        }
    }
}
fn record(history: &mut VecDeque<Sample>, sample: Sample) {
    while history
        .front()
        .is_some_and(|s| s.at < sample.at - HISTORY_SECONDS)
    {
        history.pop_front();
    }
    if history.back().is_none_or(|s| s.at != sample.at) {
        history.push_back(sample);
    }
    while history.len() > MAX_SAMPLES {
        history.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn baseline_is_bounded_deduplicated_and_deducts_deposits() {
        let mut history = VecDeque::new();
        for at in (0..=1800).step_by(5) {
            record(
                &mut history,
                Sample {
                    at,
                    equity: 10000.0,
                    deposited: 0.0,
                },
            );
            record(
                &mut history,
                Sample {
                    at,
                    equity: 10000.0,
                    deposited: 0.0,
                },
            );
        }
        assert_eq!(history.len(), 181);
        assert_eq!(history.front().unwrap().at, 900);
        let mut row = json!({"equity":11200.0});
        returns(&mut row, &history, 1000.0, 1800);
        assert_eq!(row["pnl_5m"], 200.0);
        assert_eq!(row["ret_5m"], 0.02);
        returns(&mut row, &VecDeque::new(), 1000.0, 1800);
        assert!(row["pnl_5m"].is_null());
        assert!(row["ret_5m"].is_null());
    }
    #[test]
    fn deleting_user_removes_history_and_reranks_cached_rows() {
        let board = Leaderboard::default();
        {
            let mut data = board.data.lock().unwrap();
            data.baselines.insert(
                "gone".into(),
                VecDeque::from([Sample {
                    at: 0,
                    equity: 1.0,
                    deposited: 0.0,
                }]),
            );
            data.snapshot = Some(Arc::new(Snapshot::new(
                vec![json!({"name":"gone"}), json!({"name":"kept"})],
                1,
                0,
            )));
        }
        board.remove("gone");
        assert!(!board.data.lock().unwrap().baselines.contains_key("gone"));
        let current = board.current().unwrap();
        assert_eq!(current.me("kept")["rank"], 1);
        assert!(current.me("gone").is_null());
    }
}
