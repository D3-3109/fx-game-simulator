use super::*;
use crate::trading::{Position, Prices};
use std::sync::Arc;

async fn setup() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open_legacy(&dir.path().join("test.db"))
        .await
        .unwrap();
    s.register("player", "password").await.unwrap();
    (dir, s)
}
fn quotes() -> Prices {
    [("FXSIM".into(), 1.1)].into()
}

#[tokio::test]
async fn trade_failure_rolls_back_account_and_history_and_restart_recovers() {
    let (dir, s) = setup().await;
    s.transaction("player", |a, sym, id| {
        *sym = "FXSIM".into();
        Ok(vec![a.open("long", 0.1, "FXSIM", &quotes(), id, 1)?])
    })
    .await
    .unwrap();
    let before = s.user("player").await.unwrap();
    sqlx::raw_sql("CREATE TRIGGER fail_trade BEFORE INSERT ON trades BEGIN SELECT RAISE(ABORT,'test failure'); END;").execute(&s.pool).await.unwrap();
    assert!(
        s.transaction("player", |a, _, _| a.close(None, &quotes(), 2))
            .await
            .is_err()
    );
    let next_id = s.next_id.load(Ordering::SeqCst);
    assert!(
        s.transaction("player", |a, _, id| {
            Ok(vec![a.open("long", 0.1, "FXSIM", &quotes(), id, 2)?])
        })
        .await
        .is_err()
    );
    assert_eq!(s.next_id.load(Ordering::SeqCst), next_id);
    assert_eq!(s.user("player").await.unwrap().account, before.account);
    assert_eq!(s.history("player").await.unwrap().len(), 1);
    s.close().await;
    let s = Store::open_legacy(&dir.path().join("test.db"))
        .await
        .unwrap();
    assert_eq!(s.user("player").await.unwrap().account, before.account);
    sqlx::query("DROP TRIGGER fail_trade")
        .execute(&s.pool)
        .await
        .unwrap();
    s.transaction("player", |a, _, _| a.close(None, &quotes(), 3))
        .await
        .unwrap();
    assert_eq!(s.history("player").await.unwrap().len(), 2);
    assert!(s.user("player").await.unwrap().account.positions.is_empty());
}

#[tokio::test]
async fn concurrent_risk_calls_settle_exactly_once() {
    let (_dir, s) = setup().await;
    let s = Arc::new(s);
    s.transaction("player", |a, _, _| {
        a.balance = 50.0;
        a.positions = vec![Position {
            id: 1,
            side: "long".into(),
            lots: 0.1,
            entry: 1.0,
            symbol: "FXSIM".into(),
            tp: 0.0,
            sl: 0.0,
        }];
        Ok(vec![])
    })
    .await
    .unwrap();
    let mut jobs = Vec::new();
    for _ in 0..20 {
        let s = s.clone();
        jobs.push(tokio::spawn(async move {
            s.transaction("player", |a, _, _| {
                a.check_stopout(&[("FXSIM".into(), 0.9999)].into(), 4)
            })
            .await
        }));
    }
    for j in jobs {
        j.await.unwrap().unwrap();
    }
    let a = s.user("player").await.unwrap().account;
    assert!(a.positions.is_empty());
    assert!((a.balance - 48.0).abs() < 1e-7);
    assert_eq!(s.history("player").await.unwrap().len(), 1);
}

#[tokio::test]
async fn deposits_are_atomic_concurrent_idempotent_and_audited_after_delete() {
    let (_dir, s) = setup().await;
    let s = Arc::new(s);
    let mut jobs = Vec::new();
    for _ in 0..10 {
        let s = s.clone();
        jobs.push(tokio::spawn(async move {
            s.deposit("player", 100.01, "request-00000001", 1).await
        }));
    }
    let mut rows = Vec::new();
    for j in jobs {
        rows.push(j.await.unwrap().unwrap());
    }
    assert!(rows.iter().all(|v| v == &rows[0]));
    assert_eq!(s.user("player").await.unwrap().account.balance, 10100.01);
    assert_eq!(s.events().await.unwrap().len(), 1);
    assert!(
        s.deposit("player", 100.0, "request-00000001", 2)
            .await
            .is_err()
    );
    sqlx::raw_sql("CREATE TRIGGER fail_audit BEFORE INSERT ON admin_events BEGIN SELECT RAISE(ABORT,'test failure'); END;").execute(&s.pool).await.unwrap();
    assert!(
        s.deposit("player", 200.0, "request-00000002", 3)
            .await
            .is_err()
    );
    assert_eq!(s.user("player").await.unwrap().account.balance, 10100.01);
    assert!(s.set_symbol("FXSIM", false, 4).await.is_err());
    s.require_tradable("FXSIM").await.unwrap();
    assert!(s.delete("player", "player", 5).await.is_err());
    assert!(s.user("player").await.is_ok());
    sqlx::query("DROP TRIGGER fail_audit")
        .execute(&s.pool)
        .await
        .unwrap();
    s.delete("player", "player", 6).await.unwrap();
    s.register("player", "new").await.unwrap();
    assert!(
        s.deposit("player", 100.01, "request-00000001", 7)
            .await
            .is_err()
    );
    assert_eq!(s.events().await.unwrap().len(), 2);
}

