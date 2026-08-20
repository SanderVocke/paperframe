mod config;
mod routes;
mod state;

use std::{net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result};
use config::Config;
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let config_path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.toml"));
    let config = Config::load(&config_path)?;
    let listener = TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("failed to bind server to {}", config.bind))?;

    info!(
        address = %config.bind,
        image_path = %config.image_path.display(),
        "paper frame server listening"
    );

    axum::serve(
        listener,
        routes::router().into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .context("HTTP server failed")?;

    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
