#!/usr/bin/env bash
# Start an image and prove it works, before anybody depends on it.
#
# Called from .github/workflows/build.yml, once per runtime stage, because this
# repository ships two: a Debian one with a shell in it and a distroless one
# without. Both are proven on every build even though only one is pushed, so
# that changing which one is pushed is a one-line change to a thing already
# known to run rather than a discovery made in a cluster.
#
# Usage: hack/prove-image.sh <image> [label]
#
# Everything below was written as an inline step and moved here unchanged, so
# that running it twice did not mean writing it twice.
set -eu

IMAGE="${1:?usage: hack/prove-image.sh <image> [label]}"
LABEL="${2:-$IMAGE}"
# Container names have to differ between the two runs of this script inside one
# build, or the second run collides with the first one's leftovers.
SUFFIX="$(printf '%s' "$LABEL" | tr -c 'a-zA-Z0-9' '-')"

echo "== proving $LABEL ($IMAGE)"

set -eu

config="$RUNNER_TEMP/mediacore-smoke.yaml"
sfu="mediacore-smoke-sfu-$GITHUB_RUN_ID-$GITHUB_RUN_ATTEMPT-$SUFFIX"
front="mediacore-smoke-front-$GITHUB_RUN_ID-$GITHUB_RUN_ATTEMPT-$SUFFIX"

# Every exit path, including a failed check below under `set -e` and a
# cancelled job: this is a self-hosted runner, so whatever is left
# running is left running for good, and a container holding a name or a
# port is the next build's failure. -v with it, or the anonymous
# volumes below outlive the containers they were made for.
cleanup() {
  docker rm -f -v "$sfu" "$front" >/dev/null 2>&1 || true
  rm -f "$config"
}
trap cleanup EXIT INT TERM

# Everything the server needs and nothing it does not: no database, no
# bucket, no broker, no relay. Loopback as the advertised address
# because nothing here sends a packet of media, and `keys` because a
# server with no key refuses to start -- it could mint nothing and
# verify nothing.
cat > "$config" <<'YAML'
port: 7880
bind_addresses:
  - 0.0.0.0
keys:
  smoke: smokesmokesmokesmokesmokesmokesmoke
sessions:
  directory: /var/lib/mediacore/sessions
events:
  directory: /var/lib/mediacore/events
rtc:
  udp_port: 7882
  advertise_addresses:
    - 127.0.0.1
  use_socket_address: false
  use_external_ip: false
YAML

# An ephemeral host port, because this runner hosts ten agents and a
# fixed 7880 would make two builds each other's flake. Published at
# all, rather than asked only from inside, because publishing is what a
# deployment does: a server that bound the wrong interface answers on
# its own loopback and nowhere else, and that is exactly the failure
# this step is for. The volumes are anonymous and mounted where the
# compose file and the chart mount theirs, so docker has to carry the
# image's directory ownership into a fresh volume -- checked below.
docker run -d --name "$sfu" \
  -v "$config:/etc/mediacore/mediacore.yaml:ro" \
  -v /var/lib/mediacore/sessions \
  -v /var/lib/mediacore/events \
  -p 127.0.0.1::7880 \
  "$IMAGE" /usr/local/bin/mediacore --config /etc/mediacore/mediacore.yaml

# The other half of the image, and the half nothing has ever started:
# both binaries ship here and only the SFU was ever run by anything.
# Its environment is the least a front end can be given -- the api pair
# it signs tokens with and the url it tells a browser to connect to.
#
# MEDIACORE_FRONT_PORT is set and empty on purpose, and it is the only
# place anything exercises that: both compose files set it to 8080, and
# leaving it out here takes the other branch entirely. Set and empty is
# what compose interpolation of an unset variable produces and what a
# chart renders from a value that is not there, so both halves have to
# read it as silence and settle on 8080 -- the server did and its own
# healthcheck did not, which is a container docker holds `unhealthy`
# for ever with a front end serving happily inside it, and a compose
# stack that never starts caddy at all.
docker run -d --name "$front" \
  -e MEDIACORE_API_KEY=smoke \
  -e MEDIACORE_API_SECRET=smokesmokesmokesmokesmokesmokesmoke \
  -e MEDIACORE_FRONT_WS_URL=ws://127.0.0.1:7880 \
  -e MEDIACORE_FRONT_PORT= \
  -p 127.0.0.1::8080 \
  "$IMAGE" /usr/local/bin/mediacore-front

# The wait is the image's own `healthcheck` subcommand, run inside the
# container -- which is the only way to ask from in there, the image
# having no curl, and is the exact command deploy/docker-compose.yml
# gives docker as its healthcheck. So this proves the check as well as
# the server. Quiet while it waits: the first second or two are
# connection refused and saying so thirty times helps nobody.
wait_for() {
  name="$1"
  shift
  deadline=$((SECONDS + 30))
  until docker exec "$name" "$@" >/dev/null 2>&1; do
    if [ "$SECONDS" -ge "$deadline" ]; then
      echo "$name never answered its own healthcheck:" >&2
      docker exec "$name" "$@" >&2 || true
      docker logs "$name" >&2 || true
      return 1
    fi
    sleep 1
  done
}

