use super::*;

/// The configuration every WebRTC client in the wild sends: fullband CELT
/// or hybrid, twenty milliseconds, one frame.
#[test]
fn the_ordinary_twenty_millisecond_packet_advances_the_clock_by_twenty_milliseconds() {
    // Config 15 (hybrid fullband, 20 ms), mono, one frame.
    let toc = read(&[0b0111_1000, 0x00]).expect("a plain 20 ms packet");

    assert_eq!(
        toc.samples, 960,
        "twenty milliseconds at 48 kHz is 960 samples, and a recording whose clock advances by \
             anything else drifts against every other participant's"
    );
    assert_eq!(toc.frames, 1);
    assert!(!toc.stereo);
}

#[test]
fn the_stereo_bit_is_what_says_how_many_channels_the_file_has() {
    let mono = read(&[0b0111_1000, 0x00]).expect("mono");
    let stereo = read(&[0b0111_1100, 0x00]).expect("stereo");

    assert!(!mono.stereo);
    assert!(
        stereo.stereo,
        "the channel count cannot come from the SDP -- opus/48000/2 is what a mono stream \
             negotiates too -- so it has to come from here"
    );
}

#[test]
fn two_frames_in_a_packet_are_twice_as_long_as_one() {
    let one = read(&[0b0111_1000, 0x00]).expect("one frame");
    let two = read(&[0b0111_1001, 0x00]).expect("two frames");

    assert_eq!(two.samples, one.samples * 2);
    assert_eq!(two.frames, 2);
}

/// The frame the recorder fills gaps with has to be one this reader agrees
/// about, or the granule positions and the packets in the file would
/// disagree about how long the silence was.
#[test]
fn the_silence_frame_is_twenty_milliseconds_of_one_frame() {
    for stereo in [false, true] {
        let toc = read(&silence(stereo)).expect("the silence frame is a packet");

        assert_eq!(toc.samples as u64, SILENCE_SAMPLES);
        assert_eq!(toc.frames, 1);
        assert_eq!(
            toc.stereo, stereo,
            "a filler frame that changes the channel count mid-file asks the decoder to \
                 switch modes in the middle of somebody's silence"
        );
    }
}

#[test]
fn a_packet_that_claims_more_than_opus_allows_is_refused() {
    // Code 3, 48 frames of 60 ms: 2.88 seconds, which RFC 6716 forbids.
    let absurd = read(&[0b0000_1111, 48]);

    assert!(
        absurd.is_none(),
        "a packet claiming 2.88 seconds would drag the granule position -- and the meeting's \
             recorded length -- forward by that much, once per packet"
    );
}

#[test]
fn a_truncated_packet_is_refused_rather_than_guessed_at() {
    assert!(read(&[]).is_none(), "an empty payload is not a packet");
    assert!(
        read(&[0b0000_1111]).is_none(),
        "a code-3 packet with no frame-count byte is truncated, and a guessed duration is a \
             made-up granule position that nothing downstream can check"
    );
    assert!(
        read(&[0b0000_1111, 0]).is_none(),
        "zero frames is not a packet either"
    );
}

#[test]
fn every_configuration_has_a_duration_opus_allows() {
    for config in 0u8..32 {
        let toc = read(&[config << 3, 0x00]).expect("one frame of every configuration");

        assert!(
            [120, 240, 480, 960, 1_920, 2_880].contains(&toc.samples),
            "configuration {config} decodes to {} samples, which is not one of the six frame \
                 durations Opus has",
            toc.samples
        );
    }
}
