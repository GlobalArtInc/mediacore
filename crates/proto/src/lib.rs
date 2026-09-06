#[allow(clippy::all)]
pub mod livekit {
    include!(concat!(env!("OUT_DIR"), "/livekit.rs"));
    include!(concat!(env!("OUT_DIR"), "/livekit.serde.rs"));
}

#[allow(clippy::all)]
pub mod logger {
    include!(concat!(env!("OUT_DIR"), "/logger.rs"));
}

/// The `.proto` files as protobuf sees them: an encoded `FileDescriptorSet`.
///
/// The build already parses every file to generate the types and their JSON
/// codecs, and used to throw the result away. Keeping it is what lets anything
/// else in the tree be *derived* from the schema rather than written to match
/// it -- the OpenAPI document `mediacore-server` serves at `/swagger` is built
/// from these bytes, so a field added to `CreateRoomRequest` appears on the
/// documentation page in the same build that puts it on the wire. Written by
/// hand, that document is correct for about a month.
///
/// It carries `source_code_info`, which is why it is 190 kB rather than 60 kB:
/// the comments in the `.proto` are the field documentation, and a page that
/// lists field names without saying what they mean is a schema dump, not
/// documentation.
pub const FILE_DESCRIPTOR_SET: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/descriptors.bin"));

pub const PROTOCOL_VERSION: i32 = 17;

pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
