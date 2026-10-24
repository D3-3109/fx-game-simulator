use clap::Parser;
use fx_sim_rust::{api, app::App, market::Market, runtime, storage::Store, web};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};

#[derive(Parser)]
#[command(about = "FX simulator — explicit isolated paths required")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:8000")]
    listen: SocketAddr,
    #[arg(long)]
    db: PathBuf,
    #[arg(long)]
    static_dir: PathBuf,
    #[arg(long)]
    admin_key: PathBuf,
    /// Reverse proxy address or CIDR (e.g. 172.16.0.0/12) whose X-Forwarded-For is trusted
    /// for per-IP limits; comma-separated or repeated. Without it, limits key on the TCP
    /// peer, so behind a proxy everyone shares one key.
    #[arg(long = "trusted-proxy", value_delimiter = ',')]
    trusted_proxies: Vec<fx_sim_rust::limits::Net>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let static_dir = web::validate_static(&args.static_dir).await?;
    // Validate configuration without logging its contents.
    let config: serde_json::Value =
        serde_json::from_slice(&tokio::fs::read(&args.admin_key).await?)?;
    for (key, length) in [("salt", None), ("hash", Some(32))] {
        let bytes = hex::decode(
            config[key]
                .as_str()
                .ok_or("invalid administrator configuration")?,
        )?;
        if bytes.is_empty() || length.is_some_and(|n| bytes.len() != n) {
            return Err("invalid administrator configuration".into());
        }
    }
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    let store = Arc::new(Store::open(&args.db).await?);
    let market = Arc::new(
        Market::new(store.clone())
            .await
            .map_err(std::io::Error::other)?,
    );
    let app = Arc::new(
        App::new(store.clone(), market, args.admin_key).with_trusted_proxies(args.trusted_proxies),
    );
    let mut tasks = runtime::start(app.clone());
    eprintln!("FX Rust listening on http://{}", listener.local_addr()?);
    axum::serve(
        listener,
        api::router(app.clone(), static_dir).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = tokio::signal::ctrl_c().await;
        app.shutdown();
    })
    .await?;
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    store.close().await;
    Ok(())
}
