use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use mediacore_room::TrackKind;
use mediacore_sfu::{
    DropReason, ForwardedPacket, ForwarderConfig, Forwarding, InboundPacket, StreamStart,
    TrackForwarder, VideoCodec, is_keyframe,
};
use mediacore_transport::str0m::media::{KeyframeRequestKind, Mid, Pt};
use mediacore_transport::{IncomingRtp, OutgoingRtp, PeerHandle};

pub const PLI_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubscriptionSnapshot {
    pub packets: u64,
    pub bytes: u64,
    pub keyframes: u64,
    pub dropped: u64,
    pub source_sequence: Option<u64>,
    pub first_sequence: Option<u64>,
    pub last_sequence: Option<u64>,
    pub paused: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RouterTotals {
    pub tracks: usize,
    pub subscriptions: usize,
    pub packets: u64,
    pub bytes: u64,
}

pub struct TrackSource {
    peer: PeerHandle,
    mid: Mid,
    kind: TrackKind,
    codec: Option<VideoCodec>,
    mime_type: String,
    pli: Mutex<Option<Instant>>,
}

impl std::fmt::Debug for TrackSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrackSource")
            .field("mid", &self.mid)
            .field("kind", &self.kind)
            .field("mime_type", &self.mime_type)
            .finish()
    }
}

impl TrackSource {
    pub fn new(peer: PeerHandle, mid: Mid, kind: TrackKind, mime_type: &str) -> Self {
        let codec = match kind {
            TrackKind::Audio => None,
            TrackKind::Video => match VideoCodec::from_mime(mime_type) {
                Ok(codec) => Some(codec),
                Err(error) => {
                    tracing::warn!(
                        %mime_type,
                        %error,
                        "forwarding this codec without keyframe detection"
                    );
                    None
                }
            },
        };

        Self {
            peer,
            mid,
            kind,
            codec,
            mime_type: mime_type.to_owned(),
            pli: Mutex::new(None),
        }
    }

    pub fn mime_type(&self) -> &str {
        &self.mime_type
    }

    fn is_keyframe(&self, payload: &[u8]) -> bool {
        match (self.kind, self.codec) {
            (TrackKind::Audio, _) => false,
            (TrackKind::Video, Some(codec)) => is_keyframe(codec, payload).unwrap_or(false),
            (TrackKind::Video, None) => true,
        }
    }

    fn request_keyframe(&self, force: bool) -> bool {
        if self.kind != TrackKind::Video {
            return false;
        }

        let now = Instant::now();

        {
            let mut asked = self.pli.lock().unwrap_or_else(|error| error.into_inner());

            if !force
                && let Some(at) = *asked
                && now.saturating_duration_since(at) < PLI_INTERVAL
            {
                return false;
            }

            *asked = Some(now);
        }

        if let Err(error) = self
            .peer
            .request_keyframe(self.mid, None, KeyframeRequestKind::Pli)
        {
            tracing::debug!(mid = %self.mid, %error, "cannot ask the publisher for a keyframe");
            return false;
        }

        true
    }
}

#[derive(Debug)]
struct SinkState {
    forwarder: TrackForwarder,
    counters: SubscriptionSnapshot,
}

enum SinkOutcome {
    Send(Box<ForwardedPacket>),
    Keyframe,
    Drop,
}

pub struct TrackSink {
    track_sid: String,
    subscriber_sid: String,
    peer: PeerHandle,
    mid: Mid,
    pt: Pt,
    state: Mutex<SinkState>,
}

impl std::fmt::Debug for TrackSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrackSink")
            .field("track_sid", &self.track_sid)
            .field("subscriber_sid", &self.subscriber_sid)
            .field("mid", &self.mid)
            .finish()
    }
}

impl TrackSink {
    pub fn new(
        track_sid: &str,
        subscriber_sid: &str,
        peer: PeerHandle,
        mid: Mid,
        pt: Pt,
        config: ForwarderConfig,
        start: StreamStart,
    ) -> Self {
        Self {
            track_sid: track_sid.to_owned(),
            subscriber_sid: subscriber_sid.to_owned(),
            peer,
            mid,
            pt,
            state: Mutex::new(SinkState {
                forwarder: TrackForwarder::new(config, start),
                counters: SubscriptionSnapshot::default(),
            }),
        }
    }

    pub fn set_paused(&self, paused: bool) {
        let mut state = self.state();
        state.forwarder.set_subscriber_paused(paused);
        state.counters.paused = paused;
    }

    pub fn set_publisher_muted(&self, muted: bool) {
        self.state().forwarder.set_publisher_muted(muted);
    }

    pub fn snapshot(&self) -> SubscriptionSnapshot {
        self.state().counters
    }

    fn state(&self) -> MutexGuard<'_, SinkState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn accept(&self, packet: &InboundPacket) -> SinkOutcome {
        let mut state = self.state();

