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
fn source_quote_preserves_sides_and_rejects_invalid_or_stale_trading_data() {
    let mut tick = json!({"symbol":"USDCNH","bid":6.70493,"ask":6.70746,"mid":6.706195,"last":0,
        "timestamp":"2026-10-07T09:21:32Z","marketState":"open","stale":false});
    let now = stamp(&tick["timestamp"]).unwrap();
    let q = biquote_quote(&tick, "USDCNH", now).unwrap();
    assert_eq!(q.bid_ask, Some((6.70493, 6.70746)));
    assert!((q.price - 6.706195).abs() < 1e-10);
    assert!(q.fresh(now));
    assert!(!q.fresh(now + 30001));
    assert!(biquote_quote(&tick, "USDCNY", now).is_err());
    tick["ask"] = json!(6.6);
    assert!(biquote_quote(&tick, "USDCNH", now).is_err());
    tick["ask"] = json!(6.70746);
    tick["bid"] = json!(0);
    assert!(biquote_quote(&tick, "USDCNH", now).is_err());
    tick["bid"] = json!(6.70493);
    tick["stale"] = json!(true);
    assert!(!biquote_quote(&tick, "USDCNH", now).unwrap().fresh(now));
    tick["stale"] = json!(false);
    tick["marketState"] = json!("closed");
    assert!(!biquote_quote(&tick, "USDCNH", now).unwrap().fresh(now));
    assert!(biquote_quote(&tick, "USDCNH", now - 6000).is_err());
}

#[test]
fn retry_after_seconds_dates_and_missing_headers_are_respected() {
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
    let mut headers = HeaderMap::new();
    let now = stamp(&json!("2026-10-07T09:21:32Z")).unwrap();
    assert_eq!(retry_delay(&headers, now), 60000);
    for (value, delay) in [
        ("3", 3000),
        ("0", 1000),
        ("-1", 60000),
        ("bad", 60000),
        ("Wed, 07 Oct 2026 09:22:32 GMT", 60000),
    ] {
        headers.insert(RETRY_AFTER, HeaderValue::from_str(value).unwrap());
        assert_eq!(retry_delay(&headers, now), delay);
    }
}

#[tokio::test]
async fn receipt_cache_reuses_frozen_quotes_without_making_stale_quotes_tradable() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("receipt.db")).await.unwrap());
    let mut market = Market::offline(store.clone(), Prices::new()).await.unwrap();
    market.offline = false;
    // Local unresponsive proxy makes any unintended HTTP fetch fail this bounded check.
    let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    market.client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("http://{}", proxy.local_addr().unwrap())).unwrap())
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    let now = now_ms();
    let tick = json!({"symbol":"USDCNH","bid":6.70493,"ask":6.70746,"mid":6.706195,
        "timestamp":chrono::DateTime::from_timestamp_millis(now-10000).unwrap().to_rfc3339(),
        "marketState":"open","stale":false});
    let mut q = biquote_quote(&tick, "USDCNH", now).unwrap();
    market.on_market_quote("USDCNH", q).await.unwrap();
    for _ in 0..5 {
        let hit = tokio::time::timeout(Duration::from_millis(100), market.quote("USDCNH"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(hit.at, now - 10000);
        assert_eq!(hit.received_at, now);
    }
    q.at = now - 31000;
    market.cache.write().await.insert("USDCNH".into(), q);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            market.trading_prices(["USDCNH"])
        )
        .await
        .unwrap()
        .is_err()
    );
    assert_eq!(
        market.snapshot().await["USDCNH"].packet("USDCNH", now)["quote_valid"],
        false
    );
    market
        .biquote_retry_at
        .store(now + 60000, Ordering::Relaxed);
    assert_eq!(
        tokio::time::timeout(
            Duration::from_millis(100),
            market.get("https://biquote.io/api/USDJPY")
        )
        .await
        .unwrap()
        .unwrap_err(),
        "行情源退避中"
    );
    store.close().await;
}

#[test]
fn weekly_bars_use_monday_and_keep_partial_current_week() {
    let day = 86_400_000.0;
    let rows = vec![
        [4.0 * day, 6.7, 6.8, 6.6, 6.75],
        [10.0 * day, 6.75, 6.9, 6.5, 6.76],
        [11.0 * day, 6.76, 6.85, 6.7, 6.8],
    ];
    assert_eq!(
        weekly_bars(rows.clone(), 2),
        vec![[4.0 * day, 6.7, 6.9, 6.5, 6.76], rows[2]]
    );
    assert_eq!(
        weekly_bars(rows, 1),
        vec![[11.0 * day, 6.76, 6.85, 6.7, 6.8]]
    );
    assert!(weekly_bars(vec![], 2).is_empty());
}