#[tokio::test]
async fn disabled_pairs_reject_new_trades_but_allow_existing_closes() {
    let (_dir, s) = setup().await;
    s.transaction("player", |a, sym, id| {
        *sym = "FXSIM".into();
        Ok(vec![a.open("long", 0.1, "FXSIM", &quotes(), id, 1)?])
    })
    .await
    .unwrap();
    let setting = s.set_symbol("FXSIM", false, 10).await.unwrap();
    assert_eq!(s.set_symbol("FXSIM", false, 11).await.unwrap(), setting);
    assert!(
        s.transaction("player", |a, _, id| Ok(vec![a.open(
            "long",
            0.1,
            "FXSIM",
            &quotes(),
            id,
            12
        )?]))
        .await
        .is_err()
    );
    assert_eq!(s.user("player").await.unwrap().account.positions.len(), 1);
    s.transaction("player", |a, _, _| a.close(None, &quotes(), 13))
        .await
        .unwrap();
    assert!(s.user("player").await.unwrap().account.positions.is_empty());
}

#[tokio::test]
async fn chat_retention_rate_limit_and_five_minute_deposit_adjustment() {
    let (_dir, s) = setup().await;
    for i in 0..105 {
        s.post_chat("player", &i.to_string(), i * 3000)
            .await
            .unwrap();
    }
    let chat = s.chat().await.unwrap();
    assert_eq!(chat.len(), 100);
    assert_eq!(chat[0]["text"], "5");
    assert!(
        s.post_chat("player", "too fast", 104 * 3000 + 10)
            .await
            .is_err()
    );
    let mut rows = vec![json!({"name":"player","equity":10000.0})];
    s.five_minute_returns(&mut rows, &BTreeMap::new(), 1800000000)
        .await
        .unwrap();
    assert!(rows[0]["pnl_5m"].is_null());
    let totals = [("player".into(), 1000.0)].into();
    rows[0]["equity"] = json!(11100.0);
    s.five_minute_returns(&mut rows, &totals, 1800000300)
        .await
        .unwrap();
    assert_eq!(rows[0]["pnl_5m"], 100.0);
    assert_eq!(rows[0]["ret_5m"], 0.01);
    s.five_minute_returns(&mut rows, &totals, 1800000700)
        .await
        .unwrap();
    assert!(rows[0]["pnl_5m"].is_null());
}

