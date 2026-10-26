use super::*;
use crate::{market::Market, storage::Store, trading::Prices};

#[tokio::test]
async fn nstar_only_runtime_ticks_settles_risk_and_debt_without_legacy_workers() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        Store::open(&dir.path().join("nstar-runtime.db"))
            .await
            .unwrap(),
    );
    store.register("risk", "pw").await.unwrap();
    store.register("debt", "pw").await.unwrap();
    store
        .transaction("risk", |a, _, _| {
            a.balance = 1.0;
            a.positions = vec![crate::trading::Position {
                id: 1,
                side: "long".into(),
                lots: 100.0,
                entry: 1000.0,
                symbol: symbols::NSTAR.into(),
                tp: 0.0,
                sl: 0.0,
            }];
            Ok(vec![])
        })
        .await
        .unwrap();
    store
        .transaction("debt", |a, _, _| {
            a.borrow(crate::trading::DEBT_LIMIT + 1.0, now_ms())?;
            a.debt_since = now_ms() - crate::trading::DEBT_GRACE_MS - 1000;
            Ok(vec![])
        })
        .await
        .unwrap();
    let market = Arc::new(Market::new(store.clone()).await.unwrap());
    let app = Arc::new(App::new(
        store.clone(),
        market,
        dir.path().join("unused.json"),
    ));
    assert_eq!(app.active_symbols().await.unwrap(), vec![symbols::NSTAR]);
    assert!(app.risk_symbols().await.unwrap().is_empty());
    let mut events = app.events.subscribe();
    let mut tasks = start(app.clone());
    // NSTAR tick (including risk), leaderboard, bars, debt, funding: no legacy workers.
    assert_eq!(tasks.len(), 5);
    let mut ticks = 0;
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let crate::app::Event::Message(text, symbol) = events.recv().await.unwrap() {
                let value: Value = serde_json::from_str(&text).unwrap();
                if let Some(symbol) = symbol {
                    assert_eq!(&*symbol, symbols::NSTAR, "legacy tick broadcast");
                    ticks += 1;
                }
                if value["type"] == "lb" && ticks >= 2 {
                    break;
                }
            }
        }
    })
    .await
    .unwrap();
    tasks.shutdown().await;
    assert!(
        store
            .user("risk")
            .await
            .unwrap()
            .account
            .positions
            .is_empty()
    );
    assert!(
        store
            .history("risk")
            .await
            .unwrap()
            .iter()
            .any(|t| t["kind"] == "stopout")
    );
    assert_eq!(store.user("debt").await.unwrap().account.loan, 0.0);
    assert!(
        store
            .history("debt")
            .await
            .unwrap()
            .iter()
            .any(|t| t["kind"] == "reset")
    );
    assert_eq!(
        app.market
            .snapshot()
            .await
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec![symbols::NSTAR]
    );
    for (_, sec) in symbols::INTERVALS {
        assert!(
            store
                .load_bars(symbols::SIM, *sec)
                .await
                .unwrap()
                .is_empty()
        );
    }
    store.close().await;
}

