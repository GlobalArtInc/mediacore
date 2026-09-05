use std::net::SocketAddr;

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
    #[error("{0} is not implemented in this milestone")]
    NotImplemented(&'static str),
    #[error("webrtc error")]
    Rtc(#[from] str0m::RtcError),
}
