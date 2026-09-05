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

# A secret shorter than this is refused by the demo app, so refuse it here too
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

umask 077
envsubst '${MEDIACORE_LOG_LEVEL} ${MEDIACORE_API_KEY} ${MEDIACORE_API_SECRET} ${MEDIACORE_RTC_UDP_PORT} ${MEDIACORE_ADVERTISE_IP}' \
	<"$TEMPLATE" >"$RENDERED"

echo "start-sfu: media will be advertised as $MEDIACORE_ADVERTISE_IP:$MEDIACORE_RTC_UDP_PORT" >&2

exec /usr/local/bin/mediacore --config "$RENDERED"
