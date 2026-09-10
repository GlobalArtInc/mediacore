#!/usr/bin/env bash
#
# The interop run, end to end: build the server, start it, point LiveKit's own
# CLI and LiveKit's own SDK at it, and check the webhooks it posts with LiveKit's
# own receiver.
#
# Everything is pinned. `lk` cuts a release most weeks and livekit-server-sdk
# more often than that, and a job that tracks either one goes red on somebody
# else's release schedule -- on a commit that changed nothing. The two versions
# below and the exact version in package.json are the only places to change when
# a bump is wanted, and a bump is a commit of its own so that its failures are
# attributable.
#
# Usage:
#   interop/run.sh                  build the server and run everything
#   MEDIACORE_BIN=path interop/run.sh   use a binary that is already built
#   MEDIACORE_KEEP=1 interop/run.sh     keep the run directory for a post-mortem
set -euo pipefail

readonly LK_VERSION="2.18.6"

readonly HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly ROOT="$(cd "${HERE}/.." && pwd)"
readonly CACHE="${HERE}/.cache"

# Read out of config.yaml only in the sense that they have to agree with it: a
# config file cannot be told what the run picked, so these are the file's values
# written twice, and the run fails loudly below if the two ever drift apart.
readonly LIVEKIT_URL="${LIVEKIT_URL:-http://127.0.0.1:7880}"
readonly LIVEKIT_API_KEY="${LIVEKIT_API_KEY:-interopkey}"
readonly LIVEKIT_API_SECRET="${LIVEKIT_API_SECRET:-interopsecretinteropsecretinteropsecret00}"
readonly WEBHOOK_PORT="${MEDIACORE_INTEROP_WEBHOOK_PORT:-7899}"
readonly CLI_ROOM="interop-cli"
readonly SDK_ROOM="interop-sdk"

export LIVEKIT_URL LIVEKIT_API_KEY LIVEKIT_API_SECRET
export MEDIACORE_INTEROP_WEBHOOK_PORT="${WEBHOOK_PORT}"
export MEDIACORE_INTEROP_CLI_ROOM="${CLI_ROOM}"
export MEDIACORE_INTEROP_SDK_ROOM="${SDK_ROOM}"

server_pid=""
receiver_pid=""
run_dir=""

# Every exit path. A server left holding 7880 makes the next run fail with
# something that looks nothing like the reason, and on a runner it holds the job
# open until the step times out.
stop() {
  local pid="$1"
  [[ -n "${pid}" ]] || return 0
  kill -0 "${pid}" 2>/dev/null || return 0
  kill "${pid}" 2>/dev/null || true
  for ((waited = 0; waited < 50; waited++)); do
    kill -0 "${pid}" 2>/dev/null || break
    sleep 0.1
  done
  kill -9 "${pid}" 2>/dev/null || true
  wait "${pid}" 2>/dev/null || true
}

cleanup() {
  local status=$?
  local log
  stop "${server_pid}"
  stop "${receiver_pid}"
  if [[ ${status} -ne 0 && -n "${run_dir}" ]]; then
    # Both logs, because either process can be the one that never came up, and
    # the run directory goes away on the next line. A server that lost the bind
    # says so in one line at the top of a log nobody would otherwise see.
    for log in server receiver; do
      [[ -f "${run_dir}/${log}.log" ]] || continue
      echo
      echo "--- the ${log} log, last 80 lines ---"
      tail -n 80 "${run_dir}/${log}.log" || true
    done
  fi
  if [[ -n "${run_dir}" && -z "${MEDIACORE_KEEP:-}" ]]; then
    rm -rf "${run_dir}"
  elif [[ -n "${run_dir}" ]]; then
    echo "the run directory was kept at ${run_dir}"
  fi
  exit "${status}"
}
trap cleanup EXIT INT TERM

step() { echo; echo "### $*"; }
die() { echo "interop: $*" >&2; exit 1; }

