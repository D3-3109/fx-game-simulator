use super::*;
use crate::trading::{Account, Position};

#[tokio::test]
async fn nstar_only_market_has_no_external_client_or_legacy_feed() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("nstar.db")).await.unwrap());
    let market = Market::new(store.clone()).await.unwrap();
    assert!(
        market.client.is_none(),
        "production NSTAR must not build a proxy/client"
    );
    assert!(market.sim.is_none(), "production NSTAR must not load FXSIM");
    assert!(!market.is_offline());
    assert!(market.price(symbols::NSTAR).await.unwrap() > 0.0);
    assert_eq!(market.tick_nstar().await.unwrap().0["sym"], symbols::NSTAR);
    assert!(
        !market.klines(symbols::NSTAR, "1s", 5).await.unwrap()["klines"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(market.tick_sim().await.is_err());
    assert!(market.record_sim(1.1, now_ms()).await.is_err());
    assert!(store.load_bars(symbols::SIM, 60).await.unwrap().is_empty());
    assert_eq!(
        market
            .snapshot()
            .await
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec![symbols::NSTAR]
    );
    store.close().await;
}

#[tokio::test]
async fn nstar_only_blocks_cached_quotes_history_and_direct_upstreams_without_egress() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        Store::open(&dir.path().join("nstar-guard.db"))
            .await
            .unwrap(),
    );
    let mut market = Market::offline(
        store.clone(),
        [
            (symbols::NSTAR.into(), 100.0),
            (symbols::SIM.into(), 1.1),
            ("USDJPY".into(), 150.0),
            ("AAPL".into(), 200.0),
            ("BTCUSDT".into(), 60000.0),
        ]
        .into(),
    )
    .await
    .unwrap();
    assert!(market.client.is_none());
    assert_eq!(
        market.snapshot().await.len(),
        1,
        "initial legacy cache leaked"
    );
    // Inject a real client and legacy cache entries to exercise guards, not just None/offline.
    market.offline = false;
    let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    market.client = Some(
        reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(format!("http://{}", proxy.local_addr().unwrap())).unwrap())
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap(),
    );
    for symbol in [symbols::SIM, "USDJPY", "AAPL", "BTCUSDT", "UNKNOWN"] {
        market
            .cache
            .write()
            .unwrap()
            .insert(symbol.into(), Quote::single(100.0, now_ms()));
        assert!(market.price(symbol).await.is_err(), "{symbol}");
        assert!(market.quote(symbol).await.is_err(), "{symbol}");
        assert!(market.prices([symbol]).await.is_err(), "{symbol}");
        assert!(market.trading_prices([symbol]).await.is_err(), "{symbol}");
        assert!(market.klines(symbol, "1m", 1).await.is_err(), "{symbol}");
        assert!(
            market.on_quote(symbol, 101.0, now_ms()).await.is_err(),
            "{symbol}"
        );
        assert!(
            market
                .on_market_quote(symbol, Quote::single(101.0, now_ms()))
                .await
                .is_err(),
            "{symbol}"
        );
        assert_eq!(market.cache.read().unwrap()[symbol].price, 100.0);
    }
    let blocked = "NSTAR-only 已禁用外部行情";
    assert_eq!(market.biquote_price("USDJPY").await.unwrap_err(), blocked);
    assert_eq!(market.biquote_quote("USDJPY").await.unwrap_err(), blocked);
    assert_eq!(
        market.biquote_quotes(&["USDJPY".into()]).await.unwrap_err(),
        blocked
    );
    assert_eq!(market.biquote_quotes(&[]).await.unwrap_err(), blocked);
    assert_eq!(market.yahoo_price("AAPL").await.unwrap_err(), blocked);
    assert_eq!(market.fetch_price("BTCUSDT").await.unwrap_err(), blocked);
    assert_eq!(
        market
            .get("https://biquote.io/api/USDJPY")
            .await
            .unwrap_err(),
        blocked
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), proxy.accept())
            .await
            .is_err(),
        "a blocked entry point opened a network connection"
    );
    store.close().await;
}

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
    let store = Arc::new(
        Store::open_legacy(&dir.path().join("receipt.db"))
            .await
            .unwrap(),
    );
    let mut market = Market::offline(store.clone(), Prices::new()).await.unwrap();
    market.offline = false;
    // Local unresponsive proxy makes any unintended HTTP fetch fail this bounded check.
    let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    market.client = Some(
        reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(format!("http://{}", proxy.local_addr().unwrap())).unwrap())
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap(),
    );
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
    market.cache.write().unwrap().insert("USDCNH".into(), q);
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
    let store = Arc::new(
        Store::open_legacy(&dir.path().join("quotes.db"))
            .await
            .unwrap(),
    );
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
    let store = Store::open_legacy(&path).await.unwrap();
    let t = 1800000000000;
    let mut sim = SimFeed::new("FXSIM", 1.1, t);
    sim.record(0.9, t + 200);
    sim.record(1.09, t + 400);
    store.save_bars("FXSIM", &sim.pending()).await.unwrap();
    sim.record(1.11, t + 60000);
    store.save_bars("FXSIM", &sim.pending()).await.unwrap();
    let old = sim.klines(60, 1000);
    store.close().await;
    let store = Store::open_legacy(&path).await.unwrap();
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
    let store = Arc::new(
        Store::open_legacy(&dir.path().join("risk.db"))
            .await
            .unwrap(),
    );
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
    let store = Arc::new(
        Store::open_legacy(&dir.path().join("sim.db"))
            .await
            .unwrap(),
    );
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
fn nstar_run_seeded(sim: &mut SimFeed, seed: u64, ticks: usize) {
    use rand::{SeedableRng, rngs::StdRng};
    let mut rng = StdRng::seed_from_u64(seed);
    for _ in 0..ticks {
        sim.next_book_price_with(&mut rng);
    }
}

