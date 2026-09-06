use std::net::SocketAddr;

use clap::Parser;
use mediacore_config::Config;
use mediacore_server::{http, state::AppState};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "mediacore", version, about = "Selective forwarding unit")]
struct Cli {
    #[arg(long, short, env = "MEDIACORE_CONFIG", default_value = "config.yaml")]
    config: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;
    init_tracing(&config.log_level);

    if config.bind_addresses.len() > 1 {
        tracing::warn!("only the first bind address is used for now");
    }

    let addr: SocketAddr = format!("{}:{}", config.bind_addresses[0], config.port).parse()?;
    let node_id = config.node_id.clone();
    let state = AppState::start(config).await?;
    let ice = state.media.local_addr();
    // Kept past the move into the router so that the stop below can wait for
    // the records. Meeting records are written by a thread of their own rather
    // than on the join path, so a process that exits without waiting loses
    // whatever that thread had not reached -- which, at the end of a working
    // day, is the end of every meeting still running.
    let sessions = std::sync::Arc::clone(state.registry.sessions());
    let listener = tokio::net::TcpListener::bind(addr).await?;

    tracing::info!(%addr, %ice, %node_id, "mediacore is listening");
    axum::serve(listener, http::router(state))
        .with_graceful_shutdown(shutdown())
        .await?;

    sessions.flush();
    tracing::info!("every meeting record is on the disk");

    Ok(())
}

fn init_tracing(level: &str) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("mediacore={level}")));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
