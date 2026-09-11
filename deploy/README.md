# Deploying mediacore

Three containers, one host, one URL — and a fourth, off until it is asked for,
which relays media for the clients whose own network will not carry it. Someone
opens the URL, types a name, and hears everyone else in the room. Audio by
default; the camera stays off unless somebody ticks the box in the lobby. Point
it at an identity provider and it stops asking for a name — see "Signing people
in" below.

```
                              the host
        ┌────────────────────────────────────────────────────┐
        │  caddy   80/tcp, 443/tcp   terminates TLS          │
        │    ├── /rtc*     ──► sfu   :7880  signalling       │
        │    ├── /twirp*   ──► sfu   :7880  server API       │
        │    ├── /swagger* ──► sfu   :7880  API docs         │
        │    └── /         ──► front :8080  page, /api/token │
        │                                                    │
        │  sfu     7882/udp          media, not proxied      │
        │  turn    3478/udp+tcp      a relay, off by default │
        └────────────────────────────────────────────────────┘
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

Everyone who opens the bare URL lands in `MEDIACORE_FRONT_ROOM`. A room has an
address of its own: `https://meet.example.com/r/standup` puts that visitor in
`demo.standup`, and it is a link to paste to a colleague — reloading it comes
back to the same room. Rooms are created on demand and disappear when the last
person leaves. The prefix is not decoration: the front end mints tokens without
asking anyone who they are, so it is confined to `MEDIACORE_FRONT_ROOM` and the
namespace hanging off it, and cannot issue a token for a room named anything
else on the same SFU.

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

## The API page at `/swagger`

`https://<MEDIACORE_PUBLIC_HOST>/swagger` is `livekit.RoomService` written out
with a button next to every method. It is generated from the protobuf
descriptors the binary was built from, so it describes the wire this build
speaks rather than the wire somebody once wrote down.

Three things to know before sending anyone the link.

**It is served by the SFU, not by the front end, and that is deliberate.** The
Try-it-out button posts to `/twirp/mediacore.RoomService/…` on whatever origin
served the page. Serve the page from one service and the API from another and
every button is a cross-origin call to a different port — preflighted, possibly
on a certificate this host's does not cover, and failing in the browser with
nothing for an operator to read. Both come from the SFU, so the page works
through Caddy on the public origin, and works exactly the same over an SSH
tunnel to `127.0.0.1:7880/swagger` with no proxy at all.

**Try-it-out is not a sandbox, and the page says so at the top.** `DeleteRoom`
really deletes the room and hangs up on everyone in it. Nothing on that page is
a dry run.

**Reading it costs nothing; using it needs a server token.** Every method wants
`Authorization: Bearer <jwt>` signed with `MEDIACORE_API_SECRET` and carrying
`roomCreate`, `roomList` or `roomAdmin` — a backoffice credential, not the join
token the front end mints for a browser. The page explains which method wants
which, and that `roomAdmin` is the only grant scoped to one room. There is no
token in the page and nothing to leak by publishing it, but if you would rather
it were not on the public origin, delete the `handle /swagger*` block from the
`Caddyfile` and restart Caddy: the API is unaffected and the page stays
readable from the host on `127.0.0.1:7880`.

Nothing is fetched from a CDN. swagger-ui is vendored in
`crates/server/assets/vendor/`, compiled into the `mediacore` binary and served
from this origin under a version-stamped path, and the page carries
`default-src 'none'; script-src 'self'` like the front end does. Its licence
notice is served beside it at `/swagger/NOTICE.txt`.

## Upgrading a deployment that already runs

### The config file is read differently in this release

Two changes here can stop a server that started yesterday, so they come before
the commands.

**The server fills in its own variables now, and `$$` is one `$`.** Nothing
renders the config any more: `start-sfu.sh` exports what
`mediacore.yaml.template` names and hands the server the template itself,
variables and all, and the server fills every `${MEDIACORE_...}` in from its own
environment as it reads the file. That is what gets the api secret off the
filesystem — there is no rendered copy to read, to back up, or to leave behind on
a volume. The expansion applies to **every** config this server reads, a
hand-written `config.yaml` and a ConfigMap somebody else rendered included, and
there is no switch to turn it off for one file: one rule everywhere is the only
version of this that stays reasonable, and two would mean a deployment that
renders its own config and forgets which mode it is in. So check two things in
any config you already have:

