use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use mediacore_client::ClientOptions;
use tokio::sync::watch;

use mediacore_client::proto::VideoQuality;

use crate::bot::{Bot, Media, Plan};
use crate::summary::{Counters, Frame, Gave, Limits, Sample, Snapshot, casualties, kinds, rooms};
use crate::token::Tokens;
use crate::usage::Usage;

/// A ctrl-c that is a question the run asks rather than an exit.
///
/// The point of the whole binary is the summary, and a bench interrupted after
/// nine minutes of a ten minute run still knows nine minutes' worth. So the
/// signal sets a flag and every wait in here watches it: whoever is waiting
/// stops waiting, the caller finishes the sample it is in the middle of, prints
/// it, and returns.
#[derive(Clone)]
pub struct Stop {
    asked: watch::Receiver<bool>,
}

impl Stop {
    pub fn on_ctrl_c() -> Self {
        let (asked, watching) = watch::channel(false);

        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("\ninterrupted -- winding the bots up and printing what there is");
                let _ = asked.send(true);
            }
        });

        Self { asked: watching }
    }

    pub fn asked(&self) -> bool {
        *self.asked.borrow()
    }

    /// Waits out `duration`, or until the interrupt. `true` means interrupted.
    pub async fn wait(&mut self, duration: Duration) -> bool {
        if self.asked() {
            return true;
        }

        tokio::select! {
            _ = tokio::time::sleep(duration) => false,
            _ = self.asked.changed() => true,
        }
    }
}

/// How a fleet is built: who joins what, and how fast.
#[derive(Clone)]
pub struct Settings {
    pub url: String,
    pub media: Media,
    pub options: ClientOptions,
    pub patience: Duration,
    pub rooms: usize,
    pub room_prefix: String,
    pub identity_prefix: String,
    pub join_gap: Duration,
    /// How often every bot asks for a different video quality, or zero.
    pub churn: Duration,
}

/// The bots, and everything they were built from.
pub struct Fleet {
    settings: Settings,
    tokens: Tokens,
    bots: Vec<Bot>,
    dressed: usize,
    join_failures: usize,
}

impl Fleet {
    pub fn new(settings: Settings, tokens: Tokens) -> Self {
        Self {
            settings,
            tokens,
            bots: Vec::new(),
            dressed: 0,
            join_failures: 0,
        }
    }

    pub fn rooms(&self) -> usize {
        self.settings.rooms.max(1)
    }

    pub fn joined(&self) -> usize {
        self.bots.len()
    }

    pub fn join_failures(&self) -> usize {
        self.join_failures
    }

    /// Refuses a run the tokens cannot dress, before a socket is opened.
    pub fn check_capacity(&self, wanted: usize) -> Result<()> {
        match self.tokens.capacity() {
            Some(capacity) if capacity < wanted => bail!(
                "{wanted} bots were asked for and {capacity} tokens were given; pass more \
                 --token, or an --api-secret to mint them"
            ),
            _ => Ok(()),
        }
    }

    /// Brings the fleet up to `target` bots, one at a time.
    ///
    /// One at a time and with a gap, because a hundred DTLS handshakes started
    /// in the same millisecond measure the handshake and not the forwarding --
    /// and because a bench that cannot tell an overloaded server from a
    /// thundering herd it made itself is not measuring the server.
    ///
    /// A bot that cannot get in is counted rather than fatal: how many failed to
    /// join is one of the things the ladder is looking for.
    pub async fn grow_to(&mut self, target: usize, stop: &mut Stop) -> usize {
        let mut joined = 0;

        while self.dressed < target && !stop.asked() {
            let index = self.dressed;
            self.dressed += 1;

            let room = format!("{}-{}", self.settings.room_prefix, index % self.rooms());
            let identity = format!("{}-{index}", self.settings.identity_prefix);

            let plan = match self.tokens.issue(index, &identity, &room) {
                Ok(token) => Plan {
                    identity,
                    room,
                    token,
                },
                Err(error) => {
                    tracing::warn!(%error, "a bot has no token");
                    self.join_failures += 1;
                    continue;
                }
            };

            match Bot::join(
                plan,
                &self.settings.url,
                self.settings.media,
                self.settings.options.clone(),
                self.settings.patience,
            )
            .await
            {
                Ok(bot) => {
                    self.bots.push(bot);
                    joined += 1;
                }
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), "a bot did not get in");
                    self.join_failures += 1;
                }
            }

            let gap = self.settings.join_gap;
            if !gap.is_zero() && stop.wait(gap).await {
                break;
            }
        }

        joined
    }

    /// Asks every bot for `quality` on every video track it receives.
    pub fn ask_quality(&self, quality: VideoQuality) {
        for bot in &self.bots {
            bot.ask_quality(quality);
        }
    }

    pub async fn snapshots(&self) -> Vec<Snapshot> {
        let mut snapshots = Vec::with_capacity(self.bots.len());
        for bot in &self.bots {
            snapshots.push(bot.snapshot().await);
        }
        snapshots
    }

    pub async fn shut_down(self) {
        for bot in self.bots {
            bot.leave().await;
        }
    }
}

