use std::net::SocketAddr;
use std::time::Duration;

use clap::Parser;
use mediacore_config::Config;
use mediacore_proto::mediacore;
use mediacore_server::{http, state::AppState};
use tracing_subscriber::EnvFilter;

/// How long a planned stop waits for the meeting records to land.
///
/// Long enough for a database that is merely busy and short enough that a
/// database that is gone does not hold a deployment open.
const SHUTDOWN_FLUSH: Duration = Duration::from_secs(10);

/// How long the sockets have to go after their rooms were closed.
///
/// Closing a room drops its participants, which drops the senders their
/// signalling tasks are waiting on, so the tasks end and the sockets close on
/// their own -- usually in milliseconds. This is the bound on that going
/// wrong, because `with_graceful_shutdown` waits for every connection and a
/// websocket nobody closes would hold the process open for ever. Overrunning
/// it is logged and then ignored: a deployment that will not finish is worse
/// than a socket that was not said goodbye to properly.
const SHUTDOWN_SOCKETS: Duration = Duration::from_secs(5);

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

    // The state is kept past the move into the router so the stop below can
    // reach the rooms. Closing them is what makes the graceful shutdown
    // finish at all: axum waits for every connection, and a signalling
    // websocket is a connection that never ends by itself.
    let draining = state.clone();
    let (drained, wait_for_drain) = tokio::sync::oneshot::channel();
    // Wrapped in an `async move` rather than spawned directly: the future
    // `with_graceful_shutdown` builds is not `'static` on its own.
    let served = tokio::spawn(async move {
        axum::serve(listener, http::router(state))
            .with_graceful_shutdown(async move {
                drain(draining).await;
                let _ = drained.send(());
            })
            .await
    });

    // The clock on the sockets starts when the rooms are closed and not before,
    // or a node that served happily for a week would be timed out on the way
    // out for having been up too long.
    let _ = wait_for_drain.await;

    match tokio::time::timeout(SHUTDOWN_SOCKETS, served).await {
        Ok(Ok(result)) => result?,
        Ok(Err(error)) => tracing::error!(%error, "the server task did not end cleanly"),
        Err(_) => tracing::warn!(
            seconds = SHUTDOWN_SOCKETS.as_secs(),
            "some sockets were still open when this node stopped waiting for them"
        ),
    }

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

/// Waits for the stop, then lets go of the rooms before returning.
///
/// Returning is what tells axum to stop accepting and to wait for what is
/// still open, so everything that has to happen while the server is still
/// serving happens here: saying `/readyz` 503 so the load balancer stops
/// sending arrivals, refusing the ones already in flight, and then telling
/// everybody in every room that this node is going.
///
/// The order matters. Announcing first and closing second means nobody is cut
/// off before their client has been told why; closing first would disconnect a
/// room full of people and only then stop new ones arriving into it.
async fn drain(state: AppState) {
    stop_signal().await;

    if !state.begin_draining() {
        return;
    }

    let rooms = state.registry.rooms();
    tracing::info!(rooms = rooms.len(), "draining: this node stops taking arrivals");

    // Each one ends as a meeting somebody watched end -- `Ending::Closed` at
    // the time it really stopped -- rather than being found unfinished later
    // and recorded as a restart with an end time nobody observed.
    for room in &rooms {
        state.registry.close(&room.name, mediacore::DisconnectReason::ServerShutdown);
    }

    if !rooms.is_empty() {
        tracing::info!(rooms = rooms.len(), "every meeting on this node was closed");
    }
}

/// SIGTERM as well as ctrl-c.
///
/// SIGTERM is the one that matters: this binary is PID 1 in its container and
/// kubelet sends it on every rolling deploy. Without it the process ignored the
/// signal, hung until SIGKILL, and every meeting in flight was found unfinished
/// afterwards and closed as a restart.
async fn stop_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(terminate) => terminate,
            Err(error) => {
                tracing::error!(%error, "cannot listen for SIGTERM; only ctrl-c will stop this node");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };

        tokio::select! {
            _ = terminate.recv() => tracing::info!("SIGTERM received"),
            _ = tokio::signal::ctrl_c() => tracing::info!("interrupted"),
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("interrupted");
    }
}
