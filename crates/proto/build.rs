use std::io::Result;
use std::path::PathBuf;

use prost::Message as _;

const PROTOS: &[&str] = &[
    "protobufs/livekit_models.proto",
    "protobufs/livekit_rtc.proto",
    "protobufs/livekit_metrics.proto",
    "protobufs/livekit_room.proto",
    "protobufs/livekit_egress.proto",
    "protobufs/livekit_agent.proto",
    "protobufs/livekit_agent_dispatch.proto",
];

/// Where the compiled descriptors are kept for `FILE_DESCRIPTOR_SET`.
///
/// `prost_build`'s own `file_descriptor_set_path` writes this file only on the
/// `protoc` path; `compile_fds` takes the set by value and never writes it out.
/// So the bytes are written here, from the same `protox` compile that feeds
/// both generators -- one parse of the `.proto` files, three consumers of it,
/// and no way for the descriptors the code is generated from to differ from
/// the ones the OpenAPI document is generated from.
const DESCRIPTOR_SET: &str = "descriptors.bin";

fn main() -> Result<()> {
    for proto in PROTOS {
        println!("cargo:rerun-if-changed={proto}");
    }
    println!("cargo:rerun-if-changed=protobufs/logger/options.proto");

    let descriptors = protox::compile(PROTOS, ["protobufs"])
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let encoded = descriptors.encode_to_vec();

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo always sets OUT_DIR"));
    std::fs::write(out_dir.join(DESCRIPTOR_SET), &encoded)?;

    prost_build::Config::new()
        .compile_well_known_types()
        .extern_path(".google.protobuf", "::pbjson_types")
        .compile_fds(descriptors)?;

    pbjson_build::Builder::new()
        .register_descriptors(&encoded)?
        .build(&[".livekit"])?;

    Ok(())
}
