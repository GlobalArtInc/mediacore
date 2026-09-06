use std::io::Result;
use std::path::PathBuf;

use prost::Message as _;
use prost_types::{DescriptorProto, FieldDescriptorProto, FileDescriptorSet};

const PROTOS: &[&str] = &[
    "protobufs/livekit_models.proto",
    "protobufs/livekit_rtc.proto",
    "protobufs/livekit_metrics.proto",
    "protobufs/livekit_room.proto",
    "protobufs/livekit_egress.proto",
    "protobufs/livekit_agent.proto",
    "protobufs/livekit_agent_dispatch.proto",
];

/// The package the vendored files declare, and the one this build gives them.
///
/// The `.proto` files under `protobufs/` are copied from `livekit/protocol`
/// and say `package livekit;`, because that is what upstream says. mediacore
/// is its own product, and a foreign vendor's name in `livekit::ParticipantInfo`
/// and in every Twirp path is a tax on everyone who reads the code -- so the
/// package is renamed to `mediacore` here, once, and both consequences follow
/// from the descriptors rather than from anything that rewrites names later.
/// The Rust module becomes `mediacore_proto::mediacore`; the room API is
/// served at `/twirp/mediacore.RoomService/<Method>` because the service's
/// full name is what Twirp puts in the path.
///
/// The rename lives here rather than in the `.proto` text so that re-vendoring
/// stays a straight copy: nothing under `protobufs/` differs from upstream by
/// so much as a byte, the diff against a new release is honest, and there is
/// no patch to half-apply. See `protobufs/README.md`, which is where somebody
/// dropping a new file in will be standing.
///
/// The package name never travels on the wire -- protobuf sends field numbers,
/// not names -- and nothing here uses `google.protobuf.Any`, which is the only
/// construct that would put a fully-qualified type name into the bytes. So no
/// client can observe this through signalling; `livekit-client` included. The
/// Twirp URL was the one place the package was ever visible outside the
/// process, and `mediacore_server::rooms` keeps the old spelling of it alive
/// as an alias.
const UPSTREAM_PACKAGE: &str = "livekit";

/// What the package is called here. Everything else follows from it.
const PACKAGE: &str = "mediacore";

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

    let mut descriptors = protox::compile(PROTOS, ["protobufs"])
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    rename_package(&mut descriptors);
    let encoded = descriptors.encode_to_vec();

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo always sets OUT_DIR"));
    std::fs::write(out_dir.join(DESCRIPTOR_SET), &encoded)?;

    prost_build::Config::new()
        .compile_well_known_types()
        .extern_path(".google.protobuf", "::pbjson_types")
        .compile_fds(descriptors)?;

    pbjson_build::Builder::new()
        .register_descriptors(&encoded)?
        .build(&[format!(".{PACKAGE}")])?;

    Ok(())
}

/// Moves every type out of the upstream package and into ours.
///
/// This works on the compiled descriptors rather than on the `.proto` text,
/// and that is deliberate. A search-and-replace over the text has to tell a
/// package declaration and a type reference apart from the same word inside
/// `option go_package = "github.com/livekit/protocol/livekit"`, a licence
/// header and a comment -- three of which must not change and one of which
/// upstream rewrites whenever it likes. Here there is nothing to guess: the
/// package is a field, a fully-qualified reference is a field, and everything
/// else is untouched by construction.
///
/// The references matter as much as the declaration. Every `type_name`,
/// `extendee`, `input_type` and `output_type` in a descriptor is already
/// fully qualified and leading-dotted -- `.livekit.TrackInfo` -- whatever the
/// `.proto` wrote, so renaming the package without renaming these leaves the
/// generated code pointing at a module that no longer exists. That fails the
/// build rather than shipping, which is the good failure mode, but it is
/// cheaper to be exhaustive here.
fn rename_package(descriptors: &mut FileDescriptorSet) {
    let from = format!(".{UPSTREAM_PACKAGE}.");
    let to = format!(".{PACKAGE}.");
    let mut renamed = 0;

    for file in &mut descriptors.file {
        if file.package() == UPSTREAM_PACKAGE {
            file.package = Some(PACKAGE.to_owned());
            renamed += 1;
        }

        for message in &mut file.message_type {
            rename_in_message(message, &from, &to);
        }
        for extension in &mut file.extension {
            rename_in_field(extension, &from, &to);
        }
        for service in &mut file.service {
            for method in &mut service.method {
                rename_reference(&mut method.input_type, &from, &to);
                rename_reference(&mut method.output_type, &from, &to);
            }
        }
    }

    assert_eq!(
        renamed,
        PROTOS.len(),
        "{renamed} of the {} vendored files declare `package {UPSTREAM_PACKAGE}`, and this build \
         renames the package by name. Upstream has either split the schema across packages or \
         renamed its own -- decide which package mediacore's types come from before the generated \
         module quietly becomes two.",
        PROTOS.len()
    );
}

fn rename_in_message(message: &mut DescriptorProto, from: &str, to: &str) {
    for field in &mut message.field {
        rename_in_field(field, from, to);
    }
    for extension in &mut message.extension {
        rename_in_field(extension, from, to);
    }
    for nested in &mut message.nested_type {
        rename_in_message(nested, from, to);
    }
}

fn rename_in_field(field: &mut FieldDescriptorProto, from: &str, to: &str) {
    rename_reference(&mut field.type_name, from, to);
    rename_reference(&mut field.extendee, from, to);
}

fn rename_reference(name: &mut Option<String>, from: &str, to: &str) {
    if let Some(value) = name
        && let Some(rest) = value.strip_prefix(from)
    {
        *value = format!("{to}{rest}");
    }
}
