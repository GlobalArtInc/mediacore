use std::net::{Ipv4Addr, SocketAddr};

use anyhow::Context as _;
use mediacore_front::{Config, http};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let config =
        Config::from_env().context("the front end cannot start without its environment")?;
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, config.port));
    let room = config.room.clone();
    let ws_url = config.ws_url.clone();
    // Which of the two things this binary is, said once at startup. Reading a
    // log to work out whether a deployment asks anybody to sign in is a bad
    // half hour, and the issuer is the whole of the answer.
    let sign_in = config
        .oidc
        .as_ref()
        .map_or_else(|| "off".to_owned(), |oidc| oidc.issuer.clone());

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;

    tracing::info!(%addr, %room, %ws_url, %sign_in, "mediacore-front is listening");
    axum::serve(listener, http::router(config))
        .with_graceful_shutdown(shutdown())
        .await?;

    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("mediacore_front=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// SIGTERM as well as ctrl-c.
///
/// SIGTERM is the one a deploy sends, and this binary is PID 1 in its
/// container. Without it the process ignored the signal and ran until SIGKILL:
/// nothing was lost -- this server holds no meetings and no records, only a
/// page and the tokens it mints -- but every rollout waited out the grace
/// period for every pod before anything moved.
///
/// There is nothing to drain. The connections here are ordinary HTTP requests
/// and they end by themselves, which is why this stops at catching the signal
/// while the server's own stop has an order to it.
async fn shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = terminate.recv() => tracing::info!("SIGTERM received"),
                    _ = tokio::signal::ctrl_c() => tracing::info!("interrupted"),
                }
            }
            Err(error) => {
                tracing::error!(%error, "cannot listen for SIGTERM; only ctrl-c will stop this server");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("interrupted");
    }

    tracing::info!("shutting down");
}
