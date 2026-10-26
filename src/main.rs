use clap::Parser;
use fx_sim_rust::{api, app::App, market::Market, runtime, storage::Store, web};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};

#[derive(Parser)]
#[command(about = "NSTAR simulator — explicit isolated paths required")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:8000")]
    listen: SocketAddr,
    #[arg(long)]
    db: PathBuf,
    #[arg(long, required_unless_present = "migrate_nstar_only")]
    static_dir: Option<PathBuf>,
    #[arg(long, required_unless_present = "migrate_nstar_only")]
    admin_key: Option<PathBuf>,
    /// Back up the stopped database, discard non-NSTAR holdings/orders without settlement, then exit.
    #[arg(long, requires = "migration_backup")]
    migrate_nstar_only: bool,
    /// New backup file; an existing file is never overwritten.
    #[arg(long, requires = "migrate_nstar_only")]
    migration_backup: Option<PathBuf>,
    /// Reverse proxy address or CIDR (e.g. 172.16.0.0/12) whose X-Forwarded-For is trusted
    /// for per-IP limits; comma-separated or repeated. Without it, limits key on the TCP
    /// peer, so behind a proxy everyone shares one key.
    #[arg(long = "trusted-proxy", value_delimiter = ',')]
    trusted_proxies: Vec<fx_sim_rust::limits::Net>,
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {},
                    _ = terminate.recv() => {},
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if args.migrate_nstar_only {
        if !args.db.is_file() {
            return Err("migration requires an existing database file".into());
        }
        let store = Store::open_for_migration(&args.db).await?;
        let result = store
            .migrate_nstar_only(
                args.migration_backup
                    .as_deref()
                    .ok_or("missing backup path")?,
            )
            .await;
        store.close().await;
        println!("{}", serde_json::to_string(&result?)?);
        return Ok(());
    }
    let static_dir = web::validate_static(
        args.static_dir
            .as_deref()
            .ok_or("missing static directory")?,
    )
    .await?;
    let admin_key = args
        .admin_key
        .ok_or("missing administrator configuration path")?;
    // Validate configuration without logging its contents.
    let config: serde_json::Value = serde_json::from_slice(&tokio::fs::read(&admin_key).await?)?;
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
    let store = Arc::new(Store::open(&args.db).await?);
    store.ensure_nstar_only().await?;
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    let market = Arc::new(
        Market::new(store.clone())
            .await
            .map_err(std::io::Error::other)?,
    );
    let app = Arc::new(
        App::new(store.clone(), market, admin_key)
            .with_trusted_proxies(args.trusted_proxies)
            .with_limits(fx_sim_rust::limits::Limits::from_env().map_err(std::io::Error::other)?),
    );
    let mut tasks = runtime::start(app.clone());
    eprintln!("NSTAR Rust listening on http://{}", listener.local_addr()?);
    let shutting_down = app.clone();
    let served = axum::serve(
        listener,
        api::router(app.clone(), static_dir).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        shutting_down.shutdown();
    })
    .await;
    app.shutdown();
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    let pending = store.flush_pending().await;
    let bars = app.market.flush_pending().await;
    store.close().await;
    pending?;
    bars.map_err(std::io::Error::other)?;
    served?;
    Ok(())
}
