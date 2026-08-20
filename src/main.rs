pub mod cache;
pub mod catalog;
pub mod config;
mod routes;
pub mod scheduler;
pub mod selection;
pub mod source;
mod state;

use std::{net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result};
use cache::CacheStore;
use config::{Config, SettingsManager};
use tokio::{net::TcpListener, sync::watch};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use crate::{source::SourceStore, state::AppState};

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let config_path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.toml"));
    let config = Config::load(&config_path)?;
    let settings = SettingsManager::load(config.settings_path.clone())?;
    let cache = CacheStore::new(&config.cache_dir)?;
    let initial_settings = settings.snapshot().await;
    SourceStore::new(&initial_settings.settings, cache.root())
        .context("source/cache safety validation failed")?;
    let listener = TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("failed to bind server to {}", config.bind))?;
    let local_address = listener
        .local_addr()
        .context("failed to determine server address")?;

    info!(
        address = %local_address,
        cache_dir = %config.cache_dir.display(),
        settings_path = %config.settings_path.display(),
        "paper frame server listening"
    );

    let state = AppState::new(config, settings, cache);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let scheduler_task = tokio::spawn(scheduler::run(state.clone(), shutdown_rx.clone()));
    let signal_task = tokio::spawn(signal_shutdown(shutdown_tx.clone()));

    let server_result = axum::serve(
        listener,
        routes::router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_requested(shutdown_rx))
    .await
    .context("HTTP server failed");

    let _ = shutdown_tx.send(true);
    scheduler_task.await.context("scheduler task failed")?;
    signal_task.abort();
    server_result?;
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn signal_shutdown(shutdown: watch::Sender<bool>) {
    if let Err(error) = wait_for_shutdown_signal().await {
        error!(%error, "failed to listen for a shutdown signal");
    }
    info!("shutting down");
    let _ = shutdown.send(true);
}

async fn shutdown_requested(mut shutdown: watch::Receiver<bool>) {
    while !*shutdown.borrow() {
        if shutdown.changed().await.is_err() {
            return;
        }
    }
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