#[tokio::test]
async fn python_era_schema_accounts_tokens_and_legacy_currency_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    let s = Store::open_legacy(&path).await.unwrap();
    // Python-era rows: space-formatted account JSON and a trade written before
    // the currency column existed.
    let account = json!({"balance": 10000.0, "leverage": 100, "next_id": 2,
        "positions": [{"id": 1, "side": "long", "lots": 0.1, "entry": 150.01, "symbol": "USDJPY"}]});
    sqlx::query(
        "INSERT INTO users(name,salt,hash,token,symbol,account,created) VALUES(?,?,?,?,?,?,'2026')",
    )
    .bind("旧用户")
    .bind("unitsalt")
    .bind(password_hash("unitsalt", "兼容密码"))
    .bind("legacy-token-0001")
    .bind("USDJPY")
    .bind(serde_json::to_string(&account).unwrap())
    .execute(&mut *s.pool.acquire().await.unwrap())
    .await
    .unwrap();
    sqlx::query("INSERT INTO trades(name,time,kind,position_id,symbol,side,lots,entry,price,pnl,balance,margin_level) VALUES('旧用户',1791307402948,'open',1,'USDJPY','long',0.1,150.0,150.0,NULL,10000.0,NULL)")
        .execute(&mut *s.pool.acquire().await.unwrap())
        .await
        .unwrap();
    sqlx::query("ALTER TABLE trades DROP COLUMN currency")
        .execute(&s.pool)
        .await
        .unwrap();
    s.close().await;
    let s = Store::open_legacy(&path).await.unwrap();
    let u = s.user("旧用户").await.unwrap();
    assert_eq!(u.account.positions[0].symbol, "USDJPY");
    assert_eq!(
        u.account.leverage, 10,
        "legacy leverage migrates to the new ceiling"
    );
    let old: String = sqlx::query_scalar("SELECT token FROM users WHERE name='旧用户'")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(old, "legacy-token-0001");
    assert_eq!(
        s.authenticate(&old).await.unwrap().as_deref(),
        Some("旧用户")
    );
    let new = s.login("旧用户", "兼容密码").await.unwrap();
    assert_ne!(new, old);
    assert!(s.authenticate(&old).await.unwrap().is_some());
    let history = s.history("旧用户").await.unwrap();
    assert_eq!(history[0]["currency"], "LEGACY");
    assert_eq!(history[0]["balance"], 10000.0);
    s.transaction("旧用户", |a, _, _| {
        a.close(None, &[("USDJPY".into(), 151.0)].into(), 5)
    })
    .await
    .unwrap();
    let history = s.history("旧用户").await.unwrap();
    assert_eq!(history[0]["currency"], "USD");
    assert!((history[0]["balance"].as_f64().unwrap() - (10000.0 + 9800.0 / 151.0)).abs() < 1e-7);
    assert_eq!(history[1]["currency"], "LEGACY");
    let account: String = sqlx::query_scalar("SELECT account FROM users WHERE name='旧用户'")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    let account: serde_json::Value = serde_json::from_str(&account).unwrap();
    assert!(account["positions"].as_array().unwrap().is_empty());
    assert!((account["balance"].as_f64().unwrap() - (10000.0 + 9800.0 / 151.0)).abs() < 1e-7);
    s.close().await;
    let s = Store::open_legacy(&path).await.unwrap();
    assert!(s.authenticate(&old).await.unwrap().is_none());
    assert!(s.authenticate(&new).await.unwrap().is_some());
}

#[tokio::test]
async fn cache_tracks_commits_rollbacks_tokens_and_deletion() {
    let (dir, s) = setup().await;
    let tok = s.login("player", "password").await.unwrap();
    assert_eq!(
        s.authenticate(&tok).await.unwrap().as_deref(),
        Some("player")
    );
    s.transaction("player", |a, sym, id| {
        *sym = "FXSIM".into();
        Ok(vec![a.open("long", 0.1, "FXSIM", &quotes(), id, 1)?])
    })
    .await
    .unwrap();
    let cached = s.user("player").await.unwrap();
    assert_eq!(cached.account.positions.len(), 1);
    assert_eq!(cached.symbol, "FXSIM");
    assert_eq!(cached.last_trade.as_ref().unwrap()["kind"], "open");
    // A failing commit must not leak its in-memory mutation into the cache.
    sqlx::raw_sql(
        "CREATE TRIGGER fail_trade BEFORE INSERT ON trades BEGIN SELECT RAISE(ABORT,'x'); END;",
    )
    .execute(&s.pool)
    .await
    .unwrap();
    assert!(
        s.transaction("player", |a, _, _| a.close(None, &quotes(), 2))
            .await
            .is_err()
    );
    assert_eq!(s.user("player").await.unwrap().account, cached.account);
    // An operation error also leaves it untouched.
    assert!(
        s.transaction("player", |_, _, _| Err("nope".into()))
            .await
            .is_err()
    );
    assert_eq!(s.users().await.unwrap()[0].account, cached.account);
    s.close().await;
    // Restart rebuilds the cache from SQLite, matching what was committed.
    let s = Store::open_legacy(&dir.path().join("test.db"))
        .await
        .unwrap();
    assert_eq!(s.user("player").await.unwrap().account, cached.account);
    assert_eq!(
        s.authenticate(&tok).await.unwrap().as_deref(),
        Some("player")
    );
    s.delete("player", "player", 9).await.unwrap();
    assert!(s.user("player").await.is_err());
    assert!(s.users().await.unwrap().is_empty());
    assert_eq!(s.authenticate(&tok).await.unwrap(), None);
}

