
use super::*;
use crate::packet::DropReason;

const PUBLISHER: u32 = 0x1111_1111;
const RESTARTED: u32 = 0x2222_2222;
const TICK: Duration = Duration::from_millis(20);
const STEP: u32 = 1800;

fn video(seed: u64) -> TrackForwarder {
    TrackForwarder::new(ForwarderConfig::video(), StreamStart::from_seed(seed))
}

fn audio(seed: u64) -> TrackForwarder {
    TrackForwarder::new(ForwarderConfig::audio(), StreamStart::from_seed(seed))
}

fn base() -> Instant {
    Instant::now()
}

fn packet(ssrc: u32, seq: u16, timestamp: u32, arrival: Instant) -> InboundPacket {
    InboundPacket::new(ssrc, seq, timestamp, vec![0xab, 0xcd, 0xef], arrival)
}

fn keyframe(ssrc: u32, seq: u16, timestamp: u32, arrival: Instant) -> InboundPacket {
    packet(ssrc, seq, timestamp, arrival).with_keyframe(true)
}

fn sent(result: &Forwarding) -> &ForwardedPacket {
    result.sent().expect("packet is forwarded")
}

#[test]
fn a_video_subscriber_waits_for_a_keyframe_before_seeing_anything() {
    let now = base();
    let mut forwarder = video(1);

    assert!(forwarder.needs_keyframe());
    for step in 0..3u16 {
        let result = forwarder.forward(&packet(
            PUBLISHER,
            100 + step,
            9000 + u32::from(step) * STEP,
            now + TICK * u32::from(step),
        ));
        assert_eq!(result.dropped(), Some(DropReason::AwaitingKeyframe));
    }

    assert!(forwarder.poll_keyframe_request(now));
    assert!(!forwarder.poll_keyframe_request(now + Duration::from_millis(100)));
    assert!(forwarder.poll_keyframe_request(now + Duration::from_millis(600)));

    let result = forwarder.forward(&keyframe(PUBLISHER, 103, 9000 + 3 * STEP, now + TICK * 3));
    let first = sent(&result);

    assert_eq!(*first.seq, StreamStart::from_seed(1).seq);
    assert_eq!(first.ext_timestamp, StreamStart::from_seed(1).timestamp);
    assert!(first.switched);
    assert!(first.nackable);
    assert!(!forwarder.needs_keyframe());
    assert!(!forwarder.poll_keyframe_request(now + Duration::from_secs(5)));
    assert_eq!(forwarder.stats().gated, 3);
}

#[test]
fn an_audio_subscriber_starts_on_the_first_packet() {
    let now = base();
    let mut forwarder = audio(1);

    assert!(!forwarder.needs_keyframe());
    let result = forwarder.forward(&packet(PUBLISHER, 7, 480, now));
    let first = sent(&result);

    assert_eq!(*first.seq, StreamStart::from_seed(1).seq);
    assert!(!first.nackable);
}

#[test]
fn the_outgoing_stream_survives_a_publisher_restart() {
    let now = base();
    let mut forwarder = video(1);

    let mut last_seq = 0;
    let mut last_time = 0;
    for step in 0..4u16 {
        let result = forwarder.forward(
            &packet(
                PUBLISHER,
                500 + step,
                70_000 + u32::from(step) * STEP,
                now + TICK * u32::from(step),
            )
            .with_keyframe(step == 0),
        );
        let out = sent(&result);
        last_seq = *out.seq;
        last_time = out.ext_timestamp;
    }

    let held = forwarder.forward(&packet(RESTARTED, 9, 12, now + TICK * 5));
    assert_eq!(held.dropped(), Some(DropReason::AwaitingKeyframe));
    assert!(forwarder.needs_keyframe());

    let result = forwarder.forward(&keyframe(RESTARTED, 10, 21, now + TICK * 6));
    let first = sent(&result);

    assert_eq!(*first.seq, last_seq + 1);
    assert!(first.switched);
    assert!(
        first.ext_timestamp > last_time,
        "restart must not move the subscriber clock backwards"
    );
    assert_eq!(
        first.ext_timestamp - last_time,
        u64::from(TICK.as_millis() as u32 * 3 * 90),
        "the restart gap is paid for in the subscriber timestamp space"
    );

    let follow = forwarder.forward(&packet(RESTARTED, 11, 21 + STEP, now + TICK * 7));
    let follow = sent(&follow);
    assert_eq!(*follow.seq, last_seq + 2);
    assert_eq!(follow.ext_timestamp, first.ext_timestamp + u64::from(STEP));
    assert_eq!(forwarder.stats().source_starts, 2);
    assert_eq!(forwarder.stats().layer_switches, 1);
}