- **A value with a literal `$$` in it loses one dollar.** `pa$$word` is read as
  `pa$word`, and what you see then is a database refusing the password, a long
  way from the config that changed it. `$$` is how one literal dollar is
  written, so `pa$$$$word` is the value that was meant.
- **A value with `${MEDIACORE_...}` in it, or something close enough to be a
  mistake, is a refusal that names it** rather than text left as it stands:
  `$MEDIACORE_X` with no braces (which `envsubst` did substitute), or
  `${mediacore_x}` in lower case. A `${...}` that is genuinely somebody else's —
  `${HOME}`, or one inside a room's metadata — is left exactly as written.

**A token-signing secret has to be at least 32 bytes, and may not be empty.** The
server refuses both now, naming the key. The front end has always refused to
start below 32 bytes on this same secret, so a shorter one was never a deployment
that worked end to end — but an SFU running without the front end would have
started on it, and on this tag it will not: generate one with
`openssl rand -hex 32`. Empty is the case worth the paragraph. HS256 signs and
verifies with an empty key perfectly happily, so a Secret whose `api-secret` key
existed and held nothing used to give a server that was Ready, answered
`/healthz`, and accepted a join token for any room minted by anybody who knew
that the key is *called* `mediacore` — a name that is in this repository and is
not a secret. Only `start-sfu.sh` refused it, and a cluster does not run
`start-sfu.sh`. Check what your Secret actually holds before rolling this image;
a deployment that was running on a short secret stops starting, by design.

### Using the compose file in this repository

```
git pull
docker compose up -d --build
docker compose restart caddy
```

The restart is not optional and is easy to forget: the `Caddyfile` gained a
route, and Caddy's admin API is turned off here, so a running container will not
pick it up on its own. Without it the stack comes up with the page compiled into
the SFU and no way to reach it from the outside — `/swagger` falls through to
the front end and answers 404.

### A deployment whose compose file is not in this repository

The one running the front end on **5401** behind Caddy on **5400** needs three
changes and no new infrastructure.

1. **Rebuild the SFU image from this tree.** The page, the vendored swagger-ui
   and the OpenAPI document are all compiled into the `mediacore` binary. There
   is no new file to mount, no new volume, no new environment variable and no
   new port: `/swagger` is served by the process that already serves `/twirp`
   on 7880.

2. **Add one route to the reverse proxy, pointing at the same upstream as
   `/twirp`.** In a Caddyfile:

   ```
   handle /swagger* {
   	reverse_proxy sfu:7880
   }
   ```

   In nginx, beside the `/twirp/` location:

   ```
   location /swagger {
       proxy_pass http://sfu:7880;
   }
   ```

   If `/twirp` is not routed on 5400 yet, route it in the same change. A page
   whose every button 404s is worse than no page: it reads as a broken server.

3. **Leave the front end on 5401 alone.** It serves nothing new, and `/swagger`
   must *not* be routed to it — the front end does not have the page, and
   pointing at it would put the documentation on a different origin from the API
   it documents, which is the one arrangement that cannot work.

Then check it, from a machine that reaches 5400:

```
curl -sS -o /dev/null -w '%{http_code}\n' https://<host>:5400/swagger
curl -sS https://<host>:5400/swagger/openapi.json | head -c 32
```

`200`, and a document that begins `{ "openapi": "3.0.3", …`. If the first is a
404, the route went to the front end or was never added. If the page loads and
every Try-it-out comes back 404 with an HTML body rather than a Twirp error,
`/twirp` is not on 5400 either — the request reached the front end, which has no
such path, and the two halves are not one origin after all.

### The volumes this release adds

The server now keeps a record of every meeting — one JSON file per meeting,
read back at `/api/rooms/<room>/sessions`, `/api/rooms/<room>/session` and
`/api/sessions/<id>`. They are written under `sessions.directory`, which
`mediacore.yaml.template` sets to `/var/lib/mediacore/sessions`, and
`docker-compose.yml` mounts a named `sessions` volume there. Using the compose
file in this repository, `docker compose up -d --build` creates it and there is
nothing to do.

**A deployment whose compose file is not in this repository has to add that
volume itself**, or every meeting it records goes with the container on the next
release — which is the one failure this feature exists to prevent. The image
creates the directory owned by the user the server runs as, so a *named* volume
mounted over it inherits that ownership and works; a bind mount from a host
directory does not, and the directory has to be `chown 10001:10001` first. The
server refuses to start when it cannot write there, deliberately: one that
answers with an empty history because it never opened its own directory is a
failure nobody discovers until they are asked what happened at a meeting last
week.

