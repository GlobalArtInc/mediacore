#!/bin/sh
# Checks MEDIACORE_PUBLIC_HOST before Caddy ever reads it, then hands over.
#
# The variable is pasted into two places that want two different shapes: the
# Caddy site address (a host, optionally with a port) and, in the front end's
# container, `wss://<this>`. A scheme or a trailing slash is accepted by both
# and corrupts both, so it is refused here with the reason.
set -eu

fail() {
	echo "start-caddy: $1" >&2
	echo "start-caddy: see deploy/.env.example" >&2
	exit 1
}

host=${MEDIACORE_PUBLIC_HOST:-}

[ -n "$host" ] || fail "MEDIACORE_PUBLIC_HOST is empty; set it to the domain or IP visitors type"

case "$host" in
	*://*)
		fail "MEDIACORE_PUBLIC_HOST is \`$host\`; drop the scheme, this is a host name, not a url"
		;;
esac

case "$host" in
	*/*)
		fail "MEDIACORE_PUBLIC_HOST is \`$host\`; drop the slash and anything after it, this is a host name, not a url"
		;;
esac

case "$host" in
	*[[:space:]]*|*'?'*|*'#'*|*@*)
		fail "MEDIACORE_PUBLIC_HOST is \`$host\`; a host name has no spaces and no \`?\`, \`#\` or \`@\`"
		;;
esac

echo "start-caddy: serving https://$host" >&2

exec caddy run --config /etc/caddy/Caddyfile --adapter caddyfile
