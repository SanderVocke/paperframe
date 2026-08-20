pub mod cache;
pub mod catalog;
pub mod config;
mod routes;
pub mod selection;
pub mod source;
mod state;

use std::{net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result};
use config::{Config, SettingsManager};
use tokio::net::TcpListener;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use crate::state::AppState;

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
    let local_address = listener
        .local_addr()
        .context("failed to determine server address")?;

    let settings = SettingsManager::load(config.settings_path.clone())?;
    std::fs::create_dir_all(&config.cache_dir).with_context(|| {
        format!(
            "failed to create cache directory {}",
            config.cache_dir.display()
        )
    })?;

    info!(
        address = %local_address,
        cache_dir = %config.cache_dir.display(),
        settings_path = %config.settings_path.display(),
        "paper frame server listening"
    );

    let state = AppState::new(config, settings);
    axum::serve(
        listener,
        routes::router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .context("HTTP server failed")?;

    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn shutdown_signal() {
    if let Err(error) = wait_for_shutdown_signal().await {
        error!(%error, "failed to listen for a shutdown signal");
    }
    info!("shutting down");
}

#[cfg(unix)]
async fn wait_for_shutdown_signal() -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate =
        signal(SignalKind::terminate()).context("failed to register SIGTERM handler")?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result.context("failed to listen for Ctrl-C")?,
        _ = terminate.recv() => {}
    }
    Ok(())
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal() -> Result<()> {
    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for Ctrl-C")
}
