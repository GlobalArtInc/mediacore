# mediacore

A selective forwarding unit written in Rust. The signalling protocol is compatible with LiveKit
clients, so existing `livekit-client` SDKs for web, iOS and Android connect to this server without
modification. Our own Rust and TypeScript SDKs are planned as additional clients speaking the same
wire protocol.

## Status

Early. Two participants in a room can see and hear each other: media published to the server is
forwarded to everyone who is subscribed to it, and a simulcast publisher's layers are picked apart
so every subscriber gets the one it can use.

| Milestone | Scope | State |
|---|---|---|
| M0 | Protocol profile, vendored protobufs, workspace | done |
| M1 | Signalling, auth, room and participant state | done |
| M2 | Publisher transport, RTP intake, NACK, PLI | done |
| M3 | Subscriber transport, packet forwarding | done |
| M4 | Simulcast, layer selection, dynacast | done |
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
every 500 ms and aimed at the layer that subscriber is on, and a new subscription asks for one as
soon as there is a layer arriving to ask. A muted publisher is never asked at all, and neither is a
paused subscription; asking is bounded to 20 tries for one need, so a publisher that has gone quiet
is not poked for ever. The count starts again whenever something changes that asking could fix: a
new target layer, an unmute, a resume, or the publisher restarting its stream. LiveKit throttles the
same way on the receiver side (`PLIThrottleConfig`, 500 ms for the low layer and a second for the
others) and bounds its gratuitous requests by a 30 s key frame seeder that stops as soon as a
keyframe arrives; its mute path stops the requester because a muted forwarder reports no layer to
ask on.

The subscriber is told the stream is active with `StreamStateUpdate`, and refusals come back as
`SubscriptionResponse`. The publisher is told its track has a subscriber with `TrackSubscribed`.

A simulcast publisher declares its encodings in `AddTrackRequest.layers`; those layers are carried in
`TrackInfo` and then kept current. Each negotiated rid becomes a spatial layer — by the quality the
publisher declared for it, falling back to the well known `q`/`h`/`f` and `2`/`1`/`0` ladders and
then to the order the rids were offered in — and a stream tracker watches which of them are actually
arriving. What the publisher declared is kept as one ladder, and both directions are read off it: the
quality a layer is reported and asked for under, and the layer a subscriber's `quality` request lands
on. A publisher that only sends `MEDIUM` and `HIGH` therefore has its medium encoding at the bottom
rung, and a subscriber asking for `MEDIUM` is put on that rung rather than on a middle layer that
does not exist. LiveKit derives both directions from the same declared layers as well
(`SpatialLayerToVideoQuality` and `VideoQualityToSpatialLayer`). A quality below everything published
is answered by the smallest rung and one above it by the largest. When the declaration cannot be
lined up with the rids at all — a different number of nameless layers than rids offered — nothing is
guessed: the declaration is logged and dropped for the rids it cannot name, and those layers keep the
well known ladder with no size. A rid the publisher did name is always believed, however many layers
it declared. A track that carries no rid is one layer described by the top rung declared for it.

`TrackInfo.layers` reports the layers that are arriving, each with the SSRC it arrived on, and the
declared ladder is what is reported until the first packet lands. A layer that goes quiet is
dropped from the list and re-appears when it comes back, so a subscriber is never offered a quality
nobody is sending.

Every subscriber picks its own layer. `UpdateTrackSettings` is now acted on in full: `disabled`
pauses the stream, a non-zero `width`/`height` picks the smallest layer whose shortest side covers
90% of what was asked for, and `quality` is used when no size is given — the same precedence the
reference uses. Settings that arrive before the subscription is bound are remembered and applied when
it binds. Nothing is answered back: like the reference, the server sends no reply to
`UpdateTrackSettings`, only a `StreamStateUpdate` when the paused state actually changes.

