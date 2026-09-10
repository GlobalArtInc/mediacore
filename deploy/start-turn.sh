#!/bin/sh
# Renders turnserver.conf.template into a config coturn can read, then hands the
# process over to it. Everything it needs comes from deploy/.env.
#
# The refusals live here and not in docker-compose.yml, unlike every other
# variable in this deployment, and the reason is worth knowing before somebody
# "fixes" it: compose interpolates every service in the file before it decides
# which profiles are running, so one ${VAR:?...} on the turn service would stop
# `docker compose up` for every deployment that never asked for a relay. A
# service nobody starts must cost nobody anything.
set -eu

TEMPLATE=/etc/coturn/turnserver.conf.template
RENDERED=/tmp/turnserver.conf

fail() {
	echo "start-turn: $1" >&2
	echo "start-turn: see deploy/.env.example" >&2
	exit 1
}

# These two numbers are defaulted the same way in start-sfu.sh, which is the
# side that tells clients where to connect. They have to agree: this side only
# opens the ports, and a client sent to a port nothing listens on gets no relay
# and no error either.
: "${MEDIACORE_TURN_PORT:=3478}"
: "${MEDIACORE_TURN_TLS_PORT:=5349}"

# Above the 32768-60999 a default Linux hands out to everything else on this
# host, because these two say what the relay may bind and nothing tells the rest
# of the host to stay off it: an overlap is coturn losing an allocation to a
# port some outbound connection took first, on a day with more traffic than the
# day this was set up.
: "${MEDIACORE_TURN_RELAY_MIN_PORT:=61000}"
: "${MEDIACORE_TURN_RELAY_MAX_PORT:=61099}"
: "${MEDIACORE_TURN_SECRET:=}"
: "${MEDIACORE_TURN_TLS_CERT:=}"
: "${MEDIACORE_TURN_TLS_KEY:=}"

# The three that follow the rest of the deployment when they are not set for
# themselves: the site's own name is a realm worth reading in a log, and the
# address clients are already sent to for media is both the address to
# advertise and the only peer this relay has any business reaching.
: "${MEDIACORE_TURN_REALM:=${MEDIACORE_PUBLIC_HOST:-}}"
: "${MEDIACORE_TURN_EXTERNAL_IP:=${MEDIACORE_ADVERTISE_IP:-}}"
: "${MEDIACORE_TURN_ALLOWED_PEER_IP:=${MEDIACORE_ADVERTISE_IP:-}}"

[ -n "$MEDIACORE_TURN_SECRET" ] || fail "MEDIACORE_TURN_SECRET is empty; this container is the relay and there would be nothing to authenticate against it. Generate one with \`openssl rand -hex 32\` and put it in deploy/.env, where the SFU reads the same variable"

# Short enough to guess is short enough to have somebody else's clients relaying
# through this host. A warning and not a refusal, because the secret may belong
# to a coturn that has been running with it for years and this is not the place
# to break that.
if [ "${#MEDIACORE_TURN_SECRET}" -lt 32 ]; then
	echo "start-turn: MEDIACORE_TURN_SECRET is shorter than 32 characters; \`openssl rand -hex 32\` is the length worth having" >&2
fi

[ -n "$MEDIACORE_TURN_REALM" ] || fail "MEDIACORE_TURN_REALM is empty and MEDIACORE_PUBLIC_HOST, which it would otherwise follow, is empty too; coturn needs a realm to challenge a client with"
[ -n "$MEDIACORE_TURN_EXTERNAL_IP" ] || fail "MEDIACORE_TURN_EXTERNAL_IP is empty and MEDIACORE_ADVERTISE_IP, which it would otherwise follow, is empty too; the relay has to know which address to hand out or its candidates point nowhere"

# The peer rules deny the entire address space, so an empty allow list is a
# relay that authenticates a client, hands it a candidate and then drops every
# packet it is asked to forward -- which is exactly the silence this service was
# added to cure, arriving from the cure.
[ -n "$MEDIACORE_TURN_ALLOWED_PEER_IP" ] || fail "MEDIACORE_TURN_ALLOWED_PEER_IP is empty and MEDIACORE_ADVERTISE_IP, which it would otherwise follow, is empty too; every peer is denied by default here, so the relay would forward nothing at all"

is_port() {
	case "$1" in
		''|*[!0-9]*) return 1 ;;
	esac
	[ "$1" -ge 1 ] && [ "$1" -le 65535 ]
}

for name in MEDIACORE_TURN_PORT MEDIACORE_TURN_TLS_PORT MEDIACORE_TURN_RELAY_MIN_PORT MEDIACORE_TURN_RELAY_MAX_PORT; do
	eval "value=\${$name}"
	is_port "$value" || fail "$name is \`$value\`, which is not a port number"
done

if [ "$MEDIACORE_TURN_RELAY_MIN_PORT" -gt "$MEDIACORE_TURN_RELAY_MAX_PORT" ]; then
	fail "MEDIACORE_TURN_RELAY_MIN_PORT ($MEDIACORE_TURN_RELAY_MIN_PORT) is above MEDIACORE_TURN_RELAY_MAX_PORT ($MEDIACORE_TURN_RELAY_MAX_PORT); there is no relay port in that range and every allocation would be refused"
fi

