//! One reading of everything this node counts, and the two dialects it is read
//! out in.
//!
//! `/healthz` and `/metrics` answer the same question to two different readers,
//! and the failure worth designing against is the one where they stop agreeing:
//! a number added to the line and not to the exposition is a number no alert
//! will ever see, and two handlers each gathering their own totals half a
//! second apart disagree even when nobody has touched either of them. So the
//! numbers are gathered once, into a [`Snapshot`], and each handler is a
//! renderer over it.
//!
//! **The health line is a wire format.** A compose healthcheck curls it and
//! several test files parse it field by field, so its shape is not free to
//! change: [`Snapshot::health_line`] is pinned whole in `metrics_tests.rs`, and
//! a field renamed there breaks a test rather than a deployment. The exposition
//! is younger and may grow -- growing is the only thing it may do, because a
//! series that disappears takes a dashboard and an alert with it.
//!
//! **No room, participant or track labels.** Every series here is one value for
//! the whole node. Rooms are named by whoever created them, and a label per
//! room would write the cardinality of this server's busiest hour into a time
//! series database that keeps it for ever -- along with the names of the
//! meetings, to anybody who can reach the scrape. The only labels in the whole
//! document are this node's own id, version and region, on
//! `mediacore_build_info`.

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;

use crate::state::AppState;

/// What a scrape is told it is reading. Naming the version is what lets a
/// collector pick a parser instead of sniffing one, and every Prometheus-shaped
/// agent recognises `0.0.4`.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Whether this node can record, and whether it still can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorder {
    /// This node records nothing. A deployment that means to record and has a
    /// configuration problem looks like this too, which is otherwise discovered
    /// a week later when somebody asks for a recording.
    Off,
    On,
    /// The one thread that writes files is gone, so this node is holding
    /// meetings and recording none of them. Nothing restarts it -- what it was
    /// writing is on the disk for the next start to finish, and the answer is
    /// to restart the process. The one to alert on.
    Stopped,
}

impl Recorder {
    /// The word the health line has always carried.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
            Self::Stopped => "stopped",
        }
    }
}