Nothing prunes it. One small file per meeting, so
`find /var/lib/mediacore/sessions -name '*.json' -mtime +90 -delete` is the
whole of a retention policy for whoever wants one, and costs nothing to whoever
does not.

**This stack is one server process, which is what makes a directory the right
store for it.** Two things make it the wrong one, and both are a matter of
setting `sessions.postgres.url` instead: running more than one SFU process
against the same rooms, because three processes cannot share a directory and a
node that dies takes its unfinished meetings out of reach of the ones still up;
and recording anything at all, because the upload queue is a thing several
processes poll and a directory refuses to be one. The database is down does not
stop the server -- calls carry on and `unwritten_sessions=` on `/healthz`
climbs -- and the schema is the server's to create, so there is nothing to run
before starting it. See "Sessions" in the top-level README.

The second volume is `events`, at `/var/lib/mediacore/events`: the outbox of
events promised to a webhook endpoint or a broker and not yet taken by one. It
stays empty until `events.webhooks.urls` or `events.kafka.brokers` names
somebody, and it does not need pruning — a record exists only while an event is
in a queue, and the queue is bounded, so the directory is bounded with it. The
same ownership rule applies: a named volume inherits the image's ownership and
works, a bind mount wants `chown 10001:10001` first, and the server refuses to
start when it cannot write there.

**A deployment that adds webhooks without adding that volume loses exactly the
events its endpoint was away for**, which is the one thing the outbox is there
to prevent. Everything announced is also recorded, so what is lost is the
notification and never the fact: the meeting is still under
`/api/rooms/<room>/sessions`.

### Recording, and what this stack would need to do it

`recording:` is in the template and switched off, because turning it on takes
more than a flag. It needs a database — an upload is a row several processes
claim, and the sessions directory cannot be a queue — and it needs a bucket and
a third volume at `recording.directory`, where audio is written while a meeting
is happening and from which it is uploaded whole when the meeting ends. A
recording volume inside the image would take every not-yet-uploaded recording
with the next release, which is the same failure as a sessions directory inside
the image and costs more.

The server refuses to start if `recording.enabled` or a bucket is set without a
database behind it. That is deliberate: the alternative is a deployment that
holds meetings for a week and has no recordings, discovered when somebody asks
for one.

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
curl -sk -o /dev/null -w '%{http_code}\n' \
     https://$MEDIACORE_PUBLIC_HOST/swagger        # -> 200     (the API docs, on the SFU)
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

A third possibility, once the host-side check shows packets arriving from
everybody except one person: it is not this host's firewall but theirs, and no
setting here will fix it. That is what the relay below is for.

## Relaying media for clients that cannot reach the host (optional)

Everything above assumes a browser can send a UDP packet to
`MEDIACORE_ADVERTISE_IP:7882` and get one back. Nearly all of them can. The ones
that cannot are on a network that will not pass it: UDP dropped outright, or
allowed only towards a couple of well-known ports. A hotel, a hospital, a
corporate laptop on a VPN. For those people the room is silent, and the server
log is indistinguishable from case 2 and case 3 above.

**It is the network's UDP policy and not the NAT**, which is worth saying
because a relay is usually described as the cure for symmetric NAT. That is the
peer-to-peer case. Here a client only ever talks to a server that already has a
public address, and this server is ice-lite — it answers whoever reaches it and
never has to reach anybody first — so whatever the NAT does to the client's
source port is of no consequence. What decides is whether the packet is allowed
out at all.

A TURN server is the answer: a relay they *can* reach, which forwards their
packets to the SFU and the SFU's back to them. `deploy/` ships one, and it does
not run unless you ask for it.

```
openssl rand -hex 32                    # paste into MEDIACORE_TURN_SECRET in .env
docker compose --profile turn up -d
```

`docker compose up` on its own never starts it: the service sits behind a
compose profile, because a deployment that does not need a relay should not be
running one more process on a public address than it has to. With
`MEDIACORE_TURN_SECRET` empty the SFU renders no `ice_servers` at all and the
browser is configured exactly as it was before this existed — a client offered
a relay pointing at nothing spends its gathering time proving it, and that time
is silence.

**There is no password to distribute and none in the page.** coturn is started
in its `use-auth-secret` mode, where a username is `<expiry>:<identity>` and the
password is that username's HMAC-SHA1 under the shared secret. The SFU mints one
per person as they join and sends it in the join response; coturn checks it by
computing the same thing, with no user database anywhere. The consequences worth
knowing: the credential expires (`MEDIACORE_TURN_TTL_SECONDS`, a day by
default — long enough to outlast the call, because coturn re-checks it on every
allocation refresh and not only at the start), it names the person in coturn's
own log, and a stolen one is worth bandwidth through this one relay towards this
one server.