#[test]
fn a_source_switch_never_reuses_an_outgoing_sequence_number() {
    let now = base();
    let mut forwarder = video(21);
    let mut emitted: Vec<u64> = Vec::new();
    let mut record = |result: &Forwarding| {
        if let Some(packet) = result.sent() {
            emitted.push(*packet.seq);
        }
    };

    for step in 0..6u16 {
        let result = forwarder.forward(
            &packet(
                PUBLISHER,
                600 + step,
                30_000 + u32::from(step) * STEP,
                now + TICK * u32::from(step),
            )
            .with_keyframe(step == 0),
        );
        record(&result);
    }

    let late = forwarder.forward(&packet(PUBLISHER, 603, 30_000 + 3 * STEP, now + TICK * 6));
    assert_eq!(late.dropped(), Some(DropReason::Duplicate));

    let restart = forwarder.forward(&keyframe(RESTARTED, 1_000, 77, now + TICK * 7));
    record(&restart);
    assert!(sent(&restart).switched);

    for (index, seq) in [998u16, 999, 1_001, 1_002, 997].into_iter().enumerate() {
        let step = index as u32 + 8;
        let result = forwarder.forward(&packet(
            RESTARTED,
            seq,
            77 + u32::from(seq.wrapping_sub(1_000)) * STEP,
            now + TICK * step,
        ));
        record(&result);
    }

    let mut seen: Vec<u64> = emitted.clone();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(
        seen.len(),
        emitted.len(),
        "the outgoing stream reused a sequence number across the source switch: {emitted:?}"
    );

    let switch = emitted[6];
    assert_eq!(
        switch,
        emitted[5] + 1,
        "the new source must continue where the old one stopped"
    );
    assert!(
        emitted[6..].iter().all(|seq| *seq >= switch),
        "a packet from before the switch point was mapped below it: {emitted:?}"
    );
    assert_eq!(
        forwarder.stats().too_old,
        3,
        "packets the new source sent before the switch point must be refused"
    );
}

#[test]
fn sixteen_bit_wraparound_is_invisible_to_the_subscriber() {
    let now = base();
    let mut forwarder = video(3);

    let start = forwarder.forward(&keyframe(PUBLISHER, 65_533, 1000, now));
    let first = *sent(&start).seq;

    for (index, seq) in [65_534u16, 65_535, 0, 1, 2].into_iter().enumerate() {
        let step = index as u32 + 1;
        let result = forwarder.forward(&packet(
            PUBLISHER,
            seq,
            1000 + step * STEP,
            now + TICK * step,
        ));
        assert_eq!(*sent(&result).seq, first + u64::from(step));
    }
}

#[test]
fn a_late_packet_from_before_the_wrap_still_maps_below_it() {
    let now = base();
    let mut forwarder = video(3);

    forwarder.forward(&keyframe(PUBLISHER, 65_534, 20_000, now));
    forwarder.forward(&packet(PUBLISHER, 65_535, 20_000 + STEP, now + TICK));
    let after_wrap = forwarder.forward(&packet(PUBLISHER, 1, 20_000 + 3 * STEP, now + TICK * 2));
    let after_wrap = *sent(&after_wrap).seq;

    let late = forwarder.forward(&packet(PUBLISHER, 0, 20_000 + 2 * STEP, now + TICK * 3));
    assert_eq!(*sent(&late).seq, after_wrap - 1);

    let older = forwarder.forward(&packet(PUBLISHER, 65_533, 20_000 - STEP, now + TICK * 4));
    assert_eq!(*sent(&older).seq, after_wrap - 4);
}

