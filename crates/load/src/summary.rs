use std::time::{Duration, Instant};

use mediacore_client::{TrackKind, TrackReport, TrackStats};

use crate::usage::Spend;

/// What one bot had to say when it was asked.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub identity: String,
    pub room: String,
    pub published: Vec<TrackStats>,
    pub received: Vec<TrackReport>,
    /// Why the server said goodbye, for a bot that is no longer connected.
    pub disconnect: Option<String>,
}

/// Everything that is a running total, so that a rung can be told from the run.
///
/// A bot's reports never reset -- they count from the moment it joined -- so a
/// ladder that read them straight would say that the twelfth rung lost the
/// packets the fourth one lost. Every number here subtracts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub published_tracks: u64,
    pub published_packets: u64,
    pub published_bytes: u64,
    pub received_tracks: u64,
    pub received_packets: u64,
    pub received_bytes: u64,
    pub missing: u64,
    pub duplicates: u64,
    pub reordered: u64,
    pub gaps: u64,
    pub switches: u64,
}

impl Counters {
    pub fn of(snapshots: &[Snapshot]) -> Self {
        let mut counters = Self::default();

        for snapshot in snapshots {
            for track in &snapshot.published {
                counters.published_tracks += 1;
                counters.published_packets += track.packets;
                counters.published_bytes += track.payload_bytes;
            }

            for report in &snapshot.received {
                counters.received_tracks += 1;
                counters.received_packets += report.packets;
                counters.received_bytes += report.payload_bytes;
                counters.missing += report.missing;
                counters.duplicates += report.duplicates;
                counters.reordered += report.reordered;
                counters.gaps += report.gaps;
                counters.switches += report.switch_count;
            }
        }

        counters
    }

    /// This reading minus an earlier one.
    ///
    /// The track counts are not differences: they are how many tracks were
    /// being published and received when this reading was taken, which is what
    /// a rung is asking about. Everything else is what happened since.
    pub fn since(&self, earlier: &Counters) -> Self {
        Self {
            published_tracks: self.published_tracks,
            published_packets: self
                .published_packets
                .saturating_sub(earlier.published_packets),
            published_bytes: self.published_bytes.saturating_sub(earlier.published_bytes),
            received_tracks: self.received_tracks,
            received_packets: self
                .received_packets
                .saturating_sub(earlier.received_packets),
            received_bytes: self.received_bytes.saturating_sub(earlier.received_bytes),
            missing: self.missing.saturating_sub(earlier.missing),
            duplicates: self.duplicates.saturating_sub(earlier.duplicates),
            reordered: self.reordered.saturating_sub(earlier.reordered),
            gaps: self.gaps.saturating_sub(earlier.gaps),
            switches: self.switches.saturating_sub(earlier.switches),
        }
    }

    /// Sequence numbers that never turned up, as a share of what was sent to us.
    ///
    /// Against what should have arrived -- the packets that did plus the ones
    /// missing from the sequence -- and not against what the publishers sent,
    /// because a subscriber is not sent every layer of every track and dividing
    /// by the fleet's output would call an unsubscribed layer packet loss.
    pub fn loss_percent(&self) -> f64 {
        let expected = self.received_packets + self.missing;
        if expected == 0 {
            return 0.0;
        }

        self.missing as f64 / expected as f64 * 100.0
    }
}

/// A spread of durations, for the timings that have one per track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub count: usize,
    pub min: Duration,
    pub p50: Duration,
    pub p95: Duration,
    pub max: Duration,
}

impl Stat {
    /// `None` for an empty set, which is not the same as a set of zeroes.
    ///
    /// A run where nothing was ever received has no time-to-first-packet, and
    /// printing `0.00ms` for it would be the most flattering possible way to
    /// report a total failure.
    pub fn of(values: &mut [Duration]) -> Option<Self> {
        if values.is_empty() {
            return None;
        }

        values.sort_unstable();

        Some(Self {
            count: values.len(),
            min: values[0],
            p50: percentile(values, 50),
            p95: percentile(values, 95),
            max: values[values.len() - 1],
        })
    }
}

/// The nearest-rank percentile: the smallest value at or above the share.
///
/// No interpolation between neighbours, because every value here was really
/// measured and a p95 that is the average of two tracks is a number no track
/// ever saw.
fn percentile(sorted: &[Duration], percent: usize) -> Duration {
    let rank = (sorted.len() * percent).div_ceil(100).max(1);
    sorted[rank.min(sorted.len()) - 1]
}

