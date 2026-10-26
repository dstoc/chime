mod config;
mod data_dir;
mod store;
mod web;

use anyhow::{Context, Result};
use axum::serve;
use clap::Parser;
use config::{Args, Config};
use std::net::SocketAddr;
use store::Store;
use tokio::net::TcpListener;

const LISTEN_ADDRESS: SocketAddr = SocketAddr::from(([127, 0, 0, 1], 8080));

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::checked(Args::parse())?;
    let _store = Store::open(&config.data_dir).context("initialize Chime state")?;

    let listener = TcpListener::bind(LISTEN_ADDRESS)
        .await
        .with_context(|| format!("bind to {LISTEN_ADDRESS}"))?;
    eprintln!(
        "Chime is listening on http://{LISTEN_ADDRESS}; configured HTTPS origin is {}",
        config.origin.as_str()
    );

    serve(listener, web::router())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serve HTTP requests")
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(error) = result {
                    eprintln!("failed to listen for Ctrl-C: {error}");
                }
            }
            _ = terminate.recv() => {}
        }
    }

    #[cfg(not(unix))]
    if let Err(error) = tokio::signal::ctrl_c().await {
        eprintln!("failed to listen for Ctrl-C: {error}");
    }
}