#[tokio::test]
async fn live_fx_cache_cannot_be_overwritten_by_cny_or_single_price_feed() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("quotes.db")).await.unwrap());
    let mut market = Market::offline(store.clone(), Prices::new()).await.unwrap();
    market.offline = false;
    let at = now_ms();
    let tick = json!({"symbol":"USDCNH","bid":6.70493,"ask":6.70746,"mid":6.706195,
        "timestamp":chrono::DateTime::from_timestamp_millis(at).unwrap().to_rfc3339(),
        "marketState":"open","stale":false});
    let q = biquote_quote(&tick, "USDCNH", at).unwrap();
    market.on_market_quote("USDCNH", q).await.unwrap();
    assert!(market.on_quote("USDCNH", 6.693300, at + 1).await.is_err());
    assert_eq!(market.snapshot().await["USDCNH"].price, 6.706195);
    let prices = market.trading_prices(["USDCNH"]).await.unwrap();
    assert_eq!(prices.bid_ask["USDCNH"], (6.70493, 6.70746));
    let packet = q.packet("USDCNH", at);
    assert_eq!(packet["price"], 6.706195);
    assert_eq!(packet["bid"], 6.70493);
    assert_eq!(packet["ask"], 6.70746);
    assert_eq!(packet["quote_at"], at);
    assert_eq!(packet["quote_valid"], true);
    assert_eq!(q.packet("USDCNH", at + 30001)["quote_valid"], false);
    let mut closed = q;
    closed.open = false;
    market.on_market_quote("USDCNH", closed).await.unwrap();
    assert!(market.trading_prices(["USDCNH"]).await.is_err());
    assert_eq!(market.prices(["USDCNH"]).await.unwrap()["USDCNH"], 6.706195);
    assert_eq!(symbols::yahoo("USDCNH"), None);
    assert_eq!(symbols::yahoo("EURCNH"), None);
    assert_eq!(symbols::provider("USDCNH"), Some("biquote.io"));
    store.close().await;
}