#[test]
fn out_of_order_input_keeps_its_place_and_duplicates_are_dropped() {
    let now = base();
    let mut forwarder = video(4);

    let start = forwarder.forward(&keyframe(PUBLISHER, 200, 5000, now));
    let first = *sent(&start).seq;

    let third = forwarder.forward(&packet(PUBLISHER, 202, 5000 + 2 * STEP, now + TICK * 2));
    assert_eq!(*sent(&third).seq, first + 2);

    let second = forwarder.forward(&packet(PUBLISHER, 201, 5000 + STEP, now + TICK * 3));
    let second = sent(&second);
    assert_eq!(*second.seq, first + 1);
    assert_eq!(
        second.ext_timestamp,
        sent(&start).ext_timestamp + u64::from(STEP)
    );

    let again = forwarder.forward(&packet(PUBLISHER, 201, 5000 + STEP, now + TICK * 4));
    assert_eq!(again.dropped(), Some(DropReason::Duplicate));

    let repeat_head = forwarder.forward(&packet(PUBLISHER, 202, 5000 + 2 * STEP, now + TICK * 5));
    assert_eq!(repeat_head.dropped(), Some(DropReason::Duplicate));

    let ancient = forwarder.forward(&packet(
        PUBLISHER,
        200u16.wrapping_sub(2000),
        1,
        now + TICK * 6,
    ));
    assert_eq!(ancient.dropped(), Some(DropReason::TooOld));

    let stats = forwarder.stats();
    assert_eq!(stats.duplicates, 2);
    assert_eq!(stats.out_of_order, 1);
    assert_eq!(stats.too_old, 1);
}

#[test]
fn a_gap_in_the_publisher_stream_stays_a_gap() {
    let now = base();
    let mut forwarder = video(5);

    let start = forwarder.forward(&keyframe(PUBLISHER, 100, 4000, now));
    let first = *sent(&start).seq;

    let after_gap = forwarder.forward(&packet(PUBLISHER, 105, 4000 + 5 * STEP, now + TICK * 5));
    assert_eq!(*sent(&after_gap).seq, first + 5);

    let next = forwarder.forward(&packet(PUBLISHER, 106, 4000 + 6 * STEP, now + TICK * 6));
    assert_eq!(*sent(&next).seq, first + 6);
    assert_eq!(forwarder.stats().gaps, 1);
    assert_eq!(forwarder.stats().forwarded, 3);
}

#[test]
fn a_padding_only_packet_is_dropped_without_leaving_a_hole() {
    let now = base();
    let mut forwarder = video(6);

    let start = forwarder.forward(&keyframe(PUBLISHER, 300, 8000, now));
    let first = *sent(&start).seq;

    let padding = forwarder.forward(&InboundPacket::new(
        PUBLISHER,
        301,
        8000,
        Vec::new(),
        now + TICK,
    ));
    assert_eq!(padding.dropped(), Some(DropReason::PaddingOnly));

    let next = forwarder.forward(&packet(PUBLISHER, 302, 8000 + STEP, now + TICK * 2));
    assert_eq!(*sent(&next).seq, first + 1);

    let late = forwarder.forward(&packet(PUBLISHER, 301, 8000, now + TICK * 3));
    assert_eq!(late.dropped(), Some(DropReason::Duplicate));

    let following = forwarder.forward(&packet(PUBLISHER, 303, 8000 + 2 * STEP, now + TICK * 4));
    assert_eq!(*sent(&following).seq, first + 2);
    assert_eq!(forwarder.stats().padding, 1);
}

#[test]
fn two_subscribers_of_one_track_own_their_sequence_spaces() {
    let now = base();
    let mut first = video(11);
    let mut second = video(12);

    let opening = keyframe(PUBLISHER, 900, 60_000, now);
    let head = *sent(&first.forward(&opening)).seq;
    let other_head = *sent(&second.forward(&opening)).seq;
    assert_ne!(head, other_head);

    second.set_subscriber_paused(true);

    for step in 1..=4u16 {
        let feed = packet(
            PUBLISHER,
            900 + step,
            60_000 + u32::from(step) * STEP,
            now + TICK * u32::from(step),
        );
        assert_eq!(*sent(&first.forward(&feed)).seq, head + u64::from(step));
        assert_eq!(
            second.forward(&feed).dropped(),
            Some(DropReason::SubscriberPaused)
        );
    }

    second.set_subscriber_paused(false);
    assert!(second.needs_keyframe());

    let resumed = keyframe(PUBLISHER, 905, 60_000 + 5 * STEP, now + TICK * 5);
    assert_eq!(*sent(&first.forward(&resumed)).seq, head + 5);

    let restart = second.forward(&resumed);
    assert_eq!(*sent(&restart).seq, other_head + 1);
    assert_eq!(first.stats().forwarded, 6);
    assert_eq!(second.stats().forwarded, 2);
}