# --- the pinned CLI -------------------------------------------------------

# Downloaded rather than built: the release archive is what an integrator runs,
# and building it from source would test a `lk` nobody has.
fetch_lk() {
  local os arch
  case "$(uname -s)" in
    Darwin) os=darwin ;;
    Linux) os=linux ;;
    *) die "no pinned lk build for $(uname -s)" ;;
  esac
  case "$(uname -m)" in
    x86_64 | amd64) arch=amd64 ;;
    arm64 | aarch64) arch=arm64 ;;
    *) die "no pinned lk build for $(uname -m)" ;;
  esac

  local dir="${CACHE}/lk-${LK_VERSION}-${os}-${arch}"
  LK="${dir}/lk"
  if [[ -x "${LK}" ]]; then
    return
  fi

  local url="https://github.com/livekit/livekit-cli/releases/download/v${LK_VERSION}/lk_${LK_VERSION}_${os}_${arch}.tar.gz"
  mkdir -p "${dir}"
  echo "fetching ${url}"
  curl --fail --silent --show-error --location --retry 3 "${url}" | tar -xz -C "${dir}"
  [[ -x "${LK}" ]] || die "the ${LK_VERSION} archive did not contain lk"
}

# --- waiting, and on whose behalf -----------------------------------------
#
# An answer on a port is not evidence that the process this run started is the
# one giving it. A mediacore left over from an interrupted run, another
# checkout, or an operator's own server on LiveKit's default port answers
# /healthz exactly as well as ours does -- and ours, having lost the bind, is
# already gone with "Address already in use" in a log the run deletes when it
# passes. Every assertion after that is a stranger's, and the transcript reads
# green for a build nobody tested.
#
# The two checks below close that between them. `port_free` refuses to start a
# process on a port somebody is already answering; `alive` catches the other
# half, where the port is held by something that answers nothing at all and our
# own process dies on the bind instead.

port_free() {
  local what="$1" url="$2"
  if curl --fail --silent --output /dev/null --max-time 2 "${url}"; then
    die "something is already answering ${url}: this run would drive it rather than the ${what} it starts"
  fi
}

alive() {
  local what="$1" pid="$2"
  kill -0 "${pid}" 2>/dev/null ||
    die "${what} exited; anything still answering on its port is not the process this run started"
}

wait_for() {
  local what="$1" url="$2" pid="$3" attempts="${4:-100}"
  for ((attempt = 0; attempt < attempts; attempt++)); do
    alive "${what}" "${pid}"
    if curl --fail --silent --output /dev/null --max-time 2 "${url}"; then
      return 0
    fi
    sleep 0.2
  done
  die "${what} never answered at ${url}"
}

# --- build ----------------------------------------------------------------

step "the server binary"
if [[ -n "${MEDIACORE_BIN:-}" ]]; then
  # `-f` as well as `-x`, because a directory is executable to `test` and the
  # failure it buys is `exec: is a directory` in a log tailed at the end.
  [[ -f "${MEDIACORE_BIN}" && -x "${MEDIACORE_BIN}" ]] ||
    die "MEDIACORE_BIN=${MEDIACORE_BIN} is not an executable file"
  # Made absolute against the caller's cwd, which is the only cwd it was ever
  # meant to be read against: the server is started from the mktemp run
  # directory below, and `target/release/mediacore` -- the form the usage above
  # and the README both suggest -- would be looked for in there. That failure
  # arrives twenty seconds later as "mediacore never answered", which blames the
  # server for a path.
  BIN="$(cd "$(dirname "${MEDIACORE_BIN}")" && pwd)/$(basename "${MEDIACORE_BIN}")"
else
  # Release, because the debug build spends its time on assertions this job has
  # no interest in and the whole run is bounded by how long the server takes to
  # answer twenty calls.
  (cd "${ROOT}" && cargo build --release -p mediacore-server)
  BIN="${ROOT}/target/release/mediacore"
