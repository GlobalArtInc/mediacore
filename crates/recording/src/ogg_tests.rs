
use super::*;

/// Twenty milliseconds of fullband Opus, mono, with a body that is not
/// audio and does not need to be: nothing in this crate looks past the
/// first byte.
fn packet(bytes: usize) -> Vec<u8> {
    let mut payload = vec![0u8; bytes.max(1)];
    payload[0] = 0b0111_1000;
    payload
}

fn stream() -> OggWriter<Vec<u8>> {
    OggWriter::new(Vec::new(), 0x1234_5678, 1, 48_000).expect("a stream into memory")
}

#[test]
fn a_stream_opens_with_the_two_headers_ogg_opus_requires() {
    let writer = stream();
    let bytes = writer.out;

    assert_eq!(&bytes[0..4], b"OggS");
    assert_eq!(
        bytes[5] & BEGIN_OF_STREAM,
        BEGIN_OF_STREAM,
        "the first page has to be marked as the start of a logical stream or no player will \
             open the file"
    );
    assert!(
        bytes.windows(8).any(|window| window == b"OpusHead"),
        "a file without an identification header is not Ogg Opus"
    );
    assert!(
        bytes.windows(8).any(|window| window == b"OpusTags"),
        "the comment header is not optional; a file without it is refused by strict readers"
    );
}

#[test]
fn every_page_carries_the_checksum_ogg_specifies() {
    let mut writer = stream();
    for index in 0..120 {
        writer
            .packet(&packet(80), (index + 1) * 960, false)
            .expect("a packet");
    }
    writer.flush(true).expect("the last page");

    let bytes = writer.out;
    let survey = survey(&bytes[..]).expect("a walk over memory");

    assert_eq!(
        survey.good_bytes,
        bytes.len() as u64,
        "a page whose checksum does not verify is a file players refuse, and this is the only \
             place the checksum is computed"
    );
    assert!(survey.complete, "the last page marks the end of the stream");
}

/// At this layer a gap is a jump in the clock, which is exactly right: this
/// writer is told where each packet ends and puts it there. Turning the
/// jump into silence a decoder can hear is the job of the layer above --
/// see `crate::writer` -- because that is where the sender's frame size and
/// channel count are known.
#[test]
fn a_gap_moves_the_clock_by_exactly_the_time_that_passed() {
    let mut writer = stream();

    // Ten packets, then two minutes in which the sender says nothing --
    // which is what discontinuous transmission does on every browser -- then
    // ten more.
    for index in 0..10u64 {
        writer
            .packet(&packet(80), (index + 1) * 960, false)
            .expect("a packet");
    }
    let after_gap = 10 * 960 + 48_000 * 120;
    writer
        .packet(&packet(80), after_gap + 960, true)
        .expect("the packet after the silence");
    for index in 1..10u64 {
        writer
            .packet(&packet(80), after_gap + (index + 1) * 960, false)
            .expect("a packet");
    }
    writer.flush(true).expect("the last page");

    let survey = survey(&writer.out[..]).expect("a walk over memory");
    let seconds = survey.granule as f64 / 48_000.0;

    assert!(
        (seconds - 120.4).abs() < 0.001,
        "the participant was there for two minutes and 400 milliseconds of speech, and the \
             file says {seconds:.3} seconds; a file that dropped the silence is wrong by however \
             long somebody was quiet, and everything after it is early by the same amount"
    );
}

#[test]
fn a_gap_ends_the_page_it_interrupts() {
    let mut writer = stream();
    for index in 0..5u64 {
        writer
            .packet(&packet(80), (index + 1) * 960, false)
            .expect("a packet");
    }
    let before = writer.written();
    writer
        .packet(&packet(80), 5 * 960 + 48_000 + 960, true)
        .expect("the packet after the gap");

    assert!(
        writer.written() > before,
        "the page under construction has to be written out before a discontinuity, or a \
             demuxer subtracting durations backwards from the page's granule attributes the gap \
             to the wrong packet"
    );
}

#[test]
fn a_page_never_describes_more_segments_than_ogg_allows() {
    let mut writer = stream();
    // Packets of 600 bytes take three lacing values each, so eighty-six of
    // them would overflow a segment table if the writer did not break the
    // page itself.
    for index in 0..200u64 {
        writer
            .packet(&packet(600), (index + 1) * 960, false)
            .expect("a packet");
    }
    writer.flush(true).expect("the last page");

    let bytes = writer.out;
    assert_eq!(
        survey(&bytes[..]).expect("a walk over memory").good_bytes,
        bytes.len() as u64,
        "a page with an overflowing segment table is a corrupt file"
    );
}

#[test]
fn a_torn_tail_is_told_from_the_part_of_the_file_that_is_whole() {
    let mut writer = stream();
    for index in 0..100u64 {
        writer
            .packet(&packet(80), (index + 1) * 960, false)
            .expect("a packet");
    }
    writer.flush(false).expect("a page");
    let whole = writer.written();
    let granule = writer.granule();

    // The power goes out halfway through a page.
    let mut bytes = writer.out;
    bytes.extend_from_slice(b"OggS\x00\x00\x01\x02\x03");

    let survey = survey(&bytes[..]).expect("a walk over memory");

    assert_eq!(
        survey.good_bytes, whole,
        "a node coming back from a crash has to be able to say where the file stopped being a \
             file"
    );
    assert_eq!(
        survey.granule, granule,
        "and how much audio it holds, which is the last whole page's granule and not the \
             number of bytes"
    );
    assert!(
        !survey.complete,
        "a file that was cut off must not look like one that was closed"
    );
}

#[test]
fn a_packet_longer_than_a_page_can_hold_is_dropped_rather_than_written_wrong() {
    let mut writer = stream();
    writer
        .packet(&packet(MAX_PACKET_BYTES + 1), 960, false)
        .expect("no error");
    writer.flush(true).expect("the last page");

    assert_eq!(
        survey(&writer.out[..]).expect("a walk over memory").granule,
        0,
        "no Opus packet is this long, so this is not one; writing it would need a continued \
             page and would put something that is not audio into the file"
    );
}
