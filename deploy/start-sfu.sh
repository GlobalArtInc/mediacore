#!/bin/sh
# Renders mediacore.yaml.template into a config the server can read, then hands
# the process over to it. Everything it needs comes from deploy/.env.
set -eu

TEMPLATE=/etc/mediacore/mediacore.yaml.template
RENDERED=/tmp/mediacore.yaml

fail() {
	echo "start-sfu: $1" >&2
	echo "start-sfu: see deploy/.env.example" >&2
	exit 1
}

for name in MEDIACORE_API_KEY MEDIACORE_API_SECRET MEDIACORE_ADVERTISE_IP MEDIACORE_RTC_UDP_PORT; do
	eval "value=\${$name:-}"
	[ -n "$value" ] || fail "$name is empty"
done

: "${MEDIACORE_LOG_LEVEL:=info}"
export MEDIACORE_LOG_LEVEL

# A secret shorter than this is refused by the front end, so refuse it here too
# rather than let the two halves disagree about which tokens are valid.
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

umask 077
# Unset, these render empty, which the server reads as "no database, no
# recording" -- the single-process stack. Only the four checked at the top are
# required.
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

envsubst '${MEDIACORE_LOG_LEVEL} ${MEDIACORE_API_KEY} ${MEDIACORE_API_SECRET} ${MEDIACORE_RTC_UDP_PORT} ${MEDIACORE_ADVERTISE_IP} ${MEDIACORE_POSTGRES_URL} ${MEDIACORE_RECORDING_ENABLED} ${MEDIACORE_S3_ENDPOINT} ${MEDIACORE_S3_REGION} ${MEDIACORE_S3_BUCKET} ${MEDIACORE_S3_ACCESS_KEY_ID} ${MEDIACORE_S3_SECRET_ACCESS_KEY} ${MEDIACORE_ICE_SERVERS_BLOCK}' \
	<"$TEMPLATE" >"$RENDERED"

echo "start-sfu: media will be advertised as $MEDIACORE_ADVERTISE_IP:$MEDIACORE_RTC_UDP_PORT" >&2

if [ -n "$MEDIACORE_ICE_SERVERS_BLOCK" ]; then
	echo "start-sfu: clients will also be offered turn:$MEDIACORE_TURN_HOST:$MEDIACORE_TURN_PORT" >&2
fi

exec /usr/local/bin/mediacore --config "$RENDERED"