fi
echo "using ${BIN}"

step "the pinned lk ${LK_VERSION}"
fetch_lk
"${LK}" --version

step "the pinned livekit-server-sdk"
(cd "${HERE}" && npm ci --no-audit --no-fund)
# What was actually installed, not what package.json asked for. The lockfile is
# the thing under test here: a job that reported the range rather than the
# resolved version would say "2.19.0" on the day it silently ran something else.
(cd "${HERE}" && npm ls --depth 0)

# --- the run --------------------------------------------------------------

run_dir="$(mktemp -d "${TMPDIR:-/tmp}/mediacore-interop.XXXXXX")"
cp "${HERE}/config.yaml" "${run_dir}/config.yaml"
export MEDIACORE_INTEROP_WEBHOOK_LOG="${run_dir}/webhooks.jsonl"
: >"${MEDIACORE_INTEROP_WEBHOOK_LOG}"
echo "run directory ${run_dir}"

# The receiver goes up first and comes down last. mediacore posts an event the
# moment a room is made, so a receiver started after the server would miss the
# first one -- and with attempts: 1 in config.yaml there is no retry to catch it.
step "the webhook receiver"
port_free "webhook receiver" "http://127.0.0.1:${WEBHOOK_PORT}/ready"
# `exec`, so that $! is the process holding the port rather than the subshell
# that started it. Killing the subshell would leave both of these behind, and
# the next run would fail on "address already in use" -- which says nothing at
# all about what went wrong in this one.
(cd "${HERE}" && exec node interop.mjs receiver >"${run_dir}/receiver.log" 2>&1) &
receiver_pid=$!
wait_for "the webhook receiver" "http://127.0.0.1:${WEBHOOK_PORT}/ready" "${receiver_pid}"

step "mediacore"
port_free "mediacore" "${LIVEKIT_URL}/healthz"
(cd "${run_dir}" && exec "${BIN}" --config config.yaml >"${run_dir}/server.log" 2>&1) &
server_pid=$!
wait_for "mediacore" "${LIVEKIT_URL}/healthz" "${server_pid}"
# Named in the transcript, because "which binary answered all of this" is the
# first question asked of a run that disagrees with the one before it.
echo "${BIN} is answering ${LIVEKIT_URL} as pid ${server_pid}"
echo "healthz: $(curl --silent "${LIVEKIT_URL}/healthz")"

# --- the CLI --------------------------------------------------------------
#
# `lk` builds /twirp/livekit.RoomService/<Method> inside generated code and mints
# its own tokens, so this half of the job is somebody else's client from end to
# end. --quiet keeps its "using credentials from environment" line off stdout so
# the JSON can be parsed.

step "lk token create"
token="$("${LK}" --quiet token create --join --room "${CLI_ROOM}" --identity casey --valid-for 10m --token-only)"
[[ "${token}" == *.*.* ]] || die "lk token create did not produce a JWT: ${token}"
echo "lk minted a token of $(printf %s "${token}" | wc -c | tr -d ' ') bytes"

step "lk room create"
created="$("${LK}" --quiet room create --empty-timeout 600 --departure-timeout 45 "${CLI_ROOM}")"
echo "${created}"
# Parsed rather than grepped, and not for tidiness. What `lk` prints here is
# protobuf-go's protojson, which puts either one space or two after every colon
# on purpose -- internal/detrand, seeded from a hash of the binary it is linked
# into, so that nobody can treat the output as a wire format. The choice is
# fixed for a given build and differs between builds: the pinned darwin_arm64
# archive prints two spaces and the linux_amd64 one prints one. A grep for
# `"name":  "<room>"` therefore asserts which release archive was downloaded,
# and a job whose whole value is that red means a real divergence would have
# gone red on its first CI run, blaming the server for the platform.
node --input-type=module -e "
  const room = JSON.parse(process.argv[1]);
  if (room.name !== '${CLI_ROOM}') {
    throw new Error('lk room create did not report the room name: ' + process.argv[1]);
  }
  if (room.emptyTimeout !== 600) {
    throw new Error('lk room create lost emptyTimeout: ' + process.argv[1]);
  }
  if (room.departureTimeout !== 45) {
    throw new Error('lk room create lost departureTimeout: ' + process.argv[1]);
  }
