use std::net::SocketAddr;

use mediacore_proto::livekit;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("`{0}` is not a usable server url")]
    InvalidUrl(String),
    #[error("signalling socket failed: {0}")]
    Socket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("signalling connection is closed")]
    Closed,
    #[error("server disconnected us: {0:?}")]
    Disconnected(livekit::DisconnectReason),
    #[error("undecodable signal response: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),
    #[error("missed {0} signal responses, the client fell behind")]
    Lagged(u64),
    #[error("webrtc failed: {0}")]
    Rtc(#[from] str0m::RtcError),
    #[error("io failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("the crypto provider produced no dtls certificate")]
    NoDtlsCertificate,
    #[error("nothing to negotiate after adding media")]
    NothingToNegotiate,
    #[error("no payload type for {codec} was negotiated on mid {mid}")]
    CodecNotNegotiated { codec: &'static str, mid: String },
    #[error("the publisher transport is gone")]
    TransportGone,
    #[error("cannot use {0} as a host candidate: {1}")]
    HostCandidate(SocketAddr, String),
}
