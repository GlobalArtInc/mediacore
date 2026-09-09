use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use mediacore_proto::mediacore::{self, signal_response};
use mediacore_room::TrackKind;
use mediacore_sfu::{
    Allowance, Bitrate, DropReason, ForwardedPacket, ForwarderConfig, Forwarding, InboundPacket,
    LAYER_COUNT, LayerLadder, LayerPreference, LayerSet, LayerTracker, SelectorConfig,
    SpatialLayer, StreamStart, TrackDemand, TrackForwarder, TrackerConfig, VideoCodec, is_keyframe,
};
use mediacore_transport::str0m::media::{Mid, Pt, Rid};
use mediacore_transport::{IncomingRtp, LayerStats, OutgoingRtp, PeerHandle};

use crate::simulcast::{LayerMap, QualityUpdate, keyframe_kind, quality_message};

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
    pub bandwidth_paused: bool,
    pub layer: Option<u8>,
    pub target_layer: u8,
    pub switches: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RouterTotals {
    pub tracks: usize,
    pub subscriptions: usize,
    pub packets: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrackEffects {
    pub layers_changed: bool,
    pub demand_changed: bool,
}

impl TrackEffects {
    pub fn demand() -> Self {
        Self {
            layers_changed: false,
            demand_changed: true,
        }
    }

    pub fn is_empty(&self) -> bool {
        !self.layers_changed && !self.demand_changed
    }

    pub fn merge(&mut self, other: Self) {
        self.layers_changed |= other.layers_changed;
        self.demand_changed |= other.demand_changed;
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SourceConfig {
    pub pause_delay: Duration,
    pub tracker: TrackerConfig,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self {
            pause_delay: Duration::from_secs(5),
            tracker: TrackerConfig::video(),
        }
    }
}

pub fn tracker_config(source: mediacore::TrackSource) -> TrackerConfig {
    match source {
        mediacore::TrackSource::ScreenShare | mediacore::TrackSource::ScreenShareAudio => {
            TrackerConfig::screenshare()
        }
        mediacore::TrackSource::Camera
        | mediacore::TrackSource::Microphone
        | mediacore::TrackSource::Unknown => TrackerConfig::video(),
    }
}

struct Dynacast {
    committed: Option<LayerSet>,
    applied: Option<LayerSet>,
    deadline: Option<Instant>,
    pause_delay: Duration,
}

impl Dynacast {
    fn poll(&mut self, desired: LayerSet, now: Instant) -> Option<LayerSet> {
        if let Some(decided) = self.decide(desired, now) {
            self.committed = Some(decided);
            return Some(decided);
        }

        self.unannounced()
    }

    fn unannounced(&self) -> Option<LayerSet> {
        let committed = self.committed?;

        (self.applied != Some(committed)).then_some(committed)
    }

    fn decide(&mut self, desired: LayerSet, now: Instant) -> Option<LayerSet> {
        let committed = self.committed.map(|set| set.highest());
        let wanted = desired.highest();

        match committed {
            Some(committed) if committed == wanted => {
                self.deadline = None;
                None
            }
            Some(committed) if wanted > committed => {
                self.deadline = None;
                Some(desired)
            }
            _ if wanted.is_some() && self.committed.is_none() => {
                self.deadline = None;
                Some(desired)
            }
            _ => match self.deadline {
                Some(deadline) if now >= deadline => {
                    self.deadline = None;
                    Some(desired)
                }
                Some(_) => None,
                None => {
                    self.deadline = Some(now + self.pause_delay);
                    None
                }
            },
        }
    }
}

struct SourceState {
    tracker: LayerTracker,
    active: LayerSet,
    dirty: bool,
    ssrcs: [Option<u32>; LAYER_COUNT],
    pli: [Option<Instant>; LAYER_COUNT],
    demand: TrackDemand<String>,
    dynacast: Dynacast,
    reported: Option<Vec<mediacore::VideoLayer>>,
}

pub struct TrackSource {
    peer: PeerHandle,
    mid: Mid,
    kind: TrackKind,
    codec: Option<VideoCodec>,
    mime_type: String,
    publisher_sid: String,
    layers: LayerMap,
    state: Mutex<SourceState>,
    updates: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for TrackSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrackSource")
            .field("mid", &self.mid)
            .field("kind", &self.kind)
            .field("mime_type", &self.mime_type)
            .field("layers", &self.layers.len())
            .finish()
    }
}

impl TrackSource {
    pub fn new(
        peer: PeerHandle,
        mid: Mid,
        kind: TrackKind,
        mime_type: &str,
        publisher_sid: &str,
        layers: LayerMap,
        config: SourceConfig,
    ) -> Self {
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
            publisher_sid: publisher_sid.to_owned(),
            layers,
            state: Mutex::new(SourceState {
                tracker: LayerTracker::new(config.tracker),
                active: LayerSet::EMPTY,
                dirty: false,
                ssrcs: [None; LAYER_COUNT],
                pli: [None; LAYER_COUNT],
                demand: TrackDemand::new(),
                dynacast: Dynacast {
                    committed: None,
                    applied: None,
                    deadline: None,
                    pause_delay: config.pause_delay,
                },
                reported: None,
            }),
            updates: tokio::sync::Mutex::new(()),
        }
    }

    pub fn mime_type(&self) -> &str {
        &self.mime_type
    }

    pub fn codec_name(&self) -> String {
        self.mime_type
            .rsplit('/')
            .next()
            .unwrap_or(&self.mime_type)
            .to_lowercase()
    }

    pub fn active_layers(&self) -> LayerSet {
        self.state().active
    }

    pub fn ladder(&self) -> LayerLadder {
        self.layers.ladder()
    }

    pub fn is_simulcast(&self) -> bool {
        self.layers.is_simulcast()
    }

    pub fn rates(&self, overhead_bytes: u32, now: Instant) -> [Option<Bitrate>; LAYER_COUNT] {
        self.state().tracker.rates(overhead_bytes, now)
    }

    pub fn declared_bitrates(&self) -> [Option<Bitrate>; LAYER_COUNT] {
        self.layers.ladder().declared_bitrates()
    }

    fn state(&self) -> MutexGuard<'_, SourceState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn is_keyframe(&self, payload: &[u8]) -> bool {
        match (self.kind, self.codec) {
            (TrackKind::Audio, _) => false,
            (TrackKind::Video, Some(codec)) => is_keyframe(codec, payload).unwrap_or(false),
            (TrackKind::Video, None) => true,
        }
    }

    fn observe(&self, layer: SpatialLayer, ssrc: u32, payload_len: usize, now: Instant) -> bool {
        let mut state = self.state();
        let changes = state.tracker.observe(layer, payload_len, now);
        let slot = layer.slot();

        if state.ssrcs[slot] != Some(ssrc) {
            state.ssrcs[slot] = Some(ssrc);
            state.active = state.tracker.active();
            return true;
        }

        if changes.is_empty() {
            return false;
        }

        state.active = state.tracker.active();
        true
    }

    fn tick(&self, now: Instant) -> bool {
        let mut state = self.state();
        let moved = !state.tracker.tick(now).is_empty();
        let dirty = std::mem::take(&mut state.dirty);

        if !moved && !dirty {
            return false;
        }

        state.active = state.tracker.active();
        true
    }

    fn set_demand(&self, subscriber_sid: &str, wanted: Option<SpatialLayer>) -> bool {
        self.state().demand.set(subscriber_sid.to_owned(), wanted)
    }

    fn forget_demand(&self, subscriber_sid: &str) -> bool {
        self.state().demand.remove(&subscriber_sid.to_owned())
    }

    fn reported_layers(&self) -> Option<Vec<mediacore::VideoLayer>> {
        let mut state = self.state();
        let layers = self.layers.video_layers(state.active, &state.ssrcs);

        if state.reported.as_ref() == Some(&layers) {
            return None;
        }

        state.reported = Some(layers.clone());
        Some(layers)
    }

    fn pending_quality_update(&self, track_sid: &str, now: Instant) -> Option<QualityUpdate> {
        if self.kind != TrackKind::Video {
            return None;
        }

        let mut state = self.state();
        let desired = state.demand.enabled();
        let enabled = state.dynacast.poll(desired, now)?;

        Some(QualityUpdate {
            track_sid: track_sid.to_owned(),
            publisher_sid: self.publisher_sid.clone(),
            enabled,
        })
    }

    pub fn committed_layers(&self) -> Option<LayerSet> {
        self.state().dynacast.committed
    }

    pub fn announced_layers(&self) -> Option<LayerSet> {
        self.state().dynacast.applied
    }

    fn adopt_committed(&self, enabled: LayerSet) {
        let mut state = self.state();
        let previous = state
            .dynacast
            .applied
            .replace(enabled)
            .unwrap_or_else(|| self.layers.all());
        let mut moved = false;

        for layer in self.layers.all().iter() {
            if enabled.contains(layer)
                && !previous.contains(layer)
                && !state.tracker.reset_layer(layer).is_empty()
            {
                moved = true;
            }
        }

        if moved {
            state.active = state.tracker.active();
            state.dirty = true;
        }
    }

    pub async fn reconcile<F>(&self, track_sid: &str, announce: F) -> bool
    where
        F: FnOnce(signal_response::Message, LayerSet) -> bool,
    {
        let _guard = self.updates.lock().await;

        let Some(enabled) = self.committed_layers() else {
            return true;
        };

        let message = quality_message(
            track_sid,
            &self.codec_name(),
            self.layers.subscribed_qualities(enabled.highest()),
        );

        for (layer, rid) in self.layers.rids() {
            if !enabled.contains(layer) {
                self.set_layer_wanted(rid, false).await;
            }
        }

        for rid in self.layers.surplus() {
            self.set_layer_wanted(*rid, false).await;
        }

        let delivered = announce(message, enabled);

        if delivered {
            self.adopt_committed(enabled);
        } else {
            tracing::debug!(
                track = %track_sid,
                %enabled,
                "the publisher was not told which layers are subscribed, trying again later"
            );
        }

        for (layer, rid) in self.layers.rids() {
            if enabled.contains(layer) {
                self.set_layer_wanted(rid, true).await;
            }
        }

        delivered
    }

    async fn set_layer_wanted(&self, rid: Rid, wanted: bool) {
        let outcome = if wanted {
            self.peer.resume_layer(self.mid, Some(rid)).await
        } else {
            self.peer.pause_layer(self.mid, Some(rid)).await
        };

        if let Err(error) = outcome {
            tracing::debug!(
                mid = %self.mid,
                %rid,
                wanted,
                %error,
                "cannot tell the publisher whether to send a layer"
            );
        }
    }

    fn request_keyframe(&self, wanted: SpatialLayer, force: bool) -> bool {
        if self.kind != TrackKind::Video {
            return false;
        }

        let now = Instant::now();

        let layer = {
            let mut state = self.state();
            let Some(layer) =
                arriving(state.active, wanted).or_else(|| arriving(started(&state.ssrcs), wanted))
            else {
                tracing::trace!(
                    mid = %self.mid,
                    layer = %wanted,
                    "the publisher has not opened a stream to ask a keyframe on"
                );
                return false;
            };
            let asked = &mut state.pli[layer.slot()];

            if !force
                && let Some(at) = *asked
                && now.saturating_duration_since(at) < PLI_INTERVAL
            {
                return false;
            }

            *asked = Some(now);
            layer
        };

        let asked = match self.layers.rid_of(layer) {
            Some(rid) => self
                .peer
                .request_keyframe(self.mid, Some(rid), keyframe_kind()),
            None if self.layers.is_simulcast() => self
                .peer
                .request_keyframe_on_every_layer(self.mid, keyframe_kind()),
            None => self.peer.request_keyframe(self.mid, None, keyframe_kind()),
        };

        if let Err(error) = asked {
            tracing::debug!(
                mid = %self.mid,
                layer = %layer,
                %error,
                "cannot ask the publisher for a keyframe"
            );
            return false;
        }

        true
    }

    pub fn missed_keyframe_requests(&self) -> u64 {
        self.peer.missed_keyframe_requests()
    }

    pub async fn layer_stats(&self) -> Vec<LayerStats> {
        self.peer.layers(self.mid).await.unwrap_or_default()
    }
}

