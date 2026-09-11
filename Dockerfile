# syntax=docker/dockerfile:1

# Both binaries come out of one cargo build. Dependencies are compiled in a
# layer of their own (cargo-chef), so editing a crate under crates/ rebuilds
# only the workspace, not the ~200 crates underneath it.
#
# Two runtime stages come out of that one build and only the last is published.
# `runtime` is distroless: no shell, no package manager, nothing to run but the
# two binaries, and it is what .github/workflows/build.yml pushes and Flux
# deploys. `compose` is the same two binaries on debian, because
# deploy/docker-compose*.yml start the SFU through deploy/start-sfu.sh and a
# shell script needs a shell. Each stage says below why it is the way it is.

FROM rust:1-bookworm AS chef
WORKDIR /src
RUN cargo install cargo-chef --locked

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path /recipe.json

FROM chef AS builder
COPY --from=planner /recipe.json /recipe.json
# Dependencies only: this layer is reused until Cargo.toml or Cargo.lock change.
# `--features kafka` here as well as below, or the layer would be cooked without
# librdkafka and the workspace build would compile it every time.
RUN cargo chef cook --release --features kafka --recipe-path /recipe.json
COPY . .
# The protobufs under crates/proto/protobufs are compiled in process by protox,
# so no protoc is installed here.
RUN cargo build --release --features kafka -p mediacore-server -p mediacore-front \
    && mkdir -p /out \
    && cp target/release/mediacore target/release/mediacore-front /out/

# The shared objects the distroless base does not carry, resolved here because
# the runtime stage has neither ldd to ask with nor a shell to ask in.
#
# The skipped names are what `gcr.io/distroless/cc-debian12` ships -- read out of
# the published image's own layers rather than guessed -- so what is left is what
# would otherwise be missing. Today the whole of it is libz.so.1: between them
# the two binaries link libc, libm, libgcc_s and, in the server, that -- which
# librdkafka pulls in through libz-sys and which no distroless variant carries.
# Anything added later arrives here with its own dependencies, because ldd
# resolves the whole closure, and the plain `ldd` at the top of this step puts
# what a release really links into the build log.
RUN set -eu; \
    ldd /out/mediacore /out/mediacore-front; \
    mkdir -p /runtime-libs; \
    ldd /out/mediacore /out/mediacore-front \
    | awk '/=> \//{ print $1, $3 }' | sort -u \
    | while read -r soname path; do \
        case "$soname" in \
            libc.so.6|libm.so.6|libdl.so.2|libpthread.so.0|librt.so.1|libresolv.so.2) ;; \
            libgcc_s.so.1|libstdc++.so.6|libssl.so.3|libcrypto.so.3) ;; \
            *) cp -L "$path" /runtime-libs/ ;; \
        esac; \
    done; \
    ls -l /runtime-libs

# Where the session records, the event outbox and the recordings go. Made here
# and copied with an owner below, because the distroless stage cannot run
# install(1): it has no shell.
RUN mkdir -p /skel/sessions /skel/events /skel/recordings

# The stage the compose files build, and the only image in this repository with
# a shell in it.
#
# deploy/docker-compose.yml runs the SFU as `/bin/sh /etc/mediacore/start-sfu.sh`,
# and that script is not decoration: it refuses an advertise address that parses
# and is unreachable, refuses a secret the front end would reject as too short,
# and builds the whole ice_servers block, which is structure rather than a value
# and so cannot be filled into a template. The published image has no shell to
# run it in, so the compose files ask for this stage by name -- `target: shell`
# -- and get today's runtime minus curl, because the healthcheck is now
# `mediacore healthcheck`.
#
# THIS IS THE STAGE PRODUCTION RUNS TODAY, and the `runtime` stage below is
# where it is going. The reason is in another repository: the Helm chart starts
# an init container out of this same image, and that container is `/bin/sh -c`
# running `envsubst`. A distroless image has neither, so publishing one before
# the chart stops asking for them is an SFU that never starts -- and `Prove the
# image runs` cannot catch it, because the contract it breaks is the chart's and
# not the image's. So both stages are built and proven on every build, and this
# is the one that gets the tag until the chart lands.
FROM debian:bookworm-slim AS shell
# ca-certificates is what the front end validates the identity provider's
# certificate against when signing in is switched on -- without it every sign-in
# fails as "the identity provider could not be reached", which reads like a
# network problem and is not one.
#
# gettext-base is envsubst, and it is here for exactly one caller: the chart's
# init container. Nothing in this repository renders a config with it any more --
# the server fills in its own `${MEDIACORE_*}` as it reads the file. IT GOES
# WHEN THE CHART DOES, in the same change that makes `runtime` the stage this
# build pushes.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates gettext-base \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --system --create-home --uid 10001 mediacore
# Docker gives a named volume the ownership of the path it is mounted over, so a
# volume mounted at a path the image does not have would be root-owned and the
# server -- running as uid 10001 -- would refuse to start because it cannot
# create its own directory. deploy/docker-compose.yml mounts one at each of
# these.
RUN install -d -o mediacore -g mediacore /var/lib/mediacore/sessions \
    && install -d -o mediacore -g mediacore /var/lib/mediacore/events \
    && install -d -o mediacore -g mediacore /var/lib/mediacore/recordings
