//! Opt-in external connectivity check. Never uses accounts or API keys.
use futures_util::StreamExt;
use fx_sim_rust::{market::Market, runtime::binance_stream, storage::Store};
use serde_json::json;
use std::{sync::Arc, time::Duration};
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let store = Arc::new(Store::open_legacy(&dir.path().join("smoke.db")).await?);
    let market = Market::new(store).await.map_err(std::io::Error::other)?;
    let started = std::time::Instant::now();
    let (biquote, yahoo) =
        tokio::join!(market.biquote_price("USDJPY"), market.yahoo_price("USDJPY"));
    println!(
        "{}",
        json!({"symbol":"USDJPY","biquote":biquote,"yahoo":yahoo,"elapsed_s":started.elapsed().as_secs_f64()})
    );
    for (symbol, interval) in [("USDJPY", "1m"), ("BTCUSDT", "1m")] {
        let result = market.klines(symbol, interval, 2).await;
        println!("{}", json!({"symbol":symbol,"history":result}));
    }
    let url = binance_stream(vec!["BTCUSDT".into()]).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let (mut ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| e.to_string())?;
        loop {
            if let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) = ws.next().await {
                return Ok::<_, String>(text.to_string());
            }
        }
    })
    .await;
    println!(
        "{}",
        json!({"binance_ws":match result {Ok(Ok(_))=>"quote received",Ok(Err(_))=>"connection error",Err(_)=>"timeout"}})
    );
    Ok(())
}