#[test]
fn nstar_regimes_create_persistent_trends_and_local_ranges() {
    let t = 1800000000000;
    let base = SimFeed::new("NSTAR", 100.0, t);
    let mut bull = base.clone();
    bull.kind = 4;
    bull.left = 10_000;
    bull.trend = 0.000014;
    let mut bear = base.clone();
    bear.kind = 3;
    bear.left = 10_000;
    bear.trend = -0.000014;
    let mut range = base;
    range.kind = 0;
    range.left = 10_000;
    range.trend = 0.0;
    range.range_center = range.book.fair;
    for sim in [&mut bull, &mut bear, &mut range] {
        sim.weekly_left = 100_000;
        sim.day_left = 100_000;
        sim.pulse_left = 100_000;
    }
    bull.book.news_in = 100_000;
    bear.book.news_in = 100_000;
    range.book.news_in = 100_000;
    nstar_run_seeded(&mut bull, 7, 1_500);
    nstar_run_seeded(&mut bear, 7, 1_500);
    nstar_run_seeded(&mut range, 7, 1_500);
    let up = bull.book.log - 100.0f64.ln();
    let down = bear.book.log - 100.0f64.ln();
    let sideways = range.book.log - 100.0f64.ln();
    assert!(up > 0.005, "five-minute bull move: {up}");
    assert!(down < -0.005, "five-minute bear move: {down}");
    assert!(
        sideways.abs() < 0.02,
        "range did not stay local: {sideways}"
    );
    assert!((range.book.fair - range.range_center).abs() < 0.02);
}

#[test]
fn nstar_regime_durations_are_minutes_not_seconds() {
    use rand::{SeedableRng, rngs::StdRng};
    let t = 1800000000000;
    let mut rng = StdRng::seed_from_u64(19);
    for _ in 0..100 {
        let mut sim = SimFeed::new("NSTAR", 100.0, t);
        sim.book_regime(&mut rng);
        let expected = match sim.kind {
            0 => RANGE_TICKS,
            1 => VOLATILE_TICKS,
            3 | 4 => TREND_TICKS,
            other => panic!("invalid regime {other}"),
        };
        assert!(expected.contains(&(sim.left + 1)));
        if matches!(sim.kind, 3 | 4) {
            assert!(sim.trend.abs() >= *TREND_DRIFT.start());
        }
    }
}
#[test]
fn nstar_weekly_driver_moves_the_long_term_benchmark() {
    let t = 1800000000000;
    let mut sim = SimFeed::new("NSTAR", 100.0, t);
    sim.kind = 0;
    sim.left = 500_000;
    sim.weekly_trend = 0.000_000_05;
    sim.weekly_left = 500_000;
    sim.day_left = 500_000;
    sim.pulse_left = 500_000;
    sim.book.news_in = 1_000_000;
    let base = sim.book.base;
    nstar_run_seeded(&mut sim, 41, 432_000);
    let weekly_move = sim.book.base - base;
    assert!((weekly_move - 0.0216).abs() < 1e-9, "{weekly_move}");
    assert!(
        sim.book.fair.exp() > 100.0,
        "secular trend was pulled back to $100"
    );
}