#[tokio::test]
async fn five_hundred_unchanged_accounts_issue_no_sql_or_transactions() {
    let (dir, s) = setup().await;
    let mut names = vec!["player".to_owned()];
    let mut tx = s.pool.begin().await.unwrap();
    for i in 1..500 {
        let name = format!("player{i}");
        sqlx::query("INSERT INTO users(name,symbol,account,token) VALUES(?,'USDJPY',?,'')")
            .bind(&name)
            .bind(serde_json::to_string(&Account::default()).unwrap())
            .execute(&mut *tx)
            .await
            .unwrap();
        names.push(name);
    }
    tx.commit().await.unwrap();
    s.close().await;
    let s = Store::open_legacy(&dir.path().join("test.db"))
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TRIGGER reject_update BEFORE UPDATE OF account ON users BEGIN SELECT RAISE(ABORT,'unexpected UPDATE'); END;")
        .execute(&s.pool).await.unwrap();
    let before = s.persistence_stats();
    let revision = s.revision();
    // Occupying the only SQLite connection proves there is not even BEGIN/SELECT.
    let conn = s.pool.acquire().await.unwrap();
    for _ in 0..5 {
        tokio::time::timeout(
            Duration::from_millis(500),
            s.transaction_batch(&names, |_, _, _, _| Ok(vec![])),
        )
        .await
        .unwrap()
        .unwrap();
    }
    tokio::time::timeout(
        Duration::from_millis(500),
        s.transaction("player", |_, _, _| Ok(vec![])),
    )
    .await
    .unwrap()
    .unwrap();
    let mut visits = 0;
    tokio::time::timeout(
        Duration::from_millis(500),
        s.transaction_batch(&["player".into(), "player".into()], |_, account, _, _| {
            account.balance += if visits == 0 { 1.0 } else { -1.0 };
            visits += 1;
            Ok(vec![])
        }),
    )
    .await
    .unwrap()
    .unwrap();
    let snapshot = tokio::time::timeout(Duration::from_millis(500), s.leaderboard_snapshot())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.0.len(), 500);
    let after = s.persistence_stats();
    for key in ["transactions", "commits", "account_updates", "batch_rows"] {
        assert_eq!(after[key], before[key], "{key}");
    }
    assert_eq!(
        after["noop_accounts"].as_u64().unwrap() - before["noop_accounts"].as_u64().unwrap(),
        2501
    );
    assert_eq!(s.revision(), revision);
    drop(conn);
    s.close().await;
}

#[tokio::test]
async fn full_account_symbol_and_trade_only_changes_are_not_noops() {
    let (_dir, s) = setup().await;
    let revision = s.revision();
    s.transaction("player", |account, _, _| {
        account.interest_at = 123;
        account.order_seq = 7;
        account.funding_at = 42;
        Ok(vec![])
    })
    .await
    .unwrap();
    s.transaction("player", |_, symbol, _| {
        *symbol = "FXSIM".into();
        Ok(vec![])
    })
    .await
    .unwrap();
    let before = s.persistence_stats();
    s.transaction("player", |account, _, _| {
        Ok(vec![Trade {
            time: 1,
            kind: "audit".into(),
            position_id: 0,
            symbol: String::new(),
            side: String::new(),
            lots: 0.0,
            entry: 0.0,
            price: 0.0,
            pnl: None,
            balance: account.balance,
            margin_level: None,
            currency: "USD".into(),
        }])
    })
    .await
    .unwrap();
    let after = s.persistence_stats();
    assert_eq!(
        after["commits"].as_u64().unwrap(),
        before["commits"].as_u64().unwrap() + 1
    );
    assert_eq!(after["account_updates"], before["account_updates"]);
    let user = s.user("player").await.unwrap();
    assert_eq!(user.symbol, "FXSIM");
    assert_eq!(user.account.interest_at, 123);
    assert_eq!(user.account.funding_at, 42);
    assert_eq!(user.account.order_seq, 7);
    assert_eq!(user.last_trade.unwrap()["kind"], "audit");
    assert_eq!(s.revision(), revision + 3);
    let mut conn = s.pool.acquire().await.unwrap();
    assert_eq!(
        load_user(&mut conn, "player").await.unwrap().account,
        user.account
    );
}

