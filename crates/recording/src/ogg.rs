//! Opus packets in an Ogg container, written straight through.
//!
//! The packets arriving here came off the wire as Opus and go into the file as
//! Opus: no decoder, no encoder, no resampler, nothing between the bytes a
//! participant's browser produced and the bytes in the object store. What the
//! container adds is framing and a clock, and both are arithmetic.
//!
//! # The clock, which is the whole point
//!
//! An Ogg page carries a **granule position**: the number of 48 kHz samples
//! decodable from the start of the stream up to the end of the last packet
//! that finishes on that page (RFC 7845 section 4). This writer computes it
//! from the **RTP timestamps of the packets themselves** -- the sender's clock,
//! which runs whether or not the sender is talking.
//!
//! That one decision is what makes silence survive. A recorder that counted
//! the samples it wrote instead would put a participant's two minutes of
//! silence nowhere: the file would be two minutes shorter than the meeting,
//! every later word in it would be two minutes early, and the error would
//! compound across every gap. Opus senders stop sending during silence -- that
//! is what discontinuous transmission is for, and it is on by default in every
//! browser -- so this is not an unusual case, it is most of a meeting.
//!
//! So a gap in the arriving timestamps moves the granule position by exactly
//! the time that passed, and the file comes out as long as the participant was
//! in the meeting.
//!
//! **The granule alone is not enough**, and finding that out is worth writing
//! down. A demuxer does not read a position off every page: it works out where
//! each packet starts by adding up the durations of the packets before it, and
//! uses the last page's granule for the file's total length. A file with a
//! granule jump and no packets to account for it therefore *reports* two
//! minutes and hands a decoder two seconds with the words pushed together --
//! which is the failure this whole design exists to avoid, arriving through the
//! back door. So gaps are filled with silence, at the length the sender's clock
//! gives, and the file's packets and its granule agree everywhere. See
//! [`crate::opus::silence`] and [`Survey::packet_samples`].
//!
//! **A page is flushed before a discontinuity.** After the fill there is at most
//! a fraction of a frame left over, and ending the page there keeps even that
//! exactly where it belongs. It costs 27 bytes.
//!
//! # What the header says
//!
//! `OpusHead` declares a channel count and a pre-skip. The channel count is
//! read from the first packet's own table of contents rather than from the SDP,
//! because `a=rtpmap:111 opus/48000/2` says two channels for every Opus stream
//! including the mono ones every browser actually sends. The pre-skip is zero:
//! pre-skip exists to discard an encoder's warm-up, and nothing here encodes
//! anything, so there is nothing to discard and claiming otherwise would throw
//! away the first milliseconds of somebody's first word.

use std::io::{self, Write};

/// The CRC Ogg uses: polynomial `0x04c11db7`, most significant bit first, no
/// reflection of input or output and no final inversion.
///
/// Deliberately not the CRC-32 everything else in the world means by that
/// name, which reflects both and inverts the result. This is the one place a
/// generic checksum routine would produce a file that every player refuses,
/// with nothing on screen but "not an Ogg stream", so the table is built here
/// from the polynomial rather than borrowed from anything.
const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0usize;

    while index < 256 {
        let mut value = (index as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 0x8000_0000 != 0 {
                (value << 1) ^ 0x04c1_1db7
            } else {
                value << 1
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }

    table
}

const CRC_TABLE: [u32; 256] = crc_table();

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0;

    for byte in bytes {
        crc = (crc << 8) ^ CRC_TABLE[usize::from(((crc >> 24) as u8) ^ byte)];
    }

    crc
}

/// The most segments one Ogg page can describe, one byte each in its segment
/// table.
const MAX_SEGMENTS: usize = 255;

/// The longest packet that fits in a page of its own: 255 segments of 255
/// bytes.
///
/// Ogg can carry a longer one by continuing it onto the next page, and this
/// writer does not, because an Opus packet cannot be this long. The format's
/// own ceiling is 48 frames of 1275 bytes, and an RTP packet has to fit in a
/// datagram besides. A payload larger than this is not an Opus packet and is
/// dropped rather than written into a file that claims it is one.
pub const MAX_PACKET_BYTES: usize = MAX_SEGMENTS * 255;