/// One line of the run: a fixed load held, or one rung of a ladder.
#[derive(Debug, Clone)]
pub struct Sample {
    pub label: String,
    pub bots: usize,
    pub rooms: usize,
    pub joined: usize,
    pub join_failures: usize,
    pub disconnected: usize,
    pub elapsed: Duration,
    pub counters: Counters,
    pub first_packet: Option<Stat>,
    pub jitter: Option<Stat>,
    pub switch_gap: Option<Stat>,
    pub idle: Option<Stat>,
    pub spend: Option<Spend>,
}

/// What a sample is measured against before the ladder decides to stop.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_loss: f64,
    pub max_cpu: f64,
    pub max_idle: Duration,
}

/// What gave, when something did.
#[derive(Debug, Clone, PartialEq)]
pub enum Gave {
    Joins { failures: usize },
    Disconnected { bots: usize },
    Loss { percent: f64 },
    Silence { tracks: usize, worst: Duration },
    BenchCpu { percent: f64 },
}

impl std::fmt::Display for Gave {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Gave::Joins { failures } => {
                write!(f, "{failures} bots could not join or publish")
            }
            Gave::Disconnected { bots } => {
                write!(f, "{bots} bots were disconnected by the server")
            }
            Gave::Loss { percent } => {
                write!(f, "packet loss across the bots reached {percent:.2}%")
            }
            Gave::Silence { tracks, worst } => write!(
                f,
                "{tracks} subscribed tracks went quiet, the worst for {}",
                millis(*worst)
            ),
            Gave::BenchCpu { percent } => write!(
                f,
                "the bots themselves ran out of cpu at {percent:.0}% of this machine -- \
                 the bench gave, not the server"
            ),
        }
    }
}

impl Sample {
    /// The first limit this sample crosses, in the order they are worth saying.
    ///
    /// The bots' own cpu is checked first on purpose. Every other reading in a
    /// sample taken on a pinned generator is suspect -- packets arrive late
    /// because nothing read the socket, tracks go quiet because nothing polled
    /// them -- so a run that reports loss while its own cpu is at the limit is
    /// reporting the wrong thing.
    pub fn gave(&self, limits: &Limits) -> Option<Gave> {
        if let Some(spend) = self.spend
            && spend.cpu_percent >= limits.max_cpu
        {
            return Some(Gave::BenchCpu {
                percent: spend.cpu_percent,
            });
        }

        if self.join_failures > 0 {
            return Some(Gave::Joins {
                failures: self.join_failures,
            });
        }

        if self.disconnected > 0 {
            return Some(Gave::Disconnected {
                bots: self.disconnected,
            });
        }

        let loss = self.counters.loss_percent();
        if loss > limits.max_loss {
            return Some(Gave::Loss { percent: loss });
        }

        if let Some(idle) = self.idle
            && idle.max > limits.max_idle
        {
            return Some(Gave::Silence {
                tracks: idle.count,
                worst: idle.max,
            });
        }

        None
    }

    pub fn print(&self) {
        let counters = &self.counters;

        println!(
            "  {} -- {} bots in {}, {} joined, {:.1}s",
            self.label,
            self.bots,
            rooms(self.rooms),
            self.joined,
            self.elapsed.as_secs_f64(),
        );
        println!(
            "    published  {} tracks  {} packets  {}",
            counters.published_tracks,
            counters.published_packets,
            bytes(counters.published_bytes),
        );
        println!(
            "    received   {} tracks  {} packets  {}  loss {:.3}%  dup {}  reordered {}",
            counters.received_tracks,
            counters.received_packets,
            bytes(counters.received_bytes),
            counters.loss_percent(),
            counters.duplicates,
            counters.reordered,
        );
        print_stat("    first packet ", "tracks", self.first_packet.as_ref());
        print_stat("    jitter       ", "tracks", self.jitter.as_ref());
        print_stat("    idle         ", "tracks", self.idle.as_ref());

        // The one line with two counts on it, and they are worth telling apart.
        // The first is the switches the subscriptions counted; the second is
        // how many of those the percentiles were taken over. They part company
        // when a switch happened between packets that carried no arrival time,
        // or when a track switched more than its history is deep and the oldest
        // fell out of the ring before the rung ended.
        match self.switch_gap.as_ref() {
            Some(gap) => println!(
                "    layer switch  {} switches, {} timed  min {}  p50 {}  p95 {}  max {}",
                counters.switches,
                gap.count,
                millis(gap.min),
                millis(gap.p50),
                millis(gap.p95),
                millis(gap.max),
            ),
            None if counters.switches > 0 => println!(
                "    layer switch  {} switches, none of them timed",
                counters.switches
            ),
            None => println!("    layer switch  none"),
        }

        match self.spend {
            Some(spend) => println!(
                "    the bots     {:.1}% of this machine's cpu, {} resident",
                spend.cpu_percent,
                bytes(spend.rss_bytes),
            ),
            None => println!("    the bots     cpu and rss unavailable off linux"),
        }

        if self.join_failures > 0 || self.disconnected > 0 {
            println!(
                "    trouble      {} bots never got in, {} were disconnected",
                self.join_failures, self.disconnected
            );
        }
    }