#[tokio::test]
async fn batch_is_one_atomic_commit_and_assigns_unique_ids_in_name_order() {
    let (_dir, s) = setup().await;
    s.register("second", "password").await.unwrap();
    let names = vec!["second".into(), "player".into(), "second".into()];
    let mut seen = Vec::new();
    let before = s.persistence_stats();
    let revision = s.revision();
    s.transaction_batch(&names, |name, account, symbol, id| {
        seen.push((name.to_owned(), id));
        *symbol = "FXSIM".into();
        Ok(vec![account.open(
            "long",
            0.01,
            "FXSIM",
            &quotes(),
            id,
            1,
        )?])
    })
    .await
    .unwrap();
    assert_eq!(
        seen.iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>(),
        names
    );
    assert!(seen.windows(2).all(|pair| pair[0].1 < pair[1].1));
    let after = s.persistence_stats();
    assert_eq!(
        after["commits"].as_u64().unwrap() - before["commits"].as_u64().unwrap(),
        1
    );
    assert_eq!(
        after["transactions"].as_u64().unwrap() - before["transactions"].as_u64().unwrap(),
        1
    );
    assert_eq!(
        after["account_updates"].as_u64().unwrap() - before["account_updates"].as_u64().unwrap(),
        2
    );
    assert_eq!(
        after["batch_rows"].as_u64().unwrap() - before["batch_rows"].as_u64().unwrap(),
        2
    );
    assert_eq!(s.revision(), revision + 1);
    assert_eq!(s.user("second").await.unwrap().account.positions.len(), 2);
    assert_eq!(s.history("second").await.unwrap().len(), 2);
    assert_eq!(s.next_id.load(Ordering::SeqCst), seen.last().unwrap().1 + 1);
}

#[tokio::test]
async fn batch_operation_and_sql_failures_roll_back_every_account_flow_id_and_revision() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(&dir.path().join("batch.db")).await.unwrap();
    s.register("first", "password").await.unwrap();
    s.register("second", "password").await.unwrap();
    let names = vec!["first".into(), "second".into()];
    let users = s.users().await.unwrap();
    let id = s.next_id.load(Ordering::SeqCst);
    let revision = s.revision();
    let before = s.persistence_stats();
    assert!(
        s.transaction_batch(&names, |name, account, _, _| {
            account.balance += 1.0;
            if name == "second" {
                Err("stop batch".into())
            } else {
                Ok(vec![])
            }
        })
        .await
        .is_err()
    );
    assert_eq!(
        s.persistence_stats()["transactions"],
        before["transactions"]
    );
    sqlx::raw_sql("CREATE TRIGGER fail_second BEFORE INSERT ON trades WHEN NEW.name='second' BEGIN SELECT RAISE(ABORT,'batch failure'); END;")
        .execute(&s.pool).await.unwrap();
    assert!(
        s.transaction_batch(&names, |_, account, _, id| {
            Ok(vec![account.open(
                "long",
                1.0,
                "NSTAR",
                &[("NSTAR".into(), 100.0)].into(),
                id,
                1,
            )?])
        })
        .await
        .is_err()
    );
    for before in users {
        assert_eq!(s.user(&before.name).await.unwrap().account, before.account);
        let mut conn = s.pool.acquire().await.unwrap();
        assert_eq!(
            load_user(&mut conn, &before.name).await.unwrap().account,
            before.account
        );
        drop(conn);
        assert!(s.history(&before.name).await.unwrap().is_empty());
    }
    assert_eq!(s.take_nstar_flow(), (0.0, 0.0));
    assert_eq!(s.next_id.load(Ordering::SeqCst), id);
    assert_eq!(s.revision(), revision);
    assert_eq!(s.persistence_stats()["commits"], before["commits"]);
    assert_eq!(
        s.persistence_stats()["failures"].as_u64().unwrap(),
        before["failures"].as_u64().unwrap() + 1
    );
}