#[test]
fn recorded_ticks_preserve_wicks_freeze_closed_candles_and_bound_retention() {
    for (_, sec) in symbols::INTERVALS {
        let t = 1800000000 / sec * sec;
        let mut sim = SimFeed::new("FXSIM", 1.1, t * 1000);
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
    let mut sim = SimFeed::new("FXSIM", 1.1, t);
    sim.record(0.9, t + 200);
    sim.record(1.09, t + 400);
    store.save_bars("FXSIM", &sim.pending()).await.unwrap();
    sim.record(1.11, t + 60000);
    store.save_bars("FXSIM", &sim.pending()).await.unwrap();
    let old = sim.klines(60, 1000);
    store.close().await;
    let store = Store::open(&path).await.unwrap();
    let restored = SimFeed::load(&store, "FXSIM", 1.1, t + 600000)
        .await
        .unwrap();
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
                tp: 0.0,
                sl: 0.0,
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
    let now = (now_ms() + 60000) / 1000 * 1000;
    // The first tick into a new candle carries the closed one too.
    let rolled = market.record_sim(1.12, now).await.unwrap();
    assert_eq!(rolled["klines"]["1s"].as_array().unwrap().len(), 2);
    market.record_sim(0.8, now + 100).await.unwrap();
    let packet = market.record_sim(1.1, now + 200).await.unwrap();
    for (interval, _) in symbols::INTERVALS {
        let rest = market.klines("FXSIM", interval, 2).await.unwrap();
        // Later ticks inside the candle send only the forming one.
        let live = packet["klines"][interval].as_array().unwrap();
        assert_eq!(live.len(), 1, "{interval}");
        assert_eq!(live[0], *rest["klines"].as_array().unwrap().last().unwrap());
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

fn nstar_run(sim: &mut SimFeed, t: i64, from: i64, to: i64) {
    for step in from..to {
        let price = sim.next_price();
        sim.record(price, t + step * 200);
    }
}

#[test]
fn nstar_flow_moves_price_by_depth_then_mostly_reverts() {
    let t = 1800000000000;
    let mut sim = SimFeed::new("NSTAR", 100.0, t);
    // 5% of depth (2,500 NSTAR, ~$250k): +5.1% on a flat book, ~+4.6% on the AMM curve.
    sim.flow(0.05 * crate::trading::depth("NSTAR"), 0.0);
    let moved = sim.book.log.exp();
    assert!((moved / 100.0 - 1.0 - 0.0461).abs() < 1e-3, "{moved}");
    assert!(sim.book.events.iter().any(|e| e["kind"] == "whale"));
    // Within ~5 minutes most of the push is gone; only conviction (20%) and noise remain.
    let mut errs = Vec::new();
    for _ in 0..40 {
        let mut s = sim.clone();
        nstar_run(&mut s, t, 1, 1500);
        errs.push(s.book.log - s.book.fair);
    }
    let mean = errs.iter().sum::<f64>() / errs.len() as f64;
    assert!(mean.abs() < 0.02, "price should rejoin fair value: {mean}");
}

#[test]
fn nstar_anchor_trails_slowly_and_big_flow_never_halts() {
    let t = 1800000000000;
    let mut sim = SimFeed::new("NSTAR", 100.0, t);
    let start = sim.book.anchor;
    // 40,000 NSTAR at once: ~+25% on the AMM curve (flat book: +122%).
    sim.flow(40_000.0, 0.0);
    let lifted = sim.book.log - start;
    assert!(lifted < 0.23, "{lifted}");
    assert_eq!(sim.book.anchor, start, "flow never drags the anchor");
    // No circuit breaker: the next tick publishes the moved price and trading stays open.
    nstar_run(&mut sim, t, 1, 2);
    assert!(sim.price > 115.0, "{}", sim.price);
    assert!(sim.quote().open);
    assert!(sim.quote().anchor.is_some());
    assert!(!sim.book.events.iter().any(|e| e["kind"] == "halt"));
    // The anchor re-centres in minutes, not ticks.
    let after_tick = sim.book.anchor - start;
    assert!(after_tick > 0.0 && after_tick < 0.001, "{after_tick}");
    let mut s = sim.clone();
    nstar_run(&mut s, t, 2, 1500);
    let caught = (s.book.anchor - start) / (sim.book.log - start);
    assert!(caught > 0.05 && caught < 0.8, "{caught}");
}

#[test]
fn nstar_price_has_no_hard_ceiling_or_floor_and_stays_finite() {
    let t = 1800000000000;
    let mut sim = SimFeed::new("NSTAR", 100.0, t);
    let depth = crate::trading::depth("NSTAR");
    // The old clamps stopped everything at 800 / 12.5; now only the curve pushes back,
    // and hard: 2.5M NSTAR of buying (that pinned 800 before) only reaches ~266.
    for _ in 0..50 {
        sim.flow(depth, 0.0);
    }
    let lifted = sim.book.log.exp();
    assert!(lifted > 200.0 && lifted < 320.0, "{lifted}");
    let mut bought = 50.0 * depth;
    while sim.book.log.exp() <= 800.0 && bought < 1e9 {
        sim.flow(10.0 * depth, 0.0);
        bought += 10.0 * depth;
    }
    let high = sim.book.log.exp();
    assert!(high.is_finite() && high > 800.0, "{high}");
    assert!(bought > 2e7, "passing 800 took only {bought} NSTAR");
    while sim.book.log.exp() >= 12.5 {
        sim.flow(-10.0 * depth, 0.0);
    }
    let low = sim.book.log.exp();
    assert!(low.is_finite() && low > 0.0, "{low}");
    // Left alone, far-off fair value is pulled home hard by the soft band: from 400 it is
    // ~120 within an hour (a plain 1 h half-life would leave ~200), and price follows.
    let mut s = SimFeed::new("NSTAR", 100.0, t);
    s.book.fair = 400.0f64.ln();
    s.book.log = s.book.fair;
    s.book.anchor = s.book.fair;
    nstar_run(&mut s, t, 1, 18_000);
    let fair = s.book.fair.exp();
    assert!(fair > 90.0 && fair < 165.0, "{fair}");
    let price = s.book.log.exp();
    assert!(price > 80.0 && price < 200.0, "{price}");
}

#[test]
fn nstar_day_stays_in_a_sane_range_with_news_and_rumors() {
    let t = 1800000000000;
    let mut sim = SimFeed::new("NSTAR", 100.0, t);
    let (mut lo, mut hi) = (f64::MAX, f64::MIN);
    let mut kinds = std::collections::BTreeMap::<String, usize>::new();
    // Two simulated hours.
    for step in 1..36_000 {
        let price = sim.next_price();
        sim.record(price, t + step * 200);
        lo = lo.min(price);
        hi = hi.max(price);
        for e in sim.book.events.drain(..) {
            *kinds.entry(e["kind"].as_str().unwrap().into()).or_default() += 1;
        }
    }
    assert!(lo > 25.0 && hi < 400.0, "range {lo}..{hi}");
    assert!(kinds.get("news").copied().unwrap_or(0) >= 3, "{kinds:?}");
    assert_eq!(kinds.get("news"), kinds.get("rumor"), "{kinds:?}");
}

#[test]
fn fxsim_keeps_its_original_walk() {
    let t = 1800000000000;
    let mut sim = SimFeed::new("FXSIM", 1.1, t);
    for step in 1..3000 {
        let price = sim.next_price();
        sim.record(price, t + step * 200);
    }
    assert!(sim.price > 0.55 && sim.price < 2.0, "{}", sim.price);
    assert!(sim.book.events.is_empty());
}