/// The CSV beside the printed run.
///
/// Every row is flushed as it is written rather than at the end, so that a run
/// stopped with ctrl-c -- or killed outright, which is what happens to a bench
/// that pushed a machine into swap -- leaves a file with every rung it finished
/// in it.
pub struct Recorder {
    file: Option<std::fs::File>,
}

impl Recorder {
    pub fn create(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path else {
            return Ok(Self { file: None });
        };

        let mut file = std::fs::File::create(path)
            .with_context(|| format!("cannot write {}", path.display()))?;
        writeln!(file, "{}", Sample::csv_header())?;

        Ok(Self { file: Some(file) })
    }

    pub fn record(&mut self, sample: &Sample) -> Result<()> {
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };

        writeln!(file, "{}", sample.csv_row())?;
        file.flush()?;
        Ok(())
    }
}

/// Where a measured stretch began: the clock, the counters, the cpu.
///
/// Taken apart from [`hold`] so that the two scenarios can start measuring in
/// different places. A fixed run begins before the first bot exists, and its
/// numbers are the whole run including the ramp; a rung of a ladder begins once
/// its bots are in, and its numbers are that rung's alone.
struct Stretch {
    since: Instant,
    arrivals_since: Instant,
    baseline: Counters,
    usage: Option<Usage>,
}

impl Stretch {
    /// A stretch that opens before whatever it is going to measure exists.
    async fn begin(fleet: &Fleet) -> Self {
        Self::after_ramp(fleet, Instant::now()).await
    }

    /// A stretch that opens now, over the tracks a ramp begun at `ramp` made.
    ///
    /// The two instants are what a ladder needs and a fixed run does not. A
    /// rung wants to be judged on the load once it is up, so its counters and
    /// its clock start after the ramp -- but the subscriptions the rung is
    /// asking about are the ones that ramp created, and they got their first
    /// packets while it was still running. Told only when the rung's window
    /// opened, time-to-first-packet would throw away every track the rung
    /// added and keep the handful belonging to whichever bot joined last.
    async fn after_ramp(fleet: &Fleet, ramp: Instant) -> Self {
        let baseline = Counters::of(&fleet.snapshots().await);

        Self {
            baseline,
            usage: Usage::read(),
            since: Instant::now(),
            arrivals_since: ramp,
        }
    }
}

/// Holds the load for `duration`, churning the quality if that was asked for.
///
/// The wait is broken into churn-sized pieces rather than run as one sleep,
/// because asking for a different layer is the only thing in a bench that makes
/// the forwarder switch one -- and the interrupt has to be watched in every one
/// of those pieces or ctrl-c would wait out the whole rung.
async fn hold(
    stretch: Stretch,
    label: String,
    fleet: &Fleet,
    duration: Duration,
    stop: &mut Stop,
) -> (Sample, Vec<Snapshot>) {
    // Counted from here rather than from where the stretch began, because a
    // fixed run starts measuring before the first bot exists: a deadline off
    // `stretch.since` would spend a minute of a two minute run joining and hold
    // the load for the other one. The sample still reports the whole stretch,
    // so its elapsed time is the ramp plus this.
    let deadline = Instant::now() + duration;
    let mut wanted = VideoQuality::Low;

    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }

        let step = match fleet.settings.churn.is_zero() {
            true => left,
            false => fleet.settings.churn.min(left),
        };

        if stop.wait(step).await {
            break;
        }

        if !fleet.settings.churn.is_zero() {
            wanted = match wanted {
                VideoQuality::Low => VideoQuality::High,
                _ => VideoQuality::Low,
            };
            fleet.ask_quality(wanted);
        }
    }

    let snapshots = fleet.snapshots().await;
    let now = Instant::now();
    let spend = Usage::read()
        .zip(stretch.usage)
        .map(|(after, before)| after.since(&before));

    let sample = Sample::build(
        Frame {
            label,
            bots: fleet.joined() + fleet.join_failures(),
            rooms: fleet.rooms(),
            joined: fleet.joined(),
            join_failures: fleet.join_failures(),
            since: stretch.since,
            arrivals_since: stretch.arrivals_since,
            baseline: stretch.baseline,
            spend,
        },
        &snapshots,
        now,
    );

    (sample, snapshots)
}