#[derive(Debug)]
struct SinkState {
    forwarder: TrackForwarder,
    counters: SubscriptionSnapshot,
}

enum SinkOutcome {
    Send(Box<ForwardedPacket>),
    Keyframe(SpatialLayer),
    Drop,
}

#[derive(Debug, Clone, Copy)]
pub struct SinkConfig {
    pub forwarder: ForwarderConfig,
    pub selector: SelectorConfig,
    pub start: StreamStart,
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
        config: SinkConfig,
    ) -> Self {
        Self {
            track_sid: track_sid.to_owned(),
            subscriber_sid: subscriber_sid.to_owned(),
            peer,
            mid,
            pt,
            state: Mutex::new(SinkState {
                forwarder: TrackForwarder::with_selector(
                    config.forwarder,
                    config.start,
                    config.selector,
                ),
                counters: SubscriptionSnapshot::default(),
            }),
        }
    }

    pub fn set_paused(&self, paused: bool) {
        let mut state = self.state();
        state.forwarder.set_subscriber_paused(paused);
        state.counters.paused = paused;
    }

    pub fn set_allowance(&self, allowance: Allowance, now: Instant) -> bool {
        let mut state = self.state();
        let moved = state.forwarder.set_allowance(allowance, now);
        state.counters.bandwidth_paused = state.forwarder.is_bandwidth_paused();
        state.counters.target_layer = state.forwarder.target_layer().index();

        moved
    }

    pub fn allowance(&self) -> Allowance {
        self.state().forwarder.allowance()
    }

    pub fn is_bandwidth_paused(&self) -> bool {
        self.state().forwarder.is_bandwidth_paused()
    }

    pub fn available_layers(&self) -> LayerSet {
        self.state().forwarder.selector().available()
    }

    pub fn set_publisher_muted(&self, muted: bool) {
        self.state().forwarder.set_publisher_muted(muted);
    }

    pub fn is_publisher_muted(&self) -> bool {
        self.state().forwarder.is_publisher_muted()
    }

    pub fn set_ladder(&self, ladder: LayerLadder, now: Instant) {
        self.state().forwarder.set_ladder(ladder, now);
    }

    pub fn set_preference(&self, preference: LayerPreference, now: Instant) {
        let mut state = self.state();
        state.forwarder.set_preference(preference, now);
        state.counters.target_layer = state.forwarder.target_layer().index();
    }

    pub fn observe_layers(&self, available: LayerSet, now: Instant) -> bool {
        let mut state = self.state();
        let moved = state.forwarder.observe_layers(available, now);
        state.counters.target_layer = state.forwarder.target_layer().index();

        moved
    }

    pub fn tick(&self, now: Instant) -> bool {
        let mut state = self.state();
        let moved = state.forwarder.tick(now);
        state.counters.target_layer = state.forwarder.target_layer().index();

        moved
    }

    pub fn wanted_layer(&self) -> Option<SpatialLayer> {
        self.state().forwarder.wanted_layer()
    }

    fn keyframe_layer(&self, now: Instant) -> Option<SpatialLayer> {
        let mut state = self.state();
        let layer = state.forwarder.keyframe_layer()?;

        state.forwarder.poll_keyframe_request(now).then_some(layer)
    }

    fn target_layer(&self) -> SpatialLayer {
        self.state().forwarder.target_layer()
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
                let switched = forwarded.switched;
                let current = state.forwarder.current_layer();
                let counters = &mut state.counters;
                counters.packets += 1;
                counters.bytes += forwarded.payload.len() as u64;
                if forwarded.keyframe {
                    counters.keyframes += 1;
                }
                if counters.first_sequence.is_none() {
                    counters.first_sequence = Some(*forwarded.seq);
                    counters.source_sequence = Some(u64::from(packet.seq));
                } else if switched {
                    counters.switches += 1;
                }
                counters.last_sequence = Some(*forwarded.seq);
                counters.layer = current.map(SpatialLayer::index);

                SinkOutcome::Send(forwarded)
            }
            Forwarding::Drop(reason) => {
                state.counters.dropped += 1;

                if reason == DropReason::AwaitingKeyframe {
                    let layer = state.forwarder.keyframe_layer();
                    if let Some(layer) = layer
                        && state.forwarder.poll_keyframe_request(packet.arrival)
                    {
                        return SinkOutcome::Keyframe(layer);
                    }
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

    fn sinks(&self) -> Vec<Arc<TrackSink>> {
        self.sinks.values().cloned().collect()
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
        let ladder = source.ladder();
        let now = Instant::now();

        let mut routes = self.routes();
        let route = routes.entry(track_sid.to_owned()).or_default();
        route.source = Some(Arc::clone(&source));

        for sink in route.sinks.values() {
            sink.set_ladder(ladder, now);
        }

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
        let (source, count) = {
            let mut routes = self.routes();
            let route = routes.entry(sink.track_sid.clone()).or_default();
            route
                .sinks
                .insert(sink.subscriber_sid.clone(), Arc::clone(&sink));

            (route.source.clone(), route.sinks.len())
        };

        if let Some(source) = source {
            let now = Instant::now();
            sink.set_ladder(source.ladder(), now);
            sink.observe_layers(source.active_layers(), now);
            source.set_demand(&sink.subscriber_sid, sink.wanted_layer());
        }

        count
    }

    pub fn remove_sink(&self, track_sid: &str, subscriber_sid: &str) -> usize {
        let (source, remaining) = {
            let mut routes = self.routes();
            let Some(route) = routes.get_mut(track_sid) else {
                return 0;
            };

            route.sinks.remove(subscriber_sid);
            let remaining = route.sinks.len();
            let source = route.source.clone();

            if route.is_idle() {
                routes.remove(track_sid);
            }

            (source, remaining)
        };

        if let Some(source) = source {
            source.forget_demand(subscriber_sid);
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
            if route.sinks.remove(subscriber_sid).is_some()
                && let Some(source) = &route.source
            {
                source.forget_demand(subscriber_sid);
            }

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
        for sink in self.sinks_of(track_sid) {
            sink.set_publisher_muted(muted);
        }
    }

    pub fn set_paused(&self, track_sid: &str, subscriber_sid: &str, paused: bool) -> TrackEffects {
        let (source, sink) = self.pair(track_sid, subscriber_sid);
        let Some(sink) = sink else {
            return TrackEffects::default();
        };

        sink.set_paused(paused);

        let Some(source) = source else {
            return TrackEffects::default();
        };

        TrackEffects {
            layers_changed: false,
            demand_changed: source.set_demand(subscriber_sid, sink.wanted_layer()),
        }
    }

    pub fn set_preference(
        &self,
        track_sid: &str,
        subscriber_sid: &str,
        preference: LayerPreference,
    ) -> TrackEffects {
        let (source, sink) = self.pair(track_sid, subscriber_sid);
        let (Some(source), Some(sink)) = (source, sink) else {
            return TrackEffects::default();
        };

        let now = Instant::now();
        sink.set_preference(preference, now);

        let demand_changed = source.set_demand(subscriber_sid, sink.wanted_layer());
        self.serve_keyframes(&source, std::slice::from_ref(&sink), now);

        TrackEffects {
            layers_changed: false,
            demand_changed,
        }
    }

    pub fn set_allowance(
        &self,
        track_sid: &str,
        subscriber_sid: &str,
        allowance: Allowance,
    ) -> bool {
        let (source, sink) = self.pair(track_sid, subscriber_sid);
        let Some(sink) = sink else {
            return false;
        };

        let now = Instant::now();
        let moved = sink.set_allowance(allowance, now);

        if let Some(source) = source {
            self.serve_keyframes(&source, std::slice::from_ref(&sink), now);
        }

        moved
    }

    pub fn tick_track(&self, track_sid: &str, now: Instant) -> TrackEffects {
        let (source, sinks) = self.route(track_sid);
        let Some(source) = source else {
            return TrackEffects::default();
        };

        let mut effects = TrackEffects {
            layers_changed: source.tick(now),
            demand_changed: false,
        };

        let active = source.active_layers();

        for sink in &sinks {
            if effects.layers_changed {
                sink.observe_layers(active, now);
            } else {
                sink.tick(now);
            }

            effects.demand_changed |= source.set_demand(&sink.subscriber_sid, sink.wanted_layer());
        }

        self.serve_keyframes(&source, &sinks, now);

        effects
    }

    pub fn live_layers(&self, track_sid: &str) -> Option<Vec<mediacore::VideoLayer>> {
        self.source(track_sid)?.reported_layers()
    }

    pub fn pending_quality_update(&self, track_sid: &str, now: Instant) -> Option<QualityUpdate> {
        self.source(track_sid)?
            .pending_quality_update(track_sid, now)
    }

    pub fn request_keyframe_for(&self, track_sid: &str, subscriber_sid: &str, force: bool) -> bool {
        let (source, sink) = self.pair(track_sid, subscriber_sid);
        let Some(source) = source else {
            return false;
        };
        if let Some(sink) = &sink
            && sink.is_publisher_muted()
        {
            return false;
        }

        let layer = sink
            .map(|sink| sink.target_layer())
            .unwrap_or(SpatialLayer::BASE);

        source.request_keyframe(layer, force)
    }

    pub fn forward(&self, track_sid: &str, rtp: &IncomingRtp) -> TrackEffects {
        let (source, sinks) = self.route(track_sid);

        let Some(source) = source else {
            return TrackEffects::default();
        };
        if sinks.is_empty() {
            return TrackEffects::default();
        }

        let header = &rtp.packet.header;
        let Some(layer) = source.layers.layer_of(rtp.rid) else {
            tracing::trace!(
                track = %track_sid,
                rid = ?rtp.rid,
                "dropping a packet from a layer this ladder does not carry"
            );
            return TrackEffects::default();
        };
        let arrival = rtp.packet.timestamp;
        let mut effects = TrackEffects {
            layers_changed: source.observe(layer, *header.ssrc, rtp.packet.payload.len(), arrival),
            demand_changed: false,
        };

        let packet = InboundPacket::new(
            *header.ssrc,
            header.sequence_number,
            header.timestamp,
            Arc::clone(&rtp.packet.payload),
            arrival,
        )
        .with_layer(layer)
        .with_marker(header.marker)
        .with_keyframe(source.is_keyframe(&rtp.packet.payload))
        .with_ext(header.ext_vals.clone());

        if effects.layers_changed {
            let active = source.active_layers();

            for sink in &sinks {
                let moved = sink.observe_layers(active, arrival);
                effects.demand_changed |=
                    source.set_demand(&sink.subscriber_sid, sink.wanted_layer());

                if moved {
                    self.serve_keyframes(&source, std::slice::from_ref(sink), arrival);
                }
            }
        }

        let mut wanted = LayerSet::EMPTY;

        for sink in &sinks {
            match sink.accept(&packet) {
                SinkOutcome::Send(forwarded) => {
                    self.packets.fetch_add(1, Ordering::Relaxed);
                    self.bytes
                        .fetch_add(forwarded.payload.len() as u64, Ordering::Relaxed);
                    sink.write(*forwarded, arrival);
                }
                SinkOutcome::Keyframe(layer) => {
                    wanted.insert(layer);
                }
                SinkOutcome::Drop => {}
            }
        }

        for layer in wanted.iter() {
            source.request_keyframe(layer, false);
        }

        effects
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

    fn serve_keyframes(&self, source: &TrackSource, sinks: &[Arc<TrackSink>], now: Instant) {
        for sink in sinks {
            if let Some(layer) = sink.keyframe_layer(now) {
                source.request_keyframe(layer, false);
            }
        }
    }

    fn route(&self, track_sid: &str) -> (Option<Arc<TrackSource>>, Vec<Arc<TrackSink>>) {
        let routes = self.routes();
        let Some(route) = routes.get(track_sid) else {
            return (None, Vec::new());
        };

        (route.source.clone(), route.sinks())
    }

    fn pair(
        &self,
        track_sid: &str,
        subscriber_sid: &str,
    ) -> (Option<Arc<TrackSource>>, Option<Arc<TrackSink>>) {
        let routes = self.routes();
        let Some(route) = routes.get(track_sid) else {
            return (None, None);
        };

        (
            route.source.clone(),
            route.sinks.get(subscriber_sid).cloned(),
        )
    }

    fn sinks_of(&self, track_sid: &str) -> Vec<Arc<TrackSink>> {
        self.routes()
            .get(track_sid)
            .map(TrackRoute::sinks)
            .unwrap_or_default()
    }

    fn routes(&self) -> MutexGuard<'_, HashMap<String, TrackRoute>> {
        self.tracks
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

fn started(ssrcs: &[Option<u32>; LAYER_COUNT]) -> LayerSet {
    (0..LAYER_COUNT as u8)
        .map(SpatialLayer::new)
        .filter(|layer| ssrcs[layer.slot()].is_some())
        .collect()
}

fn arriving(active: LayerSet, wanted: SpatialLayer) -> Option<SpatialLayer> {
    if active.contains(wanted) {
        return Some(wanted);
    }

    active.highest_at_most(wanted).or_else(|| active.lowest())
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
#[path = "forward_tests.rs"]
mod tests;
