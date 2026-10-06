use clap::Parser;
use fx_sim_rust::{storage::Store, web};
use std::{net::SocketAddr, path::PathBuf};

#[derive(Parser)]
#[command(about = "FX simulator — explicit isolated paths required")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:8001")]
    listen: SocketAddr,
    #[arg(long)]
    db: PathBuf,
    #[arg(long)]
    static_dir: PathBuf,
    #[arg(long)]
    admin_key: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let static_dir = web::validate_static(&args.static_dir).await?;
    // Validate configuration without logging its contents.
    let _: serde_json::Value = serde_json::from_slice(&tokio::fs::read(&args.admin_key).await?)?;
    let store = Store::open(&args.db).await?;
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    eprintln!("FX Rust listening on http://{}", listener.local_addr()?);
    axum::serve(listener, web::static_router(static_dir))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    store.close().await;
    Ok(())
}
