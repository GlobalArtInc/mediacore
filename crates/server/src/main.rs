use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use mediacore_config::Config;
use mediacore_proto::mediacore;
use mediacore_server::{http, state::AppState};
use tracing_subscriber::EnvFilter;

mod check;

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

/// What `healthcheck` reads.
const HEALTH_PATH: &str = "/healthz";

/// The field `healthcheck` insists on finding in the answer.
///
/// A substring rather than the whole line, and deliberately so: the health line
/// grows on the end (see `mediacore_server::metrics`), and a check that pinned
/// the whole of it would call a working server unhealthy the day somebody adds a
/// counter. `rooms=` is the first field it has ever had, and finding it is what
/// tells a 200 from this server apart from a 200 from whatever else may end up
/// on the port.
const HEALTH_FIELD: &str = "rooms=";

/// How long `healthcheck` waits for the answer.
///
/// Shorter than the three seconds `deploy/docker-compose.yml` gives the check,
/// so a server that accepts the connection and then says nothing is reported by
/// this process -- with the url in the message -- rather than killed by docker
/// with nothing said at all.
const HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Parser)]
#[command(name = "mediacore", version, about = "Selective forwarding unit")]
struct Cli {
    // `global = true`, so that both orderings work:
    // `mediacore --config /etc/mediacore/mediacore.yaml check --expect-postgres`
    // and `mediacore check --config /etc/mediacore/mediacore.yaml`. The second
    // is the one a chart author writes first -- it reads as the flag belonging
    // to the subcommand that uses it -- and parent-only made it
    // `error: unexpected argument '--config' found` over an argument order,
    // in the `args:` of an init container, where a clap usage line is all the
    // evidence there is.
    //
    // It leaves `healthcheck --config <path>` accepted and ignored, which is no
    // new hazard: the parent position was always accepted and ignored there,
    // and a healthcheck that read this file would refuse to start on a
    // perfectly healthy server (see the subcommand below for why).
    #[arg(
        long,
        short,
        global = true,
        env = "MEDIACORE_CONFIG",
        default_value = "config.yaml"
    )]
    config: String,
    #[command(subcommand)]
    command: Option<Command>,
}

/// Two subcommands, and neither of them starts a server. Without one this binary
/// is the server, exactly as it was before there was a subcommand at all.
#[derive(Subcommand)]
enum Command {
    /// Ask the server in this container whether it is answering. Exit 0 if it
    /// is, and print why not if it is not.
    Healthcheck {
        /// The signalling port to ask on.
        //
        // Not read out of the config file, and that is the whole point: the
        // config is rendered from the environment as it is loaded, and a
        // healthcheck is a fresh process holding only what the container was
        // created with. `MEDIACORE_ICE_SERVERS_BLOCK` and the rest are exported
        // by `deploy/start-sfu.sh` into the server's own environment and
        // nowhere else, so a check that loaded the config would refuse to start
        // on a perfectly healthy server, by name, and report it as unhealthy.
        //
        // The default is `mediacore-config`'s own default, taken from it rather
        // than written here a second time: a config that says nothing about
        // `port:` gets that number, so this asks on it, and a 7880 copied into
        // this file is a number that agrees until somebody changes the other
        // one. A config that does name a port names it here too --
        // `healthcheck --port <n>` -- which is the one thing this cannot work
        // out for itself, and is what the compose files' healthchecks say.
        #[arg(long, default_value_t = Config::default().port)]
        port: u16,
    },