#[test]
fn nstar_five_minute_burst_is_strong_but_not_deterministic() {
    let t = 1800000000000;
    let mut base = SimFeed::new("NSTAR", 100.0, t);
    base.kind = 0;
    base.left = 2_000;
    base.weekly_left = 2_000;
    base.day_left = 2_000;
    base.pulse_trend = 0.00005;
    base.pulse_left = 2_000;
    base.book.news_in = 100_000;
    let mut returns = (0..64)
        .map(|seed| {
            let mut sim = base.clone();
            let start = sim.book.log;
            nstar_run_seeded(&mut sim, 100 + seed, 1_500);
            sim.book.log - start
        })
        .collect::<Vec<_>>();
    returns.sort_by(f64::total_cmp);
    let percentile = |p: usize| returns[p * (returns.len() - 1) / 100];
    println!(
        "5-minute bullish burst p10/median/p90: {:.3}% / {:.3}% / {:.3}%",
        percentile(10) * 100.0,
        percentile(50) * 100.0,
        percentile(90) * 100.0
    );
    assert!(percentile(50) > 0.01, "median={}", percentile(50));
    assert!(percentile(90) > 0.04, "p90={}", percentile(90));
    assert!(
        percentile(10) < percentile(50),
        "all paths became identical"
    );
}

#[test]
fn nstar_mixed_market_has_quiet_paths_and_fast_five_minute_tails() {
    let t = 1800000000000;
    let base = SimFeed::new("NSTAR", 100.0, t);
    let mut returns = (0..256)
        .map(|seed| {
            let mut sim = base.clone();
            nstar_run_seeded(&mut sim, 1_000 + seed, 1_500);
            sim.book.log - 100.0f64.ln()
        })
        .collect::<Vec<_>>();
    returns.sort_by(f64::total_cmp);
    let signed = |p: usize| returns[p * (returns.len() - 1) / 100];
    let mut absolute = returns.iter().map(|r| r.abs()).collect::<Vec<_>>();
    absolute.sort_by(f64::total_cmp);
    let abs = |p: usize| absolute[p * (absolute.len() - 1) / 100];
    let over_five = absolute.iter().filter(|r| **r >= 0.05).count() as f64 / absolute.len() as f64;
    println!(
        "mixed 5-minute signed p10/median/p90: {:.2}% / {:.2}% / {:.2}%",
        signed(10) * 100.0,
        signed(50) * 100.0,
        signed(90) * 100.0
    );
    println!(
        "mixed 5-minute |return| median/p90/p99, >=5%: {:.2}% / {:.2}% / {:.2}%, {:.1}%",
        abs(50) * 100.0,
        abs(90) * 100.0,
        abs(99) * 100.0,
        over_five * 100.0
    );
    assert!(abs(50) < 0.05, "every period became a burst: {}", abs(50));
    assert!(abs(90) > 0.015, "no fast tails: {}", abs(90));
    assert!(
        over_five > 0.0 && over_five < 0.8,
        "liquidation-sized moves: {over_five}"
    );
}