async fn wait_for_stat(s: &Store, key: &str, expected: u64) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while s.persistence_stats()[key].as_u64() != Some(expected) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cancelled_enqueued_account_still_commits_and_publishes_before_next_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let s = Arc::new(Store::open(&dir.path().join("cancel.db")).await.unwrap());
    s.register("player", "password").await.unwrap();
    let conn = s.pool.acquire().await.unwrap();
    let before = s.user("player").await.unwrap();
    let revision = s.revision();
    let task = {
        let s = s.clone();
        tokio::spawn(async move {
            s.transaction("player", |a, _, id| {
                Ok(vec![a.open(
                    "long",
                    1.0,
                    "NSTAR",
                    &[("NSTAR".into(), 100.0)].into(),
                    id,
                    1,
                )?])
            })
            .await
        })
    };
    wait_for_stat(&s, "queue_depth", 1).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(s.user("player").await.unwrap().account, before.account);
    assert_eq!(s.revision(), revision);
    assert_eq!(s.take_nstar_flow(), (0.0, 0.0));
    assert!(s.gate.try_lock().is_err());
    drop(conn);
    s.flush_pending().await.unwrap();
    let after = s.user("player").await.unwrap();
    assert_eq!(after.account.positions.len(), 1);
    assert_eq!(s.revision(), revision + 1);
    // Compare persisted JSON, avoiding the default JSON parser's possible one-ULP
    // rounding of an otherwise identical stored floating-point value.
    let raw: String = sqlx::query_scalar("SELECT account FROM users WHERE name='player'")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(raw, serde_json::to_string(&after.account).unwrap());
    let flow = s.take_nstar_flow();
    assert_eq!(flow, (trading::contract("NSTAR"), 0.0));
    s.restore_nstar_flow(flow.0, flow.1);
    assert_eq!(s.take_nstar_flow(), flow);
    let id = s.next_id.load(Ordering::SeqCst);
    s.transaction("player", |a, _, supplied| {
        assert_eq!(supplied, id);
        a.balance += 1.0;
        Ok(vec![])
    })
    .await
    .unwrap();
    let before_restart = s.user("player").await.unwrap();
    s.close().await;
    let reopened = Store::open(&dir.path().join("cancel.db")).await.unwrap();
    let recovered = reopened.user("player").await.unwrap();
    assert_eq!(recovered.account.balance, before_restart.account.balance);
    assert_eq!(recovered.account.next_id, before_restart.account.next_id);
    assert_eq!(recovered.account.positions.len(), 1);
    assert_eq!(
        recovered.account.positions[0].id,
        before_restart.account.positions[0].id
    );
    assert!(
        (recovered.account.positions[0].entry - before_restart.account.positions[0].entry).abs()
            <= 1e-10
    );
    assert_eq!(recovered.last_trade, before_restart.last_trade);
    reopened.close().await;
}

#[tokio::test]
async fn overload_is_bounded_before_gate_and_cancelled_waiters_release_admission() {
    let (_dir, s) = setup().await;
    let s = Arc::new(s);
    let gate = s.gate.lock().await;
    let capacity = s.persistence_stats()["capacity"].as_u64().unwrap();
    let mut tasks = Vec::new();
    for _ in 0..capacity {
        let s = s.clone();
        tasks.push(tokio::spawn(async move {
            s.transaction("player", |_, _, _| Ok(vec![])).await
        }));
    }
    wait_for_stat(&s, "admitted", capacity).await;
    assert!(matches!(
        s.transaction("player", |_, _, _| Ok(vec![])).await,
        Err(Error::Unavailable(_))
    ));
    assert_eq!(s.persistence_stats()["queue_depth"], 0);
    for task in tasks {
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }
    wait_for_stat(&s, "admitted", 0).await;
    drop(gate);
    s.transaction("player", |_, _, _| Ok(vec![])).await.unwrap();
    s.close().await;
}

