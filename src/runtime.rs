//! Shared upstream tasks, independent of the number of frontend sockets.
#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
use crate::{app::App, storage::now_ms, symbols};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use std::{collections::BTreeSet, sync::Arc, time::Duration};
use tokio::{task::JoinSet, time::sleep};

pub fn start(app: Arc<App>) -> JoinSet<()> {
    let mut tasks = JoinSet::new();
    let a = app.clone();
    tasks.spawn(async move {
        loop {
            sleep(Duration::from_secs(5)).await;
            match a.refresh_leaderboard().await {
                Ok(_) => a.broadcast_leaderboard(),
                Err(e) => eprintln!("[leaderboard] {e}; will retry"),
            }
        }
    });
    let a = app.clone();
    tasks.spawn(async move {
        let mut clock = tokio::time::interval(Duration::from_secs(1));
        clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            clock.tick().await;
            if let Err(e) = a.market.flush_bars().await {
                eprintln!("[bars] {e}; dirty bars retained for retry");
            }
        }
    });
    if !app.store.is_nstar_only() {
        let a = app.clone();
        tasks.spawn(async move {
            loop {
                if let Ok(symbols) = a.risk_symbols().await {
                    for symbol in symbols {
                        let _ = a.market.price(&symbol).await;
                        if let Some(q) = a.market.snapshot().await.get(&symbol)
                            && a.market.on_market_quote(&symbol, *q).await.is_err()
                        {
                            eprintln!("[risk] quote/settlement failed; will retry");
                        }
                    }
                }
                sleep(Duration::from_secs(2)).await;
            }
        });
    }
    let a = app.clone();
    tasks.spawn(async move {
        loop {
            if let Err(e) = a.reset_expired_debts().await {
                eprintln!("[debt] check failed: {e}; will retry");
            }
            sleep(Duration::from_secs(2)).await;
        }
    });
    let a = app.clone();
    tasks.spawn(async move {
        loop {
            sleep(Duration::from_secs(60)).await;
            if let Err(e) = a.settle_funding().await {
                eprintln!("[funding] {e}; will retry");
            }
        }
    });
    if !app.market.is_offline() {
        let a = app.clone();
        tasks.spawn(async move {
            loop {
                match a.market.tick_nstar().await {
                    Ok((packet, events)) => {
                        a.broadcast(packet);
                        events.into_iter().for_each(|e| a.broadcast(e));
                    }
                    Err(_) => eprintln!("[nstar] tick persistence/settlement failed; will retry"),
                }
                sleep(Duration::from_millis(200)).await;
            }
        });
        if !app.store.is_nstar_only() {
            let a = app.clone();
            tasks.spawn(async move {
                loop {
                    match a.market.tick_sim().await {
                        Ok(packet) => a.broadcast(packet),
                        Err(_) => eprintln!("[sim] tick persistence/settlement failed; will retry"),
                    }
                    sleep(Duration::from_millis(200)).await;
                }
            });
            let a = app.clone();
            tasks.spawn(async move {
                fx_loop(a).await;
            });
            let a = app.clone();
            tasks.spawn(async move {
                stock_loop(a).await;
            });
            tasks.spawn(async move {
                binance_loop(app, "wss://data-stream.binance.vision").await;
            });
        }
    }
    tasks
}

async fn stock_loop(app: Arc<App>) {
    if app.store.is_nstar_only() {
        return;
    }
    loop {
        let active = app.active_symbols().await.unwrap_or_default();
        for symbol in active
            .iter()
            .filter(|s| symbols::STOCK.contains(&s.as_str()))
        {
            if app.market.price(symbol).await.is_err() {
                eprintln!("[stock] quote unavailable for {symbol}; will retry");
            }
        }
        sleep(Duration::from_secs(10)).await;
    }
}

