mod admin;
mod config;
mod data_dir;
mod devices;
mod images;
mod notify;
mod push;
mod store;
mod vapid;
mod web;

use anyhow::{Context, Result};
use axum::serve;
use clap::Parser;
use config::{Args, Config};
use notify::NotifyState;
use push::WebPushTransport;
use std::sync::Arc;
use std::{ffi::OsStr, net::SocketAddr, path::PathBuf};
use store::Store;
use tokio::net::TcpListener;

#[derive(Debug, Parser)]
#[command(
    name = "reset-admin",
    bin_name = "chime-server reset-admin",
    about = "Replace administrator passkeys while Chime is stopped"
)]
struct ResetAdminArgs {
    /// Private Chime data directory
    #[arg(long, value_name = "PATH")]
    data_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut raw_args = std::env::args_os();
    let program = raw_args.next().unwrap_or_else(|| "chime-server".into());
    if raw_args.next().as_deref() == Some(OsStr::new("reset-admin")) {
        let args = ResetAdminArgs::parse_from(std::iter::once(program).chain(raw_args));
        let code = Store::reset_admin(&args.data_dir)?;
        println!(
            "Administrator passkeys were cleared. Send keys, device subscriptions, and the VAPID identity were preserved; revoke them separately if compromise is suspected."
        );
        println!("New single-use setup code (expires in 15 minutes): {code}");
        return Ok(());
    }

    let config = Config::checked(Args::parse())?;
    let store = Store::open(&config.data_dir).context("initialize Chime state")?;
    let images = images::ImageStore::new(store.clone()).context("initialize image storage")?;
    images
        .cleanup_startup()
        .await
        .context("clean expired images")?;
    let cleanup_images = images.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5 * 60));
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(error) = cleanup_images.cleanup_expired().await {
                eprintln!("image cleanup failed: {error:#}");
            }
        }
    });
    if let Some(code) = store.initialize_setup_code().await? {
        eprintln!("First-use setup code (sensitive; save it now): {code}");
    }
    let admin_state = admin::AdminState::new(store.clone(), &config.origin)
        .context("initialize passkey administration")?;
    let notify_state = NotifyState::new(
        store,
        Arc::new(WebPushTransport::new().context("initialize Web Push client")?),
    );

    let listen_address = SocketAddr::from(([127, 0, 0, 1], 8080));
    let listener = TcpListener::bind(listen_address)
        .await
        .with_context(|| format!("bind to {listen_address}"))?;
    eprintln!(
        "Chime is listening on http://{listen_address}; configured HTTPS origin is {}",
        config.origin.as_str()
    );

    serve(listener, web::router(admin_state, notify_state, images))
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
