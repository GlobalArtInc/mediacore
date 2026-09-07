use std::net::SocketAddr;
use std::time::Duration;

use clap::Parser;
use mediacore_config::Config;
use mediacore_server::{http, state::AppState};
use tracing_subscriber::EnvFilter;

/// How long a planned stop waits for the meeting records to land.
///
/// Long enough for a database that is merely busy and short enough that a
/// database that is gone does not hold a deployment open.
const SHUTDOWN_FLUSH: Duration = Duration::from_secs(10);

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

    // Bounded, because the store may be a database that is down, and "wait
    // until it lands" would then be "never shut down". A server that will not
    // stop is a worse problem than a handful of meetings whose last minute is
    // missing from the record.
    if sessions.flush(SHUTDOWN_FLUSH) {
        tracing::info!("every meeting record is in the store");
    } else {
        tracing::error!(
            owed = sessions.owed(),
            "the session store did not take these meetings before this server stopped; they are \
             lost"
        );
    }

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