    pub fn csv_header() -> &'static str {
        "label,bots,rooms,joined,join_failures,disconnected,seconds,\
         published_tracks,published_packets,published_bytes,\
         received_tracks,received_packets,received_bytes,\
         missing,loss_percent,duplicates,reordered,gaps,switches,\
         first_packet_p50_ms,first_packet_p95_ms,first_packet_max_ms,\
         jitter_p50_ms,jitter_p95_ms,jitter_max_ms,\
         switch_gap_p50_ms,switch_gap_p95_ms,switch_gap_max_ms,\
         idle_p95_ms,idle_max_ms,cpu_percent,rss_bytes"
    }

    pub fn csv_row(&self) -> String {
        let counters = &self.counters;
        let mut row = String::new();

        row.push_str(&format!(
            "{},{},{},{},{},{},{:.3},",
            csv_field(&self.label),
            self.bots,
            self.rooms,
            self.joined,
            self.join_failures,
            self.disconnected,
            self.elapsed.as_secs_f64(),
        ));
        row.push_str(&format!(
            "{},{},{},{},{},{},{},{:.4},{},{},{},{},",
            counters.published_tracks,
            counters.published_packets,
            counters.published_bytes,
            counters.received_tracks,
            counters.received_packets,
            counters.received_bytes,
            counters.missing,
            counters.loss_percent(),
            counters.duplicates,
            counters.reordered,
            counters.gaps,
            counters.switches,
        ));
        row.push_str(&csv_stat(self.first_packet.as_ref(), true));
        row.push_str(&csv_stat(self.jitter.as_ref(), true));
        row.push_str(&csv_stat(self.switch_gap.as_ref(), true));
        row.push_str(&csv_stat(self.idle.as_ref(), false));

        match self.spend {
            Some(spend) => row.push_str(&format!("{:.2},{}", spend.cpu_percent, spend.rss_bytes)),
            None => row.push(','),
        }

        row
    }
}

/// What the scenario knows about a stretch and the reports do not.
#[derive(Debug, Clone)]
pub struct Frame {
    pub label: String,
    pub bots: usize,
    pub rooms: usize,
    pub joined: usize,
    pub join_failures: usize,
    /// When this stretch began, and what the counters read then.
    pub since: Instant,
    /// When the subscriptions this stretch is answering for started being made.
    ///
    /// Earlier than `since` on a ladder and equal to it on a fixed run, because
    /// a rung's own tracks are subscribed by the ramp that precedes its measured
    /// window while a fixed run opens before there is anything to subscribe.
    /// The two clocks are kept apart rather than collapsed into one: everything
    /// counted -- packets, loss, switches -- is a difference against `baseline`,
    /// which was read at `since`, and a track's time to its first packet is not
    /// a difference against anything, so the only way to say which stretch it
    /// belongs to is to say when the stretch went looking for it.
    pub arrivals_since: Instant,
    pub baseline: Counters,
    pub spend: Option<Spend>,
}

