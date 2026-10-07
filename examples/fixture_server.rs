//! Offline black-box fixture server. Not a production entry point.
use fx_sim_rust::{api, app::App, market::Market, runtime, storage::Store};
use std::{path::PathBuf, sync::Arc};
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let dir = PathBuf::from(args.get(1).ok_or("temporary fixture directory required")?);
    let store = Arc::new(Store::open(&dir.join("fixture.db")).await?);
    let prices = [
        ("USDJPY".into(), 150.0),
        ("EURJPY".into(), 165.0),
        ("EURUSD".into(), 1.1),
        ("FXSIM".into(), 1.1),
        ("BTCUSDT".into(), 60000.0),
    ]
    .into();
    let market = Arc::new(
        Market::offline(store.clone(), prices)
            .await
            .map_err(std::io::Error::other)?,
    );
    let app = Arc::new(App::new(store, market, dir.join("admin.json")));
    let _tasks = runtime::start(app.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    println!("http://{}", listener.local_addr()?);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static");
    axum::serve(listener, api::router(app, root)).await?;
    Ok(())
}