/// Everything this node counts, as it was at one instant.
///
/// Owned rather than borrowed, and gathered by [`snapshot`], so that a
/// renderer cannot hold a lock, cannot ask the registry a second question
/// halfway through a line, and cannot answer two fields from two different
/// moments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub rooms: usize,
    pub participants: usize,
    pub publishers: usize,
    pub rtp_packets: u64,
    pub rtp_bytes: u64,
    pub tracks: usize,
    pub subscriptions: usize,
    pub forwarded_packets: u64,
    pub forwarded_bytes: u64,
    pub bwe: bool,
    pub allocated_subscribers: usize,
    /// Where the meetings go: `files:/var/lib/...` or `postgres:host:5432/db`.
    /// A server whose database is away carries calls perfectly well and records
    /// nothing; this and `store_up` are the pair that says which of those is
    /// happening.
    pub store: String,
    /// Whether the store answered the last time anybody asked it anything.
    pub store_up: bool,
    /// How many meetings the store holds. Against a database this is the whole
    /// cluster's, and it is a recent answer rather than a fresh one: it is a
    /// `count(*)`, and scraping must not be a reason to walk the table.
    pub sessions: usize,
    /// Running on this node. Against a database the cluster may be holding
    /// more.
    pub live_sessions: usize,
    /// What the history costs in memory. `sessions` grows for every meeting
    /// this server has ever held; this one is meant not to, and an operator
    /// watching it climb beside a flat `live_sessions` is watching a leak.
    pub resident_sessions: usize,
    /// Records recorded and not yet in the store. It rises while a burst of
    /// arrivals outruns the store and falls again; one that stays up is a store
    /// that cannot keep pace with the meetings held on top of it, and what a
    /// crash would cost is however long it stays up.
    pub owed_sessions: usize,
    /// Not decoration. Every other number here goes up when the server is
    /// working; this one goes up when a meeting has been held and will not be
    /// there to look up tomorrow, and there is nowhere else an operator would
    /// see that. It comes back down if a later write of the same record lands,
    /// so a store that hiccuped once is not reported for ever as a meeting that
    /// is going to be lost. It is what a database outage looks like from
    /// outside: calls carry on, this climbs.
    pub unwritten_sessions: u64,
    pub events: u64,
    /// Events promised to somebody and not yet taken by them. A number that
    /// sits above zero is an endpoint or a broker that is behind or away; one
    /// that climbs is one that is not coming back.
    pub owed_events: usize,
    /// The alertable one. Something outside this server is missing events and
    /// nothing else will ever tell anybody: given up on after every attempt, or
    /// never attempted because that destination was too far behind. What they
    /// were announcing is still recorded here, which is the only reason this is
    /// a number and not a crash.
    pub undelivered_events: u64,
    /// Outbox records that could not be written: events that are being
    /// attempted now and will not be retried after a restart.
    pub unwritten_events: u64,
    pub recorder: Recorder,
    /// Recordings finished on this node and not yet written into the record
    /// store -- `unwritten_recordings` on the health line. Above zero means the
    /// store is away: the audio and everything that describes it are on this
    /// node's disk, and what the outage costs is the delay before the object
    /// store gets them.
    pub owed_recordings: usize,
    /// The alertable one. Audio this node threw away because the queue between
    /// the media path and the writer was full, which means the disk could not
    /// keep up with the meetings on top of it. It is bounded on purpose -- a
    /// wedged disk must cost recording and never the calls -- and this is the
    /// number that says what the bound cost.
    pub dropped_audio_packets: u64,
    /// Recordings the writer gave up on because something in them made it
    /// panic. What reaches that thread came off the network from somebody
    /// else's encoder, so anything above zero says a publisher sent something
    /// this server could not handle -- and that it cost one recording rather
    /// than every recording on the node, which is what it used to cost.
    pub abandoned_recordings: u64,
    /// Recording work in the shared queue that nobody is going to move: given
    /// up on, or waiting for a node that has not come back. The commonest cause
    /// is an upload whose node is gone for good -- the file is on a disk nobody
    /// has -- and without this it is invisible: the row is not `failed`, it is
    /// not claimed, and every node walks past it for ever. Cluster-wide, and a
    /// recent answer rather than a fresh one.
    pub stuck_recording_work: u64,
    pub ice: SocketAddr,
    /// Whether this node has been told to stop. On the exposition and not on
    /// the health line, which stays 200 through a drain on purpose: the two
    /// readers want opposite answers, and the one that reads a status code is
    /// `/readyz`.
    pub draining: bool,
    pub node: String,
    pub version: &'static str,
    pub region: String,
    /// Whether this node registers with any others. `false` is a deployment
    /// running one replica, and then `cluster_nodes` is zero -- which is a
    /// different thing from a cluster whose nodes have all stopped, and the
    /// reason this is a series of its own rather than something to infer.
    pub cluster_joined: bool,
    /// Rows in the registry when the beat last read it, and how many of those
    /// were still beating. The gap between the two is the alertable part: nodes
    /// that stopped without taking their row away.
    pub cluster_nodes: usize,
    pub cluster_nodes_alive: usize,
}