Ports the host must accept, on top of the table further up:

| Port | Protocol | For |
|---|---|---|
| 3478 | UDP | the relay, for a client that may send UDP to 3478 but not to 7882 |
| 3478 | TCP | the relay, for a client whose network drops UDP altogether |
| 61000–61099 | UDP | the relayed media itself. `MEDIACORE_TURN_RELAY_*_PORT` |

That range is one port per allocation and two allocations per person, because a
client keeps a publisher transport and a subscriber transport and each is an ICE
session of its own. Fifty people on the relay at once, as shipped.

It starts above 60999 for a reason worth keeping if you widen it: a default
Linux hands out 32768–60999 as ephemeral ports to everything else on the host,
nothing keeps that machinery off a relay range, and an overlap is coturn losing
an allocation to a port an outbound connection took first — on a busy day, not
on the day it was configured. `sysctl net.ipv4.ip_local_port_range` says what
this host actually does; a machine tuned for many outbound connections often has
that range up against 65535 already, and then the relay range has to be carved
out of it with the same sysctl.

The `turn` service runs with `network_mode: host` rather than publishing those
ports. Publishing a range means a docker userland proxy per port — a hundred
processes, slow to start, each one a copy through userland in the middle of the
media path — and it would rewrite the addresses the relay and its peers see of
each other, which is what coturn's permission checks are made of. The cost is
the isolation: coturn binds the host's own addresses, and this is Linux-only.

**The relay is deliberately useless for anything but this server.**
`turnserver.conf.template` denies the entire address space as a peer and then
allows exactly one address back in — `MEDIACORE_TURN_ALLOWED_PEER_IP`, which
follows `MEDIACORE_ADVERTISE_IP` unless you set it. Without that, a TURN server
whose credentials anybody joining can obtain is an open proxy: a way to reach
anything this host can reach — another container, the cloud provider's metadata
service, a database on the LAN — from an address that is not the sender's.
Somebody sweeping for one will find it. What the rule costs is that this coturn
cannot be borrowed for anything else, which is the point of it.

There are three ways to spend a day on TURN and still have a silent room. Each
is below, and each is worth reading before rather than after.

### 1. The relay has to reach the address the SFU advertises

coturn is allowed exactly one peer, and it is the address in the SFU's ICE
candidates. **Both on this host is the case that works**: the relayed packet
leaves coturn addressed to the host's own public address, arrives at the
published UDP port, and the answer comes back the same way. That is the default
here, and it needs no thought.

**coturn somewhere else while the SFU advertises a private address does not
work, and it fails as silence.** The client reaches the relay, authenticates,
gets its allocation, asks permission to talk to `10.0.0.5:7882`, and coturn —
on a different network — sends into a void. Nothing errors: an allocation is not
a connection, and TURN has no way to report that a peer never answered. On the
SFU it looks exactly like case 2 above, right down to `rtp_packets=0`.

So if the relay is not this host, `MEDIACORE_ADVERTISE_IP` has to be an address
the relay can route to as well as the clients, and
`MEDIACORE_TURN_ALLOWED_PEER_IP` has to name that same address. The same applies
the day the advertised address changes: left empty the allow rule follows it,
set by hand it does not, and what you get is

```
docker compose logs turn
ERROR A peer IP 203.0.113.7 denied in the range: ::-ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff
```

which is the one failure in this whole section that says plainly what happened.

### 2. TURN over TLS, and port 443

Be clear about what each url cures, because they are not the same thing:

- `turn:…?transport=udp` on 3478 cures **a network that passes UDP to a
  well-known port and not to 7882**. It does nothing where UDP is dropped
  outright, because it is UDP.
- `turn:…?transport=tcp` on 3478 cures **a network that drops UDP but allows
  outbound TCP anywhere**. This stack offers it already; for hotel and hospital
  networks it is frequently the one that works.
- `turns:` on **443** is the one that cures **a network that allows nothing out
  but 443**, because it is then a TLS connection to port 443 like every other
  one leaving that building.

And 443 on this host belongs to Caddy: the page and the signalling WebSocket are
there, and they cannot move without moving the site. The honest options are
three, and there is no fourth:

1. **A second address on the host.** The clean answer, and the one line to get
   right is not in the `Caddyfile`. Caddy's listener on this host is docker's:
   it comes from

   ```yaml
   ports:
     - "80:80"
     - "443:443"
   ```

   on the `caddy` service, which publishes on every address the host has and
   installs a rule that catches anything arriving on those ports whatever
   address it was sent to. A `bind` directive in the site block changes only
   which address Caddy listens on inside its own bridge namespace, and leaves
   coturn — on host networking — without 443 on the second address either way:
   docker's own listener holds that port on every address the host has, and
   traffic that did arrive on the second one would have been handed to Caddy by
   that rule regardless. So pin the publish to the first address,

   ```yaml
   ports:
     - "203.0.113.7:80:80"
     - "203.0.113.7:443:443"
   ```

   and give coturn the second with `listening-ip=203.0.113.8` and
   `tls-listening-port=443` in `turnserver.conf.template`. `listening-ip` is the
   line the template deliberately does not carry — without it coturn discovers
   the host's own addresses, which is what lets it work on a cloud VM whose
   public address is on the provider's NAT and not on any interface here — so
   this option also makes this host's addresses something a config file states
   and somebody has to maintain. On most providers a second IP is a few minutes
   and a small monthly charge.
2. **SNI routing in front of both.** One listener on 443 reads the SNI out of
   the TLS handshake and hands `turn.example.com` to coturn and
   `meet.example.com` to Caddy — HAProxy, `sslh`, or Caddy's `layer4` module,
   which is not in the stock `caddy` image and needs an `xcaddy` build. It works;
   browsers do send SNI for `turns:`. It costs a component in front of
   everything, a second name, and the relay seeing the proxy's address instead of
   the client's — which matters to its logs and its per-IP quotas and to nothing
   else here.
3. **Accept 3478 and say so.** `turns:` on 5349, which is what this stack
   configures when you give it certificate files, is TLS on a port that a 443-only
   network blocks exactly as it blocks 3478. It buys you the networks that
   inspect protocols but not ports, and nothing more. If the people who cannot
   join are behind a corporate proxy with a 443 allowlist, option 3 will not
   reach them and no tuning will change that.

One trap to close: **443/udp is free on this host** — the `Caddyfile` turns
HTTP/3 off deliberately — so it looks as though coturn's DTLS listener could
have it. It cannot, twice over. Browsers implement `turns:` over TCP only, and
a `turns:…?transport=udp` url is refused by Chrome and Firefox both; and there
is no separate UDP port to point at 443 in any case, because DTLS shares
`tls-listening-port` with TLS — one number for both listeners. There is also no
listener to move: coturn 4.18 documents `--dtls` as "Start DTLS client
listeners. DTLS is not started by default", nothing in
`turnserver.conf.template` asks for one, and `no-dtls` is not a line worth
adding — this coturn answers it with `WARNING Bad configuration format:
no-dtls`.

### 3. Caddy's certificate, and whether coturn can have it

Caddy keeps what it fetches in the `caddy_data` volume, under a directory named
after the issuer:

```
docker compose exec caddy ls /data/caddy/certificates
acme-v02.api.letsencrypt.org-directory

docker compose exec caddy ls /data/caddy/certificates/acme-v02.api.letsencrypt.org-directory/meet.example.com
meet.example.com.crt  meet.example.com.json  meet.example.com.key
```

It is a real path and the volume can be mounted read-only into the turn
container in one line. Do not do it. Three things have to be true at once for it
to work, and all three fail quietly:

- **The directory is named after the issuer, not after you.**
  `acme-v02.api.letsencrypt.org-directory` today,
  `acme.zerossl.com-v2-dv90-directory` if Caddy falls back to its second issuer,
  which it will do on its own when Let's Encrypt refuses. A path written into a
  config file is a guess that stops being true on a day nobody chose.
- **The key is `0600` and owned by root**, because root is what Caddy runs as in
  its image. Whether coturn can open it then depends on which user the coturn
  image runs as, and if the answer is root you have arranged for a relay facing
  the internet to run as root in order to read a certificate. What coturn does
  with a key it cannot open is print one more warning among the certificate
  warnings it prints anyway, and serve no TLS.
- **Renewal.** Caddy replaces those files every sixty days without telling
  anybody, and coturn reads its certificate once, at start. `SIGHUP` does not
  reload it and neither does time — coturn 4.18 goes on serving the old
  certificate until the process restarts. So the failure is TURN/TLS dying two
  months after it was set up, on the one path your hardest-to-reach users take.