#[tokio::test]
async fn close_stops_admission_and_drains_cancelled_writer_before_pool_shutdown() {
    let (dir, s) = setup().await;
    let s = Arc::new(s);
    let conn = s.pool.acquire().await.unwrap();
    let task = {
        let s = s.clone();
        tokio::spawn(async move {
            s.transaction("player", |a, _, _| {
                a.balance += 5.0;
                Ok(vec![])
            })
            .await
        })
    };
    wait_for_stat(&s, "queue_depth", 1).await;
    task.abort();
    let _ = task.await;
    let close = {
        let s = s.clone();
        tokio::spawn(async move { s.close().await })
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        while s.persistence_stats()["accepting"] != false {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        s.transaction("player", |_, _, _| Ok(vec![])).await,
        Err(Error::Unavailable(_))
    ));
    assert!(!close.is_finished());
    assert_eq!(s.user("player").await.unwrap().account.balance, 10000.0);
    drop(conn);
    tokio::time::timeout(Duration::from_secs(3), close)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(s.persistence_stats()["queue_depth"], 0);
    assert_eq!(s.persistence_stats()["close_failures"], 0);
    assert_eq!(s.user("player").await.unwrap().account.balance, 10005.0);
    let reopened = Store::open_legacy(&dir.path().join("test.db"))
        .await
        .unwrap();
    assert_eq!(
        reopened.user("player").await.unwrap().account.balance,
        10005.0
    );
    reopened.close().await;
    s.close().await; // idempotent
}

#[tokio::test]
async fn leaderboard_deposit_cache_is_consistent_idempotent_and_rebuilt_on_restart() {
    let (dir, s) = setup().await;
    let revision = s.revision();
    s.deposit("player", 25.25, "snapshot-deposit-01", 1)
        .await
        .unwrap();
    assert_eq!(s.revision(), revision + 1);
    s.deposit("player", 25.25, "snapshot-deposit-01", 2)
        .await
        .unwrap();
    assert_eq!(s.revision(), revision + 1);
    let conn = s.pool.acquire().await.unwrap();
    let (users, deposits) =
        tokio::time::timeout(Duration::from_millis(500), s.leaderboard_snapshot())
            .await
            .unwrap()
            .unwrap();
    assert_eq!(users[0].account.balance, 10025.25);
    assert_eq!(deposits["player"], 25.25);
    assert_eq!(s.deposit_totals().await.unwrap(), deposits);
    drop(conn);
    s.close().await;
    let s = Store::open_legacy(&dir.path().join("test.db"))
        .await
        .unwrap();
    assert_eq!(s.leaderboard_snapshot().await.unwrap().1, deposits);
    s.delete("player", "player", 3).await.unwrap();
    let (users, totals) = s.leaderboard_snapshot().await.unwrap();
    assert!(users.is_empty() && totals.is_empty());
    s.close().await;
}

#[tokio::test]
async fn bar_cleanup_advances_only_after_successful_commit() {
    let (_dir, s) = setup().await;
    let bar = |time: f64, oldest| [(60, [time, 1.0, 1.0, 1.0, 1.0], oldest)];
    s.save_bars("NSTAR", &bar(1.0, 0)).await.unwrap();
    let before = s.persistence_stats();
    s.save_bars("NSTAR", &bar(2.0, 0)).await.unwrap();
    assert_eq!(
        s.persistence_stats()["bar_cleanups"],
        before["bar_cleanups"]
    );
    s.save_bars("NSTAR", &[]).await.unwrap();
    assert_eq!(
        s.persistence_stats()["bar_cleanups"],
        before["bar_cleanups"]
    );
    sqlx::raw_sql("CREATE TRIGGER reject_bar_delete BEFORE DELETE ON sim_bars BEGIN SELECT RAISE(ABORT,'cleanup failure'); END;")
        .execute(&s.pool).await.unwrap();
    assert!(s.save_bars("NSTAR", &bar(3.0, 2)).await.is_err());
    assert_eq!(
        s.persistence_stats()["bar_cleanups"],
        before["bar_cleanups"]
    );
    assert_eq!(s.load_bars("NSTAR", 60).await.unwrap().len(), 2);
    sqlx::query("DROP TRIGGER reject_bar_delete")
        .execute(&s.pool)
        .await
        .unwrap();
    s.save_bars("NSTAR", &bar(3.0, 2)).await.unwrap();
    assert_eq!(
        s.persistence_stats()["bar_cleanups"].as_u64().unwrap(),
        before["bar_cleanups"].as_u64().unwrap() + 1
    );
    let bars = s.load_bars("NSTAR", 60).await.unwrap();
    assert_eq!(
        bars.iter().map(|b| b[0]).collect::<Vec<_>>(),
        vec![2.0, 3.0]
    );
    s.close().await;
}