#[test]
fn a_muted_track_resumes_on_a_fresh_keyframe() {
    let now = base();
    let mut forwarder = video(7);

    let start = forwarder.forward(&keyframe(PUBLISHER, 10, 1000, now));
    let head = *sent(&start).seq;
    let head_time = sent(&start).ext_timestamp;

    forwarder.set_publisher_muted(true);
    let muted = forwarder.forward(&packet(PUBLISHER, 11, 1000 + STEP, now + TICK));
    assert_eq!(muted.dropped(), Some(DropReason::PublisherMuted));

    forwarder.set_publisher_muted(false);
    assert!(forwarder.needs_keyframe());

    let ignored = forwarder.forward(&packet(PUBLISHER, 40, 1000 + 30 * STEP, now + TICK * 30));
    assert_eq!(ignored.dropped(), Some(DropReason::AwaitingKeyframe));

    let resumed = forwarder.forward(&keyframe(
        PUBLISHER,
        41,
        1000 + 31 * STEP,
        now + Duration::from_secs(2),
    ));
    let resumed = sent(&resumed);

    assert_eq!(*resumed.seq, head + 1);
    assert_eq!(resumed.ext_timestamp, head_time + 2 * 90_000);
}

#[test]
fn the_marker_bit_and_frame_boundary_follow_the_source() {
    let now = base();
    let mut forwarder = video(8);

    forwarder.forward(&keyframe(PUBLISHER, 1, 1000, now));
    assert!(!forwarder.on_frame_boundary());

    let end = forwarder.forward(&packet(PUBLISHER, 2, 1000, now + TICK).with_marker(true));
    assert!(sent(&end).marker);
    assert!(forwarder.on_frame_boundary());

    let next = forwarder.forward(&packet(PUBLISHER, 3, 1000 + STEP, now + TICK * 2));
    assert!(!sent(&next).marker);
    assert!(!forwarder.on_frame_boundary());
}

#[test]
fn a_single_layer_track_never_reports_a_layer_switch_after_the_first_packet() {
    let now = base();
    let mut forwarder = video(9);

    let start = forwarder.forward(&keyframe(PUBLISHER, 1, 1000, now));
    assert!(sent(&start).switched);
    assert_eq!(forwarder.current_layer(), Some(SpatialLayer::BASE));

    for step in 1..5u16 {
        let result = forwarder.forward(
            &packet(
                PUBLISHER,
                1 + step,
                1000 + u32::from(step) * STEP,
                now + TICK * u32::from(step),
            )
            .with_keyframe(step == 2),
        );
        assert!(!sent(&result).switched);
    }

    assert!(!forwarder.request_layer(SpatialLayer::BASE));
    assert_eq!(forwarder.stats().layer_switches, 0);
}

#[test]
fn a_layer_request_asks_the_publisher_for_a_keyframe_at_once() {
    let now = base();
    let mut forwarder = video(13);

    forwarder.forward(&keyframe(PUBLISHER, 1, 1000, now));
    assert!(!forwarder.needs_keyframe());
    assert_eq!(forwarder.keyframe_layer(), None);

    assert!(forwarder.request_layer(SpatialLayer::new(1)));
    assert!(forwarder.needs_keyframe());
    assert_eq!(forwarder.keyframe_layer(), Some(SpatialLayer::new(1)));
    assert!(
        forwarder.poll_keyframe_request(now + TICK),
        "a switch must not wait out the keyframe interval of the previous request"
    );
}

