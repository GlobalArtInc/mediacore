# Deploying mediacore

Three containers, one host, one URL. Someone opens it, types a name, and hears
everyone else in the room. Audio by default; the camera stays off unless
somebody ticks the box in the lobby.

```
                              the host
        ┌──────────────────────────────────────────────────┐
        │  caddy   80/tcp, 443/tcp   terminates TLS        │
        │    ├── /rtc*  ──► sfu   :7880   signalling       │
        │    └── /      ──► front :8080   page, /api/token │
        │                                                  │
        │  sfu     7882/udp          media, not proxied    │
        └──────────────────────────────────────────────────┘
              ▲                              ▲
              │ https page, wss signalling   │ microphone audio
              └───────────── browser ────────┘
```

Three things about that picture matter more than anything else here.

**The page has to be served over TLS.** A browser hands out a microphone only in
a secure context, and `http://203.0.113.7` is not one. That is why Caddy is in
the stack even on a LAN, and why the signalling WebSocket is proxied through it:
a page on `https://` may only open `wss://`.

**On a host with no domain, give it a name anyway.** `203.0.113.7.sslip.io` is a
free name that resolves to `203.0.113.7`, and with it Caddy gets an ordinary
Let's Encrypt certificate and nobody sees a warning. Serving TLS on the bare IP
works, but every visitor meets a full-page certificate warning first and phones
frequently refuse the microphone afterwards regardless. Read "TLS without a
domain" below before choosing; it is the single decision that most affects
whether a stranger you send the link to actually gets into the room.

**Media is not proxied.** It is UDP, straight from the browser to the host's
`7882/udp`, and the SFU has to tell the browser an address it can actually
reach. That address is `MEDIACORE_ADVERTISE_IP`. Get it wrong and everything
looks fine — people join, the participant list fills in — and the room is
silent.

## Run it

On a fresh host with Docker and the compose plugin:

```
git clone https://github.com/GlobalArtInc/mediacore
cd mediacore/deploy
cp .env.example .env
openssl rand -hex 32          # paste into MEDIACORE_API_SECRET
$EDITOR .env                  # and set MEDIACORE_PUBLIC_HOST, MEDIACORE_ADVERTISE_IP, MEDIACORE_TLS
docker compose up -d --build
```

`MEDIACORE_PUBLIC_HOST`, `MEDIACORE_ADVERTISE_IP` and `MEDIACORE_API_SECRET`
ship empty. Leave any of them empty and `docker compose up` stops before
starting anything, naming the variable and what it wants:

```
required variable MEDIACORE_API_SECRET is missing a value: set MEDIACORE_API_SECRET
in deploy/.env; generate one with `openssl rand -hex 32`. ...
```

That is deliberate. A plausible-looking example address left in place starts a
stack that signals perfectly and carries no audio, which is far harder to
diagnose than a refusal.

The first build compiles the whole Rust workspace and takes a few minutes;
after that only changed crates are rebuilt, because dependencies are a layer of
their own. `docker compose up -d --build` again after changing any code. Every
variable is explained in `.env.example`, including what breaks when it is
missing.

Everyone who opens the URL lands in `MEDIACORE_FRONT_ROOM`. Adding
`?room=standup` to the link puts that visitor in `demo.standup` instead; rooms
are created on demand and disappear when the last person leaves. The prefix is
not decoration: the front end mints tokens without asking anyone who they are,
so it is confined to `MEDIACORE_FRONT_ROOM` and the namespace hanging off it,
and cannot issue a token for a room named anything else on the same SFU.

Ports the host must accept:

| Port | Protocol | For |
|---|---|---|
| 443 | TCP | the page and the signalling WebSocket |
| 80 | TCP | the Let's Encrypt challenge, and the redirect to https |
| 7882 | UDP | all audio. `MEDIACORE_RTC_UDP_PORT` |

On a cloud host that means a security group rule for **UDP** 7882. A TCP rule
for that port does nothing at all.

