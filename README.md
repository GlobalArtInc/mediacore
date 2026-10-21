# mediacore

mediacore is GlobalArt's media server: a selective forwarding unit written in Rust on
[str0m](https://github.com/algesten/str0m). It speaks the LiveKit signalling protocol and room API,
so LiveKit's client SDKs for web, iOS and Android and its server SDKs work against it unmodified. It
forwards audio and simulcast video, picks each subscriber's layer from its bandwidth estimate, carries
data channels, records meeting audio to Ogg and uploads it to S3, sends events to webhooks and Kafka,
and keeps a record of every meeting in PostgreSQL or in a directory. Several nodes can run as one
cluster, with a node registry and a signal relay between them.

## Quick start

On a host with Docker and the compose plugin:

```shell
git clone https://github.com/GlobalArtInc/mediacore
cd mediacore/deploy
cp .env.example .env
openssl rand -hex 32    # paste into MEDIACORE_API_SECRET
$EDITOR .env            # set MEDIACORE_PUBLIC_HOST, MEDIACORE_ADVERTISE_IP and MEDIACORE_TLS
docker compose up -d --build
```

This starts the SFU, the front end and Caddy terminating TLS in front of both. Open
`https://<MEDIACORE_PUBLIC_HOST>` to join the demo room. A browser gives a page the microphone only
over TLS. [deploy/README.md](deploy/README.md) explains every variable and how to get a certificate
without a domain, and it lists the three failures behind nearly every silent room.

## Build

Requirements:

- the Rust toolchain named in `rust-toolchain.toml` (rustup installs it)
- a C compiler, for aws-lc and, with `--features kafka`, librdkafka

```shell
cargo build --release --locked -p mediacore-server -p mediacore-front
```

or build the container image, which includes Kafka support:

```shell
docker build -t mediacore:local .
```

The protobuf definitions are compiled by `protox`, so no system `protoc` is needed.

## Run

```shell
cp config-sample.yaml config.yaml
cargo run -p mediacore-server -- --config config.yaml
```

The server takes signalling and the room API on TCP `7880` and all media on UDP `7882`. The config
path can also come from `MEDIACORE_CONFIG`. Every setting is described in
[config-sample.yaml](config-sample.yaml), and any value can be written as `${MEDIACORE_NAME}` to be
read from the environment. The key pair in the sample is public: replace it on anything but your own
machine.

| Path | What it is |
|---|---|
| `/rtc` | LiveKit signalling WebSocket |
| `/twirp/mediacore.RoomService/*` | room API; also served at LiveKit's `/twirp/livekit.RoomService/*` |
| `/swagger` | the room and session API, documented and callable |
| `/healthz`, `/readyz` | liveness and readiness |
| `/metrics` | Prometheus metrics |

Plain HTTP is enough for the tests and for clients on localhost. For anything else, put TLS in front
as `deploy/` does. The front end is documented in [crates/front/README.md](crates/front/README.md).

## Test

```shell
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
MEDIACORE_TEST_POSTGRES=postgres://mediacore:mediacore@127.0.0.1:5432/postgres cargo test --workspace

cargo clippy -p mediacore-server --features kafka --all-targets -- -D warnings
cargo test -p mediacore-events --features kafka
```

Each test in `crates/session/tests/postgres.rs` creates its own database on the server named by
`MEDIACORE_TEST_POSTGRES`. Without that variable those tests do nothing.

`interop/run.sh` builds the server and runs LiveKit's own CLI and Node server SDK against it; see
[interop/README.md](interop/README.md). `mediacore-load` (`crates/load`) runs a crowd of bots against
a running server and reports where it breaks:

```shell
mediacore-load --api-secret <secret> --rooms 4 --churn-secs 2 ladder --start 24 --step 24 --max 240
```

## Layout

| Path | Holds |
|---|---|
| `crates/server` | the `mediacore` binary: HTTP and WebSocket endpoints, the room API and `/swagger` |
| `crates/room` | rooms, participants, tracks, subscriptions and signalling fan-out |
| `crates/transport` | ICE, DTLS-SRTP, RTP in and out, one task per peer connection |
| `crates/sfu` | packet forwarding: sequence and timestamp rewriting, keyframes, simulcast layers |
| `crates/session` | meeting records and the recording upload queue, in a directory or in PostgreSQL |
| `crates/recording` | Opus into Ogg without decoding, and the upload to object storage |
| `crates/events` | webhooks, the Kafka producer and the outbox behind them |
| `crates/auth`, `crates/config`, `crates/proto` | access tokens, YAML configuration, LiveKit protobufs |
| `crates/client` | a client that publishes real RTP, for the tests |
| `crates/load` | `mediacore-load`, the load bench |
| `crates/front` | `mediacore-front`: an audio room page, the token endpoint, optional OpenID Connect sign-in |
| `deploy/` | Docker Compose deployment with Caddy and an optional TURN relay |
| `interop/` | tests against LiveKit's own SDKs |
| `Dockerfile` | builds both binaries into one image |

What is planned next, and the known gaps, are in [ROADMAP.md](ROADMAP.md).

## License

MIT, see [LICENSE](LICENSE). Every feature is free to use, with no licence key and no paid tier.
Vendored third-party code keeps its own licence, listed in the `NOTICE.txt` next to it.