/// How many packets accumulate on one page before it is written out.
///
/// Fifty twenty-millisecond packets is a second of audio behind a 27-byte
/// header, which is the trade this number is: a page per packet would spend a
/// quarter of the file on headers, and a page per minute would lose a minute
/// to a power cut. A second is also the resolution a player seeks at.
const PACKETS_PER_PAGE: usize = 50;

/// Ogg's own page-header flags. There is no continuation flag here because
/// nothing this writer produces spans two pages: see [`MAX_PACKET_BYTES`].
const BEGIN_OF_STREAM: u8 = 0x02;
const END_OF_STREAM: u8 = 0x04;

/// Writes one logical Ogg stream carrying one participant's Opus.
pub struct OggWriter<W: Write> {
    out: W,
    serial: u32,
    page: u32,
    /// The packet bodies waiting to go into the page being built.
    body: Vec<u8>,
    /// Their lengths in Ogg's lacing form: as many 255s as fit, then the
    /// remainder.
    lacing: Vec<u8>,
    packets: usize,
    /// The granule the page under construction will carry: the end of the last
    /// packet added to it.
    staged: u64,
    /// The granule the last page written out carried, which is how much audio
    /// is actually in the file.
    granule: u64,
    written: u64,
}

impl<W: Write> OggWriter<W> {
    /// Opens a stream and writes its two header pages.
    ///
    /// `serial` names this logical stream inside the file. It is random rather
    /// than counted so that two files concatenated by accident -- by a backup
    /// tool, by a shell loop -- are two streams a player can tell apart rather
    /// than one it silently mangles.
    pub fn new(out: W, serial: u32, channels: u8, input_sample_rate: u32) -> io::Result<Self> {
        let mut writer = Self {
            out,
            serial,
            page: 0,
            body: Vec::new(),
            lacing: Vec::new(),
            packets: 0,
            staged: 0,
            granule: 0,
            written: 0,
        };

        // Each header goes on a page of its own with granule zero, which is
        // what RFC 7845 requires: a demuxer reads the identification header out
        // of the first page of the stream and nothing else may share it.
        writer.stage(&opus_head(channels, input_sample_rate));
        writer.emit(BEGIN_OF_STREAM, 0)?;
        writer.stage(&opus_tags());
        writer.emit(0, 0)?;

        Ok(writer)
    }

    /// How many bytes of file this stream has produced, which is what a size
    /// limit is judged against.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Hands back whatever was being written into, once nothing more is going
    /// into it.
    pub fn into_inner(self) -> W {
        self.out
    }

    /// Adds one packet, ending `granule_end` samples into the stream.
    ///
    /// `discontinuous` says that this packet does not begin where the previous
    /// one ended -- a gap. The page under construction is written out first, so
    /// that the gap falls on a page boundary and every packet's position stays
    /// exactly recoverable.
    pub fn packet(
        &mut self,
        payload: &[u8],
        granule_end: u64,
        discontinuous: bool,
    ) -> io::Result<()> {
        if payload.len() > MAX_PACKET_BYTES {
            return Ok(());
        }

        if discontinuous && self.packets > 0 {
            self.flush(false)?;
        }

        let segments = payload.len() / 255 + 1;
        if self.lacing.len() + segments > MAX_SEGMENTS || self.packets >= PACKETS_PER_PAGE {
            self.flush(false)?;
        }

        self.stage(payload);
        self.staged = granule_end;

        Ok(())
    }

    /// Writes out the page under construction, if there is one.
    ///
    /// `last` marks it as the end of the stream, which is what tells a player
    /// the file is whole rather than cut off.
    pub fn flush(&mut self, last: bool) -> io::Result<()> {
        if self.packets == 0 && !last {
            return Ok(());
        }

        let granule = self.staged;
        self.emit(if last { END_OF_STREAM } else { 0 }, granule)?;
        self.granule = granule;

        Ok(())
    }

    fn emit(&mut self, flags: u8, granule: u64) -> io::Result<()> {
        let page = self.take_page(flags, granule);
        self.out.write_all(&page)?;
        self.written += page.len() as u64;

        Ok(())
    }

