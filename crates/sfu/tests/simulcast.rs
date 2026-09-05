use std::collections::HashMap;
use std::time::{Duration, Instant};

use mediacore_sfu::{
    DropReason, ForwarderConfig, Forwarding, InboundPacket, LayerLadder, LayerPreference, LayerSet,
    LayerTracker, SpatialLayer, StreamStart, TrackDemand, TrackForwarder, TrackerConfig,
    VideoQuality,
};

const FRAME_TICKS: u32 = 3_000;
const FRAME_TIME: Duration = Duration::from_millis(33);
const PACKETS_PER_FRAME: usize = 3;

const LOW: SpatialLayer = SpatialLayer::BASE;

fn mid() -> SpatialLayer {
    SpatialLayer::new(1)
}

fn high() -> SpatialLayer {
    SpatialLayer::new(2)
}

struct Publisher {
    ssrc: [u32; 3],
    seq: [u16; 3],
    timestamp: [u32; 3],
    frame: u16,
}

impl Publisher {
    fn new() -> Self {
        Self {
            ssrc: [0x1000_0001, 0x2000_0002, 0x3000_0003],
            seq: [40_000, 100, 9_000],
            timestamp: [1_000_000, 77, 4_000_000_000],
            frame: 0,
        }
    }

    fn frame_of(&mut self, layer: SpatialLayer, keyframe: bool, at: Instant) -> Vec<InboundPacket> {
        let slot = layer.index() as usize;
        let mut packets = Vec::with_capacity(PACKETS_PER_FRAME);

        for index in 0..PACKETS_PER_FRAME {
            let payload = vec![
                layer.index(),
                self.frame as u8,
                (self.frame >> 8) as u8,
                index as u8,
            ];
            let packet = InboundPacket::new(
                self.ssrc[slot],
                self.seq[slot],
                self.timestamp[slot],
                payload,
                at + Duration::from_millis(slot as u64 * 3 + index as u64),
            )
            .with_layer(layer)
            .with_keyframe(keyframe && index == 0)
            .with_marker(index + 1 == PACKETS_PER_FRAME);

            self.seq[slot] = self.seq[slot].wrapping_add(1);
            packets.push(packet);
        }

        self.timestamp[slot] = self.timestamp[slot].wrapping_add(FRAME_TICKS);
        packets
    }

