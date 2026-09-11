#!/bin/sh
# Checks what the SFU needs out of deploy/.env, builds the one part of the config
# a template cannot express, and hands the process over to the server.
#
# It does not render the config any more. The server reads
# mediacore.yaml.template itself and fills the ${MEDIACORE_...} in it from this
# environment, so everything named in that file is exported here rather than
# substituted into a copy of it. Two things come of that: the api secret stays an
# environment variable of one process instead of sitting in a file anything in
# this container could read, and a name nothing set is a refusal from the server
# that says which one, where envsubst filled it in with nothing and said
# nothing. What is left in this script is the part the server cannot do -- an
# address that parses and is unreachable -- and the ice_servers block, which is
# structure and not a value. The two checks below that the server now makes for
# itself, an empty value and a secret too short to sign with, are kept because
# here they can name the file in this repository the value comes from.
set -eu

TEMPLATE=/etc/mediacore/mediacore.yaml.template

fail() {
	echo "start-sfu: $1" >&2
	echo "start-sfu: see deploy/.env.example" >&2
	exit 1
}

# Set and empty is a decision -- the server honours it for the values that have
# one -- and unset is the absence of one, which the server refuses by name.
# Neither is what these four want, so emptiness is refused here, first, where the
# message can name deploy/.env instead of a key in a config file the operator of
# this stack never wrote. The server refuses an empty api key or secret as well,
# because a chart does not run this script.
for name in MEDIACORE_API_KEY MEDIACORE_API_SECRET MEDIACORE_ADVERTISE_IP MEDIACORE_RTC_UDP_PORT; do
	eval "value=\${$name:-}"
	[ -n "$value" ] || fail "$name is empty"
done
# Exported because the server reads them from its own environment now. They
# arrive exported when compose or Kubernetes sets them, and this line is what
# makes the script correct on its own terms rather than by inheritance.
export MEDIACORE_API_KEY MEDIACORE_API_SECRET MEDIACORE_ADVERTISE_IP MEDIACORE_RTC_UDP_PORT

: "${MEDIACORE_LOG_LEVEL:=info}"
export MEDIACORE_LOG_LEVEL

# One floor, in three places that cannot disagree: the server refuses a signing
# secret shorter than this in Config::validate, the front end refuses to start
# below it, and this is the same number said where the answer is "edit
# deploy/.env" rather than "edit a Secret".
if [ "${#MEDIACORE_API_SECRET}" -lt 32 ]; then
	fail "MEDIACORE_API_SECRET is shorter than 32 characters"
fi

# The address is written into ICE candidates verbatim. A name would be rejected
# by the server's own validation a second later; say so now, with the reason.
#
# `deadbeef` is the case worth spelling out: it is a perfectly good host name
# made only of hex digits, so "contains nothing but hex and colons" lets it
# through. An ipv6 literal has to actually contain a colon.
is_ipv4() {
	printf '%s' "$1" | grep -Eq '^([0-9]|[1-9][0-9]|1[0-9]{2}|2[0-4][0-9]|25[0-5])(\.([0-9]|[1-9][0-9]|1[0-9]{2}|2[0-4][0-9]|25[0-5])){3}$'
}

is_ipv6() {
	case "$1" in
		*:*) ;;
		*) return 1 ;;
	esac
	case "$1" in
		*[!0-9a-fA-F:.]*) return 1 ;;
	esac
	return 0
}

if ! is_ipv4 "$MEDIACORE_ADVERTISE_IP" && ! is_ipv6 "$MEDIACORE_ADVERTISE_IP"; then
	fail "MEDIACORE_ADVERTISE_IP is \`$MEDIACORE_ADVERTISE_IP\`, which is not an ip address; ice candidates carry addresses, not names"
fi

