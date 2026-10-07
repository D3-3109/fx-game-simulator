use super::*;
use crate::trading::{Position, Prices};
use std::sync::Arc;

async fn setup() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(&dir.path().join("test.db")).await.unwrap();
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
    let s = Store::open(&dir.path().join("test.db")).await.unwrap();
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
async fn python_schema_accounts_tokens_and_legacy_currency_survive_both_directions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("compat.db");
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = r#"
import sys,sqlite3,json
import fx_sim_web as w
w.DB_FILE=sys.argv[1]
token=w.register_user('旧用户','兼容密码')
a=w.user_state('旧用户')['account']
a.open('long',.1,{'USDJPY':150},symbol='USDJPY')
w.persist_user('旧用户')
db=sqlite3.connect(w.DB_FILE)
db.execute('ALTER TABLE trades DROP COLUMN currency');db.commit();db.close()
"#;
    assert!(
        std::process::Command::new("python3")
            .args(["-B", "-c", script, path.to_str().unwrap()])
            .current_dir(root)
            .output()
            .unwrap()
            .status
            .success()
    );
    let s = Store::open(&path).await.unwrap();
    let u = s.user("旧用户").await.unwrap();
    assert_eq!(u.account.positions[0].symbol, "USDJPY");
    let old: String = sqlx::query_scalar("SELECT token FROM users WHERE name='旧用户'")
        .fetch_one(&s.pool)
        .await
        .unwrap();
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
    s.close().await;
    let check = r#"
import sys,math
import fx_sim_web as w
w.DB_FILE=sys.argv[1];w.load_users()
a=w.user_state('旧用户')['account']
assert not a.positions
assert math.isclose(a.balance,10000+9800/151)
h=w.trade_history('旧用户');assert len(h)==2 and h[0]['currency']=='USD' and h[1]['currency']=='LEGACY'
assert w.user_by_token(sys.argv[2])=='旧用户'
"#;
    let result = std::process::Command::new("python3")
        .args(["-B", "-c", check, path.to_str().unwrap(), &new])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let s = Store::open(&path).await.unwrap();
    assert!(s.authenticate(&old).await.unwrap().is_none());
    assert!(s.authenticate(&new).await.unwrap().is_some());
}
