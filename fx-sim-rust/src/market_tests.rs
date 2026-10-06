use super::*;
use crate::trading::Position;

#[test]
fn biquote_latest_is_order_independent_and_active_close_changes_intraminute() {
    let old = json!({"openTime":"2026-10-06T19:29:00Z","open":150.0,"high":150.1,"low":149.9,"close":150.02,"isOpen":false});
    let mut live = json!({"openTime":"2026-10-06T19:30:00Z","open":150.02,"high":150.3,"low":150.0,"close":150.24,"isOpen":true});
    assert_eq!(latest_biquote(&json!({"bars":[live,old]})).unwrap(), 150.24);
    assert_eq!(latest_biquote(&json!({"bars":[old,live]})).unwrap(), 150.24);
    live["close"] = json!(150.28);
    assert_eq!(latest_biquote(&json!({"bars":[live,old]})).unwrap(), 150.28);
    let bars = biquote_bars(&json!({"bars":[live,old]})).unwrap();
    assert!(bars[0][0] < bars[1][0]);
    assert_eq!(bars[1][4], 150.28);
    assert!(latest_biquote(&json!({"bars":[]})).is_err());
    assert!(latest_biquote(&json!({"bars":[{"openTime":"bad","close":1.0}]})).is_err());
    assert!(
        latest_biquote(&json!({"bars":[{"openTime":"2026-10-06T19:30:00Z","close":0.0}]})).is_err()
    );
}

#[test]
fn recorded_ticks_preserve_wicks_freeze_closed_candles_and_bound_retention() {
    for (_, sec) in symbols::INTERVALS {
        let t = 1800000000 / sec * sec;
        let mut sim = SimFeed::new(t * 1000);
        assert_eq!(sim.klines(*sec, 500).len(), 1, "invented history");
        sim.record(0.9, t * 1000 + 200);
        sim.record(1.09, t * 1000 + 400);
        let expected = [(t * 1000) as f64, 1.1, 1.1, 0.9, 1.09];
        assert_eq!(sim.klines(*sec, 500), vec![expected]);
        sim.record(1.11, (t + sec) * 1000);
        sim.record(1.2, (t + sec) * 1000 + 200);
        assert_eq!(sim.klines(*sec, 500)[0], expected);
        sim.record(5.0, t * 1000);
        assert_eq!(
            sim.klines(*sec, 500)[0],
            expected,
            "backwards clock rewrote closed candle"
        );
        for i in 2..1005 {
            sim.record(1.1, (t + sec * i) * 1000);
        }
        assert_eq!(sim.klines(*sec, 2000).len(), 1000);
        assert_eq!(sim.klines(*sec, 2).len(), 2);
    }
}

#[tokio::test]
async fn sqlite_sim_restart_retains_closed_extremes_and_does_not_fill_gap() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bars.db");
    let store = Store::open(&path).await.unwrap();
    let t = 1800000000000;
    let mut sim = SimFeed::new(t);
    sim.record(0.9, t + 200);
    sim.record(1.09, t + 400);
    store.save_bars(&sim.pending()).await.unwrap();
    sim.record(1.11, t + 60000);
    store.save_bars(&sim.pending()).await.unwrap();
    let old = sim.klines(60, 1000);
    store.close().await;
    let store = Store::open(&path).await.unwrap();
    let restored = SimFeed::load(&store, t + 600000).await.unwrap();
    let bars = restored.klines(60, 1000);
    assert_eq!(bars.len(), 3);
    assert_eq!(bars[..2], old);
    assert_eq!(bars[2][0], (t + 600000) as f64);
    assert_eq!(bars[0][3], 0.9);
}

#[tokio::test]
async fn stale_missing_and_out_of_order_quotes_never_misliquidate() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("risk.db")).await.unwrap());
    store.register("player", "pw").await.unwrap();
    let market = Market::offline(store.clone(), Prices::new()).await.unwrap();
    store
        .transaction("player", |a, _, _| {
            a.balance = 1.0;
            a.positions = vec![Position {
                id: 1,
                side: "long".into(),
                lots: 0.1,
                entry: 165.0,
                symbol: "EURJPY".into(),
            }];
            Ok(vec![])
        })
        .await
        .unwrap();
    let now = now_ms();
    market.on_quote("USDJPY", 150.0, now - 31000).await.unwrap();
    market.on_quote("EURJPY", 164.0, now).await.unwrap();
    assert_eq!(
        store.user("player").await.unwrap().account.positions.len(),
        1,
        "used stale conversion"
    );
    assert!(!market.on_quote("EURJPY", 100.0, now - 1000).await.unwrap());
    assert_eq!(market.snapshot().await["EURJPY"].price, 164.0);
    assert!(market.on_quote("EURJPY", f64::NAN, now + 1).await.is_err());
    assert_eq!(store.history("player").await.unwrap().len(), 0);
    market.on_quote("USDJPY", 150.0, now).await.unwrap();
    assert!(
        store
            .user("player")
            .await
            .unwrap()
            .account
            .positions
            .is_empty()
    );
    assert_eq!(store.history("player").await.unwrap().len(), 1);
}

#[tokio::test]
async fn sim_rest_and_ws_share_authoritative_ohlc() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("sim.db")).await.unwrap());
    let market = Market::offline(store, Prices::new()).await.unwrap();
    let now = now_ms() + 60000;
    market.record_sim(1.12, now).await.unwrap();
    market.record_sim(0.8, now + 100).await.unwrap();
    let packet = market.record_sim(1.1, now + 200).await.unwrap();
    for (interval, _) in symbols::INTERVALS {
        let rest = market.klines("FXSIM", interval, 2).await.unwrap();
        assert_eq!(packet["klines"][interval], rest["klines"]);
        assert_eq!(packet["time"], rest["updated_at"]);
        let last = rest["klines"].as_array().unwrap().last().unwrap();
        assert_eq!(last[3], 0.8);
        assert_eq!(last[2], 1.12);
    }
}

#[test]
fn combined_upstream_subscription_is_shared_sorted_and_filters_fx() {
    assert_eq!(
        crate::runtime::binance_stream(vec!["USDJPY".into(), "FXSIM".into()]),
        None
    );
    assert_eq!(
        crate::runtime::binance_stream(vec!["ETHUSDT".into(), "BTCUSDT".into(), "ETHUSDT".into()]),
        Some(
            "wss://data-stream.binance.vision/stream?streams=btcusdt@miniTicker/ethusdt@miniTicker"
                .into()
        )
    );
}
