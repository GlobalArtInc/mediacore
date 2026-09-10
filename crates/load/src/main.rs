//! A crowd of bots against a mediacore server, and what it costs them.
//!
//! Everything in here rides on `crates/client`: a bot is a `Client` that
//! publishes the same synthetic Opus and simulcast VP8 the M2 to M5 tests
//! publish, and the numbers at the end come out of the same `TrackReport` those
//! tests assert against. What this adds is the outside: tokens, two scenarios,
//! a summary, and the bots' own cost beside the server's.
//!
//! It does NOT answer anything about bandwidth estimation. `video.bwe.enabled`
//! wants a run over a link that is actually narrow, and a hundred bots on
//! loopback constrain nobody's link -- they run out of cpu long before anything
//! runs out of bandwidth, which is a different question with a different answer.

mod bot;
mod scenario;
mod summary;
mod token;
mod usage;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use mediacore_client::ClientOptions;
use tracing_subscriber::EnvFilter;

use crate::bot::Media;
use crate::scenario::{Fleet, Recorder, Rungs, Settings, Stop};
use crate::summary::Limits;
use crate::token::Tokens;

/// How much of every received track is kept, in packets.
///
/// Far below the client's own default of 128. The window is a per-track ring of
/// whole payloads, and a room of twenty bots is nineteen subscriptions each:
/// at the default a simulcast run would carry hundreds of megabytes of RTP
/// payload that nothing in here ever reads. Nothing in the summary comes out of
/// the window -- the counters and the timings are kept as numbers -- so this is
/// only what is left for a caller who wants to look at a payload.
const PAYLOAD_WINDOW: usize = 8;

#[derive(Parser)]
#[command(
    name = "mediacore-load",
    version,
    about = "Bots against a mediacore server: fixed load, or a ladder until something gives"
)]
struct Cli {
    #[command(flatten)]
    common: Common,

    #[command(subcommand)]
    scenario: Scenario,
}

#[derive(Args, Clone)]
struct Common {
    /// The signalling url of the server under test.
    #[arg(
        long,
        global = true,
        env = "MEDIACORE_URL",
        default_value = "ws://127.0.0.1:7880"
    )]
    url: String,

    /// The api key to mint tokens with.
    #[arg(
        long,
        global = true,
        env = "MEDIACORE_API_KEY",
        default_value = "devkey"
    )]
    api_key: String,

    /// Its secret. Without it, bring tokens with --token.
    #[arg(long, global = true, env = "MEDIACORE_API_SECRET", default_value = "")]
    api_secret: String,

    /// A ready token, once per bot. Replaces minting entirely.
    #[arg(long = "token", global = true)]
    tokens: Vec<String>,

    /// How many rooms the bots are spread across.
    #[arg(long, global = true, default_value_t = 1)]
    rooms: usize,

    #[arg(long, global = true, default_value = "load")]
    room_prefix: String,

    #[arg(long, global = true, default_value = "bot")]
    identity_prefix: String,

    /// Publish no audio track.
    #[arg(long, global = true)]
    no_audio: bool,

    /// Publish no video track.
    #[arg(long, global = true)]
    no_video: bool,

    /// Publish one video layer instead of the three-rung simulcast ladder.
    #[arg(long, global = true)]
    no_simulcast: bool,

    /// How long to wait between two bots joining.
    #[arg(long, global = true, default_value_t = 100)]
    join_gap_ms: u64,

    /// How often every bot asks for a different video quality. 0 turns it off.
    ///
    /// This is what makes the forwarder move subscribers between simulcast
    /// layers, and so the only thing in a bench that gives the layer-switch
    /// latency anything to measure.
    #[arg(long, global = true, default_value_t = 0)]
    churn_secs: u64,

    /// How long one bot has to connect, publish and negotiate.
    #[arg(long, global = true, default_value_t = 20)]
    patience_secs: u64,

    /// How long a minted token is good for.
    #[arg(long, global = true, default_value_t = 3_600)]
    token_ttl_secs: u64,

    /// Write the samples here as CSV as well as printing them.
    #[arg(long, global = true)]
    csv: Option<PathBuf>,

    /// The client's own log level. The bench's output is not a log.
    #[arg(long, global = true, default_value = "warn")]
    log: String,
}

