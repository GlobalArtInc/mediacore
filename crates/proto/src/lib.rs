#[allow(clippy::all)]
pub mod livekit {
    include!(concat!(env!("OUT_DIR"), "/livekit.rs"));
}

#[allow(clippy::all)]
pub mod logger {
    include!(concat!(env!("OUT_DIR"), "/logger.rs"));
}

pub const PROTOCOL_VERSION: i32 = 17;

pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
