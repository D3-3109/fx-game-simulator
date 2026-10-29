//! Load client, not shipped. Point it at a running `nstar_fixture` (or any server with
//! unlimited limits) and it registers N players with open NSTAR positions and resting
//! orders, keeps one WebSocket per player, trades from a subset, and reports latencies.
//!   cargo run --release --example nstar_fixture -- <tmp.db> 8077
//!   cargo run --release --example bench_ws -- http://127.0.0.1:8077 500 30 20
//! Arguments: base URL, players, seconds, trades per second (all players combined).
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;

#[derive(Default)]
struct Latencies(Mutex<Vec<f64>>);
impl Latencies {
    async fn push(&self, started: Instant) {
        self.0
            .lock()
            .await
            .push(started.elapsed().as_secs_f64() * 1000.0);
    }
    async fn report(&self, label: &str) {
        let mut v = self.0.lock().await.clone();
        if v.is_empty() {
            println!("{label:<22} no samples");
            return;
        }
        v.sort_by(f64::total_cmp);
        let at = |q: f64| v[((v.len() - 1) as f64 * q) as usize];
        println!(
            "{label:<22} n={:<6} p50={:>7.2}ms p90={:>7.2}ms p99={:>7.2}ms max={:>7.2}ms",
            v.len(),
            at(0.5),
            at(0.9),
            at(0.99),
            v[v.len() - 1]
        );
    }
}

