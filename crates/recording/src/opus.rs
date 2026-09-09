//! Reading an Opus packet's first byte, which is as far into one as this
//! server ever goes.
//!
//! **This is not decoding.** The table of contents byte -- the TOC, RFC 6716
//! section 3.1 -- says how many frames the packet holds, how long each one is
//! and whether it is stereo, in a fixed table that every Opus implementation
//! shares. Reading it costs a shift and an array index; decoding costs a
//! decoder, a sample buffer and the fidelity of whatever is re-encoded
//! afterwards. Everything this crate needs -- how far a packet advances the
//! clock, and how many channels to declare in the file's header -- is in that
//! one byte, so that is where it stops.
//!
//! Why the duration matters more than it looks: it is what makes a silence a
//! silence. The Ogg granule position of a page is the position of the end of
//! the last packet on it, and the end of a packet is its start plus its
//! duration. A recorder that took the *next* packet's timestamp as this one's
//! end -- which is the obvious shortcut, and what a writer driven by timestamp
//! deltas does -- loses the last packet of every talkspurt and, at the end of
//! the file, the last packet altogether.

/// The 48 kHz sample counts an Opus frame can be, which is every duration the
/// format allows: 2.5, 5, 10, 20, 40 and 60 milliseconds.
const MS_2_5: u32 = 120;
const MS_5: u32 = 240;
const MS_10: u32 = 480;
const MS_20: u32 = 960;
const MS_40: u32 = 1_920;
const MS_60: u32 = 2_880;

/// How long one frame of each of the 32 TOC configurations is, in samples at
/// 48 kHz.
///
/// The configuration number also says which mode and which bandwidth the frame
/// uses, and neither of those matters here: two packets of the same length
/// advance the clock by the same amount whether they were coded as SILK or as
/// CELT. Only the duration is read out.
const FRAME_SAMPLES: [u32; 32] = [
    // 0..11 -- SILK, narrow/medium/wideband, 10 20 40 60 ms each.
    MS_10, MS_20, MS_40, MS_60, //
    MS_10, MS_20, MS_40, MS_60, //
    MS_10, MS_20, MS_40, MS_60, //
    // 12..15 -- hybrid, super-wideband and fullband, 10 and 20 ms.
    MS_10, MS_20, MS_10, MS_20, //
    // 16..31 -- CELT, four bandwidths, 2.5 5 10 20 ms each.
    MS_2_5, MS_5, MS_10, MS_20, //
    MS_2_5, MS_5, MS_10, MS_20, //
    MS_2_5, MS_5, MS_10, MS_20, //
    MS_2_5, MS_5, MS_10, MS_20,
];

/// The longest an Opus packet may be, in 48 kHz samples: 120 milliseconds.
///
/// RFC 6716 forbids more, and the code-3 frame count is six bits wide, so a
/// corrupt or hostile packet can otherwise claim 48 frames of 60 milliseconds
/// -- nearly three seconds -- and drag the granule position, and with it the
/// file's idea of how long the meeting was, three seconds forward per packet.
pub const MAX_PACKET_SAMPLES: u32 = 5_760;

/// How long the silence frame below is: twenty milliseconds at 48 kHz.
pub const SILENCE_SAMPLES: u64 = 960;

/// One Opus packet that decodes to silence, as constant bytes.
///
/// **Why a recorder writes silence at all.** An Opus sender stops sending while
/// nobody is talking, and the gap it leaves is real: the next packet's
/// timestamp says how long it lasted. Writing that gap only into the Ogg
/// granule position looks right -- the file's *length* comes out correct, and
/// `ffprobe` reports it correctly -- and is not enough. A demuxer works out
/// each packet's position by adding up the durations of the packets before it,
/// so a granule that jumps without packets to account for it produces a file
/// that claims two minutes and hands a decoder two seconds with the words
/// pushed together. Anything aligned against that is wrong by however long
/// somebody was quiet, which is the exact failure this recorder exists not to
/// have. So the silence is written down.
///
/// **This is not encoding anything.** These three bytes are a constant. There
/// is no encoder here, nothing is analysed and no audio is invented: a sender
/// that says nothing is saying "silence", and this writes that down at the
/// length its own clock gives. Measured rather than assumed -- decoded, the
/// frames come out at -91 dB, which is the sixteen-bit floor.
///
/// The table of contents byte is CELT fullband, 20 ms, one frame, with the
/// stereo bit set to match the stream so that a decoder is not asked to change
/// channel count in the middle of a file.
pub fn silence(stereo: bool) -> [u8; 3] {
    [if stereo { 0xFC } else { 0xF8 }, 0xFF, 0xFE]
}

/// What the first byte of a packet says about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Toc {
    /// How far this packet moves the 48 kHz clock.
    pub samples: u32,
    /// Whether the frames in it are stereo. This is what the file's channel
    /// count is taken from -- see [`crate::ogg`] -- because the SDP cannot
    /// answer it: `a=rtpmap:111 opus/48000/2` says 2 for every Opus stream
    /// there has ever been, mono ones included.
    pub stereo: bool,
    /// How many frames the packet holds, between 1 and 48.
    pub frames: u8,
}

/// Reads the table of contents, or `None` for a packet that is not one.
///
/// A packet this refuses is dropped rather than guessed at. The alternative --
/// assuming twenty milliseconds, which is what almost every packet is -- would
/// put a made-up duration into the granule position, and a granule position is
/// the one thing in this file nobody downstream can sanity-check.
pub fn read(payload: &[u8]) -> Option<Toc> {
    let toc = *payload.first()?;
    let frame = FRAME_SAMPLES[usize::from(toc >> 3)];
    let stereo = toc & 0b0000_0100 != 0;

    let frames: u8 = match toc & 0b0000_0011 {
        0 => 1,
        1 | 2 => 2,
        // Code 3: the byte after the TOC carries the count in its low six
        // bits. A packet that says it has frames and carries no second byte is
        // truncated, and a truncated packet is not one this writes down.
        _ => payload.get(1)? & 0b0011_1111,
    };

    if frames == 0 {
        return None;
    }

    let samples = frame.checked_mul(u32::from(frames))?;
    if samples > MAX_PACKET_SAMPLES {
        return None;
    }

    Some(Toc {
        samples,
        stereo,
        frames,
    })
}

#[cfg(test)]
#[path = "opus_tests.rs"]
mod tests;
