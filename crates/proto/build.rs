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

    let mut config = prost_build::Config::new();
    config.protoc_arg("--experimental_allow_proto3_optional");
    config.compile_protos(PROTOS, &["protobufs"])
}
