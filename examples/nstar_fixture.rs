//! Loopback-only NSTAR browser fixture with an explicitly supplied temporary database.
//! cargo run --example nstar_fixture -- <temporary.db> [port]
use fx_sim_rust::{api, app::App, limits::Limits, market::Market, runtime, storage::Store, web};
use std::{path::PathBuf, sync::Arc, time::Duration};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let db = PathBuf::from(
        args.next()
            .ok_or("temporary fixture database path required")?,
    );
    let port: u16 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(0);
    let root =
        web::validate_static(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static")).await?;
    let store = Arc::new(Store::open(&db).await?);
    let market = Arc::new(
        Market::offline(store.clone(), Default::default())
            .await
            .map_err(std::io::Error::other)?,
    );
    market.tick_nstar().await.map_err(std::io::Error::other)?;
    let app = Arc::new(
        App::new(store.clone(), market, db.with_file_name("admin.json"))
            .with_limits(Limits::unlimited()),
    );
    let mut tasks = runtime::start(app.clone());
    // Offline suppresses runtime feed workers; drive only the local NSTAR book explicitly.
    let a = app.clone();
    tasks.spawn(async move {
        loop {
            match a.market.tick_nstar().await {
                Ok((packet, events)) => {
                    a.broadcast(packet);
                    events.into_iter().for_each(|event| a.broadcast(event));
                }
                Err(err) => eprintln!("[nstar fixture] {err}"),
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    println!("http://{}", listener.local_addr()?);
    let closing = app.clone();
    let served = axum::serve(
        listener,
        api::router(app.clone(), root)
            .into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = tokio::signal::ctrl_c().await;
        closing.shutdown();
    })
    .await;
    app.shutdown();
    tasks.shutdown().await;
    let pending = store.flush_pending().await;
    let bars = app.market.flush_pending().await;
    store.close().await;
    pending?;
    bars.map_err(std::io::Error::other)?;
    served?;
    Ok(())
}