Nothing else is fetched. The browser SDK is compiled into the front end and
served from the same origin as the page, so a host with no route to the
internet still works, and the page's content security policy (`default-src
'none'`, `script-src 'self'`) forbids third-party scripts outright. The only
cross-origin thing the page is permitted to open is the signalling WebSocket at
`MEDIACORE_FRONT_WS_URL`, which is this same origin anyway.

## TLS without a domain

This is the part that goes wrong, and the shape of the answer matters more than
the settings. **A certificate nobody trusts is not a working deployment.** It is
a deployment where every visitor meets a full-page browser warning before they
see the page, and where phones often refuse the microphone even after they click
through it. Prefer, in this order:

1. your own domain,
2. a wildcard-DNS name for the host's public IP — `<ip>.sslip.io`,
3. Caddy's own CA, only when neither of the above is possible.

### 1. A real domain

DNS for `meet.example.com` points at the host, ports 80 and 443 are reachable
from the internet.

```
MEDIACORE_PUBLIC_HOST=meet.example.com
MEDIACORE_TLS=you@example.com
MEDIACORE_ADVERTISE_IP=203.0.113.7     # still an IP, and still required
```

Caddy gets a certificate from Let's Encrypt on first request and renews it by
itself. The visitor sees nothing unusual. Note that `MEDIACORE_ADVERTISE_IP`
does not become the domain: ICE candidates carry addresses, not names.

### 2. No domain, but a public IP and port 80 open — do this one

**This is the recommended shape for a plain cloud VPS, which is what most
people deploying this have.** You do not need to own a domain to be given a
name: a wildcard DNS service maps an address into a hostname that resolves back
to it. `203.0.113.7.sslip.io` and `203-0-113-7.nip.io` both resolve to
`203.0.113.7`, and Let's Encrypt will certify such a name like any other.

```
MEDIACORE_PUBLIC_HOST=203.0.113.7.sslip.io
MEDIACORE_TLS=you@example.com
MEDIACORE_ADVERTISE_IP=203.0.113.7
```

Nothing else changes: same compose file, same ports, same one URL to send
people. What you get for it is the entire difference between "open this link"
and "open this link, then click through a security warning, and if you are on a
phone it may not work at all".

Its costs, stated plainly: you depend on someone else's DNS service staying up,
Let's Encrypt's rate limits apply (five certificates per week for the same
name), the address is visible in the hostname, and it cannot work on a private
LAN address or when port 80 is blocked, because the ACME challenge fails.

### 3. A bare IP on a LAN, or no port 80

No public CA will certify `192.168.1.20`. The fallback is Caddy's own CA:

```
MEDIACORE_PUBLIC_HOST=192.168.1.20
MEDIACORE_TLS=internal
MEDIACORE_ADVERTISE_IP=192.168.1.20
```

Two things make this work at all, and the first is easy to lose.

**The browser sends no SNI for an IP address.** RFC 6066 forbids putting an IP
literal in the SNI extension, so a request to `https://192.168.1.20` arrives
with no name attached and Caddy has nothing to look a certificate up under. Left
alone it falls back to the container's own bridge address, finds no certificate
for that, and aborts the handshake — the visitor gets `tlsv1 alert internal
error` and a blank page, with no warning to click through and nothing to read.
The `Caddyfile` fixes this with one line in the global options:

```
default_sni {$MEDIACORE_PUBLIC_HOST}
```

which tells Caddy to treat an SNI-less handshake as a request for the site it
serves. If you edit the `Caddyfile`, do not drop it.

**The warning is then the visitor's whole first impression.** Caddy issues
itself a certificate for that IP, nothing trusts the CA that signed it, and
**every visitor gets a full-page browser warning and must click through it
once.** Be honest about this when you send the link. What they see:

- **Chrome, Edge** — "Your connection is not private",
  `NET::ERR_CERT_AUTHORITY_INVALID`. They click **Advanced**, then **Proceed to
  192.168.1.20 (unsafe)**. The page then loads, the address bar says "Not
  secure", and the microphone prompt still appears: for Chrome the origin is
  still a secure context, so `getUserMedia` works. The exception lasts until the
  browser is restarted.
- **Firefox** — "Warning: Potential Security Risk Ahead". **Advanced…**, then
  **Accept the Risk and Continue**. Firefox stores the exception permanently.
- **Safari on macOS** — "This Connection Is Not Private". **Show Details**, then
  **visit this website**, then confirm; it may ask for the device password.
- **Safari on iOS, and Android WebViews** — the least reliable of the lot. The
  warning can be clicked through, but a microphone on a certificate the device
  does not trust is refused more often than not, and when it is refused there is
  no error to read: the Join button simply never gets a track. If phones have to
  work, do not use this path — use the `sslip.io` route above, or install the CA
  below.

Because the page and the WebSocket are the same origin, the exception the
visitor accepted for the page also covers `wss://`. That is why Caddy proxies
`/rtc` instead of the SFU being contacted directly: a WebSocket to a second host
with an untrusted certificate is refused by the browser with nothing shown to
the user and nothing logged on the server.

To get rid of the warning entirely on machines you control, install Caddy's root
certificate on them:

```
docker compose cp caddy:/data/caddy/pki/authorities/local/root.crt ./mediacore-root.crt
```