impl Sample {
    /// Everything a sample needs out of the bots, in one pass.
    ///
    /// A bot's reports never reset, so both of the things gathered here out of a
    /// track's history -- when it first heard anything, and the switches it
    /// still remembers -- would otherwise be the whole run's rather than this
    /// stretch's, and every rung of a ladder would re-report the first rung's
    /// worst case as its own. Each is scoped by the clock that fits it:
    /// time-to-first-packet by `frame.arrivals_since`, so a rung reports the
    /// subscriptions its own ramp made and not the ones already running when it
    /// started; layer switches by `frame.since`, so that the timed switches and
    /// the counted ones are the same switches.
    ///
    /// Jitter is not filtered either way and should not be: it is a moving
    /// estimate rather than a total, so what a track reports is what it is
    /// doing now.
    pub fn build(frame: Frame, snapshots: &[Snapshot], now: Instant) -> Self {
        let counters = Counters::of(snapshots).since(&frame.baseline);

        let mut first_packet = Vec::new();
        let mut jitter = Vec::new();
        let mut switch_gap = Vec::new();
        let mut idle = Vec::new();

        for snapshot in snapshots {
            for report in &snapshot.received {
                let timings = &report.timings;

                if let Some(first) = timings.first_arrival
                    && first >= frame.arrivals_since
                    && let Some(to_first) = timings.first_packet
                {
                    first_packet.push(to_first);
                }

                if let Some(estimate) = timings.jitter {
                    jitter.push(estimate);
                }

                if let Some(quiet) = timings.idle(now) {
                    idle.push(quiet);
                }

                switch_gap.extend(
                    report
                        .switches
                        .iter()
                        .filter(|switch| switch.at.is_some_and(|at| at >= frame.since))
                        .filter_map(|switch| switch.gap),
                );
            }
        }

        Self {
            label: frame.label,
            bots: frame.bots,
            rooms: frame.rooms,
            joined: frame.joined,
            join_failures: frame.join_failures,
            disconnected: snapshots
                .iter()
                .filter(|snapshot| snapshot.disconnect.is_some())
                .count(),
            elapsed: now.saturating_duration_since(frame.since),
            counters,
            first_packet: Stat::of(&mut first_packet),
            jitter: Stat::of(&mut jitter),
            switch_gap: Stat::of(&mut switch_gap),
            idle: Stat::of(&mut idle),
            spend: frame.spend,
        }
    }
}

/// The bots that are no longer in their room, and what they were told.
///
/// Named rather than counted, because "3 bots were disconnected" and "bot-7,
/// bot-8 and bot-9 in load-1 were all told ROOM_CLOSED" are different findings:
/// the first is load and the second is one room having ended under them.
pub fn casualties(snapshots: &[Snapshot]) -> Vec<String> {
    snapshots
        .iter()
        .filter_map(|snapshot| {
            let why = snapshot.disconnect.as_ref()?;
            Some(format!(
                "{} in {} ({why})",
                snapshot.identity, snapshot.room
            ))
        })
        .collect()
}

/// How many subscribed tracks each kind accounts for, for the header line.
pub fn kinds(snapshots: &[Snapshot]) -> (usize, usize) {
    let mut audio = 0;
    let mut video = 0;

    for snapshot in snapshots {
        for report in &snapshot.received {
            match report.kind {
                Some(TrackKind::Audio) => audio += 1,
                Some(TrackKind::Video) => video += 1,
                None => {}
            }
        }
    }

    (audio, video)
}

fn print_stat(label: &str, unit: &str, stat: Option<&Stat>) {
    match stat {
        Some(stat) => println!(
            "{label} {} {unit}  min {}  p50 {}  p95 {}  max {}",
            stat.count,
            millis(stat.min),
            millis(stat.p50),
            millis(stat.p95),
            millis(stat.max),
        ),
        None => println!("{label} nothing measured"),
    }
}

fn csv_stat(stat: Option<&Stat>, with_p50: bool) -> String {
    match (stat, with_p50) {
        (Some(stat), true) => format!(
            "{:.3},{:.3},{:.3},",
            ms(stat.p50),
            ms(stat.p95),
            ms(stat.max)
        ),
        (Some(stat), false) => format!("{:.3},{:.3},", ms(stat.p95), ms(stat.max)),
        (None, true) => ",,,".to_owned(),
        (None, false) => ",,".to_owned(),
    }
}

/// A label is the only free text in a row, and a comma in one would shift it.
fn csv_field(value: &str) -> String {
    if value.contains(',') || value.contains('"') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

pub fn rooms(count: usize) -> String {
    match count {
        1 => "1 room".to_owned(),
        count => format!("{count} rooms"),
    }
}

pub fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

pub fn millis(duration: Duration) -> String {
    format!("{:.2}ms", ms(duration))
}

pub fn bytes(count: u64) -> String {
    match count {
        count if count >= 1 << 30 => format!("{:.2} GiB", count as f64 / (1u64 << 30) as f64),
        count if count >= 1 << 20 => format!("{:.2} MiB", count as f64 / (1u64 << 20) as f64),
        count if count >= 1 << 10 => format!("{:.2} KiB", count as f64 / (1u64 << 10) as f64),
        count => format!("{count} B"),
    }
}

#[cfg(test)]
#[path = "summary_tests.rs"]
mod tests;