#[test]
fn a_keyframe_of_the_layer_being_left_does_not_satisfy_the_switch() {
    let now = base();
    let mut forwarder = video(18);
    let target = SpatialLayer::new(1);

    forwarder.forward(&keyframe(PUBLISHER, 1, 1000, now));
    assert!(forwarder.request_layer(target));
    assert!(forwarder.poll_keyframe_request(now));

    let old_layer = forwarder.forward(&keyframe(PUBLISHER, 2, 1000 + STEP, now + TICK));
    assert!(sent(&old_layer).keyframe);
    assert!(!sent(&old_layer).switched);
    assert!(
        forwarder.needs_keyframe(),
        "a keyframe of the layer being left says nothing about the target"
    );
    assert_eq!(forwarder.keyframe_layer(), Some(target));
    assert!(forwarder.poll_keyframe_request(now + Duration::from_secs(1)));

    let switch =
        forwarder.forward(&keyframe(RESTARTED, 700, 50_000, now + TICK * 2).with_layer(target));
    assert!(sent(&switch).switched);
    assert!(!forwarder.needs_keyframe());
}

#[test]
fn going_back_to_the_layer_already_being_forwarded_needs_no_keyframe() {
    let now = base();
    let mut forwarder = video(14);

    forwarder.forward(&keyframe(PUBLISHER, 1, 1000, now));
    forwarder.request_layer(SpatialLayer::new(1));
    assert!(forwarder.needs_keyframe());

    forwarder.request_layer(SpatialLayer::BASE);

    assert!(!forwarder.needs_keyframe());
    assert_eq!(forwarder.keyframe_layer(), None);
    assert!(!forwarder.is_switching());
    assert!(!forwarder.poll_keyframe_request(now + Duration::from_secs(5)));
}

#[test]
fn a_switch_in_the_middle_of_a_frame_is_counted() {
    let now = base();
    let mut forwarder = video(15);
    let other = SpatialLayer::new(1);

    forwarder.forward(&keyframe(PUBLISHER, 1, 1000, now));
    forwarder.request_layer(other);
    forwarder.forward(&keyframe(RESTARTED, 500, 60_000, now + TICK).with_layer(other));

    assert_eq!(forwarder.stats().truncated_frames, 1);
    assert_eq!(forwarder.current_layer(), Some(other));

    forwarder.forward(
        &packet(RESTARTED, 501, 60_000, now + TICK * 2)
            .with_layer(other)
            .with_marker(true),
    );
    forwarder.request_layer(SpatialLayer::BASE);
    forwarder.forward(&keyframe(PUBLISHER, 40, 5000, now + TICK * 3));

    assert_eq!(
        forwarder.stats().truncated_frames,
        1,
        "a switch taken on a frame boundary breaks no frame"
    );
    assert_eq!(forwarder.stats().layer_switches, 2);
}

#[test]
fn a_paused_subscriber_wants_no_layer_at_all() {
    let now = base();
    let mut forwarder = video(16);

    assert_eq!(forwarder.wanted_layer(), Some(SpatialLayer::MAX));

    forwarder.set_subscriber_paused(true);
    assert_eq!(forwarder.wanted_layer(), None);

    forwarder.set_subscriber_paused(false);
    assert_eq!(forwarder.wanted_layer(), Some(SpatialLayer::MAX));

    forwarder.set_preference(
        crate::selector::LayerPreference::Quality(crate::ladder::VideoQuality::Low),
        now,
    );
    assert_eq!(forwarder.wanted_layer(), Some(SpatialLayer::BASE));
}

#[test]
fn a_muted_publisher_is_never_asked_for_a_keyframe() {
    let mut now = base();
    let mut forwarder = video(19);

    forwarder.forward(&keyframe(PUBLISHER, 1, 1000, now));
    forwarder.request_layer(SpatialLayer::new(1));
    forwarder.set_publisher_muted(true);

    assert_eq!(forwarder.keyframe_layer(), None);

    for _ in 0..10 {
        now += Duration::from_secs(1);
        assert!(
            !forwarder.poll_keyframe_request(now),
            "a muted publisher was poked for a keyframe it cannot answer"
        );
    }

    assert_eq!(forwarder.keyframe_attempts(), 0);

    forwarder.set_publisher_muted(false);
    assert_eq!(forwarder.keyframe_layer(), Some(SpatialLayer::new(1)));
    assert!(forwarder.poll_keyframe_request(now));
}