#[derive(Subcommand)]
enum Scenario {
    /// Hold a stated load for a while and report what it did.
    Fixed {
        #[arg(long, default_value_t = 4)]
        bots: usize,

        #[arg(long, default_value_t = 30)]
        seconds: u64,
    },

    /// Add bots until something gives, and print the ceiling and what gave.
    Ladder {
        #[arg(long, default_value_t = 4)]
        start: usize,

        #[arg(long, default_value_t = 4)]
        step: usize,

        #[arg(long, default_value_t = 64)]
        max: usize,

        /// How long each rung is held before it is judged.
        #[arg(long, default_value_t = 15)]
        hold: u64,

        /// Packet loss across the bots that ends the climb, in percent.
        #[arg(long, default_value_t = 2.0)]
        max_loss: f64,

        /// The bots' own cpu that ends the climb, as a share of this machine.
        #[arg(long, default_value_t = 85.0)]
        max_cpu: f64,

        /// How long a subscribed track may go quiet before it counts as stalled.
        #[arg(long, default_value_t = 3)]
        max_idle: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(&cli.common.log);

    let common = &cli.common;
    let media = Media {
        audio: !common.no_audio,
        video: !common.no_video,
        simulcast: !common.no_simulcast,
    };

    if media.tracks() == 0 {
        println!("note: --no-audio and --no-video together, so the bots publish nothing at all");
    }

    let tokens = match common.tokens.is_empty() {
        false => Tokens::Given(common.tokens.clone()),
        true => Tokens::mint(
            common.api_key.clone(),
            common.api_secret.clone(),
            Duration::from_secs(common.token_ttl_secs),
        )?,
    };

    let patience = Duration::from_secs(common.patience_secs);
    let options = ClientOptions {
        connect_timeout: patience,
        response_timeout: patience,
        payload_window: PAYLOAD_WINDOW,
        ..ClientOptions::default()
    };

    let fleet = Fleet::new(
        Settings {
            url: common.url.clone(),
            media,
            options,
            patience,
            rooms: common.rooms,
            room_prefix: common.room_prefix.clone(),
            identity_prefix: common.identity_prefix.clone(),
            join_gap: Duration::from_millis(common.join_gap_ms),
            churn: Duration::from_secs(common.churn_secs),
        },
        tokens,
    );

    let mut recorder = Recorder::create(common.csv.as_deref())?;
    let mut stop = Stop::on_ctrl_c();

    match cli.scenario {
        Scenario::Fixed { bots, seconds } => {
            scenario::fixed(
                fleet,
                bots,
                Duration::from_secs(seconds),
                &mut recorder,
                &mut stop,
            )
            .await
        }
        Scenario::Ladder {
            start,
            step,
            max,
            hold,
            max_loss,
            max_cpu,
            max_idle,
        } => {
            scenario::ladder(
                fleet,
                Rungs {
                    start,
                    step,
                    max,
                    hold: Duration::from_secs(hold),
                },
                Limits {
                    max_loss,
                    max_cpu,
                    max_idle: Duration::from_secs(max_idle),
                },
                &mut recorder,
                &mut stop,
            )
            .await
        }
    }
}

/// The client's own tracing, kept quiet.
///
/// A bench prints a report, and a report with a hundred `subscriber transport
/// connected` lines through the middle of it is not a report. `--log debug` or
/// `RUST_LOG` puts them back when a run is going wrong and the question has
/// become why.
fn init_tracing(level: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(format!("mediacore_load={level},mediacore_client={level}"))
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