async fn fx_loop(app: Arc<App>) {
    if app.store.is_nstar_only() {
        return;
    }
    let mut clock = tokio::time::interval(Duration::from_millis(500));
    clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        clock.tick().await;
        let active = app.active_symbols().await.unwrap_or_default();
        // Include conversion pairs and offline users' positions; one shared batch per round.
        if let Ok(needed) = crate::trading::price_symbols(
            active
                .iter()
                .map(String::as_str)
                .filter(|s| symbols::FX.contains(s)),
        ) {
            let wanted: Vec<_> = needed
                .into_iter()
                .filter(|s| symbols::FX.contains(&s.as_str()))
                .collect();
            if !wanted.is_empty() {
                match app.market.biquote_quotes(&wanted).await {
                    Ok(quotes) => {
                        for (symbol, q) in quotes {
                            if app
                                .market
                                .on_market_quote(&symbol, q)
                                .await
                                .unwrap_or(false)
                            {
                                // Send sides and original source time even when mid is unchanged.
                                app.broadcast(q.packet(&symbol, now_ms()));
                            }
                        }
                    }
                    Err(_) => {
                        eprintln!("[fx] source quote unavailable; will retry");
                        sleep(Duration::from_secs(2)).await;
                    }
                }
            }
        }
    }
}

pub fn binance_stream(symbols: impl IntoIterator<Item = String>) -> Option<String> {
    let wanted: BTreeSet<_> = symbols
        .into_iter()
        .filter(|s| crate::symbols::CRYPTO.contains(&s.as_str()))
        .map(|s| format!("{}@miniTicker", s.to_lowercase()))
        .collect();
    if wanted.is_empty() {
        None
    } else {
        Some(format!(
            "wss://data-stream.binance.vision/stream?streams={}",
            wanted.into_iter().collect::<Vec<_>>().join("/")
        ))
    }
}
async fn binance_loop(app: Arc<App>, endpoint: &str) {
    // WebSockets bypass Market::get, so this must also guard direct invocations.
    if app.store.is_nstar_only() {
        return;
    }
    loop {
        let Some(url) = binance_stream(app.active_symbols().await.unwrap_or_default()) else {
            sleep(Duration::from_secs(1)).await;
            continue;
        };
        // Matches the Python upstream's direct TLS connection; REST clients honor FX_PROXY/environment.
        let connected = tokio::time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::connect_async(url.replacen(
                "wss://data-stream.binance.vision",
                endpoint,
                1,
            )),
        )
        .await;
        let Ok(Ok((mut socket, _))) = connected else {
            sleep(Duration::from_secs(3)).await;
            continue;
        };
        let mut subscriptions = tokio::time::interval(Duration::from_secs(1));
        let idle = tokio::time::sleep(Duration::from_secs(30));
        tokio::pin!(idle);
        loop {
            tokio::select! {
                _=&mut idle=>break,
                _=subscriptions.tick()=> {
                    if binance_stream(app.active_symbols().await.unwrap_or_default()).as_ref()!=Some(&url) {break;}
                },
                message=socket.next()=> {
                    idle.as_mut().reset(tokio::time::Instant::now()+Duration::from_secs(30));
                    match message {
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text)))=> {
                            if let Ok(d)=serde_json::from_str::<Value>(&text) {
                                let d=&d["data"];
                                if let (Some(symbol),Some(price))=(d["s"].as_str(),d["c"].as_str().and_then(|s|s.parse::<f64>().ok()))
                                    && symbols::CRYPTO.contains(&symbol) && app.market.on_quote(symbol,price,now_ms()).await.unwrap_or(false)
                                    && let Some(q)=app.market.snapshot().await.get(symbol) {app.broadcast(q.packet(symbol,now_ms()));}
                            }
                        },
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(_)))=> {
                            // tungstenite queues the RFC-required pong with the identical payload.
                            if socket.flush().await.is_err() {break;}
                        },
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_)))|None|Some(Err(_))=>break,
                        _=>{},
                    }
                }
            }
        }
        let _ = tokio::time::timeout(Duration::from_secs(2), socket.close(None)).await;
        sleep(Duration::from_secs(2)).await;
    }
}