pub async fn fixed(
    mut fleet: Fleet,
    bots: usize,
    duration: Duration,
    recorder: &mut Recorder,
    stop: &mut Stop,
) -> Result<()> {
    fleet.check_capacity(bots)?;

    println!(
        "joining {bots} bots across {} at {}",
        rooms(fleet.rooms()),
        fleet.settings.url
    );

    // Begun before the first bot exists, so that the sample covers the whole
    // run: a fixed load is one measurement, and a time to first packet that
    // left out every track established during the ramp would leave out the
    // arrivals worth having.
    let stretch = Stretch::begin(&fleet).await;
    fleet.grow_to(bots, stop).await;
    println!(
        "{} of {bots} bots are in; holding for {:.0}s",
        fleet.joined(),
        duration.as_secs_f64()
    );

    let (sample, snapshots) = hold(stretch, "fixed".to_owned(), &fleet, duration, stop).await;
    announce(&sample, &snapshots);
    recorder.record(&sample)?;

    fleet.shut_down().await;
    Ok(())
}

/// How far a ladder climbs and how fast.
#[derive(Debug, Clone, Copy)]
pub struct Rungs {
    pub start: usize,
    pub step: usize,
    pub max: usize,
    pub hold: Duration,
}

pub async fn ladder(
    mut fleet: Fleet,
    rungs: Rungs,
    limits: Limits,
    recorder: &mut Recorder,
    stop: &mut Stop,
) -> Result<()> {
    fleet.check_capacity(rungs.start)?;

    let mut rung = rungs.start.max(1);
    let mut held: Option<usize> = None;
    let mut gave: Option<(usize, Gave)> = None;

    loop {
        if fleet.check_capacity(rung).is_err() {
            println!(
                "stopping at {} bots: there are no more tokens",
                fleet.joined()
            );
            break;
        }

        let ramp = Instant::now();
        fleet.grow_to(rung, stop).await;

        // After the bots are in, so the rung is judged on what it did once it
        // was up rather than on the ramp that got it there -- but told when
        // that ramp started, because the subscriptions it made are the ones
        // this rung is answering for.
        let stretch = Stretch::after_ramp(&fleet, ramp).await;
        let (sample, snapshots) =
            hold(stretch, format!("rung {rung}"), &fleet, rungs.hold, stop).await;
        announce(&sample, &snapshots);
        recorder.record(&sample)?;

        if let Some(what) = sample.gave(&limits) {
            gave = Some((rung, what));
            break;
        }

        held = Some(rung);

        if stop.asked() {
            println!("stopped before the next rung");
            break;
        }

        if rung >= rungs.max {
            break;
        }

        rung = (rung + rungs.step.max(1)).min(rungs.max);
    }

    println!();
    match (held, gave) {
        (held, Some((rung, what))) => {
            println!(
                "the ceiling on this machine is {} bots in {}",
                held.unwrap_or(0),
                rooms(fleet.rooms())
            );
            println!("at {rung} bots: {what}");
        }
        (Some(held), None) => println!(
            "no ceiling found: {held} bots in {} held everything asked of them",
            rooms(fleet.rooms())
        ),
        (None, None) => println!("no rung completed"),
    }

    fleet.shut_down().await;
    Ok(())
}

/// How many of the disconnected bots are named before the list is cut short.
///
/// A rung where the server threw everybody out would otherwise print a hundred
/// lines of the same reason, and the reason is the finding.
const NAMED_CASUALTIES: usize = 5;

fn announce(sample: &Sample, snapshots: &[Snapshot]) {
    let (audio, video) = kinds(snapshots);
    sample.print();
    println!("    subscribed   {audio} audio tracks, {video} video");

    let gone = casualties(snapshots);
    if !gone.is_empty() {
        let named = gone.len().min(NAMED_CASUALTIES);
        println!("    gone         {}", gone[..named].join(", "));
        if gone.len() > named {
            println!("    gone         and {} more", gone.len() - named);
        }
    }
}
