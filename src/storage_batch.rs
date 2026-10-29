//! Staging for multi-account batches, and the parallel risk sweep built on it.
use super::*;
use rayon::prelude::*;
use trading::Prices;

/// Accounts changed by one batch, staged in order before a single commit.
pub(super) struct Batch {
    pub rows: Vec<PreparedAccount>,
    indices: BTreeMap<String, usize>,
    pub next_id: u64,
    pub flow: (f64, f64),
    pub noop_accounts: u64,
    nstar_only: bool,
}

impl Batch {
    pub fn new(next_id: u64, nstar_only: bool) -> Self {
        Self {
            rows: Vec::new(),
            indices: BTreeMap::new(),
            next_id,
            flow: (0.0, 0.0),
            noop_accounts: 0,
            nstar_only,
        }
    }
    /// The staged copy of `name`, if an earlier operation in this batch changed it.
    pub fn current(&self, name: &str) -> Option<&User> {
        self.indices.get(name).map(|index| &self.rows[*index].user)
    }
    /// Validate and stage `user`, the result of one operation on `before`.
    pub fn stage(&mut self, before: &User, user: User, trades: Vec<Trade>) -> Result<()> {
        user.account.validate()?;
        if self.nstar_only
            && (has_legacy_exposure(&user)
                || trades
                    .iter()
                    .any(|t| !t.symbol.is_empty() && t.symbol != symbols::NSTAR))
        {
            return Err(invalid("当前服务仅支持 NSTAR"));
        }
        let changed = user.account != before.account || user.symbol != before.symbol;
        if !changed && trades.is_empty() {
            self.noop_accounts += 1;
            return Ok(());
        }
        // IDs are reserved locally, not published until the single batch commits.
        // Account.next_id also captures multiple fills in one risk operation.
        self.next_id = self.next_id.max(user.account.next_id);
        for position in &user.account.positions {
            self.next_id = self.next_id.max(position.id.saturating_add(1));
        }
        for trade in trades.iter().filter(|trade| trade.kind == "open") {
            self.next_id = self.next_id.max(trade.position_id.saturating_add(1));
        }
        let total = nstar_units(&user.account) - nstar_units(&before.account);
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
        self.flow.0 += player;
        self.flow.1 += total - player;
        if let Some(index) = self.indices.get(&user.name) {
            let row = &mut self.rows[*index];
            row.user = user;
            row.changed |= changed;
            row.trades.extend(trades);
        } else {
            self.indices.insert(user.name.clone(), self.rows.len());
            self.rows.push(PreparedAccount {
                old_symbol: before.symbol.clone(),
                user,
                changed,
                trades,
            });
        }
        Ok(())
    }
}

/// Whether a quote for `symbol` can trigger anything on this account. Invalid legacy
/// exposure still reaches the risk engine for its established error policy.
pub(super) fn exposed_to(user: &User, symbol: &str) -> bool {
    if user.account.positions.is_empty() && user.account.orders.is_empty() {
        return false;
    }
    let symbols = user
        .account
        .symbols()
        .chain(user.account.orders.iter().map(|o| o.symbol.as_str()));
    trading::price_symbols(symbols).map_or(true, |required| required.contains(symbol))
}

struct Outcome {
    account: Account,
    trades: Vec<Trade>,
    after: Prices,
}

/// Triggers, resting orders and stop-out for one account at `live`. `None` means the
/// account is unaffected: skipped, unchanged, or an operation that failed and must not
/// leak a partial candidate or tentative fills.
fn evaluate(user: &User, live: &Prices, symbol: &str, next_id: u64, now: i64) -> Option<Outcome> {
    let account = &user.account;
    if account.positions.is_empty() && account.orders.is_empty() {
        return None;
    }
    let needed = trading::price_symbols(account.symbols()).ok()?;
    if (!needed.contains(symbol) && !account.orders.iter().any(|o| o.symbol == symbol))
        || needed.iter().any(|s| !live.contains_key(s))
    {
        return None;
    }
    let id = next_id.max(account.next_id);
    let mut candidate = account.clone();
    let mut after = live.clone();
    let result: Result<Vec<Trade>> = (|| {
        let mut trades = candidate.check_triggers(&after, now)?;
        for trade in &trades {
            trading::apply_fill(&mut after, trade)?;
        }
        let fills = candidate.check_orders(&after, id, now)?;
        for trade in &fills {
            trading::apply_fill(&mut after, trade)?;
        }
        trades.extend(fills);
        // Keep liquidation pricing on the smoothed mark and executions on live AMM.
        let stops = candidate.check_stopout_at(&after.marked(), &after, now)?;
        for trade in &stops {
            trading::apply_fill(&mut after, trade)?;
        }
        trades.extend(stops);
        candidate.validate()?;
        Ok(trades)
    })();
    match result {
        Ok(trades) if !trades.is_empty() || candidate != *account => Some(Outcome {
            account: candidate,
            trades,
            after,
        }),
        _ => None,
    }
}

/// Smallest re-evaluation window after an account changed the shared prices.
const MIN_WINDOW: usize = 32;