COPY --from=builder /out/mediacore /out/mediacore-front /usr/local/bin/
# The number, not the name, and both stages say it the same way. Kubernetes can
# only check `runAsNonRoot` against a numeric USER -- a name means reading the
# image's own /etc/passwd, which the kubelet will not do, so some admission
# policies refuse such an image outright. `mediacore` here IS 10001, so this
# changes nothing about who the process is; it changes what a reader of the image
# can tell without running it. The volumes that already exist are owned by 10001
# and the chart runs the pod as 10001, which is the number that has to keep
# matching.
USER 10001:10001
# The same directory the distroless stage uses. Nothing in either image reads a
# relative path -- every caller passes --config or MEDIACORE_CONFIG absolutely --
# but a difference between the image that is tested and the image that ships is
# the kind that is discovered by the one thing nobody tried, and proving both
# stages is worth nothing if they are not the same shape.
WORKDIR /var/lib/mediacore
# 7880 signalling, 8080 front end, 7882/udp media.
EXPOSE 7880 8080 7882/udp
CMD ["/usr/local/bin/mediacore"]

# The image this repository publishes. Two binaries, the libraries they link,
# and the certificates the front end and the webhook sink validate an endpoint
# with. No shell, no package manager, no curl: nothing in the image can be
# talked into running anything, and the SFU container can be given
# readOnlyRootFilesystem now that nothing renders a config into /tmp.
#
# WHICH VARIANT, AND WHY THIS ONE.
#
# `static` is out: this build links glibc, and a static musl build is not the
# target while librdkafka is compiled in. `base` is the near miss worth naming
# -- it carries glibc and *not* libgcc_s.so.1, which every Rust binary needs to
# unwind a panic, so the server would die at exec with "error while loading
# shared libraries" and nothing about panics anywhere in the message. `cc` is
# `base` plus libgcc and libstdc++, and is the variant distroless documents for
# Rust; `:nonroot` over `:latest` so that removing the USER line below fails
# safe rather than silently running as root.
#
# What the binaries need -- what the crate graph says, and what the `ldd` in the
# builder stage prints: glibc -- libc,
# libm, and the dlopened libnss_* that getaddrinfo needs, which is how a
# postgres url, a kafka broker or a webhook host is resolved at all; libgcc_s
# for unwinding; and libz, because `--features kafka` links librdkafka and that
# links zlib through libz-sys. Nothing else: reqwest is built with
# rustls-tls-native-roots and str0m with rust-crypto, so no libssl and no
# libcrypto; librdkafka's ssl, gssapi and zstd bundles are off in Cargo.toml;
# librdkafka's C++ binding is not built and the protobufs are compiled in
# process by protox, so no libstdc++ either.
#
# cc-debian12 carries all of that except zlib. That is not a guess: the image's
# layers were listed file by file, and it holds no libz.so.1 -- which is why the
# builder stage stages what is missing and it lands in /usr/lib below. The
# loader finds it there because the image ships no ld.so.cache at all, so the
# only search path is the one built into ld.so, and /usr/lib is in it.
#
# None of that reasoning is what settles it, and it is not meant to be. The
# "Prove the image runs" step in .github/workflows/build.yml starts both binaries
# out of this image and reads /healthz from each before anything is pushed, so a
# missing library, a missing certificate bundle or a bad entrypoint is a red build
# rather than a crash loop in a cluster -- which is also what makes the base image
# a thing this repository is allowed to change at all.
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
# Everything ldd resolved that this base does not ship. The base sets
# SSL_CERT_FILE to its own /etc/ssl/certs/ca-certificates.crt, which is where
# rustls-native-certs looks first, so the trust store needs nothing here.
COPY --from=builder /runtime-libs/ /usr/lib/
# uid 10001, and not the base's own `nonroot` (65532). The chart runs the pod
# with runAsUser 10001 and fsGroup 10001, and every volume and bind mount that
# already exists is owned by 10001 -- a server that cannot write its session
# directory refuses to start, deliberately, so a different number here would put
# every deployment's records out of its own reach. Numeric because a distroless
# image cannot run useradd, there being no shell to run it in; nothing needs the
# name, because neither binary ever looks itself up and `--chown` takes numbers.
COPY --from=builder --chown=10001:10001 /skel /var/lib/mediacore
COPY --from=builder /out/mediacore /out/mediacore-front /usr/local/bin/
USER 10001:10001
# Not /home/mediacore: there is no useradd here to make one, and the base's own
# /home/nonroot is 0700 owned by 65532. A directory uid 10001 owns instead, which
# is read in exactly one case -- `mediacore` started with no --config looks for
# ./config.yaml -- and writable in every other.
WORKDIR /var/lib/mediacore
# 7880 signalling, 8080 front end, 7882/udp media.
EXPOSE 7880 8080 7882/udp
CMD ["/usr/local/bin/mediacore"]