" "${created}"

step "lk room list"
listed="$("${LK}" --quiet room list --json "${CLI_ROOM}")"
echo "${listed}"
node --input-type=module -e "
  const rooms = JSON.parse(process.argv[1]).rooms ?? [];
  const room = rooms.find((r) => r.name === '${CLI_ROOM}');
  if (!room) throw new Error('lk room list did not report ${CLI_ROOM}: ' + process.argv[1]);
  if (room.emptyTimeout !== 600) throw new Error('lk room list lost emptyTimeout: ' + JSON.stringify(room));
  if (!room.sid) throw new Error('lk room list reported no sid: ' + JSON.stringify(room));
  console.log('lk sees sid ' + room.sid);
" "${listed}"

step "lk room participants list"
# Nobody is in the CLI's room -- the participant belongs to the SDK phase, which
# owns the signalling socket -- so what is asserted is that the call is answered
# at all. `lk` exits non-zero on a Twirp error, which is what makes this a check.
"${LK}" --quiet room participants list "${CLI_ROOM}"

step "lk room update"
# The metadata is compared whole rather than grepped for: `interop` is in the
# room's own name, so a substring search for it passes against a server that
# dropped the metadata on the floor.
metadata='{"interop":"lk"}'
updated="$("${LK}" --quiet room update --metadata "${metadata}" "${CLI_ROOM}")"
echo "${updated}"
node --input-type=module -e "
  const room = JSON.parse(process.argv[1]);
  if (room.metadata !== process.argv[2]) {
    throw new Error('lk room update did not report the metadata back: ' + process.argv[1]);
  }
" "${updated}" "${metadata}"

step "lk refuses what mediacore refuses"
# ForwardParticipant is `Support::Refused`, and the point is that `lk` prints a
# twirp error with the code in it rather than a transport failure.
if refusal="$("${LK}" --quiet room participants forward --room "${CLI_ROOM}" --identity casey --destination-room elsewhere 2>&1)"; then
  die "lk was allowed to forward a participant on a single-node server"
fi
echo "${refusal}"
grep -q 'twirp error unimplemented' <<<"${refusal}" ||
  die "the refusal did not reach lk as a typed twirp error: ${refusal}"

step "lk room delete"
"${LK}" --quiet room delete "${CLI_ROOM}"
remaining="$("${LK}" --quiet room list --json)"
node --input-type=module -e "
  const rooms = JSON.parse(process.argv[1]).rooms ?? [];
  if (rooms.some((r) => r.name === '${CLI_ROOM}')) {
    throw new Error('${CLI_ROOM} survived lk room delete: ' + process.argv[1]);
  }
" "${remaining}"

# --- the SDK --------------------------------------------------------------

step "the room service through livekit-server-sdk"
(cd "${HERE}" && node interop.mjs api)

# --- the webhooks ---------------------------------------------------------

# The last events of the run are posted while DeleteRoom is still returning, so
# the receiver is given a moment before it is asked what it saw.
sleep 2

step "the webhooks their receiver verified"
# Both processes have to have survived the run for the line below to mean what
# it says. The webhook phase reads a file rather than the network, so a server
# that died after the last assertion -- or a receiver that died before the last
# delivery -- would otherwise go unremarked.
alive "mediacore" "${server_pid}"
alive "the webhook receiver" "${receiver_pid}"
(cd "${HERE}" && node interop.mjs webhooks)

echo
echo "interop: everything the pinned lk ${LK_VERSION} and livekit-server-sdk asked for was answered by ${BIN}"