async fn post(client: &reqwest::Client, url: String, token: &str, body: Value) -> Value {
    let mut request = client.post(url).json(&body);
    if !token.is_empty() {
        request = request.bearer_auth(token);
    }
    match request.send().await {
        Ok(r) => {
            let status = r.status();
            r.json()
                .await
                .unwrap_or_else(|e| json!({"status": status.as_u16(), "body": e.to_string()}))
        }
        Err(e) => json!({"transport": format!("{e:?}")}),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let base = args.next().ok_or("base URL required")?;
    let players: usize = args.next().map(|s| s.parse()).transpose()?.unwrap_or(300);
    let seconds: u64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(30);
    let trade_rate: u64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(10);
    // Loopback only: never route through a system or environment proxy.
    let client = reqwest::Client::builder()
        .no_proxy()
        .pool_max_idle_per_host(256)
        .build()?;
    let run = format!("b{}", std::process::id() % 100_000);

    let setup = Instant::now();
    let mut tokens = Vec::with_capacity(players);
    for chunk in (0..players).collect::<Vec<_>>().chunks(32) {
        let mut joins = Vec::new();
        for &i in chunk {
            let client = client.clone();
            let base = base.clone();
            let name = format!("{run}u{i:05}");
            joins.push(tokio::spawn(async move {
                let r = post(
                    &client,
                    format!("{base}/api/auth"),
                    "",
                    json!({"action":"register","name":name,"pwd":"bench-password-1"}),
                )
                .await;
                let token = r["token"].as_str().unwrap_or_default().to_owned();
                if !token.is_empty() {
                    let side = if i.is_multiple_of(2) { "buy" } else { "sell" };
                    post(
                        &client,
                        format!("{base}/api/trade"),
                        &token,
                        json!({"side":side,"lots":100 + (i % 7) * 50,"tp":0,"sl":0}),
                    )
                    .await;
                    post(
                        &client,
                        format!("{base}/api/order"),
                        &token,
                        json!({"kind":"limit","side":side,"lots":100,
                            "price":if side == "buy" { 50.0 } else { 200.0 },"limit":0,"tp":0,"sl":0}),
                    )
                    .await;
                }
                token
            }));
        }
        for join in joins {
            let token = join.await?;
            if !token.is_empty() {
                tokens.push(token);
            }
        }
    }
    println!(
        "setup: {} players with a position and a resting order in {:.1}s",
        tokens.len(),
        setup.elapsed().as_secs_f64()
    );

    let ws_base = base.replacen("http", "ws", 1);
    let frames = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    let states = Arc::new(AtomicU64::new(0));
    let connected = Arc::new(AtomicU64::new(0));
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut sockets = Vec::new();
    for token in &tokens {
        let url = format!("{ws_base}/ws?token={token}");
        let (frames, bytes, states, connected) = (
            frames.clone(),
            bytes.clone(),
            states.clone(),
            connected.clone(),
        );
        sockets.push(tokio::spawn(async move {
            let Ok((mut ws, _)) = tokio_tungstenite::connect_async(url).await else {
                return;
            };
            connected.fetch_add(1, Ordering::Relaxed);
            let _ = ws
                .send(Message::Text(
                    json!({"type":"view","visible":true,"leaderboard":true,"interval":"1m","low_power":false})
                        .to_string()
                        .into(),
                ))
                .await;
            while Instant::now() < deadline {
                match tokio::time::timeout(Duration::from_millis(500), ws.next()).await {
                    Ok(Some(Ok(Message::Text(text)))) => {
                        frames.fetch_add(1, Ordering::Relaxed);
                        bytes.fetch_add(text.len() as u64, Ordering::Relaxed);
                        if text.starts_with("{\"ok\":true,\"type\":\"state\"")
                            || text.contains("\"type\":\"state\"")
                        {
                            states.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Ok(Some(Ok(_))) | Err(_) => {}
                    _ => break,
                }
            }
            let _ = ws.close(None).await;
        }));
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    println!(
        "websockets connected: {}",
        connected.load(Ordering::Relaxed)
    );
    let measure = Instant::now();
    let start_frames = frames.load(Ordering::Relaxed);
    let start_bytes = bytes.load(Ordering::Relaxed);
    let start_states = states.load(Ordering::Relaxed);

    let trade = Arc::new(Latencies::default());
    let state = Arc::new(Latencies::default());
    let board = Arc::new(Latencies::default());
    let market = Arc::new(Latencies::default());
    let failures = Arc::new(AtomicU64::new(0));
    let mut load = Vec::new();
    // Traders: open and close small NSTAR positions at the requested combined rate.
    if let Some(micros) = 1_000_000u64.checked_div(trade_rate) {
        let period = Duration::from_micros(micros);
        let mut clock = tokio::time::interval(period);
        let tokens = tokens.clone();
        let (client, base, trade, failures) = (
            client.clone(),
            base.clone(),
            trade.clone(),
            failures.clone(),
        );
        load.push(tokio::spawn(async move {
            let mut i = 0usize;
            let mut inflight = tokio::task::JoinSet::new();
            while Instant::now() < deadline {
                clock.tick().await;
                let token = tokens[i % tokens.len()].clone();
                let side = if i.is_multiple_of(2) { "buy" } else { "sell" };
                i += 1;
                let (client, base, trade, failures) = (
                    client.clone(),
                    base.clone(),
                    trade.clone(),
                    failures.clone(),
                );
                inflight.spawn(async move {
                    let started = Instant::now();
                    let r = post(
                        &client,
                        format!("{base}/api/trade"),
                        &token,
                        json!({"side":side,"lots":10,"tp":0,"sl":0}),
                    )
                    .await;
                    trade.push(started).await;
                    if r["ok"] != true && failures.fetch_add(1, Ordering::Relaxed) < 5 {
                        eprintln!("trade failed: {r}");
                    }
                });
            }
            while inflight.join_next().await.is_some() {}
        }));
    }
    // Readers: page loads hit state, leaderboard and the market panel.
    for reader in 0..8 {
        let token = tokens[reader % tokens.len()].clone();
        let (client, base, state, board, market) = (
            client.clone(),
            base.clone(),
            state.clone(),
            board.clone(),
            market.clone(),
        );
        load.push(tokio::spawn(async move {
            while Instant::now() < deadline {
                let started = Instant::now();
                let _ = client
                    .get(format!("{base}/api/state"))
                    .bearer_auth(&token)
                    .send()
                    .await;
                state.push(started).await;
                let started = Instant::now();
                let _ = client.get(format!("{base}/api/leaderboard")).send().await;
                board.push(started).await;
                let started = Instant::now();
                let _ = client.get(format!("{base}/api/market")).send().await;
                market.push(started).await;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }));
    }
    for task in load {
        let _ = task.await;
    }
    let elapsed = measure.elapsed().as_secs_f64();
    let frame_count = frames.load(Ordering::Relaxed) - start_frames;
    let byte_count = bytes.load(Ordering::Relaxed) - start_bytes;
    let state_count = states.load(Ordering::Relaxed) - start_states;
    for socket in sockets {
        let _ = socket.await;
    }
    println!("measured window: {elapsed:.1}s");
    trade.report("POST /api/trade").await;
    state.report("GET /api/state").await;
    board.report("GET /api/leaderboard").await;
    market.report("GET /api/market").await;
    println!("trade failures: {}", failures.load(Ordering::Relaxed));
    println!(
        "ws frames/s total {:.0}, per socket {:.2}; state frames/s per socket {:.2}; KB/s per socket {:.2}",
        frame_count as f64 / elapsed,
        frame_count as f64 / elapsed / tokens.len().max(1) as f64,
        state_count as f64 / elapsed / tokens.len().max(1) as f64,
        byte_count as f64 / elapsed / tokens.len().max(1) as f64 / 1024.0
    );
    Ok(())
}