#[test]
fn a_paused_subscriber_asks_for_no_keyframe() {
    let now = base();
    let mut forwarder = video(20);

    forwarder.forward(&keyframe(PUBLISHER, 1, 1000, now));
    forwarder.request_layer(SpatialLayer::new(1));
    forwarder.set_subscriber_paused(true);

    assert_eq!(forwarder.keyframe_layer(), None);
    assert!(!forwarder.poll_keyframe_request(now + Duration::from_secs(1)));
}

#[test]
fn a_publisher_that_wakes_up_is_asked_again() {
    let mut now = base();
    let mut forwarder = video(22);
    let budget = forwarder.config().keyframe_attempts;

    forwarder.forward(&keyframe(PUBLISHER, 1, 1000, now));
    forwarder.request_layer(SpatialLayer::new(1));

    for _ in 0..(budget * 2) {
        forwarder.poll_keyframe_request(now);
        now += Duration::from_secs(1);
    }

    assert!(
        !forwarder.poll_keyframe_request(now),
        "the budget for a quiet publisher is spent"
    );
    assert!(forwarder.needs_keyframe());

    now += Duration::from_secs(1);
    forwarder.forward(&packet(PUBLISHER, 2, 2000, now));

    assert!(
        forwarder.poll_keyframe_request(now),
        "a subscriber that has never seen a frame must ask again once the publisher proves it is alive"
    );
}

#[test]
fn a_publisher_that_never_answers_is_asked_a_bounded_number_of_times() {
    let mut now = base();
    let mut forwarder = video(21);
    let budget = forwarder.config().keyframe_attempts;
    let mut asked = 0;

    forwarder.forward(&keyframe(PUBLISHER, 1, 1000, now));
    forwarder.request_layer(SpatialLayer::new(1));

    for _ in 0..(budget * 4) {
        if forwarder.poll_keyframe_request(now) {
            asked += 1;
        }
        now += Duration::from_secs(1);
    }

    assert_eq!(
        asked, budget,
        "a publisher that has gone quiet is asked for a keyframe for ever"
    );
    assert_eq!(forwarder.keyframe_layer(), None);
    assert!(
        forwarder.needs_keyframe(),
        "the subscriber still needs one, it just stopped asking"
    );

    assert!(forwarder.request_layer(SpatialLayer::MAX));
    assert!(
        forwarder.poll_keyframe_request(now),
        "a fresh decision starts a fresh budget"
    );
}

#[test]
fn a_publisher_that_comes_back_starts_a_fresh_budget() {
    let mut now = base();
    let mut forwarder = video(22);
    let budget = forwarder.config().keyframe_attempts;
    let target = SpatialLayer::new(1);

    forwarder.forward(&keyframe(PUBLISHER, 1, 1000, now));
    forwarder.request_layer(target);

    for _ in 0..budget {
        forwarder.poll_keyframe_request(now);
        now += Duration::from_secs(1);
    }

    assert!(!forwarder.poll_keyframe_request(now));

    forwarder.forward(&keyframe(RESTARTED, 700, 50_000, now).with_layer(target));

    assert!(!forwarder.needs_keyframe());
    assert_eq!(forwarder.current_layer(), Some(target));
    assert_eq!(forwarder.keyframe_attempts(), 0);
}

#[test]
fn an_audio_forwarder_never_asks_for_a_keyframe_of_a_layer() {
    let now = base();
    let mut forwarder = audio(17);

    forwarder.forward(&packet(PUBLISHER, 1, 480, now));
    assert!(!forwarder.request_layer(SpatialLayer::BASE));
    assert!(forwarder.request_layer(SpatialLayer::new(1)));
    assert!(!forwarder.needs_keyframe());
    assert_eq!(forwarder.keyframe_layer(), None);
}

#[test]
fn packets_on_a_layer_that_was_not_selected_are_dropped() {
    let now = base();
    let mut forwarder = video(10);

    forwarder.forward(&keyframe(PUBLISHER, 1, 1000, now));

    let other = forwarder
        .forward(&packet(PUBLISHER, 2, 1000 + STEP, now + TICK).with_layer(SpatialLayer::new(1)));

    assert_eq!(other.dropped(), Some(DropReason::WrongLayer));
    assert_eq!(forwarder.stats().wrong_layer, 1);
}

