# mediacore

A selective forwarding unit written in Rust. The signalling protocol is compatible with LiveKit
clients, so existing `livekit-client` SDKs for web, iOS and Android connect to this server without
modification. Our own Rust and TypeScript SDKs are planned as additional clients speaking the same
wire protocol.

## Status

Early. Two participants in a room can see and hear each other: media published to the server is
forwarded to everyone who is subscribed to it.

| Milestone | Scope | State |
|---|---|---|
| M0 | Protocol profile, vendored protobufs, workspace | done |
| M1 | Signalling, auth, room and participant state | done |
| M2 | Publisher transport, RTP intake, NACK, PLI | done |
| M3 | Subscriber transport, packet forwarding | done |
| M4 | Simulcast, layer selection, dynacast | not started |
| M5 | Bandwidth estimation and allocation | not started |
| M6 | Data channels, permissions, own SDKs | not started |
| M7 | Node registry, signal relay, TURN, drain | not started |
| M8 | Metrics, load and interop testing | not started |

A client that connects today receives a `JoinResponse`, sees other participants join and leave,
answers pings, is displaced when the same identity joins again, and is disconnected with a stated
reason after 60 seconds of silence. It can add a track, offer, complete ICE and DTLS against the
server's ice-lite agent, and send SRTP that the server receives, decrypts and accounts for. Other
participants see the published track appear, change and go away in their participant updates.

Received RTP is forwarded. A participant gets a second peer connection — the subscriber transport —
the first time it actually needs one; the server is the offering side there, sends the offer as
`SignalResponse.Offer`, takes the client's `SignalRequest.Answer` and trickles candidates in both
directions. Subscription intent is desired state: a reconcile pass compares what a participant wants
against what it has and adds or removes one media section at a time. A client joins with
`auto_subscribe` on by default and is subscribed to every track it is permitted to receive,
including tracks published later. Every packet is rewritten into a sequence number and timestamp
space of its own per subscriber, so two subscribers of one track never share numbering, and an
unsubscribe followed by a resubscribe continues the space the subscriber already had rather than
jumping. A keyframe request from any subscriber becomes a PLI to the publisher, throttled to one
every 500 ms per track, and a new subscription always asks for one.

The subscriber is told the stream is active with `StreamStateUpdate`, and refusals come back as
`SubscriptionResponse`. The publisher is told its track has a subscriber with `TrackSubscribed`, and
gets a `SubscribedQualityUpdate` when its video track gains its first subscriber and another when it
loses its last.

`JoinResponse.subscriber_primary` stays `false`. The subscriber transport is created lazily, so a
client that treats it as the primary connection and waits for it before reporting itself connected
would wait until it has something to receive. Turning it on means offering the subscriber transport
eagerly at join time, which belongs with session resume in a later milestone.

Known gaps in M3: an unsubscribe stops the flow and parks the media section rather than
renegotiating it away, so a resubscribe reuses it; there is no per-subscriber bandwidth or layer
choice, so a simulcast publisher's layers all arrive and only the base layer is selected; the
subscriber never renegotiates on codec changes; and `UpdateTrackSettings` acts on `disabled` only,
with size, framerate and priority logged and ignored until M4 and M5. Known gaps in M2: no simulcast
layer selection (the client-declared layers are carried in `TrackInfo` but nothing acts on them), no
ICE restart, no bandwidth estimation, and `rtc.use_external_ip` is accepted but not implemented.
Known gaps in M1: session resume (`reconnect=1`) falls back to a full join, and requests outside the
implemented subset are logged and ignored.

## Protocol profile

The server implements a subset of the LiveKit signalling protocol and advertises protocol version
17. Requests outside the subset are logged and ignored rather than answered with an error, so a
client that sends them stays connected. The vendored `.proto` files under
`crates/proto/protobufs` are copied from `livekit/protocol` and are the single source of truth for
the wire format.

## Running

```
cp config-sample.yaml config.yaml
cargo run -p mediacore-server -- --config config.yaml
```

The config path also comes from `MEDIACORE_CONFIG`. Two sockets are opened: TCP `port` for
signalling and one UDP socket for all media.

The media socket binds `rtc.bind_address` on `rtc.udp_port`, or on the first free port in
`rtc.port_range_start..=rtc.port_range_end` when `udp_port` is `0`, or on an ephemeral port when the
range is `0` as well. The address put into ICE candidates is the first of: `rtc.advertise_addresses`
(an ip, or an `ip:port` to override the bound port), `rtc.node_ip`, the socket's own address when
`rtc.use_socket_address` is set and the bind address is not a wildcard, and otherwise the address of
the interface holding the default route. A wildcard bind never advertises the wildcard.

Endpoints:

- `GET /` — service name
- `GET /healthz` — room, participant, RTP and forwarding counters, and the ICE socket address
- `GET /rtc/validate?access_token=…` — token check, returns the identity
- `GET /rtc?access_token=…` — signalling WebSocket. `auto_subscribe=0` joins without subscribing
  to anything; it is on by default.

Tokens are standard LiveKit access tokens: HS256, `iss` is the API key, `sub` is the participant
identity, and the `video` claim carries the grants. Any LiveKit token generator works, including
`lk token create`.

## Layout

| Crate | Contents |
|---|---|
| `crates/proto` | protobuf definitions and generated types |
| `crates/auth` | access tokens and grants |
| `crates/config` | YAML configuration |
| `crates/room` | rooms, participants, tracks, subscription intent, signalling fan-out |
| `crates/transport` | ICE, DTLS-SRTP, RTP intake and egress, one task per peer connection |
| `crates/sfu` | packet forwarding: sequence and timestamp rewriting, keyframe gating, layers |
| `crates/client` | a programmable client that publishes real RTP, used by the tests |
| `crates/server` | HTTP and WebSocket endpoints, binary |

`crates/room` owns all shared state and knows nothing about transport; `crates/transport` speaks
str0m types and knows nothing about the LiveKit protocol; `crates/sfu` is pure packet arithmetic
with no IO and no async; `crates/server` owns the sockets, the reconcile pass and the protocol
mapping between them.

## Development

```
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all
```

The protobuf definitions are compiled by `protox`, so no system `protoc` is needed.