**The honest answer is to give coturn its own certificate files.** Put
`cert.pem` and `key.pem` in `deploy/certs`, uncomment the `certs` volume on the
`turn` service, set `MEDIACORE_TURN_TLS_CERT` and `MEDIACORE_TURN_TLS_KEY`, and
make `MEDIACORE_TURN_HOST` a name that certificate covers. Get them from
`certbot` or `lego` for a `turn.` name of their own — those have a deploy hook,
and the hook is exactly the right place for the `docker compose restart turn`
that makes a renewed certificate take effect, which is the step everything else
here is missing.

### Checking that the relay actually relays

The startup lines are the first thing to read. Both containers say what they
decided:

```
docker compose logs sfu | grep start-sfu
start-sfu: media will be advertised as 203.0.113.7:7882
start-sfu: clients will also be offered turn:203.0.113.7:3478

docker compose logs turn | grep start-turn
start-turn: relaying on port 3478 for realm meet.example.com, advertising 203.0.113.7, peers limited to 203.0.113.7
```

If either line surprises you, the two halves answer differently now. `turn` still
renders, so `docker compose exec turn cat /tmp/turnserver.conf` is its rendered
truth. `sfu` renders nothing: the file it read is `mediacore.yaml.template` in
this directory, byte for byte as the repository ships it, and the values that
filled it in are the `MEDIACORE_*` in `.env` — `docker compose config` shows what
compose will hand it, `docker compose exec sfu env | grep MEDIACORE_` shows what
it was handed. With one exception, and it is the one this section is about: the
`ice_servers` block is built by `start-sfu.sh` and lives only in the environment
of the server it exec'd, which `exec` cannot show you, because `exec` starts a new
process from the container's configured environment rather than from PID 1's. The
startup line above is the whole of the report on that block.

Both `exec` commands are for this stack. The image a cluster installs is
distroless — no shell, no `cat`, nothing to exec into — so there the file the
server read is the ConfigMap the chart mounts (`kubectl get configmap <name> -o
yaml`), the values are the container's own environment (`kubectl describe pod`,
and the Secret they come from), and the startup line is in `kubectl logs`.

Five lines in coturn's own log are noise, and knowing them costs less than
chasing them:

```
ERROR no-cli option is deprecated, see --cli
WARNING Certificate file not found or not readable: …/turn_server_cert.pem
WARNING Private key file not found or not readable: …/turn_server_pkey.pem
WARNING NO EXPLICIT LISTENER ADDRESS(ES) ARE CONFIGURED
WARNING NO EXPLICIT RELAY ADDRESS(ES) ARE CONFIGURED
```

The first is a deprecation notice for a line that is there to keep the console
off on older coturn. The certificate ones are printed even when coturn was told
not to open a TLS listener at all — note that they name coturn's own built-in
default paths, and only those: a warning naming a path out of `.env` is not
noise and cannot reach the log anyway, because `start-turn.sh` refuses to start
on a `MEDIACORE_TURN_TLS_CERT` or `_KEY` it cannot read a file at. The last two
are the arrangement working as intended: no address is named, so coturn
discovers the host's own, which is what lets it run on a cloud VM whose public
address is not on any of its interfaces.

Then prove the credential, which is the assertion worth the most here: a wrong
one fails exactly like a firewall does, and looks the same from every side.
coturn's own two test tools do it, and they come with coturn — in the image, or
from `apt install coturn` or `brew install coturn` on any machine that can reach
this one.

```
expiry=$(( $(date +%s) + 3600 ))
user="$expiry:probe"
cred=$(printf '%s' "$user" | openssl dgst -sha1 -hmac "$MEDIACORE_TURN_SECRET" -binary | openssl base64)

docker compose exec -d turn turnutils_peer -L 203.0.113.7 -p 3480
docker compose exec turn turnutils_uclient -u "$user" -w "$cred" \
    -p 3478 -e 203.0.113.7 -r 3480 -n 3 -c 203.0.113.7
```

That mints a credential by hand exactly as the SFU mints one, allocates a relay
port with it, and sends three packets through the relay to a peer standing where
the SFU stands. `-e` is that peer and `-c` is a flag rather than an option —
allocate no companion port for RTCP, which is what a browser does too — so the
address at the end is the relay itself. Working, it ends:

```
start_mclient: tot_send_msgs=3, tot_recv_msgs=3
Total lost packets 0 (0.000000%), total send dropped 0 (0.000000%)
```

