use std::net::SocketAddr;

use str0m::media::{Mid, Rid};

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("binding udp socket on {addr}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("reading local address of the ice socket")]
    LocalAddr(#[source] std::io::Error),
    #[error("the crypto provider cannot generate a dtls certificate")]
    NoDtlsCertificate,
    #[error("ice ufrag `{0}` is already registered")]
    DuplicateUfrag(String),
    #[error("no advertised address produced a usable local candidate")]
    NoLocalCandidate,
    #[error("the router refused to rekey the session to ufrag `{0}`")]
    RouteRekey(String),
    #[error("peer connection is gone")]
    PeerGone,
    #[error("invalid ice candidate: {0}")]
    Candidate(String),
    #[error("nothing to negotiate")]
    NothingToNegotiate,
    #[error("no local offer is awaiting an answer")]
    NoPendingOffer,
    #[error("too many renegotiations are already queued")]
    NegotiationBacklog,
    #[error("a local offer was never answered, this session can no longer renegotiate")]
    NegotiationFailed,
    #[error("this session has no media with mid `{0}`")]
    UnknownMid(Mid),
    #[error("mid `{mid}` did not negotiate a layer with rid `{rid}`")]
    UnknownLayer { mid: Mid, rid: Rid },
    #[error("mid `{0}` carries several layers, name the rid instead of the whole track")]
    AmbiguousLayer(Mid),
    #[error("bandwidth estimation belongs on a subscriber peer, a publisher only sends rtcp")]
    BweOnPublisher,
    #[error("this peer was built without bandwidth estimation and cannot be steered")]
    BweDisabled,
    #[error("this peer was built without data channels and cannot carry data")]
    DataDisabled,
    #[error("a data packet of {bytes} bytes is over the {limit} byte limit")]
    DataPacketTooLarge { bytes: usize, limit: usize },
    #[error("{0} is not implemented in this milestone")]
    NotImplemented(&'static str),
    #[error("webrtc error")]
    Rtc(#[from] str0m::RtcError),
}