wait_for "$sfu" /usr/local/bin/mediacore healthcheck
wait_for "$front" /usr/local/bin/mediacore-front healthcheck

# And read the body from outside the container too, rather than take
# our own binary's word for it: /healthz has to look like the health
# line and not merely return 200. A proxy in the way, a stale container
# on the port or a server that answers before it has anything to
# report would all pass a check on the status code alone.
#
# python3 and not curl, because curl is nobody's dependency on this
# runner: ten agents share the machine, nothing that has ever run on it
# needed curl, and a build that stops with `curl: not found` stops every
# deploy for a reason that has nothing to do with the image it was
# examining. python3 is already load-bearing in this same job -- the tar
# header below and the Harbor cleanup at the end of it both need it. The
# empty ProxyHandler is the `no_proxy()` both binaries set, for the same
# reason: this runner's environment may name an outbound proxy, and a
# proxy asked for 127.0.0.1 answers about itself. A status that is not
# 2xx raises, which fails the assignment under `set -e`, exactly as
# `curl -fsS` did.
body() {
  port="$(docker port "$1" "$2" | head -n1 | sed 's/.*://')"
  python3 -c 'import sys, urllib.request; opener = urllib.request.build_opener(urllib.request.ProxyHandler({})); sys.stdout.write(opener.open(f"http://127.0.0.1:{sys.argv[1]}/healthz", timeout=5).read().decode())' "$port"
}

health="$(body "$sfu" 7880/tcp)"
case "$health" in
  rooms=*participants=*ice=*) echo "sfu /healthz: $health" ;;
  *)
    echo "the sfu answered /healthz with something that is not the health line: $health" >&2
    docker logs "$sfu" >&2 || true
    exit 1
    ;;
esac

page="$(body "$front" 8080/tcp)"
if [ "$page" != "ok" ]; then
  echo "the front end answered /healthz with \`$page\` rather than \`ok\`" >&2
  docker logs "$front" >&2 || true
  exit 1
fi
echo "front /healthz: $page"

# Who the image runs as, and who owns the volumes it was given. Both
# are asked of docker because there is no shell in the image to ask
# with -- `docker cp` hands back a tar, and the header carries the
# ownership. This is the check that stands between a uid changed here
# and a production volume full of records the server can no longer
# write to: the chart runs the pod as 10001 with fsGroup 10001, and
# every volume that already exists is owned by 10001.
user="$(docker image inspect --format '{{.Config.User}}' "$IMAGE")"
if [ "$user" != "10001:10001" ]; then
  echo "the image runs as \`$user\`; the chart and every existing volume say 10001:10001" >&2
  exit 1
fi

# python3 rather than `tar -tv`: the two tars disagree about which
# column is the owner -- GNU prints `uid/gid` where BSD prints a link
# count first -- and a check that reads the wrong column reports a
# perfectly good image as root-owned. The header itself does not
# disagree with anybody.
owner="$(docker cp "$sfu:/var/lib/mediacore/sessions" - \
  | python3 -c 'import sys, tarfile; entry = next(iter(tarfile.open(fileobj=sys.stdin.buffer, mode="r|"))); print(entry.uid, entry.gid)')"
if [ "$owner" != "10001 10001" ]; then
  echo "the session directory is owned by \`$owner\`, so a fresh volume mounted there is not writable by the server" >&2
  exit 1
fi

# The trust store, which nothing above would miss: it is read when the
# front end validates an identity provider's certificate and when the
# webhook sink validates its endpoint's, and neither happens in a smoke
# test. A runtime base without ca-certificates would pass every check
# above and fail at the first sign-in, which reads like a network
# problem and is not one.
#
# What is counted is the certificates inside the file, not the bytes of
# the tar around it. `docker cp` writes a 512 byte header and a 1024
# byte trailer whatever the member holds, so an empty or truncated
# bundle came to 1536 bytes through `wc -c` and passed a floor of a
# thousand -- the check only ever caught a path that was absent
# altogether. -L so that a symlink whose target is gone is an error here
# rather than a link counted as a trust store. Debian's bundle carries
# around 140 certificates; twenty is far below any real one and far
# above what a truncated copy leaves behind.
certificates="$(docker cp -L "$front:/etc/ssl/certs/ca-certificates.crt" - \
  | python3 -c 'import sys, tarfile; stream = tarfile.open(fileobj=sys.stdin.buffer, mode="r|"); print(stream.extractfile(next(iter(stream))).read().count(b"BEGIN CERTIFICATE"))')"
if [ "$certificates" -lt 20 ]; then
  echo "/etc/ssl/certs/ca-certificates.crt in this image holds $certificates certificates; that is not a trust store, and every outbound https request from it will fail" >&2
  exit 1
fi

echo "runs as $user, volumes owned by $owner, $certificates certificates in the trust store"