Three sent and three received is the whole path: authentication, allocation,
permission, and packets both ways. Anything else, read `docker compose logs
turn` — it names the reason, and there are only ever four:

| In the log | What it is |
|---|---|
| `Cannot find credentials of user <…>` | `MEDIACORE_TURN_SECRET` differs between the SFU and coturn: rotated in `.env` and only one of the two containers re-created (see the Notes), or the username was not minted from it |
| `credentials of user <…> have expired` | the clock, or a `MEDIACORE_TURN_TTL_SECONDS` shorter than the call |
| `A peer IP … denied in the range` | `MEDIACORE_TURN_ALLOWED_PEER_IP` is not the address the SFU advertises |
| nothing at all | the client never reached 3478: firewall, security group, or a relay that is not where the clients were told it is |

Finally, from a browser: `chrome://webrtc-internals` during a call, in the
candidate-pair table, a pair whose local candidate type is `relay`. Only a
client that needs the relay will use one — everyone else connects directly, and
that is the intent. To make yourself into that client for a test, the shortest
route is a phone on a mobile network with the host's UDP port closed in the
firewall for a minute.

## Signing people in (optional)

Out of the box anybody with the link types a name and joins. Point the front end
at an OpenID Connect provider and it stops asking: visitors are sent there, come
back signed in, and type only the room.

Four variables switch it on, in `deploy/.env`:

```
MEDIACORE_FRONT_OIDC_ISSUER=https://sso.example.com
MEDIACORE_FRONT_OIDC_CLIENT_ID=…
MEDIACORE_FRONT_OIDC_CLIENT_SECRET=…
```

`MEDIACORE_FRONT_PUBLIC_URL` is not among them: it is derived in
`docker-compose.yml` from `MEDIACORE_PUBLIC_HOST`, so the redirect URI is built
from the same host name as the certificate and cannot drift from it.

**Register two URLs with the provider before you start**, and register them
exactly, including the port:

```
redirect              https://<MEDIACORE_PUBLIC_HOST>/auth/callback
post-logout redirect  https://<MEDIACORE_PUBLIC_HOST>/
```

A provider refuses a request whose redirect URI it does not recognise, and the
error page it shows says nothing useful. This is the failure to expect first.

**It needs real TLS**, more than the rest of the stack does. The session cookie
is `__Host-` prefixed, which a browser will not accept without `Secure`, which it
will not send outside a secure context. On plain http nothing is stored and
nobody stays signed in, with no error anywhere. `MEDIACORE_TLS=internal` is fine
— a warning clicked through is a secure context — and so is `http://localhost`
while developing.

Two things worth knowing before you turn it on:

- **A restart signs everybody out.** Sessions live in the front end's memory.
  Nobody in a call is dropped by it: the join token was minted at join, has its
  own six-hour life and is the SFU's to check. It costs each person one press of
  the provider's already-signed-in page at their next page load.
- **Signing in says who somebody is, not where they may go.** The provider's
  roles are read and not enforced: anybody the provider will sign in can still
  enter any room whose name they know. `crates/front/README.md` has the reasoning
  and where a rule would go if there were one.

If the provider cannot be reached, signing in stops and nothing else does: a page
asked for with no session answers `503` saying so, sessions already open keep
working, and calls already running are untouched. `HTTPS_PROXY` and `NO_PROXY`
are honoured for the calls the front end makes to the provider.

## Notes

- **Changing `.env`** takes effect on `docker compose up -d`, which re-creates
  the containers whose environment changed. **With the relay running, that
  command needs the profile** — `docker compose --profile turn up -d`, or
  `COMPOSE_PROFILES=turn` in `.env` so it is said once for good. Without it the
  `turn` service is not in the set compose acts on at all, so an edited
  `MEDIACORE_TURN_SECRET` reaches the SFU and not coturn, and every allocation
  is answered `401` for exactly the people who needed the relay. `restart` is no
  substitute: it re-runs a container with the environment it was created with,
  so it never picks up `.env`.
- **Changing a file mounted from this directory** is the other half, and that
  one is what `restart` is for: the `Caddyfile` needs
  `docker compose restart caddy`, because the admin API is turned off, and
  `turnserver.conf.template` — mounted here, rendered by `start-turn.sh` at
  start, same shape as the SFU's config — needs `docker compose restart turn`.