    fn group(&mut self, layers: LayerSet, keyframes: LayerSet, at: Instant) -> Vec<InboundPacket> {
        let mut packets = Vec::new();

        for layer in layers.iter() {
            packets.extend(self.frame_of(layer, keyframes.contains(layer), at));
        }

        self.frame = self.frame.wrapping_add(1);
        packets
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Record {
    seq: u64,
    timestamp: u64,
    layer: u8,
    frame: u16,
    keyframe: bool,
}

#[derive(Debug, Default)]
struct Recorder {
    sent: Vec<Record>,
    drops: Vec<DropReason>,
}

impl Recorder {
    fn feed(&mut self, forwarder: &mut TrackForwarder, packets: &[InboundPacket]) {
        for packet in packets {
            self.observe(forwarder.forward(packet));
        }
    }

    fn observe(&mut self, result: Forwarding) {
        match result {
            Forwarding::Send(packet) => self.sent.push(Record {
                seq: *packet.seq,
                timestamp: packet.ext_timestamp,
                layer: packet.payload[0],
                frame: u16::from(packet.payload[1]) | (u16::from(packet.payload[2]) << 8),
                keyframe: packet.keyframe,
            }),
            Forwarding::Drop(reason) => self.drops.push(reason),
        }
    }

    fn layers(&self) -> Vec<u8> {
        let mut layers: Vec<u8> = self.sent.iter().map(|record| record.layer).collect();
        layers.dedup();
        layers
    }

    fn drops_of(&self, reason: DropReason) -> usize {
        self.drops.iter().filter(|drop| **drop == reason).count()
    }

    fn switch_point(&self) -> usize {
        let first = self.sent.first().expect("something was forwarded").layer;

        self.sent
            .iter()
            .position(|record| record.layer != first)
            .expect("the stream switched layers")
    }

    fn check(&self) {
        assert!(!self.sent.is_empty(), "nothing was forwarded");

        for pair in self.sent.windows(2) {
            assert!(
                pair[1].seq > pair[0].seq,
                "the outgoing sequence went backwards or repeated: {:?} then {:?}",
                pair[0],
                pair[1]
            );
            assert!(
                pair[1].timestamp >= pair[0].timestamp,
                "the outgoing timestamp went backwards: {:?} then {:?}",
                pair[0],
                pair[1]
            );
        }

        let mut owner: HashMap<u64, (u8, u16)> = HashMap::new();
        for record in &self.sent {
            let source = (record.layer, record.frame);
            match owner.get(&record.timestamp) {
                Some(existing) => assert_eq!(
                    *existing, source,
                    "outgoing timestamp {} carries packets of two source frames",
                    record.timestamp
                ),
                None => {
                    owner.insert(record.timestamp, source);
                }
            }
        }
    }
}

fn forwarder(seed: u64) -> TrackForwarder {
    TrackForwarder::new(ForwarderConfig::video(), StreamStart::from_seed(seed))
}

#[test]
fn a_switch_up_keeps_the_outgoing_stream_continuous() {
    let start = Instant::now();
    let mut publisher = Publisher::new();
    let mut forwarder = forwarder(1);
    let mut recorder = Recorder::default();

    forwarder.set_preference(LayerPreference::Quality(VideoQuality::Low), start);
    forwarder.observe_layers(LayerSet::all(), start);
    assert_eq!(forwarder.target_layer(), LOW);

    let mut at = start;
    for frame in 0..6 {
        let keyframes = if frame == 0 {
            LayerSet::all()
        } else {
            LayerSet::EMPTY
        };
        let packets = publisher.group(LayerSet::all(), keyframes, at);
        recorder.feed(&mut forwarder, &packets);
        at += FRAME_TIME;
    }

    assert_eq!(recorder.layers(), vec![LOW.index()]);
    assert_eq!(forwarder.current_layer(), Some(LOW));
    let before = recorder.sent.len();

    forwarder.set_preference(LayerPreference::Quality(VideoQuality::High), at);
    assert_eq!(forwarder.target_layer(), high());
    assert!(forwarder.is_switching());
    assert!(forwarder.needs_keyframe());
    assert_eq!(forwarder.keyframe_layer(), Some(high()));
    assert!(forwarder.poll_keyframe_request(at));

    for _ in 0..3 {
        let packets = publisher.group(LayerSet::all(), LayerSet::EMPTY, at);
        recorder.feed(&mut forwarder, &packets);
        at += FRAME_TIME;
    }

    assert_eq!(
        recorder.layers(),
        vec![LOW.index()],
        "the subscriber must keep the layer it has until the target is decodable"
    );
    assert_eq!(
        recorder.drops_of(DropReason::AwaitingKeyframe),
        3 * PACKETS_PER_FRAME,
        "every packet of the target layer before its keyframe is dropped"
    );

    let packets = publisher.group(LayerSet::all(), LayerSet::of(high()), at);
    recorder.feed(&mut forwarder, &packets);
    at += FRAME_TIME;

    for _ in 0..4 {
        let packets = publisher.group(LayerSet::all(), LayerSet::EMPTY, at);
        recorder.feed(&mut forwarder, &packets);
        at += FRAME_TIME;
    }

    recorder.check();
    assert_eq!(recorder.layers(), vec![LOW.index(), high().index()]);
    assert_eq!(forwarder.current_layer(), Some(high()));
    assert!(!forwarder.is_switching());
    assert!(!forwarder.needs_keyframe());

    let switch = recorder.switch_point();
    assert!(switch > before, "the switch happened before the request");
    assert!(
        recorder.sent[switch].keyframe,
        "the first packet of the new layer must be a keyframe"
    );
    assert_eq!(recorder.sent[switch].seq, recorder.sent[switch - 1].seq + 1);
    assert!(recorder.sent[switch].timestamp > recorder.sent[switch - 1].timestamp);
    assert_eq!(forwarder.stats().layer_switches, 1);
    assert!(recorder.drops_of(DropReason::WrongLayer) > 0);
}

#[test]
fn a_switch_down_keeps_the_outgoing_stream_continuous() {
    let start = Instant::now();
    let mut publisher = Publisher::new();
    let mut forwarder = forwarder(2);
    let mut recorder = Recorder::default();

    forwarder.observe_layers(LayerSet::all(), start);
    assert_eq!(forwarder.target_layer(), high());

    let mut at = start;
    for frame in 0..5 {
        let keyframes = if frame == 0 {
            LayerSet::all()
        } else {
            LayerSet::EMPTY
        };
        let packets = publisher.group(LayerSet::all(), keyframes, at);
        recorder.feed(&mut forwarder, &packets);
        at += FRAME_TIME;
    }

    assert_eq!(recorder.layers(), vec![high().index()]);

    forwarder.set_preference(LayerPreference::Quality(VideoQuality::Low), at);
    assert_eq!(forwarder.target_layer(), LOW);
    assert_eq!(forwarder.keyframe_layer(), Some(LOW));

    for _ in 0..2 {
        let packets = publisher.group(LayerSet::all(), LayerSet::EMPTY, at);
        recorder.feed(&mut forwarder, &packets);
        at += FRAME_TIME;
    }

    assert_eq!(
        recorder.layers(),
        vec![high().index()],
        "a downgrade also waits for a keyframe of the layer it moves to"
    );

    let packets = publisher.group(LayerSet::all(), LayerSet::of(LOW), at);
    recorder.feed(&mut forwarder, &packets);
    at += FRAME_TIME;

    for _ in 0..3 {
        let packets = publisher.group(LayerSet::all(), LayerSet::EMPTY, at);
        recorder.feed(&mut forwarder, &packets);
        at += FRAME_TIME;
    }

    recorder.check();
    assert_eq!(recorder.layers(), vec![high().index(), LOW.index()]);
    assert_eq!(forwarder.current_layer(), Some(LOW));

    let switch = recorder.switch_point();
    assert!(recorder.sent[switch].keyframe);
    assert_eq!(recorder.sent[switch].seq, recorder.sent[switch - 1].seq + 1);
    assert!(
        recorder.sent[switch].timestamp > recorder.sent[switch - 1].timestamp,
        "a lower layer starts far behind in its own clock and must not drag the subscriber back"
    );
}

#[test]
fn what_arrived_before_the_switch_point_is_never_replayed_after_it() {
    let start = Instant::now();
    let mut publisher = Publisher::new();
    let mut forwarder = forwarder(3);
    let mut recorder = Recorder::default();

    forwarder.set_preference(LayerPreference::Quality(VideoQuality::Low), start);
    forwarder.observe_layers(LayerSet::all(), start);

    let mut at = start;
    let opening = publisher.group(LayerSet::all(), LayerSet::all(), at);
    recorder.feed(&mut forwarder, &opening);
    at += FRAME_TIME;

    let stale: Vec<InboundPacket> = opening
        .iter()
        .filter(|packet| packet.layer == high())
        .cloned()
        .collect();

    for _ in 0..2 {
        let packets = publisher.group(LayerSet::all(), LayerSet::EMPTY, at);
        recorder.feed(&mut forwarder, &packets);
        at += FRAME_TIME;
    }

    forwarder.set_preference(LayerPreference::Quality(VideoQuality::High), at);
    let packets = publisher.group(LayerSet::all(), LayerSet::of(high()), at);
    recorder.feed(&mut forwarder, &packets);

    let switch = recorder.switch_point();
    assert!(recorder.sent[switch].keyframe);

    let before = recorder.sent.len();
    for packet in &stale {
        recorder.observe(forwarder.forward(packet));
    }

    assert_eq!(
        recorder.sent.len(),
        before,
        "packets the new layer sent before the switch point have no place in the outgoing stream"
    );
    assert_eq!(recorder.drops_of(DropReason::TooOld), stale.len());
    recorder.check();
}

#[test]
fn a_keyframe_of_the_layer_being_left_neither_switches_nor_stops_the_request() {
    let start = Instant::now();
    let mut publisher = Publisher::new();
    let mut forwarder = forwarder(10);
    let mut recorder = Recorder::default();

    forwarder.set_preference(LayerPreference::Quality(VideoQuality::Low), start);
    forwarder.observe_layers(LayerSet::all(), start);

    let mut at = start;
    recorder.feed(
        &mut forwarder,
        &publisher.group(LayerSet::all(), LayerSet::all(), at),
    );
    at += FRAME_TIME;

    forwarder.set_preference(LayerPreference::Quality(VideoQuality::High), at);
    assert!(forwarder.poll_keyframe_request(at));

    let mut requests = 0;
    for _ in 0..6 {
        let packets = publisher.group(LayerSet::all(), LayerSet::of(LOW), at);
        recorder.feed(&mut forwarder, &packets);
        at += Duration::from_millis(600);

        if forwarder.poll_keyframe_request(at) {
            requests += 1;
            assert_eq!(forwarder.keyframe_layer(), Some(high()));
        }
    }

    assert_eq!(
        recorder.layers(),
        vec![LOW.index()],
        "a keyframe of the layer being left is not a place to switch"
    );
    assert!(forwarder.needs_keyframe());
    assert_eq!(
        requests, 6,
        "the subscriber keeps asking the publisher for a keyframe of the layer it wants"
    );

    recorder.feed(
        &mut forwarder,
        &publisher.group(LayerSet::all(), LayerSet::of(high()), at),
    );

    recorder.check();
    assert_eq!(recorder.layers(), vec![LOW.index(), high().index()]);
    assert!(!forwarder.needs_keyframe());
    assert_eq!(forwarder.keyframe_layer(), None);
}

struct Session {
    publisher: Publisher,
    forwarder: TrackForwarder,
    tracker: LayerTracker,
    recorder: Recorder,
    targets: Vec<SpatialLayer>,
    pending: LayerSet,
    at: Instant,
}

impl Session {
    fn new(seed: u64, start: Instant) -> Self {
        let mut forwarder = forwarder(seed);
        let mut targets = Vec::new();

        if forwarder.observe_layers(LayerSet::all(), start) {
            targets.push(forwarder.target_layer());
        }

        Self {
            publisher: Publisher::new(),
            forwarder,
            tracker: LayerTracker::new(TrackerConfig::video()),
            recorder: Recorder::default(),
            targets,
            pending: LayerSet::all(),
            at: start,
        }
    }

    fn run(&mut self, frames: usize, arriving: LayerSet) {
        for _ in 0..frames {
            let keyframes = self.pending;
            self.pending = LayerSet::EMPTY;

            for packet in self.publisher.group(arriving, keyframes, self.at) {
                self.tracker
                    .observe(packet.layer, packet.payload.len(), packet.arrival);
                self.recorder.observe(self.forwarder.forward(&packet));
            }

            self.at += FRAME_TIME;
            self.tracker.tick(self.at);

            if self
                .forwarder
                .observe_layers(self.tracker.active(), self.at)
            {
                self.targets.push(self.forwarder.target_layer());
            }

            if self.forwarder.poll_keyframe_request(self.at)
                && let Some(layer) = self.forwarder.keyframe_layer()
            {
                self.pending.insert(layer);
            }
        }
    }
}

#[test]
fn a_layer_that_goes_silent_is_detected_and_left_without_flapping() {
    let mut session = Session::new(4, Instant::now());

    session.run(30, LayerSet::all());

    assert_eq!(session.tracker.active(), LayerSet::all());
    assert_eq!(session.forwarder.current_layer(), Some(high()));
    assert_eq!(session.targets, vec![high()]);

    session.run(30, LayerSet::from_iter([LOW, mid()]));

    assert!(!session.tracker.is_active(high()));
    assert_eq!(session.tracker.active(), LayerSet::from_iter([LOW, mid()]));
    assert_eq!(
        session.targets,
        vec![high(), mid()],
        "a layer that stops arriving is left for the best one that still does"
    );
    assert_eq!(session.forwarder.current_layer(), Some(mid()));

    for _ in 0..6 {
        session.run(15, LayerSet::all());
        session.run(15, LayerSet::from_iter([LOW, mid()]));
    }

    assert_eq!(
        session.targets,
        vec![high(), mid()],
        "a layer that keeps blinking must not drag the subscriber up and down"
    );
    assert_eq!(session.forwarder.current_layer(), Some(mid()));

    session.run(400, LayerSet::all());

    assert_eq!(
        session.targets,
        vec![high(), mid(), high()],
        "a layer that comes back and stays is taken again"
    );
    session.recorder.check();
    assert_eq!(
        session.recorder.layers(),
        vec![high().index(), mid().index(), high().index()]
    );
    assert_eq!(session.forwarder.stats().layer_switches, 2);
}

#[test]
fn two_subscribers_of_one_track_switch_independently() {
    let start = Instant::now();
    let mut publisher = Publisher::new();
    let mut first = forwarder(5);
    let mut second = forwarder(6);
    let mut recorded = (Recorder::default(), Recorder::default());

    first.set_preference(LayerPreference::Quality(VideoQuality::Low), start);
    first.observe_layers(LayerSet::all(), start);
    second.observe_layers(LayerSet::all(), start);

    let mut at = start;
    let feed = |publisher: &mut Publisher,
                recorded: &mut (Recorder, Recorder),
                first: &mut TrackForwarder,
                second: &mut TrackForwarder,
                at: &mut Instant,
                keyframes: LayerSet| {
        for packet in publisher.group(LayerSet::all(), keyframes, *at) {
            recorded.0.observe(first.forward(&packet));
            recorded.1.observe(second.forward(&packet));
        }
        *at += FRAME_TIME;
    };

    feed(
        &mut publisher,
        &mut recorded,
        &mut first,
        &mut second,
        &mut at,
        LayerSet::all(),
    );
    for _ in 0..4 {
        feed(
            &mut publisher,
            &mut recorded,
            &mut first,
            &mut second,
            &mut at,
            LayerSet::EMPTY,
        );
    }

    assert_eq!(recorded.0.layers(), vec![LOW.index()]);
    assert_eq!(recorded.1.layers(), vec![high().index()]);

    first.set_preference(LayerPreference::Quality(VideoQuality::Medium), at);
    assert!(first.needs_keyframe());
    assert!(!second.needs_keyframe());

    feed(
        &mut publisher,
        &mut recorded,
        &mut first,
        &mut second,
        &mut at,
        LayerSet::of(mid()),
    );
    for _ in 0..3 {
        feed(
            &mut publisher,
            &mut recorded,
            &mut first,
            &mut second,
            &mut at,
            LayerSet::EMPTY,
        );
    }

    recorded.0.check();
    recorded.1.check();
    assert_eq!(recorded.0.layers(), vec![LOW.index(), mid().index()]);
    assert_eq!(
        recorded.1.layers(),
        vec![high().index()],
        "one subscriber switching must not disturb the other"
    );
    assert_eq!(first.stats().layer_switches, 1);
    assert_eq!(second.stats().layer_switches, 0);

    let heads = (
        recorded.0.sent[0].seq as i128,
        recorded.1.sent[0].seq as i128,
    );
    assert_ne!(heads.0, heads.1);
}

#[test]
fn the_wanted_layers_follow_the_subscribers() {
    let start = Instant::now();
    let mut demand: TrackDemand<&str> = TrackDemand::new();

    assert!(demand.wanted().is_empty());
    assert_eq!(demand.quality(&LayerLadder::new()), VideoQuality::Off);

    let mut alice = forwarder(7);
    alice.set_preference(LayerPreference::Quality(VideoQuality::Low), start);
    assert!(demand.set("alice", alice.wanted_layer()));
    assert_eq!(demand.wanted(), LayerSet::of(LOW));
    assert_eq!(demand.enabled(), LayerSet::of(LOW));
    assert_eq!(demand.quality(&LayerLadder::new()), VideoQuality::Low);

    let mut bob = forwarder(8);
    assert!(demand.set("bob", bob.wanted_layer()));
    assert_eq!(demand.wanted(), LayerSet::from_iter([LOW, high()]));
    assert_eq!(demand.enabled(), LayerSet::all());
    assert_eq!(demand.quality(&LayerLadder::new()), VideoQuality::High);

    bob.set_preference(LayerPreference::Quality(VideoQuality::Medium), start);
    assert!(demand.set("bob", bob.wanted_layer()));
    assert_eq!(demand.wanted(), LayerSet::from_iter([LOW, mid()]));
    assert_eq!(demand.enabled(), LayerSet::from_iter([LOW, mid()]));

    bob.set_subscriber_paused(true);
    assert!(demand.set("bob", bob.wanted_layer()));
    assert_eq!(demand.wanted(), LayerSet::of(LOW));

    bob.set_subscriber_paused(false);
    assert!(demand.set("bob", bob.wanted_layer()));
    assert_eq!(demand.wanted(), LayerSet::from_iter([LOW, mid()]));

    alice.set_preference(LayerPreference::Quality(VideoQuality::Off), start);
    assert!(demand.set("alice", alice.wanted_layer()));
    assert_eq!(demand.wanted(), LayerSet::of(mid()));

    assert!(demand.remove(&"bob"));
    assert!(demand.wanted().is_empty());
    assert_eq!(demand.quality(&LayerLadder::new()), VideoQuality::Off);
    assert_eq!(demand.subscribers(), 1);
}

#[test]
fn a_quality_request_follows_the_ladder_the_publisher_declared() {
    let start = Instant::now();
    let mut forwarder = forwarder(23);

    forwarder.set_ladder(
        LayerLadder::new()
            .with_quality(LOW, VideoQuality::Medium)
            .with_quality(mid(), VideoQuality::High),
        start,
    );
    forwarder.observe_layers(LayerSet::from_iter([LOW, mid()]), start);

    forwarder.set_preference(LayerPreference::Quality(VideoQuality::Medium), start);
    assert_eq!(
        forwarder.wanted_layer(),
        Some(LOW),
        "the publisher's medium encoding is the bottom rung of this ladder"
    );
    assert_eq!(forwarder.target_layer(), LOW);

    forwarder.set_preference(LayerPreference::Quality(VideoQuality::High), start);
    assert_eq!(forwarder.wanted_layer(), Some(mid()));
    assert_eq!(forwarder.target_layer(), mid());
}

#[test]
fn a_subscriber_that_only_wants_a_small_picture_asks_for_the_small_layer() {
    let start = Instant::now();
    let mut forwarder = forwarder(9);

    forwarder.set_ladder(
        mediacore_sfu::LayerLadder::new()
            .with_size(LOW, 320, 180)
            .with_size(mid(), 640, 360)
            .with_size(high(), 1280, 720),
        start,
    );
    forwarder.set_preference(
        LayerPreference::Size {
            width: 320,
            height: 180,
        },
        start,
    );

    assert_eq!(forwarder.wanted_layer(), Some(LOW));
    forwarder.observe_layers(LayerSet::all(), start);
    assert_eq!(forwarder.target_layer(), LOW);

    forwarder.set_preference(
        LayerPreference::Size {
            width: 1280,
            height: 720,
        },
        start,
    );
    assert_eq!(forwarder.wanted_layer(), Some(high()));
    assert_eq!(forwarder.target_layer(), high());
}
