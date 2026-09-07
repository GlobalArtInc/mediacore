# syntax=docker/dockerfile:1

# Both binaries come out of one cargo build. Dependencies are compiled in a
# layer of their own (cargo-chef), so editing a crate under crates/ rebuilds
# only the workspace, not the ~200 crates underneath it.

FROM rust:1-bookworm AS chef
WORKDIR /src
RUN cargo install cargo-chef --locked

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path /recipe.json

FROM chef AS builder
COPY --from=planner /recipe.json /recipe.json
# Dependencies only: this layer is reused until Cargo.toml or Cargo.lock change.
RUN cargo chef cook --release --recipe-path /recipe.json
COPY . .
# The protobufs under crates/proto/protobufs are compiled in process by protox,
# so no protoc is installed here.
RUN cargo build --release -p mediacore-server -p mediacore-front \
    && mkdir -p /out \
    && cp target/release/mediacore target/release/mediacore-front /out/

FROM debian:bookworm-slim AS runtime
# gettext-base is envsubst, used by deploy/start-sfu.sh to render the config;
# curl is what the compose healthcheck calls; ca-certificates is what the front
# end validates the identity provider's certificate against when signing in is
# switched on -- without it every sign-in fails as "the identity provider could
# not be reached", which reads like a network problem and is not one.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl gettext-base \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --system --create-home --uid 10001 mediacore
# Where the session records and the event outbox go, made in the image and owned
# by the user the server runs as. Docker gives a named volume the ownership of the path it is
# mounted over, so a volume mounted at a path the image does not have would be
# root-owned and the server -- running as uid 10001 -- would refuse to start
# because it cannot create its own directory. deploy/docker-compose.yml mounts
# one here.
RUN install -d -o mediacore -g mediacore /var/lib/mediacore/sessions \
    && install -d -o mediacore -g mediacore /var/lib/mediacore/events \
    && install -d -o mediacore -g mediacore /var/lib/mediacore/recordings
COPY --from=builder /out/mediacore /out/mediacore-front /usr/local/bin/
USER mediacore
WORKDIR /home/mediacore
# 7880 signalling, 8080 front end, 7882/udp media.
EXPOSE 7880 8080 7882/udp
CMD ["/usr/local/bin/mediacore"]