Then trust it: macOS, add to the login keychain and set it to "Always Trust";
Linux, copy into `/usr/local/share/ca-certificates/` and run
`update-ca-certificates` (Firefox has its own store, import under Settings →
Privacy → Certificates); Windows, import into "Trusted Root Certification
Authorities"; iOS, mail or AirDrop the file, install the profile, then enable it
under Settings → General → About → Certificate Trust Settings.

The `caddy_data` volume holds that CA. Delete the volume and the root changes,
which means every installed copy is worthless and every warning comes back.

That list is the honest cost of option 3, and it is why option 2 is written up
above it: for a public VPS, `<ip>.sslip.io` replaces all of it with nothing.

## Check that it works

```
curl -k https://$MEDIACORE_PUBLIC_HOST/healthz     # -> ok      (the front end)
curl -s 127.0.0.1:7880/healthz                     # -> counters (the SFU, host only)
docker compose ps                                  # three services, sfu and front healthy
```

That first line is the TLS check as well as the health check. `curl` sends no
SNI for an IP literal, exactly as a browser does not, so on a bare-IP
deployment it exercises the handshake the `default_sni` line in the `Caddyfile`
exists for. `ok` means the certificate was found and served; a handshake error
there means the page will not load for anybody.

Caddy holds the URL back until both upstreams report healthy, so there is no
window where the address answers 502 while the stack is still starting; if it
does answer 502, an upstream is genuinely down.

The SFU's own line tells you what it will hand out to clients:

```
docker compose logs sfu | grep -E "media transport ready|advertised as"
```

```
start-sfu: media will be advertised as 203.0.113.7:7882
... media transport ready local=0.0.0.0:7882 advertise=[203.0.113.7:7882] ...
```

That `advertise` list is the single value the audio depends on. Read it and ask:
can the browser send a UDP packet there? Ignore the `ice socket bound` line just
above it, which reports the socket's own view (`advertise=[0.0.0.0:7882]`) and
not what clients are told.

And the front end's container prints the URL it will hand to browsers:

```
docker compose logs front | grep listening
... mediacore-front is listening addr=0.0.0.0:8080 room=demo ws_url=wss://meet.example.com
```

Then the real test, which needs two people or two devices — one browser talking
to itself proves less than you would like, because two tabs on one machine can
succeed on a candidate that nobody else can use:

1. Both open `https://<MEDIACORE_PUBLIC_HOST>`, type a name, press Join.
2. Each sees the other in the participant list.
3. Speak. The other hears it.

While that happens the SFU logs roughly this sequence per participant:

```
participant joined room=demo identity=alice-a1b2 sdk=js ...
track published identity=alice-a1b2 kind=Audio ...
publisher transport started identity=alice-a1b2 ...
publisher transport connected identity=alice-a1b2
publisher media is live identity=alice-a1b2 track=TR_... mime_type=audio/opus
```

`publisher transport connected` is the line that means UDP arrived. And
`rtp_packets` in `/healthz` climbs while someone is speaking:

```
watch -n1 'curl -s 127.0.0.1:7880/healthz'
```

## When it does not work

Three failures cover nearly everything. They are told apart by how far the log
gets, so read it from the top rather than looking for an error.

### 1. No TLS

**Symptom.** The page does not load at all, or it loads and the Join button
fails with a microphone error. In the browser console:
`navigator.mediaDevices is undefined`, or `getUserMedia` refused.

**Logs.** Nothing in the SFU log — no `participant joined`, because nobody got
that far. The evidence is in Caddy:

```
docker compose logs caddy
```

- ACME failed for a real domain:
  `could not get certificate from issuer`, or an `urn:ietf:params:acme:error:…`
  line naming the reason (`connection refused` means port 80 is not reachable
  from the internet; `unauthorized` or `dns` means the name does not resolve
  here). Caddy retries, so this repeats.
- A warning on a name that should not warn: the certificate the browser shows
  is issued by `Caddy Local Authority`, which means `MEDIACORE_TLS` is still
  `internal`. Put your email address there instead and restart Caddy.
- An email address set for a bare IP: no public CA will certify one under
  ordinary circumstances, so you get either the local certificate anyway or
  ACME failures repeating in the log. Set it back to `internal`, or take the
  `sslip.io` route above.
- Caddy never logged a request at all: the visitor is not reaching the host, or
  the handshake died before there was a request to log. Check port 443 in the
  firewall and the security group first. If the port is open and the browser
  still shows a bare connection error on a *bare IP* — `ERR_SSL_PROTOCOL_ERROR`,
  or `tlsv1 alert internal error` from `curl -k` — the `default_sni` line is
  missing from the `Caddyfile`'s global options. Reproduce it from the host
  with the same SNI-less handshake a browser makes:

  ```
  curl -k -sv https://<MEDIACORE_PUBLIC_HOST>/healthz
  ```

  `curl` sends no SNI for an IP literal either, so a working stack answers
  `ok` here and a broken one fails in the handshake.

