# mediacore

A selective forwarding unit written in Rust. The signalling protocol is compatible with LiveKit
clients, so existing `livekit-client` SDKs for web, iOS and Android connect to this server without
modification. Our own Rust and TypeScript SDKs are planned as additional clients speaking the same
wire protocol.

## Status

Early. The signalling layer works; media does not exist yet.

| Milestone | Scope | State |
|---|---|---|
| M0 | Protocol profile, vendored protobufs, workspace | done |
| M1 | Signalling, auth, room and participant state | done |
| M2 | Publisher transport, RTP intake, NACK, PLI | not started |
| M3 | Subscriber transport, packet forwarding | not started |
| M4 | Simulcast, layer selection, dynacast | not started |
| M5 | Bandwidth estimation and allocation | not started |
| M6 | Data channels, permissions, own SDKs | not started |
| M7 | Node registry, signal relay, TURN, drain | not started |
| M8 | Metrics, load and interop testing | not started |

A client that connects today receives a `JoinResponse`, sees other participants join and leave,
answers pings, is displaced when the same identity joins again, and is disconnected with a stated
reason after 60 seconds of silence. It will not reach a connected media state until M3.

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

The config path also comes from `MEDIACORE_CONFIG`. Endpoints:

- `GET /` — service name
- `GET /healthz` — room and participant counts
- `GET /rtc/validate?access_token=…` — token check, returns the identity
- `GET /rtc?access_token=…` — signalling WebSocket

Tokens are standard LiveKit access tokens: HS256, `iss` is the API key, `sub` is the participant
identity, and the `video` claim carries the grants. Any LiveKit token generator works, including
`lk token create`.

## Layout

| Crate | Contents |
|---|---|
| `crates/proto` | protobuf definitions and generated types |
| `crates/auth` | access tokens and grants |
| `crates/config` | YAML configuration |
| `crates/room` | rooms, participants, signalling fan-out |
| `crates/server` | HTTP and WebSocket endpoints, binary |

`crates/room` owns all shared state and knows nothing about transport; `crates/server` owns the
sockets and knows nothing about room rules. Media crates will land between them and will depend on
neither.

## Development

```
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all
```

`protoc` is required to build `crates/proto`.