#[tokio::test]
async fn nstar_only_upstream_loops_return_without_opening_a_socket() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        Store::open(&dir.path().join("no-upstream.db"))
            .await
            .unwrap(),
    );
    store.register("local", "pw").await.unwrap();
    let market = Arc::new(Market::new(store.clone()).await.unwrap());
    let app = Arc::new(App::new(
        store.clone(),
        market,
        dir.path().join("unused.json"),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    tokio::time::timeout(Duration::from_millis(100), async {
        tokio::join!(
            fx_loop(app.clone()),
            stock_loop(app.clone()),
            binance_loop(app, &endpoint)
        );
    })
    .await
    .expect("NSTAR-only must not run an upstream loop");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
    store.close().await;
}

#[tokio::test]
async fn shared_upstream_reconnects_and_resubscribes_without_duplicate_workers() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        Store::open_legacy(&dir.path().join("upstream.db"))
            .await
            .unwrap(),
    );
    store.register("one", "pw").await.unwrap();
    store.register("two", "pw").await.unwrap();
    for name in ["one", "two"] {
        store.select_symbol(name, "BTCUSDT").await.unwrap();
    }
    let market = Arc::new(Market::offline(store.clone(), Prices::new()).await.unwrap());
    let app = Arc::new(App::new(
        store.clone(),
        market,
        dir.path().join("unused-admin.json"),
    ));
    // Multiple browser presences must not create multiple upstream connections.
    let _a = app.connected("one");
    let _b = app.connected("one");
    let _c = app.connected("two");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    let (urls, mut received) = tokio::sync::mpsc::unbounded_channel();
    let upstream = tokio::spawn(async move {
        for index in 0..3 {
            let (socket, _) = listener.accept().await.unwrap();
            let urls = urls.clone();
            // The tungstenite callback fixes this response type; the fixture cannot box it.
            #[allow(clippy::result_large_err)]
            let mut ws = tokio_tungstenite::accept_hdr_async(
                socket,
                move |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                      response| {
                    urls.send(request.uri().to_string()).unwrap();
                    Ok(response)
                },
            )
            .await
            .unwrap();
            let sym = if index == 2 { "ETHUSDT" } else { "BTCUSDT" };
            ws.send(tokio_tungstenite::tungstenite::Message::Text(
                json!({"data":{"s":sym,"c":"60000"}}).to_string().into(),
            ))
            .await
            .unwrap();
            if index == 0 {
                ws.close(None).await.unwrap();
            } else {
                let _ = ws.next().await;
            }
        }
    });
    let a = app.clone();
    let worker = tokio::spawn(async move { binance_loop(a, &endpoint).await });
    async fn next(rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> String {
        tokio::time::timeout(Duration::from_secs(8), rx.recv())
            .await
            .unwrap()
            .unwrap()
    }
    let first = next(&mut received).await;
    let second = next(&mut received).await;
    assert_eq!(first, "/stream?streams=btcusdt@miniTicker");
    assert_eq!(second, first);
    for name in ["one", "two"] {
        store.select_symbol(name, "ETHUSDT").await.unwrap();
    }
    assert_eq!(
        next(&mut received).await,
        "/stream?streams=ethusdt@miniTicker"
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if app.market.snapshot().await.contains_key("ETHUSDT") {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    worker.abort();
    upstream.abort();
    let _ = worker.await;
    let _ = upstream.await;
}

#[tokio::test]
async fn runtime_flushes_dirty_bars_without_any_client_or_new_tick() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        Store::open(&dir.path().join("flush-runtime.db"))
            .await
            .unwrap(),
    );
    let market = Arc::new(Market::offline(store.clone(), Prices::new()).await.unwrap());
    market.tick_nstar().await.unwrap();
    assert!(market.pending_bar_count() > 0);
    // SimFeed may seed durable startup bars; compare the changed current candle instead.
    let before = store.load_bars(symbols::NSTAR, 1).await.unwrap();
    let app = Arc::new(App::new(
        store.clone(),
        market.clone(),
        dir.path().join("unused.json"),
    ));
    let mut tasks = start(app);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if market.pending_bar_count() == 0
                && store.load_bars(symbols::NSTAR, 1).await.unwrap() != before
            {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("one-second runtime worker must flush even with an offline feed");
    tasks.shutdown().await;
    store.close().await;
}

#[tokio::test]
async fn state_cache_invalidates_store_and_market_revisions_and_shares_same_account() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        Store::open(&dir.path().join("state-cache.db"))
            .await
            .unwrap(),
    );
    store.register("cached", "pw").await.unwrap();
    let market = Arc::new(Market::offline(store.clone(), Prices::new()).await.unwrap());
    market.tick_nstar().await.unwrap();
    let app = App::new(
        store.clone(),
        market.clone(),
        dir.path().join("unused.json"),
    );
    let first = app.state_packet("cached").await.unwrap();
    let same = app.state_packet("cached").await.unwrap();
    assert!(Arc::ptr_eq(&first, &same));
    store
        .transaction("cached", |a, _, _| {
            a.balance = 12345.0;
            Ok(vec![])
        })
        .await
        .unwrap();
    let changed = app.state_packet("cached").await.unwrap();
    assert!(!Arc::ptr_eq(&same, &changed));
    assert_eq!(changed.value["balance"], 12345.0);
    let mut quote = market.snapshot().await[symbols::NSTAR];
    quote.mark = Some(quote.price * 0.9);
    quote.anchor = Some(quote.price.ln() + 0.02);
    market.on_market_quote(symbols::NSTAR, quote).await.unwrap();
    let marked = app.state_packet("cached").await.unwrap();
    assert!(!Arc::ptr_eq(&changed, &marked));
    assert_eq!(marked.value["mark"], quote.mark.unwrap());
    app.refresh_leaderboard().await.unwrap();
    assert_eq!(
        app.state("cached").await.unwrap()["leaderboard_me"]["rank"],
        1
    );
    tokio::time::sleep(Duration::from_millis(550)).await;
    let before_ttl = app.state_packet("cached").await.unwrap();
    tokio::time::sleep(Duration::from_millis(550)).await;
    assert!(!Arc::ptr_eq(
        &before_ttl,
        &app.state_packet("cached").await.unwrap()
    ));
    store.close().await;
}

#[tokio::test]
async fn state_cache_expires_at_quote_freshness_boundary_without_revision_change() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        Store::open(&dir.path().join("quote-expiry.db"))
            .await
            .unwrap(),
    );
    store.register("expiry", "pw").await.unwrap();
    let market = Arc::new(Market::new(store.clone()).await.unwrap());
    // Seed the first quote directly: backdating an already-published tick is correctly rejected.
    let quote = crate::market::Quote {
        price: 100.0,
        at: now_ms() - 29600,
        received_at: now_ms(),
        bid_ask: None,
        open: true,
        anchor: Some(100.0f64.ln()),
        mark: Some(100.0),
    };
    market.on_market_quote(symbols::NSTAR, quote).await.unwrap();
    let app = App::new(
        store.clone(),
        market.clone(),
        dir.path().join("unused.json"),
    );
    let first = app.state_packet("expiry").await.unwrap();
    assert_eq!(first.value["quote_valid"], true);
    let revision = market.revision();
    tokio::time::sleep(Duration::from_millis(450)).await;
    assert_eq!(market.revision(), revision);
    let expired = app.state_packet("expiry").await.unwrap();
    assert_eq!(expired.value["quote_valid"], false);
    assert!(!Arc::ptr_eq(&first, &expired));
    store.close().await;
}