    /// The granule position of the last page written out: how much of the
    /// meeting this file covers, in 48 kHz samples.
    pub fn granule(&self) -> u64 {
        self.granule
    }

    fn stage(&mut self, packet: &[u8]) {
        let mut left = packet.len();
        while left >= 255 {
            self.lacing.push(255);
            left -= 255;
        }
        // Always a final value below 255, a zero included: it is the terminator
        // that says the packet ended here rather than continuing.
        self.lacing.push(left as u8);
        self.body.extend_from_slice(packet);
        self.packets += 1;
    }

    fn take_page(&mut self, flags: u8, granule: u64) -> Vec<u8> {
        let page = build_page(
            self.serial,
            self.page,
            flags,
            granule,
            &self.lacing,
            &self.body,
        );

        self.page = self.page.wrapping_add(1);
        self.lacing.clear();
        self.body.clear();
        self.packets = 0;

        page
    }
}

/// One Ogg page, checksum and all.
fn build_page(
    serial: u32,
    index: u32,
    flags: u8,
    granule: u64,
    lacing: &[u8],
    body: &[u8],
) -> Vec<u8> {
    let mut page = Vec::with_capacity(27 + lacing.len() + body.len());
    page.extend_from_slice(b"OggS");
    page.push(0);
    page.push(flags);
    page.extend_from_slice(&granule.to_le_bytes());
    page.extend_from_slice(&serial.to_le_bytes());
    page.extend_from_slice(&index.to_le_bytes());
    // The checksum is computed over the page with these four bytes zero and
    // then written into them.
    page.extend_from_slice(&0u32.to_le_bytes());
    page.push(lacing.len() as u8);
    page.extend_from_slice(lacing);
    page.extend_from_slice(body);

    let checksum = crc32(&page);
    page[22..26].copy_from_slice(&checksum.to_le_bytes());

    page
}

/// A page that closes a stream and carries nothing else.
///
/// What a node appends to a recording it finds half-written on its disk after a
/// crash. The audio in such a file is whole up to its last good page -- see
/// [`survey`] -- and all that is missing is the flag that says so, without
/// which every reader treats the file as truncated and some of them refuse to
/// report its length. Twenty-seven bytes turn it back into a file.
pub fn closing_page(serial: u32, index: u32, granule: u64) -> Vec<u8> {
    build_page(serial, index, END_OF_STREAM, granule, &[], &[])
}

/// The identification header, RFC 7845 section 5.1.
///
/// `input_sample_rate` is documentation and nothing else: it records the rate
/// the audio was captured at for anybody who cares, and a decoder always
/// produces 48 kHz whatever it says. Pre-skip is zero -- see the module note.
fn opus_head(channels: u8, input_sample_rate: u32) -> Vec<u8> {
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1);
    head.push(channels.clamp(1, 2));
    head.extend_from_slice(&0u16.to_le_bytes());
    head.extend_from_slice(&input_sample_rate.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes());
    head.push(0);
    head
}

/// The comment header, RFC 7845 section 5.2. Required, and there is nothing
/// worth putting in it: everything anybody needs to know about this file --
/// which meeting, which participant, where it sits in the meeting -- is in the
/// database row that names it, where it can be queried instead of parsed.
fn opus_tags() -> Vec<u8> {
    let vendor = b"mediacore";
    let mut tags = Vec::with_capacity(8 + 4 + vendor.len() + 4);
    tags.extend_from_slice(b"OpusTags");
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&0u32.to_le_bytes());
    tags
}

