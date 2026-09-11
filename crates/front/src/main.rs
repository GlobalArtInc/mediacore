use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use anyhow::Context as _;
use mediacore_front::{Config, config, http};
use tracing_subscriber::EnvFilter;

/// The word that turns this binary into its own healthcheck.
const HEALTHCHECK: &str = "healthcheck";

/// What the check reads.
const HEALTH_PATH: &str = "/healthz";

/// The whole of a healthy answer, and the check insists on the whole of it.
///
/// The front end holds nothing to count -- no rooms, no records, no queue -- so
/// its answer has been these two characters since it had the route at all, and a
/// check that accepted anything ending in them would accept `not ok`. The SFU's
/// health line grows and its check looks for one field of it; this one does not
/// and does not have to. If the answer here ever grows, this grows with it.
const HEALTH_BODY: &str = "ok";

/// How long the check waits for the answer. See the server's own; same reason.
const HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(2);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // `mediacore-front healthcheck` asks the front end in this container
    // whether it is answering, and is what the compose healthcheck runs: the
    // image carries no curl and no shell to run one in. It is checked here,
    // before anything else, because a healthcheck has to work in a process that
    // knows nothing -- no api secret, no WebSocket url, no tracing.
    //
    // Anything else on the command line is ignored, exactly as it was before
    // this binary read its arguments at all. A deployment that passes this
    // process a flag it has never read is a deployment that works today, and a
    // fresh refusal here would turn it into a crash loop.
    if std::env::args().nth(1).as_deref() == Some(HEALTHCHECK) {
        return healthcheck().await;
    }

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

/// Whether the front end on this host is working, as an exit code.
///
/// The port comes from the same code the front end binds from -- one reading of
/// `MEDIACORE_FRONT_PORT`, in `config::port_from` -- so the two cannot disagree
/// about which port to ask on. Reading the variable here as well is what had
/// them disagree: `MEDIACORE_FRONT_PORT=` is silence to the loader, which binds
/// 8080 and answers `ok`, and was a value to this check, which could not parse
/// it and reported a working front end unhealthy for ever. Unlike the SFU's
/// config, that variable is one the container was created with, so it is here in
/// a fresh process. Everything else the front end needs is deliberately not
/// read: a check that wanted the api secret would be a check that fails for a
/// reason that has nothing to do with whether the page is being served.
async fn healthcheck() -> anyhow::Result<()> {
    let port = config::port_from_env()?;
    let url = format!("http://127.0.0.1:{port}{HEALTH_PATH}");

    let answer = reqwest::Client::builder()
        .timeout(HEALTHCHECK_TIMEOUT)
        // The container's environment may name an outbound proxy -- this binary
        // reaches an identity provider through one when it is told to -- and
        // reqwest reads it. A proxy asked for 127.0.0.1 answers about itself.
        .no_proxy()
        .build()?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("asking {url}"))?;

    let status = answer.status();
    let body = answer.text().await.unwrap_or_default();

    anyhow::ensure!(status.is_success(), "{url} answered {status}: {body}");
    anyhow::ensure!(
        body.trim() == HEALTH_BODY,
        "{url} answered {status} and `{body}` rather than `{HEALTH_BODY}`, so whatever is on that \
         port is not the front end"
    );

    Ok(())
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