# Loopback and the wildcard are the two values that produce a server which
# signals perfectly and never receives a single media packet. Loopback is the
# one exception worth having: browsers on this same machine can reach it, and
# http://127.0.0.1 is a secure context, so the whole stack runs without TLS for
# a local trial. That has to be asked for, never inferred.
case "$MEDIACORE_ADVERTISE_IP" in
	127.*|::1)
		if [ "${MEDIACORE_LOOPBACK:-0}" = "1" ]; then
			echo "start-sfu: MEDIACORE_ADVERTISE_IP is $MEDIACORE_ADVERTISE_IP; only browsers on this host can reach the media port" >&2
		else
			fail "MEDIACORE_ADVERTISE_IP is \`$MEDIACORE_ADVERTISE_IP\`; no client outside this host can send media there. For a trial on this machine use deploy/docker-compose.local.yml, which sets MEDIACORE_LOOPBACK=1"
		fi
		;;
	0.0.0.0|::)
		fail "MEDIACORE_ADVERTISE_IP is \`$MEDIACORE_ADVERTISE_IP\`; no client outside this host can send media there"
		;;
esac

case "$MEDIACORE_ADVERTISE_IP" in
	10.*|192.168.*|172.1[6-9].*|172.2[0-9].*|172.3[01].*|169.254.*)
		echo "start-sfu: MEDIACORE_ADVERTISE_IP is $MEDIACORE_ADVERTISE_IP, a private address: right for a LAN, wrong for anyone off it" >&2
		;;
esac

# The relay this deployment offers its clients, if it offers one.
#
# The whole ice_servers block is built here rather than left in the template,
# because substituting values into a template can fill a value in and cannot
# leave a block out: a template that always emitted the entry would point every
# client at `turn::3478` on a deployment that has no relay, and a client takes
# that at its word and spends its gathering time proving it wrong.
: "${MEDIACORE_TURN_SECRET:=}"
MEDIACORE_ICE_SERVERS_BLOCK=""

if [ -n "$MEDIACORE_TURN_SECRET" ]; then
	# The relay is on this host unless it is said to be somewhere else, and
	# the two port numbers are defaulted the same way in start-turn.sh, which
	# is the side that opens them. They have to agree.
	: "${MEDIACORE_TURN_HOST:=$MEDIACORE_ADVERTISE_IP}"
	: "${MEDIACORE_TURN_PORT:=3478}"
	: "${MEDIACORE_TURN_TLS_PORT:=5349}"
	: "${MEDIACORE_TURN_TTL_SECONDS:=86400}"
	: "${MEDIACORE_TURN_TLS_CERT:=}"
	: "${MEDIACORE_TURN_TLS_KEY:=}"

	# Unlike MEDIACORE_ADVERTISE_IP this one may be a name -- a turn: url is
	# resolved by the client, and a name is what a certificate can be checked
	# against -- but it is still a host and not a url.
	case "$MEDIACORE_TURN_HOST" in
		*://*|*/*|*[[:space:]]*)
			fail "MEDIACORE_TURN_HOST is \`$MEDIACORE_TURN_HOST\`; it is the host part of a turn: url, so no scheme, no path and no spaces"
			;;
	esac

	case "$MEDIACORE_TURN_TTL_SECONDS" in
		''|*[!0-9]*) fail "MEDIACORE_TURN_TTL_SECONDS is \`$MEDIACORE_TURN_TTL_SECONDS\`, which is not a number of seconds" ;;
	esac
	[ "$MEDIACORE_TURN_TTL_SECONDS" -gt 0 ] || fail "MEDIACORE_TURN_TTL_SECONDS is 0, so every credential minted has expired before the client it was minted for receives it"

	# The secret is written into a double-quoted YAML scalar, where a quote or
	# a backslash would end the string early and hand the relay half a secret,
	# which authenticates nobody and says nothing about why.
	case "$MEDIACORE_TURN_SECRET" in
		*'"'*|*\\*) fail "MEDIACORE_TURN_SECRET contains a quote or a backslash; keep it to hex or letters and digits, because it is written into a YAML file" ;;
	esac

	ice_urls="        - \"turn:$MEDIACORE_TURN_HOST:$MEDIACORE_TURN_PORT?transport=udp\"
        - \"turn:$MEDIACORE_TURN_HOST:$MEDIACORE_TURN_PORT?transport=tcp\""

	# Certificate files are coturn's to read and this container never sees
	# them; what their being named decides here is whether clients are told
	# there is a turns: url to try at all. Offering one that nothing listens on
	# costs a client the wait.
	#
	# Both names or neither, which is the same rule start-turn.sh enforces on
	# the side that opens the listener: with half a pair coturn opens none, and
	# a turns: url offered against that is the wait with nothing at the end of
	# it. Said rather than refused, because this container's own job -- media on
	# the udp port -- is untouched by it, and the relay is behind a profile that
	# may not even be running. The relay itself refuses to start, and it is also
	# the only one of the two that can tell a named path from a readable file.
	if [ -n "$MEDIACORE_TURN_TLS_CERT" ] && [ -n "$MEDIACORE_TURN_TLS_KEY" ]; then
		ice_urls="$ice_urls
        - \"turns:$MEDIACORE_TURN_HOST:$MEDIACORE_TURN_TLS_PORT?transport=tcp\""
	elif [ -n "$MEDIACORE_TURN_TLS_CERT$MEDIACORE_TURN_TLS_KEY" ]; then
		echo "start-sfu: only one of MEDIACORE_TURN_TLS_CERT and MEDIACORE_TURN_TLS_KEY is set; coturn opens no TLS listener on half a pair, so no turns: url is offered" >&2
	fi

	MEDIACORE_ICE_SERVERS_BLOCK="  ice_servers:
    - urls:
