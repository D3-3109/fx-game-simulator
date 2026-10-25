//! Load probe, not shipped: N accounts with 20 open positions each, then time the hot paths.
//!   cargo run --release --example bench_load -- 200
use fx_sim_rust::{app::App, limits::Limits, market::Market, storage::Store};
use std::{sync::Arc, time::Instant};
#[tokio::main]
async fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("b.db")).await.unwrap());
    let q = [("FXSIM".to_string(), 1.1)].into();
    for i in 0..n {
        let name = format!("u{i:04}");
        store.register(&name, "password1").await.unwrap();
        for k in 0..20 {
            store
                .transaction(&name, |a, s, id| {
                    *s = "FXSIM".into();
                    let t = a.open(
                        if k % 2 == 0 { "long" } else { "short" },
                        0.01,
                        "FXSIM",
                        &q,
                        id,
                        k,
                    )?;
                    Ok(vec![t])
                })
                .await
                .unwrap();
        }
    }
    let market = Arc::new(Market::offline(store.clone(), q.clone()).await.unwrap());
    let app = Arc::new(
        App::new(store.clone(), market.clone(), dir.path().join("k"))
            .with_limits(Limits::unlimited()),
    );
    let t = Instant::now();
    for _ in 0..20 {
        store.users().await.unwrap();
    }
    println!(
        "users() x20 with {n} accounts: {:.2} ms each",
        t.elapsed().as_secs_f64() * 50.0
    );
    let t = Instant::now();
    for _ in 0..200 {
        app.state("u0000").await.unwrap();
    }
    println!(
        "state() x200: {:.3} ms each",
        t.elapsed().as_secs_f64() * 5.0
    );
    let t = Instant::now();
    for i in 0..200 {
        store
            .transaction("u0001", |a, _, id| {
                Ok(vec![a.open("long", 0.01, "FXSIM", &q, id, 100 + i)?])
            })
            .await
            .unwrap();
        store
            .transaction("u0001", |a, _, _| {
                let id = a.positions.last().unwrap().id;
                a.close(Some(id), &q, 101)
            })
            .await
            .unwrap();
    }
    println!(
        "open+close transaction x200: {:.3} ms each pair",
        t.elapsed().as_secs_f64() * 5.0
    );
    let t = Instant::now();
    for i in 0..20 {
        market
            .on_quote("FXSIM", 1.1, 1_800_000_000_000 + i)
            .await
            .unwrap();
    }
    println!(
        "risk sweep (on_quote) x20: {:.2} ms each",
        t.elapsed().as_secs_f64() * 50.0
    );
    let t = Instant::now();
    for _ in 0..5 {
        app.leaderboard().await.unwrap();
    }
    println!(
        "leaderboard x5: {:.2} ms each",
        t.elapsed().as_secs_f64() * 200.0
    );
}