    /// Whether the config this server would read is the one the deployment
    /// asked for. Exit 0 if it is, and print what is wrong if it is not.
    //
    // The counterpart of `healthcheck` and its opposite in one respect: this
    // one reads the config and nothing else, where that one deliberately reads
    // no config at all. It exists to be an init container -- the chart's is
    // `/bin/sh -c` running envsubst today, and that shell is the last thing
    // keeping the published image on Debian. `crates/server/src/check.rs` has
    // the whole of the reasoning, including why these three are not in
    // `Config::validate`.
    //
    // Every flag is a statement of intent and none of them is a default: a check
    // with no flags still reads the file, fills in the `${MEDIACORE_*}` and runs
    // the server's own validation, which is most of what an init container is
    // for.
    //
    // Each one overrides itself, which is clap's way of saying a flag may be
    // written twice. The caller is a Helm template, and recording implies a
    // database -- an upload is a row several nodes claim -- so a chart that
    // emits `--expect-postgres` for its database and again for its recording is
    // the obvious way to write it and says nothing contradictory. Without this
    // that install fails with `cannot be used multiple times` and no hint as to
    // which condition is the duplicate.
    Check {
        /// Meetings belong in Postgres: refuse an empty `sessions.postgres.url`,
        /// which would silently keep them in a directory instead.
        #[arg(long, overrides_with = "expect_postgres")]
        expect_postgres: bool,

        /// Meetings are recorded: refuse an object store with no bucket or no
        /// credentials, which would record nothing and say nothing.
        #[arg(long, overrides_with = "expect_recording")]
        expect_recording: bool,

        /// Media is advertised to clients off this node: refuse the wildcard,
        /// loopback and link-local. Private addresses are not refused.
        #[arg(long, overrides_with = "routable_advertise")]
        routable_advertise: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Healthcheck { port }) => return healthcheck(port).await,
        // Tracing is deliberately not started for this one, exactly as it is not
        // for the healthcheck: what a check writes on its way out is the whole
        // of what somebody reads with `kubectl logs` afterwards, and a log line
        // about something else on the same stream is noise in the one message
        // that had to be read.
        Some(Command::Check {
            expect_postgres,
            expect_recording,
            routable_advertise,
        }) => {
            return check::run(
                &cli.config,
                check::Expected {
                    postgres: expect_postgres,
                    recording: expect_recording,
                    routable_advertise,
                },
            );
        }
        None => {}
    }

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

/// Whether the server on this host is working, as an exit code.
///
/// This is the container healthcheck. It is a subcommand of the server rather
/// than `curl` because the image carries neither curl nor a shell to run one in
/// -- and because the thing that has to be checked is this server, so the
/// process that knows what a healthy answer looks like is this one. Nothing
/// about it is specific to docker: `docker exec … healthcheck`, a compose
/// `test`, and an `exec` probe in a chart all read the same exit code.
///
/// It asks over the loopback address on purpose. A healthcheck that resolved a
/// name would be a healthcheck that fails when DNS does, on a server that is
/// perfectly able to hold the meetings it already has.
async fn healthcheck(port: u16) -> anyhow::Result<()> {
    let url = format!("http://127.0.0.1:{port}{HEALTH_PATH}");
    let answer = reqwest::Client::builder()
        .timeout(HEALTHCHECK_TIMEOUT)
        // The one setting here that is not obvious, and the one that matters
        // most. This container's environment may well name an outbound proxy --
        // `deploy/.env` carries `HTTPS_PROXY` for webhooks that leave the host
        // -- and reqwest reads it. Asking a proxy for 127.0.0.1 asks it about
        // itself, so the check would answer about the wrong machine, or fail
        // while this server is fine.
        .no_proxy()
        .build()?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("asking {url}"))?;

    let status = answer.status();
    // Read before the status is judged, so a refusal can quote what was said:
    // a proxy's error page or another service's 404 is the answer to the
    // question "what is on this port", and it is worth printing.
    let body = answer.text().await.unwrap_or_default();

    anyhow::ensure!(status.is_success(), "{url} answered {status}: {body}");
    anyhow::ensure!(
        body.contains(HEALTH_FIELD),
        "{url} answered {status}, but the body has no `{HEALTH_FIELD}` in it, so whatever is on \
         that port is not this server: {body}"
    );

    Ok(())
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
    tracing::info!(
        rooms = rooms.len(),
        "draining: this node stops taking arrivals"
    );

    // Each one ends as a meeting somebody watched end -- `Ending::Closed` at
    // the time it really stopped -- rather than being found unfinished later
    // and recorded as a restart with an end time nobody observed.
    for room in &rooms {
        state
            .registry
            .close(&room.name, mediacore::DisconnectReason::ServerShutdown);
    }

    if !rooms.is_empty() {
        tracing::info!(rooms = rooms.len(), "every meeting on this node was closed");
    }

    // Last, and after the rooms: a node that took its row away while it still
    // held meetings would have the others believe those rooms were free.
    state.cluster.leave();
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