        match state.forwarder.forward(packet) {
            Forwarding::Send(forwarded) => {
                let counters = &mut state.counters;
                counters.packets += 1;
                counters.bytes += forwarded.payload.len() as u64;
                if forwarded.keyframe {
                    counters.keyframes += 1;
                }
                if counters.first_sequence.is_none() {
                    counters.first_sequence = Some(*forwarded.seq);
                    counters.source_sequence = Some(u64::from(packet.seq));
                }
                counters.last_sequence = Some(*forwarded.seq);

                SinkOutcome::Send(forwarded)
            }
            Forwarding::Drop(reason) => {
                state.counters.dropped += 1;

                if reason == DropReason::AwaitingKeyframe
                    && state.forwarder.poll_keyframe_request(packet.arrival)
                {
                    return SinkOutcome::Keyframe;
                }

                SinkOutcome::Drop
            }
        }
    }

    fn write(&self, packet: ForwardedPacket, wallclock: Instant) {
        let outgoing = OutgoingRtp {
            mid: self.mid,
            rid: None,
            pt: self.pt,
            seq_no: packet.seq,
            time: packet.timestamp,
            wallclock,
            marker: packet.marker,
            nackable: packet.nackable,
            ext_vals: packet.ext,
            payload: packet.payload,
        };

        if let Err(error) = self.peer.write_rtp(outgoing) {
            tracing::debug!(
                track = %self.track_sid,
                subscriber = %self.subscriber_sid,
                %error,
                "cannot write to a subscriber transport"
            );
        }
    }
}

#[derive(Default)]
struct TrackRoute {
    source: Option<Arc<TrackSource>>,
    sinks: HashMap<String, Arc<TrackSink>>,
    ever_subscribed: bool,
}

impl TrackRoute {
    fn is_idle(&self) -> bool {
        self.source.is_none() && self.sinks.is_empty()
    }
}

#[derive(Default)]
pub struct MediaRouter {
    tracks: Mutex<HashMap<String, TrackRoute>>,
    packets: AtomicU64,
    bytes: AtomicU64,
}

impl MediaRouter {
    pub fn register_source(&self, track_sid: &str, source: TrackSource) -> Arc<TrackSource> {
        let source = Arc::new(source);
        self.routes()
            .entry(track_sid.to_owned())
            .or_default()
            .source = Some(Arc::clone(&source));

        source
    }

    pub fn remove_source(&self, track_sid: &str) {
        let mut routes = self.routes();
        let Some(route) = routes.get_mut(track_sid) else {
            return;
        };

        route.source = None;
        if route.is_idle() {
            routes.remove(track_sid);
        }
    }

    pub fn source(&self, track_sid: &str) -> Option<Arc<TrackSource>> {
        self.routes()
            .get(track_sid)
            .and_then(|route| route.source.clone())
    }

    pub fn add_sink(&self, sink: Arc<TrackSink>) -> usize {
        let mut routes = self.routes();
        let route = routes.entry(sink.track_sid.clone()).or_default();
        route
            .sinks
            .insert(sink.subscriber_sid.clone(), Arc::clone(&sink));

        route.sinks.len()
    }

    pub fn remove_sink(&self, track_sid: &str, subscriber_sid: &str) -> usize {
        let mut routes = self.routes();
        let Some(route) = routes.get_mut(track_sid) else {
            return 0;
        };

        route.sinks.remove(subscriber_sid);
        let remaining = route.sinks.len();

        if route.is_idle() {
            routes.remove(track_sid);
        }

        remaining
    }

    pub fn drop_track(&self, track_sid: &str) -> usize {
        let Some(route) = self.routes().remove(track_sid) else {
            return 0;
        };

        for sink in route.sinks.values() {
            sink.set_paused(true);
        }

        route.sinks.len()
    }

    pub fn remove_subscriber(&self, subscriber_sid: &str) {
        let mut routes = self.routes();

        routes.retain(|_, route| {
            route.sinks.remove(subscriber_sid);
            !route.is_idle()
        });
    }

    pub fn mark_subscribed(&self, track_sid: &str) -> bool {
        let mut routes = self.routes();
        let route = routes.entry(track_sid.to_owned()).or_default();

        if route.ever_subscribed {
            return false;
        }

        route.ever_subscribed = true;
        true
    }

    pub fn subscriber_count(&self, track_sid: &str) -> usize {
        self.routes()
            .get(track_sid)
            .map(|route| route.sinks.len())
            .unwrap_or_default()
    }

    pub fn subscription(
        &self,
        track_sid: &str,
        subscriber_sid: &str,
    ) -> Option<SubscriptionSnapshot> {
        self.routes()
            .get(track_sid)?
            .sinks
            .get(subscriber_sid)
            .map(|sink| sink.snapshot())
    }

    pub fn set_publisher_muted(&self, track_sid: &str, muted: bool) {
        let sinks = self.sinks_of(track_sid);

        for sink in sinks {
            sink.set_publisher_muted(muted);
        }
    }

    pub fn request_keyframe(&self, track_sid: &str, force: bool) -> bool {
        match self.source(track_sid) {
            Some(source) => source.request_keyframe(force),
            None => false,
        }
    }