$ice_urls
      secret: \"$MEDIACORE_TURN_SECRET\"
      ttl_seconds: $MEDIACORE_TURN_TTL_SECONDS"
fi
export MEDIACORE_ICE_SERVERS_BLOCK

# Set to empty on purpose, and that is now load-bearing rather than tidiness:
# the server refuses a name it cannot find at all, and an empty value is how this
# stack says "no database, no recording" -- the single process a trial runs.
# Leaving them unset would be nobody having said anything, which is the case the
# server will not guess at. Only the four checked at the top are required.
: "${MEDIACORE_POSTGRES_URL:=}"
: "${MEDIACORE_RECORDING_ENABLED:=false}"
: "${MEDIACORE_S3_ENDPOINT:=}"
: "${MEDIACORE_S3_REGION:=us-east-1}"
: "${MEDIACORE_S3_BUCKET:=}"
: "${MEDIACORE_S3_ACCESS_KEY_ID:=}"
: "${MEDIACORE_S3_SECRET_ACCESS_KEY:=}"
export MEDIACORE_POSTGRES_URL MEDIACORE_RECORDING_ENABLED MEDIACORE_S3_ENDPOINT \
	MEDIACORE_S3_REGION MEDIACORE_S3_BUCKET MEDIACORE_S3_ACCESS_KEY_ID \
	MEDIACORE_S3_SECRET_ACCESS_KEY

echo "start-sfu: media will be advertised as $MEDIACORE_ADVERTISE_IP:$MEDIACORE_RTC_UDP_PORT" >&2

if [ -n "$MEDIACORE_ICE_SERVERS_BLOCK" ]; then
	echo "start-sfu: clients will also be offered turn:$MEDIACORE_TURN_HOST:$MEDIACORE_TURN_PORT" >&2
fi

# Inherited by the server, and it is the server that needs it. Nothing here
# writes a file any more, but the process this script execs writes the meeting
# records, the event outbox and the staged recordings, and every one of those is
# a plain create with no mode of its own: at docker's default 022 they land 0644
# on named volumes, which is meeting records and recorded audio readable by
# anything else that mounts them and by any second uid in the container. This
# line used to sit above the render that is gone and went with it, so it is back
# where what it protects still happens.
#
# The right home for it is the server -- start-turn.sh has its own copy, a chart
# does not run either script, and under Kubernetes nothing sets a umask at all,
# which no manifest can fix because a umask is not a field a pod has. Until the
# server sets its own, this covers the stack whose volumes are the shared ones.
umask 077

# The template itself, variables and all. There is no rendered copy to read
# afterwards and that is the point: what this container holds is the file the
# repository ships, and the secrets are in the process's environment.
exec /usr/local/bin/mediacore --config "$TEMPLATE"