A layer switch never breaks the stream the subscriber already has. Packets of the layer being left
keep being forwarded until the new layer produces a keyframe, the first packet of the new layer
continues the subscriber's own sequence number and timestamp space, and a keyframe request goes to
the publisher naming the rid that subscriber needs rather than the track — asking the wrong layer for
a keyframe is the failure this milestone is most exposed to. A layer that is not arriving is never
asked; the request falls to the layer that is, which is the same overshoot the selector makes. The
500 ms throttle is counted per layer. Downgrades are immediate; an upgrade that comes from a
layer re-appearing is held for `video.layer_upgrade_hold_ms` (2 s by default) so a flapping layer
cannot pull a subscriber back and forth. A quality a subscriber asks for itself is never held.

Dynacast closes the loop back to the publisher. The union of what every subscriber of a track wants,
closed downwards, is the set of layers the publisher is asked to send, and it is sent as
`SubscribedQualityUpdate` — always three entries, `LOW` through `HIGH`, enabled up to the highest
layer wanted, as the reference sends them. The timing is deliberate and asymmetric: **enabling is
immediate**, so a layer is asked for before a subscriber needs it and never after, while
**disabling waits `video.dynacast_pause_delay_ms`** (5 s by default, LiveKit's `dynacast_pause_delay`)
so a subscriber changing its mind twice does not stop and restart the publisher's encoder. Demand
that returns inside the delay cancels the pause and nothing is sent at all. A track nobody has
subscribed to yet goes through the same disable path, so a publisher that no one is watching is told
to stop after one delay rather than immediately. On the way in, a disabled layer also has its NACKs
suppressed on the publisher transport, and a re-enabled one is asked for a keyframe on that rid the
moment it is turned back on. Only that direction restarts the stream tracker for a layer: a layer
being paused is still arriving for a while, and forcing it to look stopped would make the reported
layer list bounce. A decision counts as applied only once the publisher has actually been told; an
announcement that could not be delivered — a full or closed signal queue, a publisher that has left —
leaves the decision uncommitted and is made again on the next pass, so the publisher cannot be left
disagreeing with the server. Announcements always carry the state committed at the moment they are
sent, so two decisions taken back to back cannot reach the publisher out of order.

`JoinResponse.subscriber_primary` stays `false`. The subscriber transport is created lazily, so a
client that treats it as the primary connection and waits for it before reporting itself connected
would wait until it has something to receive. Turning it on means offering the subscriber transport
eagerly at join time, which belongs with session resume in a later milestone.

Known gaps in M4: temporal layers are not selected, only spatial ones; the VP8 payload descriptor is
not rewritten across a layer switch, which a browser subscriber may notice as a stall on a VP8 track;
`UpdateTrackSettings.fps` and `priority` are accepted and ignored; and layer choice is driven by
what the subscriber asks for, not by what its link can carry, which is M5. Known gaps in M3: an
unsubscribe stops the flow and parks the media section rather than renegotiating it away, so a
resubscribe reuses it; and the subscriber never renegotiates on codec changes. Known gaps in M2: no
ICE restart, no bandwidth estimation, and `rtc.use_external_ip` is accepted but not implemented.
Known gaps: session resume (`reconnect=1`) falls back to a full join, the `/rtc/v1` signalling path a
recent client tries first is not implemented so the client falls back to `/rtc`, and requests outside
the implemented subset are logged and ignored.

Verified against an unmodified `livekit-client` 2.22.2 in a real browser: two participants join,
one publishes audio and video, the other subscribes, and media arrives. Audio forwarded one packet
for one, with nothing lost. The browser only turns simulcast on above a small resolution, so the
simulcast tests drive `crates/client`, which publishes the encodings deliberately.

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

`video.dynacast_pause_delay_ms` is how long a layer nobody wants is left running before the publisher
is told to stop it, and `video.layer_upgrade_hold_ms` is how long a layer that re-appears must keep
arriving before a subscriber is moved back up onto it. Both only delay taking something away or
moving up; nothing that a subscriber needs now waits on them.

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