- **The relay is behind a profile**, and where that bites is narrower than it
  looks. A command that names the service finds it either way: `logs turn`,
  `restart turn`, `exec turn` all work without the flag, because compose enables
  the profile of a service you asked for by name. It is the commands that act on
  the whole set that do not see it — `up -d` and `pull` — and those want
  `--profile turn`, or `COMPOSE_PROFILES=turn` in `.env`, which says it once for
  all of them and is the form that makes a bare `docker compose up -d` do the
  right thing after an edit to this file.
- **Linux hosts that care about UDP throughput** can drop the port publishing
  and give the `sfu` service `network_mode: host`. The container then binds the
  host's ports directly, which removes docker's userland UDP proxy from the
  media path. Signalling then reaches the SFU on `127.0.0.1:7880`, so Caddy
  needs `host.docker.internal` or the host address as its upstream instead of
  `sfu:7880`.
- **Images are floating tags** (`rust:1-bookworm`, `debian:bookworm-slim`,
  `gcr.io/distroless/cc-debian12`, `caddy:2-alpine`). Pin them by digest for a
  deployment you intend to reproduce.
- **The image this stack builds is not the image a cluster installs**, and the
  difference is one line in each compose file: `target: shell`. CI builds two
  runtime stages on every push and proves both of them by starting the binaries
  and reading `/healthz`: `shell`, which is debian with a certificate bundle and
  `envsubst`, and `runtime`, which is distroless — glibc, a certificate bundle,
  the two binaries, and no shell, no package manager, no curl, nothing in it that
  can be talked into running anything. Today the `shell` stage is the one that
  gets the tag, because the Helm chart still starts an init container out of this
  image and that container is a shell script; the distroless stage is built and
  exercised anyway, so making it the published one is a one-line change to
  something already known to run. The `sfu` service here starts through `start-sfu.sh`, which
  refuses an advertised address no client can reach and builds the `ice_servers`
  block, and a shell script needs a shell — so `docker compose up --build` puts
  the same two binaries on debian instead. Everything else about the two is the
  same: one cargo build, one pair of binaries, the same uid 10001, the same
  directories. What it costs is that trying something out here does not prove the
  published image; what pays for it is that the published image is started, and
  asked for `/healthz` on both binaries, before it is pushed anywhere. What it
  buys the cluster is a container with nothing to exec and no writable
  filesystem to need.
- **The health of `sfu` and `front` is the binary's own answer**, not curl's:
  `test: ["CMD", "/usr/local/bin/mediacore", "healthcheck"]`. There is no curl in
  either image any more, and the subcommand reads the body as well as the status —
  it insists on this server's health line, where `curl -fsS` would have taken a
  200 from whatever else ended up on the port. To ask by hand, from the host:
  `docker compose exec sfu /usr/local/bin/mediacore healthcheck; echo $?`. Zero,
  and silent, is healthy; anything else prints what answered and why it was not
  believed.
- **Secrets.** `.env` is gitignored here. It holds the token-signing secret;
  anyone with it can mint a token for any room and any identity. The `caddy` and
  `turn` services deliberately do not read `.env` — each is given the variables
  it uses and nothing else, so the signing secret is never in the environment of
  either, and both are images this repository does not build. `sfu` and `front`
  do read the file, because both mint or verify those tokens and because between
  them they read most of it: the OIDC pair, the branding, `HTTPS_PROXY` and
  `NO_PROXY`. Naming their variables one by one instead would mean a `.env` line
  that silently does nothing the day somebody adds one.
- **The browser SDK is vendored**, not fetched. It lives in
  `crates/front/assets/vendor/`, is compiled into `mediacore-front` and is
  served from this origin under a version-stamped path, so there is no CDN in
  the runtime path and no third-party script for the content security policy to
  allow. See that directory's README to move to another version. swagger-ui,
  under `crates/server/assets/vendor/`, is vendored into the SFU on the same
  terms and for the same reasons.
- **What this is not.** One node, no recording switched on, no relay unless you
  start one, and — unless you switch
  signing in on — a room that anyone with the URL can join under any name. There
  *is* a room management API —
  `livekit.RoomService`, documented at `/swagger` — but it needs a token signed
  with `MEDIACORE_API_SECRET`, which the front end never hands out. The token
  endpoint is unauthenticated by default, which is why it is confined to
  `MEDIACORE_FRONT_ROOM` and the namespace below it: put the front end on an SFU
  that is also serving something real and it still cannot reach that something's
  rooms. That confinement holds with signing in switched on too — a session says
  who somebody is, not which rooms exist. It is a stack you can put in front of people, not a service to run a
  business on.
