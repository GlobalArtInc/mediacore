use std::net::{Ipv4Addr, SocketAddr};

use anyhow::Context as _;
use mediacore_demo::{Config, http};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let config = Config::from_env().context("the demo cannot start without its environment")?;
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, config.port));
    let room = config.room.clone();
    let ws_url = config.ws_url.clone();

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;

    tracing::info!(%addr, %room, %ws_url, "mediacore-demo is listening");
    axum::serve(listener, http::router(config))
        .with_graceful_shutdown(shutdown())
        .await?;

    Ok(())
}

fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("mediacore_demo=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