/// What a walk over a file's pages found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Survey {
    /// How many bytes of the file are whole pages with correct checksums. A
    /// file that was being written when the power went out has fewer of these
    /// than it has bytes, and the difference is the torn tail.
    pub good_bytes: u64,
    /// The granule position of the last whole page: how much audio the file
    /// holds.
    pub granule: u64,
    /// Whether the last whole page is marked as the end of the stream, which is
    /// how a file closed properly is told from one that was cut off.
    pub complete: bool,
    /// The durations of every packet on every whole page, added up.
    ///
    /// **For a correct file this equals `granule`, and that is the invariant
    /// worth checking.** The granule position alone is what a file *claims* its
    /// length is, and what `ffprobe` reports; the packets are what a decoder
    /// actually gets, because a demuxer works out where each one starts by
    /// adding the durations before it. A file whose granule runs ahead of its
    /// packets claims two minutes and hands a decoder two seconds with the
    /// words pushed together -- which is why gaps are filled with silence
    /// rather than left as a jump. See [`crate::opus::silence`].
    pub packet_samples: u64,
    /// The logical stream this file carries, out of its first page, and the
    /// number the next page in it would have. Both only so that a repair can
    /// append a closing page that belongs to the same stream: a page with the
    /// wrong serial is a second stream, and one with a repeated sequence number
    /// is a hole.
    pub serial: u32,
    pub next_page: u32,
}

/// Walks a stream's pages and says how much of it is whole.
///
/// This is what a node runs over its own directory when it comes back from a
/// crash. It reads only page headers and checksums -- it does not decode, and
/// it does not need to: the last good page's granule position is how long the
/// recording is, because the granule position was written from the sender's
/// own clock.
///
/// It reads forwards rather than seeking to the end, because the question is
/// not "what does the last page say" but "where does this file stop being a
/// file": a page torn in half by a power cut sits after perfectly good ones,
/// and the answer is the byte where the good ones end.
pub fn survey(mut input: impl std::io::Read) -> io::Result<Survey> {
    let mut survey = Survey::default();
    let mut page: Vec<u8> = Vec::with_capacity(27 + MAX_SEGMENTS + MAX_PACKET_BYTES);

    loop {
        page.clear();
        page.resize(27, 0);
        if !fill(&mut input, &mut page)? || &page[0..4] != b"OggS" {
            break;
        }

        let segments = usize::from(page[26]);
        page.resize(27 + segments, 0);
        if !fill(&mut input, &mut page[27..])? {
            break;
        }

        let body: usize = page[27..].iter().map(|length| usize::from(*length)).sum();
        let table_end = page.len();
        page.resize(table_end + body, 0);
        if !fill(&mut input, &mut page[table_end..])? {
            break;
        }

        let claimed = u32::from_le_bytes([page[22], page[23], page[24], page[25]]);
        let granule = u64::from_le_bytes([
            page[6], page[7], page[8], page[9], page[10], page[11], page[12], page[13],
        ]);
        let flags = page[5];
        page[22..26].fill(0);
        if crc32(&page) != claimed {
            break;
        }

        // A page holding no completed packet carries -1 and does not move the
        // clock. Nothing this writer produces does, but a file that arrived
        // here some other way may.
        if granule != u64::MAX {
            survey.granule = granule;
        }
        // The packets on this page, split out of the body by the lacing table,
        // so that what the file claims and what it holds can be compared. The
        // two headers are packets and are not audio.
        let mut at = table_end;
        let mut packet = 0usize;
        for length in &page[27..table_end] {
            packet += usize::from(*length);
            if *length == 255 {
                continue;
            }

            let body = &page[at..at + packet];
            if !body.starts_with(b"OpusHead") && !body.starts_with(b"OpusTags") {
                survey.packet_samples +=
                    crate::opus::read(body).map_or(0, |toc| u64::from(toc.samples));
            }
            at += packet;
            packet = 0;
        }

        survey.complete = flags & END_OF_STREAM != 0;
        survey.good_bytes += page.len() as u64;
        survey.serial = u32::from_le_bytes([page[14], page[15], page[16], page[17]]);
        survey.next_page =
            u32::from_le_bytes([page[18], page[19], page[20], page[21]]).wrapping_add(1);
    }

    Ok(survey)
}

/// Reads exactly as many bytes as the buffer holds, or says the stream ran
/// out. A short read at the end of a file is the torn page, not an error.
fn fill(input: &mut impl std::io::Read, buffer: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;

    while filled < buffer.len() {
        match input.read(&mut buffer[filled..]) {
            Ok(0) => return Ok(false),
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }

    Ok(true)
}

#[cfg(test)]
#[path = "ogg_tests.rs"]
mod tests;