**Also check.** `MEDIACORE_PUBLIC_HOST` must be exactly what the visitor types,
and it is a host name, not a URL. Caddy serves that one name; a request with any
other `Host` header — a second DNS name, or the IP when the variable holds a
domain — gets no certificate and no site. A scheme or a trailing slash in the
variable is refused at startup by both containers that read it:

```
docker compose logs caddy | grep start-caddy
start-caddy: MEDIACORE_PUBLIC_HOST is `https://203.0.113.7`; drop the scheme, this is a host name, not a url
```

### 2. Wrong advertised address

**Symptom.** Everything works except sound. People join, the participant list is
correct, the mute button works, and nobody hears anyone.

**Logs.** The session gets far and then stops at exactly the same point every
time:

```
participant joined                ✓
track published                   ✓
publisher transport started       ✓
publisher transport connected     ← never appears
publisher media is live           ← never appears
```

After 15 seconds the transport gives up:

```
the local offer was never answered, holding it and refusing to renegotiate
```

and `/healthz` shows `participants=2` with `rtp_packets=0`.

**Confirm it.** Compare the advertised address with reality:

```
docker compose logs sfu | grep "media transport ready"
```

If `advertise` holds `127.0.0.1`, a `172.17.x.x` docker address, or the host's
private address while visitors are on the internet, that is the fault. Set
`MEDIACORE_ADVERTISE_IP` to the address the visitors' machines route to and
`docker compose up -d sfu`.

`start-sfu.sh` refuses loopback and the wildcard outright and warns when the
address is in a private range, so the values that survive to this point are the
subtle ones: the public IP of the wrong interface, or a NAT'd host whose
public address is not the one it sees on its own NIC.

### 3. UDP blocked

**Symptom and server logs are identical to case 2** — that is the trap. The
packets are addressed correctly and never arrive, so the SFU sees exactly what
it sees when they were sent somewhere else.

**Tell them apart** from the host, not from the log. With the advertised address
confirmed correct, watch for inbound packets while someone presses Join:

```
sudo tcpdump -ni any udp port 7882 -c 5
```

- **Nothing at all** while a browser is trying to connect: the packets are not
  reaching the host. That is a firewall — a cloud security group missing a UDP
  rule is the usual one, and note that Docker's own iptables rules bypass `ufw`,
  so a host firewall is a less likely culprit than the provider's.
- **Packets arrive on the host but the SFU still logs nothing**: they are not
  reaching the container. Check that the published port is the same number on
  both sides in `docker compose ps` (`0.0.0.0:7882->7882/udp`). A mapping like
  `7999->7882/udp` breaks ICE, because the client was told to expect replies
  from the port in the candidate.

From the client side, `chrome://webrtc-internals` shows the ICE candidate pair
stuck in `in-progress` or `failed`, with bytes sent and zero received — the same
picture for both causes, which is why the host-side check above is the one that
decides.

## Notes

- **Changing `.env`** takes effect on `docker compose up -d`; changing the
  `Caddyfile` needs `docker compose restart caddy`, because the admin API is
  turned off.
- **Linux hosts that care about UDP throughput** can drop the port publishing
  and give the `sfu` service `network_mode: host`. The container then binds the
  host's ports directly, which removes docker's userland UDP proxy from the
  media path. Signalling then reaches the SFU on `127.0.0.1:7880`, so Caddy
  needs `host.docker.internal` or the host address as its upstream instead of
  `sfu:7880`.
- **Images are floating tags** (`rust:1-bookworm`, `debian:bookworm-slim`,
  `caddy:2-alpine`). Pin them by digest for a deployment you intend to
  reproduce.
- **Secrets.** `.env` is gitignored here. It holds the token-signing secret;
  anyone with it can mint a token for any room and any identity. The `caddy`
  service deliberately does not read `.env` — it is given only the two variables
  it uses, so the signing secret is never in its environment.
- **The browser SDK is vendored**, not fetched. It lives in
  `crates/front/assets/vendor/`, is compiled into `mediacore-front` and is
  served from this origin under a version-stamped path, so there is no CDN in
  the runtime path and no third-party script for the content security policy to
  allow. See that directory's README to move to another version.
- **What this is not.** One node, no TURN, no recording, no room management API,
  and a room that anyone with the URL can join under any name. The token
  endpoint is unauthenticated, which is why it is confined to
  `MEDIACORE_FRONT_ROOM` and the namespace below it: put the front end on an SFU
  that is also serving something real and it still cannot reach that something's
  rooms. It is a stack you can put in front of people, not a service to run a
  business on.