#[test]
fn a_bandwidth_pause_and_a_client_pause_are_independent() {
    let now = base();
    let mut forwarder = video(3);

    forwarder.set_subscriber_paused(true);
    forwarder.set_allowance(Allowance::Paused, now);

    assert!(forwarder.is_subscriber_paused());
    assert!(forwarder.is_bandwidth_paused());

    forwarder.set_subscriber_paused(false);
    assert!(
        forwarder.is_paused(),
        "a client cannot un-pause a track the allocator paused"
    );

    forwarder.set_allowance(Allowance::Capped(SpatialLayer::BASE), now);
    assert!(!forwarder.is_paused());
    assert_eq!(forwarder.allowance(), Allowance::Capped(SpatialLayer::BASE));
}

#[test]
fn a_bandwidth_paused_track_still_wants_its_layer() {
    let now = base();
    let mut forwarder = video(4);
    forwarder.observe_layers(LayerSet::all(), now);

    assert_eq!(forwarder.wanted_layer(), Some(SpatialLayer::MAX));

    forwarder.set_allowance(Allowance::Paused, now);
    assert_eq!(
        forwarder.wanted_layer(),
        Some(SpatialLayer::MAX),
        "one congested subscriber must not tell the publisher to stop encoding"
    );

    forwarder.set_subscriber_paused(true);
    assert_eq!(
        forwarder.wanted_layer(),
        None,
        "a track the client turned off leaves the demand union"
    );
}

#[test]
fn an_allowance_never_breaks_the_outgoing_sequence() {
    let now = base();
    let mut forwarder = video(9);
    let mut emitted: Vec<u64> = Vec::new();

    for step in 0..4u16 {
        let result = forwarder.forward(
            &packet(
                PUBLISHER,
                300 + step,
                50_000 + u32::from(step) * STEP,
                now + TICK * u32::from(step),
            )
            .with_keyframe(step == 0),
        );
        emitted.push(*sent(&result).seq);
    }

    forwarder.set_allowance(Allowance::Paused, now + TICK * 4);

    for step in 4..8u16 {
        let result = forwarder.forward(&packet(
            PUBLISHER,
            300 + step,
            50_000 + u32::from(step) * STEP,
            now + TICK * u32::from(step),
        ));
        assert_eq!(result.dropped(), Some(DropReason::SubscriberPaused));
    }

    forwarder.set_allowance(Allowance::Capped(SpatialLayer::BASE), now + TICK * 8);
    assert!(forwarder.needs_keyframe());

    let held = forwarder.forward(&packet(PUBLISHER, 308, 50_000 + 8 * STEP, now + TICK * 8));
    assert_eq!(held.dropped(), Some(DropReason::AwaitingKeyframe));

    let result = forwarder.forward(&keyframe(PUBLISHER, 309, 50_000 + 9 * STEP, now + TICK * 9));
    emitted.push(*sent(&result).seq);

    let follow = forwarder.forward(&packet(PUBLISHER, 310, 50_000 + 10 * STEP, now + TICK * 10));
    emitted.push(*sent(&follow).seq);

    let expected: Vec<u64> = (emitted[0]..emitted[0] + emitted.len() as u64).collect();
    assert_eq!(
        emitted, expected,
        "a bandwidth pause and resume must leave no hole in the subscriber's sequence"
    );
}

#[test]
fn an_allowance_caps_the_layer_the_forwarder_asks_for() {
    let now = base();
    let mut forwarder = video(11);
    forwarder.observe_layers(LayerSet::all(), now);
    assert_eq!(forwarder.target_layer(), SpatialLayer::MAX);

    assert!(forwarder.set_allowance(Allowance::Capped(SpatialLayer::BASE), now));
    assert_eq!(forwarder.target_layer(), SpatialLayer::BASE);

    assert!(forwarder.set_allowance(Allowance::Unlimited, now));
    assert_eq!(
        forwarder.target_layer(),
        SpatialLayer::MAX,
        "the subscriber never asked for less, so it goes back on its own"
    );
}
