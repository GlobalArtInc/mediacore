use std::io::Result;

const PROTOS: &[&str] = &[
    "protobufs/livekit_models.proto",
    "protobufs/livekit_rtc.proto",
    "protobufs/livekit_metrics.proto",
    "protobufs/livekit_room.proto",
    "protobufs/livekit_egress.proto",
    "protobufs/livekit_agent.proto",
    "protobufs/livekit_agent_dispatch.proto",
];

fn main() -> Result<()> {
    for proto in PROTOS {
        println!("cargo:rerun-if-changed={proto}");
    }
    println!("cargo:rerun-if-changed=protobufs/logger/options.proto");

    let descriptors = protox::compile(PROTOS, ["protobufs"])
        .map_err(|error| std::io::Error::other(error.to_string()))?;

    prost_build::Config::new().compile_fds(descriptors)
}