/// Reads the lot, once.
///
/// Most of this is an atomic load or a lock held for the length of one `len()`.
/// The exceptions are the two the session store answers out of a database when
/// there is one -- `sessions` is a `count(*)` and `stuck_recording_work` walks
/// the work queue -- which is why this is called from a handler on somebody
/// else's clock and never from anything on the media path.
pub fn snapshot(state: &AppState) -> Snapshot {
    let rooms = state.registry.rooms();
    let participants = rooms.iter().map(|room| room.participant_count()).sum();
    let media = state.publishers.totals();
    let forwarding = state.router.totals();
    let sessions = state.registry.sessions();
    let delivery = state.delivery.as_ref();
    let recording = state.recording.as_ref();
    let cluster = state.cluster.view();

    Snapshot {
        rooms: rooms.len(),
        participants,
        publishers: state.publishers.count(),
        rtp_packets: media.rtp_packets,
        rtp_bytes: media.rtp_bytes,
        tracks: forwarding.tracks,
        subscriptions: forwarding.subscriptions,
        forwarded_packets: forwarding.packets,
        forwarded_bytes: forwarding.bytes,
        bwe: state.bwe_config().enabled,
        allocated_subscribers: state.bandwidth.count(),
        store: sessions.describe(),
        store_up: sessions.healthy(),
        sessions: sessions.count(),
        live_sessions: sessions.live_count(),
        resident_sessions: sessions.resident(),
        owed_sessions: sessions.owed(),
        unwritten_sessions: sessions.unwritten(),
        events: state.events.emitted(),
        owed_events: delivery.map_or(0, |delivery| delivery.owed()),
        undelivered_events: delivery.map_or(0, |delivery| delivery.undelivered()),
        unwritten_events: delivery.map_or(0, |delivery| delivery.unwritten()),
        recorder: match recording {
            None => Recorder::Off,
            Some(recording) if recording.recorder.alive() => Recorder::On,
            Some(_) => Recorder::Stopped,
        },
        owed_recordings: recording.map_or(0, |recording| recording.owed.load(Ordering::Relaxed)),
        dropped_audio_packets: recording.map_or(0, |recording| recording.dropped()),
        abandoned_recordings: recording.map_or(0, |recording| recording.abandoned()),
        stuck_recording_work: sessions.stuck(),
        ice: state.media.local_addr(),
        draining: state.draining(),
        node: state.cluster.id().to_owned(),
        version: mediacore_proto::SERVER_VERSION,
        region: state.config.region.clone(),
        cluster_joined: state.cluster.joined(),
        cluster_nodes: cluster.nodes.len(),
        cluster_nodes_alive: cluster.alive().count(),
    }
}

impl Snapshot {
    /// The health line, unchanged since before any of this existed.
    ///
    /// One line, `name=value` separated by single spaces, no trailing newline.
    /// Every part of that is load-bearing somewhere: the compose healthcheck
    /// wants a 200 and reads nothing, but three test files split it on
    /// whitespace and then on `=`, and an operator reads it in a terminal
    /// eighty columns wide. New fields go on the end.
    pub fn health_line(&self) -> String {
        let store = format!(
            "{}:{}",
            self.store,
            if self.store_up { "up" } else { "down" }
        );
        let bwe = if self.bwe { "on" } else { "off" };
        let recording = self.recorder.as_str();

        let Self {
            rooms,
            participants,
            publishers,
            rtp_packets,
            rtp_bytes,
            tracks,
            subscriptions,
            forwarded_packets,
            forwarded_bytes,
            allocated_subscribers,
            sessions,
            live_sessions,
            resident_sessions,
            owed_sessions,
            unwritten_sessions,
            events,
            owed_events,
            undelivered_events,
            unwritten_events,
            owed_recordings,
            dropped_audio_packets,
            abandoned_recordings,
            stuck_recording_work,
            ice,
            ..
        } = self;

        format!(
            "rooms={rooms} participants={participants} publishers={publishers} \
             rtp_packets={rtp_packets} rtp_bytes={rtp_bytes} tracks={tracks} \
             subscriptions={subscriptions} forwarded_packets={forwarded_packets} \
             forwarded_bytes={forwarded_bytes} bwe={bwe} \
             allocated_subscribers={allocated_subscribers} session_store={store} \
             sessions={sessions} live_sessions={live_sessions} \
             resident_sessions={resident_sessions} owed_sessions={owed_sessions} \
             unwritten_sessions={unwritten_sessions} events={events} \
             owed_events={owed_events} undelivered_events={undelivered_events} \
             unwritten_events={unwritten_events} recording={recording} \
             unwritten_recordings={owed_recordings} \
             dropped_audio_packets={dropped_audio_packets} \
             abandoned_recordings={abandoned_recordings} \
             stuck_recording_work={stuck_recording_work} ice={ice}"
        )
    }