#[tokio::test]
async fn cancelled_prepared_sql_commit_still_publishes_cached_deposits() {
    let (_dir, s) = setup().await;
    let mutation = s.writer.admit(&s.gate).await.unwrap();
    let mut tx = s.begin().await.unwrap();
    let mut user = s.user("player").await.unwrap();
    user.account.balance += 12.5;
    persist(&mut tx, &user).await.unwrap();
    sqlx::query(
        "INSERT INTO deposits VALUES('cancelled-direct-01','player',1,12.5,10012.5,'test')",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    let revision = s.revision();
    // Poll exactly through submission, then cancel the caller before the writer runs.
    let mut pending = Box::pin(s.commit(
        mutation,
        tx,
        Publication {
            users: vec![user],
            deposits: vec![("player".into(), 12.5)],
            revision: true,
            ..Publication::default()
        },
    ));
    assert!(matches!(
        futures_util::poll!(&mut pending),
        std::task::Poll::Pending
    ));
    assert_eq!(s.persistence_stats()["queue_depth"], 1);
    drop(pending);
    s.flush_pending().await.unwrap();
    let (users, totals) = s.leaderboard_snapshot().await.unwrap();
    assert_eq!(users[0].account.balance, 10012.5);
    assert_eq!(totals["player"], 12.5);
    assert_eq!(s.revision(), revision + 1);
    let total: f64 = sqlx::query_scalar("SELECT SUM(amount) FROM deposits")
        .fetch_one(&s.pool)
        .await
        .unwrap();
    assert_eq!(total, 12.5);
    s.close().await;
}

#[tokio::test]
async fn cancelled_bars_request_is_drained_and_acknowledgement_never_precedes_commit() {
    let (_dir, s) = setup().await;
    let conn = s.pool.acquire().await.unwrap();
    let before = s.persistence_stats();
    {
        let bars = [(60, [60.0, 1.0, 2.0, 0.5, 1.5], 0)];
        let pending = s.save_bars("NSTAR", &bars);
        futures_util::pin_mut!(pending);
        assert!(matches!(
            futures_util::poll!(&mut pending),
            std::task::Poll::Pending
        ));
        assert_eq!(s.persistence_stats()["queue_depth"], 1);
        assert_eq!(s.persistence_stats()["commits"], before["commits"]);
        assert!(s.gate.try_lock().is_err());
    }
    drop(conn);
    s.flush_pending().await.unwrap();
    assert_eq!(
        s.load_bars("NSTAR", 60).await.unwrap(),
        vec![[60.0, 1.0, 2.0, 0.5, 1.5]]
    );
    assert_eq!(
        s.persistence_stats()["commits"].as_u64().unwrap(),
        before["commits"].as_u64().unwrap() + 1
    );
    s.close().await;
}

#[tokio::test]
async fn risk_names_include_legacy_conversion_and_mixed_spot_dependencies() {
    let (_dir, s) = setup().await;
    s.transaction("player", |account, _, id| {
        account.positions.push(Position {
            id,
            symbol: "EURJPY".into(),
            side: "long".into(),
            lots: 0.01,
            entry: 150.0,
            tp: 0.0,
            sl: 0.0,
        });
        account.spot.insert(
            "NSTAR".into(),
            trading::Holding {
                units: 1.0,
                cost: 100.0,
            },
        );
        Ok(vec![])
    })
    .await
    .unwrap();
    assert_eq!(s.risk_user_names("EURJPY"), vec!["player"]);
    assert_eq!(s.risk_user_names("USDJPY"), vec!["player"]);
    assert_eq!(s.risk_user_names("NSTAR"), vec!["player"]);
    assert!(s.risk_user_names("BTCUSDT").is_empty());
    s.close().await;
}

#[tokio::test]
async fn production_full_and_explicit_legacy_normal_durability() {
    let dir = tempfile::tempdir().unwrap();
    for (name, legacy, expected) in [("full.db", false, 2_i64), ("normal.db", true, 1_i64)] {
        let path = dir.path().join(name);
        let s = if legacy {
            Store::open_legacy(&path).await
        } else {
            Store::open(&path).await
        }
        .unwrap();
        let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
            .fetch_one(&s.pool)
            .await
            .unwrap();
        assert_eq!(synchronous, expected);
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&s.pool)
            .await
            .unwrap();
        assert_eq!(mode, "wal");
        s.close().await;
    }
}
