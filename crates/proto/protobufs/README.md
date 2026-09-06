# Vendored protobufs

Copied from [`livekit/protocol`](https://github.com/livekit/protocol) and kept **byte for byte** as
upstream wrote them, licence headers, `option go_package` and `package livekit;` included. Do not
edit them. Re-vendoring is a straight copy, and the diff against a new upstream release should show
only what upstream changed.

## The package says `livekit` and the code says `mediacore`

`crates/proto/build.rs` renames the package to `mediacore` on the **compiled descriptors**, after
protox has parsed these files and before prost generates a line of Rust. That is why nothing in this
directory mentions mediacore.

The rename lives there rather than here on purpose. Editing `package livekit;` in seven files turns
every future re-vendor into a patching exercise — one that can half-apply, leaving some types in one
package and some in another, which is exactly the kind of breakage that compiles for a while. Doing
it on the descriptors means the package declaration and every fully-qualified reference
(`.livekit.TrackInfo` and friends) move together or the build fails, and it means this directory
stays a mirror.

Two things follow from that one rename, and neither is a rewrite of anything on the way out:

- the Rust types are `mediacore_proto::mediacore::*`;
- the room API is served at `POST /twirp/mediacore.RoomService/<Method>`, because Twirp derives its
  path from `<package>.<service>`. The old path is kept as a compatibility alias — LiveKit's server
  SDKs build it in generated code — under `twirp.livekit_alias`, on by default. See "Server API" in
  the root README.

None of it is visible to a client. Protobuf transmits field numbers, not names, and the one
construct that would put a fully-qualified type name into the bytes, `google.protobuf.Any`, appears
nowhere in this schema. An unmodified `livekit-client` cannot tell the difference over signalling.

## When you re-vendor

1. Copy the new files in unchanged. Do not fix up the package.
2. Add any new file to `PROTOS` in `crates/proto/build.rs`.
3. Build. If upstream has renamed or split its package, `rename_package` fails the build with a
   count that does not match, and the decision about which package mediacore's types come from is
   yours to make explicitly rather than to discover later in a module that quietly became two.
4. If upstream introduces `google.protobuf.Any` anywhere, stop: a type name would then travel on the
   wire, and the rename stops being invisible to clients.