    /// The same numbers in the Prometheus text exposition format.
    ///
    /// Written out by hand rather than through a client library, and the
    /// reason is that there is nothing here to register: no histograms, no
    /// per-request timing, nothing counted at the point it happens. The values
    /// already live in atomics on the objects that own them, and a registry
    /// would be a second copy of them plus a dependency.
    ///
    /// The states are numbers too -- `mediacore_recorder_running 0` rather
    /// than a `state="stopped"` label -- because an alert is an expression over
    /// numbers, and because label sets are the thing this document refuses to
    /// grow.
    pub fn exposition(&self) -> String {
        let mut out = Exposition::default();

        out.info(
            "mediacore_build_info",
            "This node: which process answered, what it runs and where it is. The only series \
             here that carries labels.",
            &[
                ("node", &self.node),
                ("version", self.version),
                ("region", &self.region),
            ],
        );

        out.counter(
            "mediacore_rtp_packets_total",
            "RTP packets received from publishers on this node.",
            self.rtp_packets,
        );
        out.counter(
            "mediacore_rtp_bytes_total",
            "Bytes of RTP received from publishers on this node.",
            self.rtp_bytes,
        );
        out.counter(
            "mediacore_forwarded_packets_total",
            "RTP packets written out to subscribers.",
            self.forwarded_packets,
        );
        out.counter(
            "mediacore_forwarded_bytes_total",
            "Bytes of RTP written out to subscribers.",
            self.forwarded_bytes,
        );
        out.counter(
            "mediacore_events_emitted_total",
            "Events this node has announced.",
            self.events,
        );
        out.counter(
            "mediacore_dropped_audio_packets_total",
            "Audio packets thrown away because the queue to the recording writer was full: a \
             disk that cannot keep up with the meetings on top of it.",
            self.dropped_audio_packets,
        );
        out.counter(
            "mediacore_abandoned_recordings_total",
            "Recordings the writer gave up on because something in them made it panic.",
            self.abandoned_recordings,
        );

        out.gauge(
            "mediacore_rooms",
            "Rooms open on this node.",
            count(self.rooms),
        );
        out.gauge(
            "mediacore_participants",
            "People in those rooms.",
            count(self.participants),
        );
        out.gauge(
            "mediacore_publishers",
            "Participants whose publishing transport this node holds.",
            count(self.publishers),
        );
        out.gauge(
            "mediacore_tracks",
            "Published tracks the forwarder holds a route for.",
            count(self.tracks),
        );
        out.gauge(
            "mediacore_subscriptions",
            "Subscriptions across those routes: every pair of a track and somebody receiving it.",
            count(self.subscriptions),
        );
        out.gauge(
            "mediacore_allocated_subscribers",
            "Subscribers the bandwidth estimator is allocating for.",
            count(self.allocated_subscribers),
        );

        out.gauge(
            "mediacore_sessions",
            "Meetings in the record store. Cluster-wide against a database, and a recent answer \
             rather than a fresh one.",
            count(self.sessions),
        );
        out.gauge(
            "mediacore_live_sessions",
            "Meetings running on this node.",
            count(self.live_sessions),
        );
        out.gauge(
            "mediacore_resident_sessions",
            "Meeting records held in memory. Climbing beside a flat mediacore_live_sessions is a \
             leak.",
            count(self.resident_sessions),
        );
        out.gauge(
            "mediacore_owed_sessions",
            "Records written and not yet in the store. What a crash would cost is however long \
             this stays up.",
            count(self.owed_sessions),
        );
        out.gauge(
            "mediacore_unwritten_sessions",
            "Meetings that were held and will not be there to look up tomorrow. What a database \
             outage looks like from outside: calls carry on, this climbs.",
            self.unwritten_sessions,
        );

        out.gauge(
            "mediacore_owed_events",
            "Events promised to somebody and not yet taken by them.",
            count(self.owed_events),
        );
        // Counters, not gauges, though the health line prints them beside the
        // gauges: both are lifetime sums that only ever rise -- `undelivered`
        // is abandoned plus dropped, `unwritten` counts write failures -- and a
        // rising number typed as a gauge is one an alert writer reaches for with
        // `> 0`, which then stays true for the life of the process however long
        // ago the one bad minute was.
        out.counter(
            "mediacore_undelivered_events_total",
            "Events nobody outside this server will ever be told about.",
            self.undelivered_events,
        );
        out.counter(
            "mediacore_unwritten_events_total",
            "Outbox records that could not be written, so the attempt in flight will not be \
             retried after a restart.",
            self.unwritten_events,
        );

        out.gauge(
            "mediacore_owed_recordings",
            "Recordings finished here and not yet in the record store. `unwritten_recordings` on \
             /healthz.",
            count(self.owed_recordings),
        );
        out.gauge(
            "mediacore_stuck_recording_work",
            "Recording work in the shared queue that nobody is going to move: given up on, or \
             waiting for a node that has not come back.",
            self.stuck_recording_work,
        );

        out.state(
            "mediacore_session_store_up",
            "1 when the record store answered the last thing that was asked of it.",
            self.store_up,
        );
        out.state(
            "mediacore_recorder_enabled",
            "1 when this node records at all. 0 with recording configured is a configuration \
             problem that is otherwise found a week later.",
            self.recorder != Recorder::Off,
        );
        out.state(
            "mediacore_recorder_running",
            "1 when the thread that writes recordings is alive. Enabled and not running means \
             this node is holding meetings and recording none of them.",
            self.recorder == Recorder::On,
        );
        out.state(
            "mediacore_bwe_enabled",
            "1 when this node estimates the bandwidth to a subscriber.",
            self.bwe,
        );
        out.state(
            "mediacore_draining",
            "1 when this node has been told to stop and is letting go of its rooms.",
            self.draining,
        );

        out.state(
            "mediacore_cluster_joined",
            "1 when this node registers with the others. 0 is a deployment running one replica, \
             which is a different thing from a cluster whose nodes have all stopped.",
            self.cluster_joined,
        );
        out.gauge(
            "mediacore_cluster_nodes",
            "Rows in the node registry when this node last read it.",
            count(self.cluster_nodes),
        );
        out.gauge(
            "mediacore_cluster_nodes_alive",
            "How many of those were still beating. Short of mediacore_cluster_nodes means \
             somebody stopped without taking their row away.",
            count(self.cluster_nodes_alive),
        );

        out.text
    }
}