/// Settle `users` in order. Each pass evaluates a window in parallel against the current
/// prices and next ID, then accepts results up to and including the first account that
/// changes anything; everything after it saw stale prices and is evaluated again. The
/// window doubles while passes stay quiet, so a calm tick costs one parallel pass and a
/// crash with many liquidations costs a few small passes per liquidation.
pub(super) fn sweep(
    mut batch: Batch,
    users: &[&User],
    mut live: Prices,
    symbol: &str,
    now: i64,
) -> Result<Batch> {
    let mut start = 0;
    let mut window = users.len();
    while start < users.len() {
        let end = (start + window).min(users.len());
        let next_id = batch.next_id;
        let outcomes: Vec<Option<Outcome>> = users[start..end]
            .par_iter()
            .map(|user| evaluate(user, &live, symbol, next_id, now))
            .collect();
        let mut accepted = end;
        for (offset, outcome) in outcomes.into_iter().enumerate() {
            let Some(outcome) = outcome else {
                batch.noop_accounts += 1;
                continue;
            };
            let before = users[start + offset];
            let mut user = before.clone();
            user.account = outcome.account;
            // Only fills move prices, and only fills or new positions reserve IDs. A
            // change without either (an order voided, say) leaves later results valid.
            let moved = !outcome.trades.is_empty();
            batch.stage(before, user, outcome.trades)?;
            if moved || batch.next_id != next_id {
                live = outcome.after;
                accepted = start + offset + 1;
                break;
            }
        }
        window = if accepted == end {
            window.saturating_mul(2).max(MIN_WINDOW)
        } else {
            MIN_WINDOW.max(rayon::current_num_threads() * 4)
        };
        start = accepted;
    }
    Ok(batch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nstar(price: f64, anchor: f64) -> Prices {
        let mut q: Prices = [(symbols::NSTAR.to_string(), price)].into_iter().collect();
        q.anchor.insert(symbols::NSTAR.into(), anchor.ln());
        q.mark.insert(symbols::NSTAR.into(), price);
        q
    }

    /// The pre-parallel algorithm: one account after another, each seeing every earlier fill.
    fn sequential(mut batch: Batch, users: &[&User], mut live: Prices, now: i64) -> Batch {
        for user in users {
            match evaluate(user, &live, symbols::NSTAR, batch.next_id, now) {
                Some(outcome) => {
                    let mut next = (*user).clone();
                    next.account = outcome.account;
                    batch.stage(user, next, outcome.trades).unwrap();
                    live = outcome.after;
                }
                None => batch.noop_accounts += 1,
            }
        }
        batch
    }

    #[test]
    fn parallel_sweep_matches_sequential_settlement() {
        let open = nstar(100.0, 100.0);
        let mut users = Vec::new();
        let mut id = 1;
        for i in 0..400u64 {
            let mut account = Account {
                balance: 2_000.0 + (i % 13) as f64 * 700.0,
                ..Account::default()
            };
            // Mixed sizes and sides: some longs liquidate in the crash, shorts profit,
            // and resting buy limits fill on the way down, each moving the AMM price.
            let side = if i % 5 == 0 { "short" } else { "long" };
            let lots = 1_000.0 + (i % 17) as f64 * 900.0;
            if account
                .open(side, lots, symbols::NSTAR, &open, id, 0)
                .is_ok()
            {
                id += 1;
            }
            if i % 3 == 0 {
                let price = 80.0 + (i % 11) as f64;
                let mut with_order = account.clone();
                if with_order
                    .place_order(
                        "limit",
                        "long",
                        200.0,
                        symbols::NSTAR,
                        price,
                        0.0,
                        0.0,
                        0.0,
                        &open,
                        0,
                    )
                    .is_ok()
                {
                    account = with_order;
                }
            }
            account.next_id = id;
            users.push(User {
                name: format!("u{i:04}"),
                symbol: symbols::NSTAR.into(),
                account,
                last_trade: None,
            });
        }
        let refs: Vec<&User> = users.iter().collect();
        for crash in [100.0, 97.0, 91.0, 84.0, 75.0] {
            let live = nstar(crash, 100.0);
            let expected = sequential(Batch::new(id, true), &refs, live.clone(), 1_000);
            let actual = sweep(Batch::new(id, true), &refs, live, symbols::NSTAR, 1_000).unwrap();
            assert_eq!(actual.rows.len(), expected.rows.len(), "crash to {crash}");
            for (a, e) in actual.rows.iter().zip(&expected.rows) {
                assert_eq!(a.user.name, e.user.name);
                assert_eq!(a.user.account, e.user.account, "{} at {crash}", a.user.name);
                assert_eq!(a.trades, e.trades, "{} at {crash}", a.user.name);
                assert_eq!(a.changed, e.changed);
            }
            assert_eq!(actual.next_id, expected.next_id);
            assert_eq!(actual.flow, expected.flow);
            assert_eq!(actual.noop_accounts, expected.noop_accounts);
            if crash < 90.0 {
                assert!(
                    actual.rows.len() > 20,
                    "the crash must exercise cascading fills"
                );
            }
        }
    }
}