#[test]
fn nstar_five_minute_adverse_burst_can_liquidate_10x_but_not_1x() {
    let t = 1800000000000;
    let initial: Prices = [("NSTAR".to_string(), 100.0)].into();
    let open = |leverage| {
        let mut account = Account {
            balance: 1_000.0,
            leverage,
            ..Account::default()
        };
        let lots = account.max_open_lots("long", "NSTAR", &initial);
        account.open("long", lots, "NSTAR", &initial, 1, 0).unwrap();
        account
    };
    let mut leveraged = open(10);
    let mut unleveraged = open(1);
    let mut sim = SimFeed::new("NSTAR", 100.0, t);
    sim.kind = 0;
    sim.left = 2_000;
    sim.weekly_left = 2_000;
    sim.day_left = 2_000;
    sim.pulse_trend = -0.00005;
    sim.pulse_left = 2_000;
    sim.book.news_in = 100_000;
    nstar_run_seeded(&mut sim, 57, 1_500);
    let mut live: Prices = [("NSTAR".to_string(), sim.book.log.exp())].into();
    live.anchor.insert("NSTAR".into(), sim.book.anchor);
    live.mark.insert("NSTAR".into(), sim.book.mark.exp());
    assert!(
        sim.book.log.exp() < 95.0,
        "scenario requires a >5% adverse move"
    );
    assert!(
        !leveraged
            .check_stopout_at(&live.marked(), &live, 1)
            .unwrap()
            .is_empty()
    );
    assert!(
        unleveraged
            .check_stopout_at(&live.marked(), &live, 1)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn nstar_forced_flow_cannot_rewrite_fair_value_or_jump_mark() {
    let t = 1800000000000;
    let mut sim = SimFeed::new("NSTAR", 100.0, t);
    let fair = sim.book.fair;
    sim.book.flow(-1_000.0, 50_000.0, 0.08, true);
    assert_eq!(sim.book.fair, fair);
    sim.book.flow(1_000.0, 50_000.0, 0.08, false);
    assert!(sim.book.fair > fair, "voluntary conviction must persist");

    let mut sim = SimFeed::new("NSTAR", 100.0, t);
    sim.flow(40_000.0, 0.0);
    nstar_run_seeded(&mut sim, 23, 300);
    let premium = sim.book.mark - sim.book.fair;
    assert!(premium <= MARK_MAX_PREMIUM + 1e-12, "{premium}");
    assert!(
        sim.book.log - sim.book.mark > 0.03,
        "mark followed the raid"
    );
}
#[test]
fn nstar_whale_wick_does_not_cascade_an_otherwise_solvent_account() {
    let t = 1800000000000;
    let initial: Prices = [("NSTAR".to_string(), 100.0)].into();
    let mut victim = Account {
        balance: 2_000.0,
        ..Account::default()
    };
    victim
        .open("short", 10_000.0, "NSTAR", &initial, 1, 0)
        .unwrap();
    let mut sim = SimFeed::new("NSTAR", 100.0, t);
    sim.flow(40_000.0, 0.0);
    nstar_run_seeded(&mut sim, 31, 5);
    let mut live: Prices = [("NSTAR".to_string(), sim.book.log.exp())].into();
    live.anchor.insert("NSTAR".into(), sim.book.anchor);
    live.mark.insert("NSTAR".into(), sim.book.mark.exp());
    assert!(live["NSTAR"] > 115.0, "test requires a real book spike");
    assert!(
        !victim.clone().check_stopout(&live, 1).unwrap().is_empty(),
        "raw price would liquidate"
    );
    assert!(
        victim
            .check_stopout_at(&live.marked(), &live, 1)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        victim.positions.len(),
        1,
        "fair-anchored mark prevented the cascade"
    );
}

#[test]
fn nstar_large_liquidation_queue_has_a_release_cap() {
    let t = 1800000000000;
    let mut sim = SimFeed::new("NSTAR", 100.0, t);
    sim.kind = 0;
    sim.left = 10_000;
    sim.flow(0.0, -60_000.0);
    let mut control = SimFeed::new("NSTAR", 100.0, t);
    control.kind = 0;
    control.left = 10_000;
    nstar_run_seeded(&mut sim, 29, 1);
    nstar_run_seeded(&mut control, 29, 1);
    assert!((sim.book.backlog + 60_000.0 - LIQ_RELEASE_CAP).abs() < 1e-9);
    assert_eq!(sim.book.fair, control.book.fair, "forced flow moved fair");
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
    // Within ~5 minutes price rejoins fair; 35% of player impact became conviction.
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
fn nstar_mark_lags_spikes_and_liquidations_unwind_gradually() {
    let t = 1800000000000;
    let mut sim = SimFeed::new("NSTAR", 100.0, t);
    // A +25% spike that is reversed within a second barely moves the mark.
    sim.flow(40_000.0, 0.0);
    nstar_run(&mut sim, t, 1, 5);
    sim.flow(-40_000.0, 0.0);
    nstar_run(&mut sim, t, 5, 6);
    let mark = sim.book.mark.exp();
    assert!((mark / 100.0 - 1.0).abs() < 0.01, "{mark}");
    assert!(sim.quote().mark.is_some());
    // Liquidated units are queued, announced, then released over tens of seconds.
    let mut sim = SimFeed::new("NSTAR", 100.0, t);
    let before = sim.book.log;
    sim.flow(0.0, -200.0);
    assert_eq!(sim.book.log, before, "nothing hits the book at once");
    nstar_run(&mut sim, t, 1, 2);
    let first = sim.book.backlog;
    assert!(first < -196.0 && first > -200.0, "{first}");
    nstar_run(&mut sim, t, 2, 51);
    let half = sim.book.backlog;
    assert!(half < -80.0 && half > -120.0, "{half}");
    nstar_run(&mut sim, t, 51, 600);
    assert!(sim.book.backlog.abs() < 1.0, "{}", sim.book.backlog);
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
    s.weekly_left = 20_000;
    s.day_left = 20_000;
    s.pulse_left = 20_000;
    nstar_run_seeded(&mut s, 99, 18_000);
    let fair = s.book.fair.exp();
    assert!(fair > 80.0 && fair < 165.0, "{fair}");
    let price = s.book.log.exp();
    assert!(price > 60.0 && price < 200.0, "{price}");
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

async fn persistence_fixture() -> (tempfile::TempDir, Arc<Store>, Arc<Market>, sqlx::SqlitePool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("faults.db");
    let store = Arc::new(Store::open(&path).await.unwrap());
    let market = Arc::new(Market::offline(store.clone(), Prices::new()).await.unwrap());
    let sql = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    (dir, store, market, sql)
}

// Exercise the same dirty merge as a tick, but use deterministic prices and timestamps.
async fn dirty_record(market: &Market, price: f64, at: i64) {
    let mut feed = market.nstar.lock().await;
    feed.record(price, at);
    market
        .dirty_bars
        .lock()
        .unwrap()
        .merge(feed.symbol, &feed.pending())
        .unwrap();
    let _ = feed.packet();
}

fn stop_order(trigger: f64, limit: f64) -> trading::Order {
    trading::Order {
        id: 1,
        symbol: symbols::NSTAR.into(),
        side: "long".into(),
        spot: false,
        kind: "stop".into(),
        lots: 1.0,
        price: trigger,
        limit,
        tp: 0.0,
        sl: 0.0,
        created: now_ms(),
    }
}

async fn wait_for_transaction(store: &Store, before: u64) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while store.persistence_stats()["transactions"].as_u64().unwrap() <= before {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("writer did not start a transaction");
}

#[tokio::test]
async fn bar_save_failure_retains_all_closed_ohlc_and_retries() {
    let (_dir, store, market, sql) = persistence_fixture().await;
    let t = (now_ms() / 60_000 + 1) * 60_000;
    dirty_record(&market, 100.0, t).await;
    dirty_record(&market, 120.0, t + 100).await;
    dirty_record(&market, 80.0, t + 200).await;
    dirty_record(&market, 110.0, t + 1100).await;
    let count = market.pending_bar_count();
    sqlx::query(
        "CREATE TRIGGER fail_bars BEFORE INSERT ON sim_bars BEGIN SELECT RAISE(ABORT,'fault'); END",
    )
    .execute(&sql)
    .await
    .unwrap();
    assert!(market.flush_bars().await.is_err());
    assert_eq!(market.pending_bar_count(), count);
    sqlx::query("DROP TRIGGER fail_bars")
        .execute(&sql)
        .await
        .unwrap();
    market.flush_bars().await.unwrap();
    assert_eq!(market.pending_bar_count(), 0);
    let bars = store.load_bars(symbols::NSTAR, 1).await.unwrap();
    assert_eq!(
        bars.iter().find(|b| b[0] == (t / 1000) as f64).unwrap(),
        &[(t / 1000) as f64, 100.0, 120.0, 80.0, 80.0]
    );
    sql.close().await;
    store.close().await;
}

#[tokio::test]
async fn concurrent_flush_preserves_new_version_and_closed_correction() {
    let (_dir, store, market, sql) = persistence_fixture().await;
    let t = (now_ms() / 60_000 + 1) * 60_000;
    dirty_record(&market, 100.0, t).await;
    let mut lock = sql.acquire().await.unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let before = store.persistence_stats()["transactions"].as_u64().unwrap();
    let flushing = tokio::spawn({
        let market = market.clone();
        async move { market.flush_bars().await }
    });
    wait_for_transaction(&store, before).await;
    // These writes happen AFTER flush took its old candle snapshot, while SQLite is blocked.
    dirty_record(&market, 125.0, t + 100).await;
    dirty_record(&market, 75.0, t + 200).await;
    dirty_record(&market, 110.0, t + 1100).await;
    sqlx::query("COMMIT").execute(&mut *lock).await.unwrap();
    flushing.await.unwrap().unwrap();
    assert!(
        market.pending_bar_count() > 0,
        "old acknowledgement erased a newer candle"
    );
    market.flush_bars().await.unwrap();
    let bars = store.load_bars(symbols::NSTAR, 1).await.unwrap();
    assert_eq!(
        bars.iter().find(|b| b[0] == (t / 1000) as f64).unwrap(),
        &[(t / 1000) as f64, 100.0, 125.0, 75.0, 75.0]
    );
    assert_eq!(bars.last().unwrap()[4], 110.0);
    drop(lock);
    sql.close().await;
    store.close().await;
}

#[tokio::test]
async fn early_tick_cancel_keeps_flow_for_immediate_next_desk() {
    let (_dir, store, market, sql) = persistence_fixture().await;
    store.restore_nstar_flow(250.0, -15.0);
    let expected = {
        let mut feed = market.nstar.lock().await.clone();
        feed.flow(250.0, -15.0);
        feed.book.log.exp()
    };
    let gate = market.quote_gate.lock().await;
    let ticking = tokio::spawn({
        let market = market.clone();
        async move { market.tick_nstar().await }
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while market.nstar.try_lock().is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    ticking.abort();
    assert!(ticking.await.unwrap_err().is_cancelled());
    drop(gate);
    // No random tick in between: a new order request must already see the committed flow.
    let desk = market.nstar_desk().await;
    let mut prices = Prices::new();
    desk.price(&mut prices).unwrap();
    assert_eq!(prices[symbols::NSTAR], expected);
    assert_eq!(store.take_nstar_flow(), (0.0, 0.0));
    drop(desk);
    market.flush_pending().await.unwrap();
    assert_eq!(
        market.nstar.lock().await.book.log.exp(),
        expected,
        "flow applied twice"
    );
    sql.close().await;
    store.close().await;
}

#[tokio::test]
async fn full_dirty_queue_rejects_tick_and_restores_both_flow_components() {
    let (_dir, store, market, sql) = persistence_fixture().await;
    let original = market.nstar.lock().await.book.log;
    {
        let mut dirty = market.dirty_bars.lock().unwrap();
        for t in 0..MAX_DIRTY_BARS as i64 {
            dirty.rows.insert(
                (symbols::NSTAR, 1, -t - 1),
                DirtyBar {
                    bar: [(-t - 1) as f64, 1.0, 1.0, 1.0, 1.0],
                    keep_from: -36_000,
                    version: 1,
                },
            );
        }
    }
    store.restore_nstar_flow(100.0, -12.0);
    assert!(market.tick_nstar().await.unwrap_err().contains("队列已满"));
    assert_eq!(market.nstar.lock().await.book.log, original);
    assert_eq!(store.take_nstar_flow(), (100.0, -12.0));
    assert_eq!(market.pending_bar_count(), MAX_DIRTY_BARS);
    sql.close().await;
    store.close().await;
}

#[tokio::test]
async fn failed_batch_retries_crossed_quote_before_new_price_and_keeps_ids_unique() {
    let (_dir, store, market, sql) = persistence_fixture().await;
    for name in ["alpha", "bravo"] {
        store.register(name, "pw").await.unwrap();
        store
            .transaction(name, |account, _, _| {
                account.orders = vec![stop_order(101.0, 0.0)];
                account.order_seq = 1;
                Ok(vec![])
            })
            .await
            .unwrap();
    }
    let now = now_ms();
    market.on_quote(symbols::NSTAR, 100.0, now).await.unwrap();
    let revision = market.revision();
    sqlx::query("CREATE TRIGGER fail_risk BEFORE UPDATE ON users WHEN NEW.name='bravo' BEGIN SELECT RAISE(ABORT,'fault'); END")
        .execute(&sql).await.unwrap();
    assert!(
        market
            .on_quote(symbols::NSTAR, 102.0, now + 1)
            .await
            .is_err()
    );
    assert_eq!(market.revision(), revision);
    assert_eq!(market.snapshot().await[symbols::NSTAR].price, 100.0);
    for name in ["alpha", "bravo"] {
        assert!(store.user(name).await.unwrap().account.positions.is_empty());
        assert!(store.history(name).await.unwrap().is_empty());
    }
    assert_eq!(store.take_nstar_flow(), (0.0, 0.0));
    sqlx::query("DROP TRIGGER fail_risk")
        .execute(&sql)
        .await
        .unwrap();
    // The rebound no longer crosses the stop. The failed crossing still must execute once.
    market
        .on_quote(symbols::NSTAR, 100.0, now + 2)
        .await
        .unwrap();
    let a = store.user("alpha").await.unwrap().account;
    let b = store.user("bravo").await.unwrap().account;
    assert_eq!(a.positions.len(), 1);
    assert_eq!(b.positions.len(), 1);
    assert_ne!(a.positions[0].id, b.positions[0].id);
    assert!(
        b.positions[0].entry > a.positions[0].entry,
        "second account reused unmoved live AMM"
    );
    for name in ["alpha", "bravo"] {
        assert_eq!(store.history(name).await.unwrap().len(), 1);
    }
    market.flush_pending().await.unwrap();
    sql.close().await;
    store.close().await;
}

#[tokio::test]
async fn stop_limit_transition_without_trade_is_durable() {
    let (dir, store, market, sql) = persistence_fixture().await;
    store.register("stopper", "pw").await.unwrap();
    store
        .transaction("stopper", |account, _, _| {
            account.orders = vec![stop_order(101.0, 90.0)];
            account.order_seq = 1;
            Ok(vec![])
        })
        .await
        .unwrap();
    let before = store.persistence_stats()["account_updates"]
        .as_u64()
        .unwrap();
    market
        .on_quote(symbols::NSTAR, 102.0, now_ms())
        .await
        .unwrap();
    assert_eq!(
        store.persistence_stats()["account_updates"]
            .as_u64()
            .unwrap(),
        before + 1
    );
    let account = store.user("stopper").await.unwrap().account;
    assert_eq!(account.orders[0].kind, "limit");
    assert_eq!(account.orders[0].price, 90.0);
    assert!(store.history("stopper").await.unwrap().is_empty());
    market.flush_bars().await.unwrap();
    sql.close().await;
    store.close().await;
    let reopened = Store::open(&dir.path().join("faults.db")).await.unwrap();
    assert_eq!(reopened.user("stopper").await.unwrap().account, account);
    reopened.close().await;
}

#[tokio::test]
async fn unchanged_five_hundred_risk_accounts_execute_zero_sql_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("five-hundred.db");
    Store::open(&path).await.unwrap().close().await;
    let sql = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
        .await
        .unwrap();
    let mut tx = sql.begin().await.unwrap();
    let account = Account {
        orders: vec![stop_order(200.0, 0.0)],
        order_seq: 1,
        ..Account::default()
    };
    let account = serde_json::to_string(&account).unwrap();
    for n in 0..500 {
        sqlx::query("INSERT INTO users(name,symbol,account,token) VALUES(?,'NSTAR',?,'')")
            .bind(format!("user{n:03}"))
            .bind(&account)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    let store = Arc::new(Store::open(&path).await.unwrap());
    let market = Market::offline(store.clone(), Prices::new()).await.unwrap();
    sqlx::query("CREATE TRIGGER forbid_update BEFORE UPDATE ON users BEGIN SELECT RAISE(ABORT,'unexpected risk write'); END")
        .execute(&sql).await.unwrap();
    let before = store.persistence_stats();
    market
        .on_quote(symbols::NSTAR, 100.0, now_ms())
        .await
        .unwrap();
    let after = store.persistence_stats();
    assert_eq!(after["transactions"], before["transactions"]);
    assert_eq!(after["account_updates"], before["account_updates"]);
    assert_eq!(
        after["noop_accounts"].as_u64().unwrap() - before["noop_accounts"].as_u64().unwrap(),
        500
    );
    assert_eq!(
        after["noops"].as_u64().unwrap() - before["noops"].as_u64().unwrap(),
        1
    );
    sql.close().await;
    store.close().await;
}

#[tokio::test]
async fn cancelled_quote_reuses_inflight_batch_result_before_publishing() {
    let (_dir, store, market, sql) = persistence_fixture().await;
    store.register("cancelled", "pw").await.unwrap();
    store
        .transaction("cancelled", |account, _, _| {
            account.orders = vec![stop_order(101.0, 90.0)];
            account.order_seq = 1;
            Ok(vec![])
        })
        .await
        .unwrap();
    let now = now_ms();
    market.on_quote(symbols::NSTAR, 100.0, now).await.unwrap();
    let mut lock = sql.acquire().await.unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let before = store.persistence_stats()["transactions"].as_u64().unwrap();
    let updating = tokio::spawn({
        let market = market.clone();
        async move { market.on_quote(symbols::NSTAR, 102.0, now + 1).await }
    });
    wait_for_transaction(&store, before).await;
    updating.abort();
    assert!(updating.await.unwrap_err().is_cancelled());
    assert_eq!(market.snapshot().await[symbols::NSTAR].price, 100.0);
    sqlx::query("COMMIT").execute(&mut *lock).await.unwrap();
    store.flush_pending().await.unwrap();
    let noops = store.persistence_stats()["noops"].as_u64().unwrap();
    market
        .on_quote(symbols::NSTAR, 100.0, now + 2)
        .await
        .unwrap();
    // Only the NEW quote gets a no-op batch; the cancelled old quote awaits its old result.
    assert_eq!(
        store.persistence_stats()["noops"].as_u64().unwrap(),
        noops + 1
    );
    assert_eq!(
        store.user("cancelled").await.unwrap().account.orders[0].kind,
        "limit"
    );
    assert!(market.quote_gate.lock().await.is_none());
    drop(lock);
    sql.close().await;
    store.close().await;
}

#[tokio::test]
async fn cancelled_committed_order_is_applied_before_next_request_price() {
    let (_dir, store, market, sql) = persistence_fixture().await;
    store.register("buyer", "pw").await.unwrap();
    let mut lock = sql.acquire().await.unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let before = store.persistence_stats()["transactions"].as_u64().unwrap();
    let request = tokio::spawn({
        let market = market.clone();
        let store = store.clone();
        async move {
            let desk = market.nstar_desk().await;
            let mut prices = Prices::new();
            desk.price(&mut prices).unwrap();
            store
                .transaction("buyer", |account, _, id| {
                    Ok(vec![account.open(
                        "long",
                        1.0,
                        symbols::NSTAR,
                        &prices,
                        id,
                        now_ms(),
                    )?])
                })
                .await
                .unwrap();
            desk.commit().await
        }
    });
    wait_for_transaction(&store, before).await;
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    sqlx::query("COMMIT").execute(&mut *lock).await.unwrap();
    let mut expected = market.nstar.lock().await.clone();
    expected.flow(trading::contract(symbols::NSTAR), 0.0);
    // No tick and no manual Store barrier: desk itself must wait for commit publication.
    let desk = market.nstar_desk().await;
    let mut prices = Prices::new();
    desk.price(&mut prices).unwrap();
    assert_eq!(prices[symbols::NSTAR], expected.book.log.exp());
    assert_eq!(
        store.user("buyer").await.unwrap().account.positions.len(),
        1
    );
    assert_eq!(store.history("buyer").await.unwrap().len(), 1);
    assert_eq!(store.take_nstar_flow(), (0.0, 0.0));
    drop(desk);
    market.flush_pending().await.unwrap();
    drop(lock);
    sql.close().await;
    store.close().await;
}

#[tokio::test]
async fn tick_does_not_write_bars_until_explicit_flush() {
    let (dir, store, market, sql) = persistence_fixture().await;
    let before = store.persistence_stats()["bar_rows"].as_u64().unwrap();
    let packet = market.tick_nstar().await.unwrap().0;
    assert_eq!(
        store.persistence_stats()["bar_rows"].as_u64().unwrap(),
        before
    );
    assert!(market.pending_bar_count() > 0);
    market.flush_pending().await.unwrap();
    assert_eq!(market.pending_bar_count(), 0);
    let expected = market.nstar.lock().await.klines(1, 1000);
    assert_eq!(
        expected.last().unwrap()[4],
        packet["price"].as_f64().unwrap()
    );
    sql.close().await;
    store.close().await;
    let reopened = Store::open(&dir.path().join("faults.db")).await.unwrap();
    let actual = reopened
        .load_bars(symbols::NSTAR, 1)
        .await
        .unwrap()
        .into_iter()
        .map(|b| [b[0] * 1000.0, b[1], b[2], b[3], b[4]])
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
    reopened.close().await;
}

#[tokio::test]
async fn five_round_reverse_stop_chain_applies_final_fill_before_desk_price() {
    let (_dir, store, market, sql) = persistence_fixture().await;
    market
        .on_quote(symbols::NSTAR, 100.0, now_ms())
        .await
        .unwrap();
    let units = 100.0;
    let mut expected = market.nstar.lock().await.clone();
    let mut trigger_prices = Vec::new();
    // Reverse account order forces exactly one stop to trigger in each risk batch.
    // Four normal rounds + the final check must all move the real book before price().
    for _ in 0..=MATCH_ROUNDS {
        let before = expected.book.log.exp();
        expected.flow(units, 0.0);
        trigger_prices.push((before + expected.book.log.exp()) / 2.0);
    }
    expected.flow(units, 0.0); // initial committed flow plus all five stop fills
    for (step, trigger) in trigger_prices.into_iter().enumerate() {
        let name = format!("chain{:02}", MATCH_ROUNDS - step);
        store.register(&name, "pw").await.unwrap();
        store
            .transaction(&name, |account, _, _| {
                let mut order = stop_order(trigger, 0.0);
                order.lots = units / trading::contract(symbols::NSTAR);
                account.orders = vec![order];
                account.order_seq = 1;
                Ok(vec![])
            })
            .await
            .unwrap();
    }
    store.restore_nstar_flow(units, 0.0);
    // No tick and no shutdown helper: constructing the new request's desk must reconcile
    // even the fifth fill, which was committed by the post-MATCH_ROUNDS risk check.
    let desk = market.nstar_desk().await;
    let mut prices = Prices::new();
    desk.price(&mut prices).unwrap();
    for step in 0..=MATCH_ROUNDS {
        let account = store
            .user(&format!("chain{step:02}"))
            .await
            .unwrap()
            .account;
        assert_eq!(
            account.positions.len(),
            1,
            "chain stage {step} did not fill"
        );
        assert!(account.orders.is_empty());
    }
    assert_eq!(
        store.take_nstar_flow(),
        (0.0, 0.0),
        "final committed fill left unapplied"
    );
    assert!(
        (prices[symbols::NSTAR] - expected.book.log.exp()).abs() < 1e-10,
        "desk reused the mid from before the final committed fill"
    );
    assert_eq!(
        market.snapshot().await[symbols::NSTAR].price,
        expected.book.published()
    );
    drop(desk);
    let desk = market.nstar_desk().await;
    desk.price(&mut prices).unwrap();
    assert!((prices[symbols::NSTAR] - expected.book.log.exp()).abs() < 1e-10);
    drop(desk);
    market.flush_pending().await.unwrap();
    assert_eq!(
        store
            .load_bars(symbols::NSTAR, 1)
            .await
            .unwrap()
            .last()
            .unwrap()[4],
        expected.book.published()
    );
    sql.close().await;
    store.close().await;
}

#[tokio::test]
async fn market_revision_includes_mark_anchor_and_source_time() {
    let (_dir, store, market, sql) = persistence_fixture().await;
    let mut quote = Quote::single(100.0, now_ms());
    for update in 0..4 {
        match update {
            1 => quote.mark = Some(99.0),
            2 => quote.anchor = Some(100.0_f64.ln()),
            3 => quote.at += 1,
            _ => {}
        }
        let before = market.revision();
        market.on_market_quote(symbols::NSTAR, quote).await.unwrap();
        assert_eq!(market.revision(), before + 1);
    }
    sql.close().await;
    store.close().await;
}