/// Counts are `usize` all over this server and a series is a number; the cast
/// is here rather than at twenty call sites.
fn count(value: usize) -> u64 {
    value as u64
}

/// The exposition as it is built.
///
/// Nothing writes a sample line directly: every method here emits `# HELP` and
/// `# TYPE` before the value, so a series cannot reach a scraper undescribed --
/// which is the one mistake in this format that costs nothing at scrape time
/// and everything three months later, when somebody is reading a graph and
/// guessing what it counts.
#[derive(Default)]
struct Exposition {
    text: String,
}

impl Exposition {
    fn counter(&mut self, name: &str, help: &str, value: u64) {
        self.describe(name, "counter", help);
        self.sample(name, "", value);
    }

    fn gauge(&mut self, name: &str, help: &str, value: u64) {
        self.describe(name, "gauge", help);
        self.sample(name, "", value);
    }

    /// A state, as the one number an alert can be written over.
    fn state(&mut self, name: &str, help: &str, raised: bool) {
        self.gauge(name, help, u64::from(raised));
    }

    /// The one series with labels: constants about this process, at a value of
    /// 1 so that joining it onto anything else costs a multiplication.
    fn info(&mut self, name: &str, help: &str, labels: &[(&str, &str)]) {
        self.describe(name, "gauge", help);

        let labels: Vec<String> = labels
            .iter()
            .map(|(label, value)| format!("{label}=\"{}\"", escape(value)))
            .collect();

        self.sample(name, &format!("{{{}}}", labels.join(",")), 1);
    }

    fn describe(&mut self, name: &str, kind: &str, help: &str) {
        self.write(format_args!("# HELP {name} {help}\n"));
        self.write(format_args!("# TYPE {name} {kind}\n"));
    }

    fn sample(&mut self, name: &str, labels: &str, value: u64) {
        self.write(format_args!("{name}{labels} {value}\n"));
    }

    fn write(&mut self, line: std::fmt::Arguments<'_>) {
        self.text
            .write_fmt(line)
            .expect("writing into a String cannot fail");
    }
}

/// What a label value may not contain unescaped. The node id and the region
/// come out of a configuration file somebody else wrote, so a stray quote in
/// either would otherwise cut the document in half at the first line.
fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
#[path = "metrics_tests.rs"]
mod tests;