    pub fn forward(&self, track_sid: &str, rtp: &IncomingRtp) {
        let (source, sinks) = {
            let routes = self.routes();
            let Some(route) = routes.get(track_sid) else {
                return;
            };
            if route.sinks.is_empty() {
                return;
            }

            (
                route.source.clone(),
                route.sinks.values().cloned().collect::<Vec<_>>(),
            )
        };

        let Some(source) = source else {
            return;
        };

        let header = &rtp.packet.header;
        let packet = InboundPacket::new(
            *header.ssrc,
            header.sequence_number,
            header.timestamp,
            Arc::clone(&rtp.packet.payload),
            rtp.packet.timestamp,
        )
        .with_marker(header.marker)
        .with_keyframe(source.is_keyframe(&rtp.packet.payload))
        .with_ext(header.ext_vals.clone());

        let mut wanted_keyframe = false;

        for sink in sinks {
            match sink.accept(&packet) {
                SinkOutcome::Send(forwarded) => {
                    self.packets.fetch_add(1, Ordering::Relaxed);
                    self.bytes
                        .fetch_add(forwarded.payload.len() as u64, Ordering::Relaxed);
                    sink.write(*forwarded, packet.arrival);
                }
                SinkOutcome::Keyframe => wanted_keyframe = true,
                SinkOutcome::Drop => {}
            }
        }

        if wanted_keyframe {
            source.request_keyframe(false);
        }
    }

    pub fn totals(&self) -> RouterTotals {
        let routes = self.routes();

        RouterTotals {
            tracks: routes.len(),
            subscriptions: routes.values().map(|route| route.sinks.len()).sum(),
            packets: self.packets.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
        }
    }

    fn sinks_of(&self, track_sid: &str) -> Vec<Arc<TrackSink>> {
        self.routes()
            .get(track_sid)
            .map(|route| route.sinks.values().cloned().collect())
            .unwrap_or_default()
    }

    fn routes(&self) -> MutexGuard<'_, HashMap<String, TrackRoute>> {
        self.tracks
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

pub fn forwarder_config(kind: TrackKind, clock_rate: u32) -> ForwarderConfig {
    let mut config = match kind {
        TrackKind::Audio => ForwarderConfig::audio(),
        TrackKind::Video => ForwarderConfig::video(),
    };

    if clock_rate > 0 {
        config.clock_rate = clock_rate;
    }

    config
}

pub fn stream_start(subscriber_sid: &str, track_sid: &str, nonce: u64) -> StreamStart {
    let mut seed = nonce;

    for value in [subscriber_sid.as_bytes(), track_sid.as_bytes()] {
        for byte in value {
            seed ^= u64::from(*byte);
            seed = seed.wrapping_mul(0x0100_0000_01b3);
        }
    }

    StreamStart::from_seed(seed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_subscribers_of_one_track_get_different_stream_starts() {
        let first = stream_start("PA_alice", "TR_cam", 1);
        let second = stream_start("PA_bob", "TR_cam", 1);

        assert_ne!(first.seq, second.seq);
        assert_ne!(first.timestamp, second.timestamp);
    }

    #[test]
    fn a_stream_start_never_lands_in_the_publishers_low_sequence_space() {
        for nonce in 0..64u64 {
            let start = stream_start("PA_alice", "TR_cam", nonce);

            assert!(start.seq >= 1 << 15, "sequence {} is too low", start.seq);
            assert!(start.seq < 1 << 16);
            assert!(start.timestamp >= 1 << 31);
        }
    }

    #[test]
    fn the_forwarder_clock_rate_follows_the_negotiated_codec() {
        assert_eq!(forwarder_config(TrackKind::Audio, 0).clock_rate, 48_000);
        assert_eq!(forwarder_config(TrackKind::Video, 0).clock_rate, 90_000);
        assert_eq!(
            forwarder_config(TrackKind::Audio, 16_000).clock_rate,
            16_000
        );
    }

    #[test]
    fn an_empty_router_reports_nothing() {
        let router = MediaRouter::default();

        assert_eq!(router.totals(), RouterTotals::default());
        assert_eq!(router.subscriber_count("TR_missing"), 0);
        assert!(router.source("TR_missing").is_none());
        assert!(!router.request_keyframe("TR_missing", true));
        assert_eq!(router.remove_sink("TR_missing", "PA_bob"), 0);
    }

    #[test]
    fn dropping_a_track_takes_its_source_and_every_sink_with_it() {
        let router = MediaRouter::default();

        assert_eq!(router.drop_track("TR_missing"), 0);

        router.mark_subscribed("TR_cam");
        assert_eq!(router.drop_track("TR_cam"), 0);
        assert_eq!(router.totals().tracks, 0);
        assert!(
            router.mark_subscribed("TR_cam"),
            "a dropped track keeps nothing behind, not even its announcement"
        );
    }

    #[test]
    fn a_track_is_announced_to_its_publisher_only_once() {
        let router = MediaRouter::default();

        assert!(router.mark_subscribed("TR_cam"));
        assert!(!router.mark_subscribed("TR_cam"));
        assert!(router.mark_subscribed("TR_mic"));
    }
}