# Both or neither. Half a pair is a TLS listener coturn cannot open, and it says
# so in a warning among the ones it prints anyway, which is the same as not
# saying it.
if [ -n "$MEDIACORE_TURN_TLS_CERT" ] && [ -z "$MEDIACORE_TURN_TLS_KEY" ]; then
	fail "MEDIACORE_TURN_TLS_CERT is set and MEDIACORE_TURN_TLS_KEY is empty; coturn needs both to open a TLS listener"
fi
if [ -n "$MEDIACORE_TURN_TLS_KEY" ] && [ -z "$MEDIACORE_TURN_TLS_CERT" ]; then
	fail "MEDIACORE_TURN_TLS_KEY is set and MEDIACORE_TURN_TLS_CERT is empty; coturn needs both to open a TLS listener"
fi

# Named and not there is how TURN/TLS is got wrong in practice: the paths in
# deploy/.env are right, and the certs volume on the turn service -- which ships
# commented out -- was never uncommented, so nothing is mounted at them. What
# coturn does then is print two more certificate warnings among the ones it
# prints anyway and open no TLS listener, while the SFU, which never sees these
# files and decides on the variable alone, has already told every client there
# is a turns: url worth trying. So the pair is checked as files here, in the one
# container the mount either reached or did not.
readable() {
	{ [ -f "$2" ] && [ -r "$2" ]; } || fail "$1 is \`$2\` and there is no file this container can read there; put the file in deploy/certs and uncomment the certs volume on the turn service in docker-compose.yml, or leave both TLS variables empty and offer clients turn: over tcp instead"
}

if [ -n "$MEDIACORE_TURN_TLS_CERT" ]; then
	readable MEDIACORE_TURN_TLS_CERT "$MEDIACORE_TURN_TLS_CERT"
	readable MEDIACORE_TURN_TLS_KEY "$MEDIACORE_TURN_TLS_KEY"
fi

# Every value below is pasted into the config by sed, which reads `&` and `\` in
# a replacement as syntax of its own and would write something other than what
# was meant -- a realm short of a word, or half a secret, which authenticates
# nobody and explains nothing. Refuse the characters rather than corrupt the
# line.
plain() {
	case "$2" in
		*[!A-Za-z0-9._:/-]*)
			fail "$1 holds a character that cannot be pasted into a config file safely; keep it to letters, digits and \`.\`, \`_\`, \`-\`, \`:\` and \`/\`"
			;;
	esac
}

plain MEDIACORE_TURN_SECRET "$MEDIACORE_TURN_SECRET"
plain MEDIACORE_TURN_REALM "$MEDIACORE_TURN_REALM"
plain MEDIACORE_TURN_EXTERNAL_IP "$MEDIACORE_TURN_EXTERNAL_IP"
plain MEDIACORE_TURN_ALLOWED_PEER_IP "$MEDIACORE_TURN_ALLOWED_PEER_IP"
plain MEDIACORE_TURN_TLS_CERT "$MEDIACORE_TURN_TLS_CERT"
plain MEDIACORE_TURN_TLS_KEY "$MEDIACORE_TURN_TLS_KEY"

# One of the two is `#` and the other is empty, which comments out whichever of
# the template's TLS groups does not apply. Both groups stay in the rendered
# file, so `docker compose exec turn cat /tmp/turnserver.conf` shows what was
# decided and not only what survived.
if [ -n "$MEDIACORE_TURN_TLS_CERT" ]; then
	tls_on=""
	tls_off="#"
else
	tls_on="#"
	tls_off=""
fi

umask 077
sed \
	-e "s|\${MEDIACORE_TURN_PORT}|$MEDIACORE_TURN_PORT|g" \
	-e "s|\${MEDIACORE_TURN_REALM}|$MEDIACORE_TURN_REALM|g" \
	-e "s|\${MEDIACORE_TURN_SECRET}|$MEDIACORE_TURN_SECRET|g" \
	-e "s|\${MEDIACORE_TURN_EXTERNAL_IP}|$MEDIACORE_TURN_EXTERNAL_IP|g" \
	-e "s|\${MEDIACORE_TURN_RELAY_MIN_PORT}|$MEDIACORE_TURN_RELAY_MIN_PORT|g" \
	-e "s|\${MEDIACORE_TURN_RELAY_MAX_PORT}|$MEDIACORE_TURN_RELAY_MAX_PORT|g" \
	-e "s|\${MEDIACORE_TURN_ALLOWED_PEER_IP}|$MEDIACORE_TURN_ALLOWED_PEER_IP|g" \
	-e "s|\${MEDIACORE_TURN_TLS_PORT}|$MEDIACORE_TURN_TLS_PORT|g" \
	-e "s|\${MEDIACORE_TURN_TLS_CERT}|$MEDIACORE_TURN_TLS_CERT|g" \
	-e "s|\${MEDIACORE_TURN_TLS_KEY}|$MEDIACORE_TURN_TLS_KEY|g" \
	-e "s|\${MEDIACORE_TURN_TLS_ON}|$tls_on|g" \
	-e "s|\${MEDIACORE_TURN_TLS_OFF}|$tls_off|g" \
	<"$TEMPLATE" >"$RENDERED"

echo "start-turn: relaying on port $MEDIACORE_TURN_PORT for realm $MEDIACORE_TURN_REALM, advertising $MEDIACORE_TURN_EXTERNAL_IP, peers limited to $MEDIACORE_TURN_ALLOWED_PEER_IP" >&2

exec turnserver -c "$RENDERED"
