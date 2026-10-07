use super::*;
use crate::{market::Market, storage::Store, trading::Prices};

#[tokio::test]
async fn shared_upstream_reconnects_and_resubscribes_without_duplicate_workers() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("upstream.db")).await.unwrap());
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
