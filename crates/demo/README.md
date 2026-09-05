# mediacore-demo

A single-page audio room. Someone opens the URL, types a name, and hears everyone else in the
room. The binary serves the page and mints access tokens; it never carries media and never talks
to the SFU itself.

```
mediacore-demo
```

## Environment

| Variable | Required | Default | Meaning |
|---|---|---|---|
| `MEDIACORE_API_KEY` | yes | | API key, becomes `iss` in the token. Must match a key in the SFU's `keys:` map. |
| `MEDIACORE_API_SECRET` | yes | | Shared secret the token is signed with. At least 32 characters. Never leaves the process. |
| `MEDIACORE_DEMO_WS_URL` | yes | | What the browser is told to connect to, e.g. `wss://rooms.example.com`. `ws://` or `wss://` followed by a bare host; a path, a userinfo or a second scheme is refused, because those are what a `MEDIACORE_PUBLIC_HOST` holding a URL turns into. |
| `MEDIACORE_DEMO_ROOM` | no | `demo` | The room everyone lands in, and the namespace every other room hangs off. Letters, digits, `-`, `_`, `.`. |
| `MEDIACORE_DEMO_PORT` | no | `8080` | TCP port for the page and the token endpoint. |

Anything required and missing, a secret that is too short, a URL that is not a WebSocket URL, a
port that is not a number and a room name with a space in it are all refused at startup with the
name of the variable in the message. The process does not start half-configured. `Config`'s
`Debug` is written by hand and renders `api_secret` as `<redacted>`, so the secret cannot reach a
log through a stray `{config:?}`.

`RUST_LOG` is honoured; without it the crate logs at `info`.

## Endpoints

| Route | Purpose |
|---|---|
| `GET /` | the page |
| `GET /app.css`, `GET /app.js` | the page's assets, compiled into the binary |
| `GET /vendor/livekit-client-<version>.esm.js` | the browser SDK, also compiled into the binary |
| `GET /api/config` | the default room and the public WebSocket URL, for the lobby |
| `POST /api/token` | `{"name": "...", "room": "..."}` in, `{"url","token","room","identity","name"}` out |
| `GET /healthz` | `ok` |

A refused request answers `400` with `{"error": "..."}`; that string is what the visitor is shown.

Every response carries `Content-Security-Policy: default-src 'none'; script-src 'self'; …
connect-src 'self' <MEDIACORE_DEMO_WS_URL>`, plus `X-Content-Type-Options: nosniff` and
`Referrer-Policy: no-referrer`. The policy is built from the configured WebSocket URL at startup,
so the one cross-origin connection the page is allowed is the signalling socket and nothing else.

## The token endpoint can only reach its own rooms

`POST /api/token` mints a join token for anyone who asks, without a password, a session or a
rate limit — that is what makes the demo a demo. It therefore must not be able to address a room
outside its own namespace, or putting it on an SFU that is also serving something real would hand
every visitor a token for that something.

So a `room` in the request body is **namespaced, not honoured**: it is checked for charset and
length and then prefixed with `MEDIACORE_DEMO_ROOM` and a `.`, and the composed name is length-
checked again. `?room=standup` with the default configuration reaches `demo.standup`. Nothing a
caller can put in that field escapes the `demo.` prefix — `.` and `-` are the only punctuation the
charset allows, and neither can walk up a namespace.

Always issuing the configured room would have been simpler, and was rejected: a shareable link to a
side room is most of what makes the demo usable in front of a group, and it costs one prefix to
keep it safe. The response's `room` field carries the composed name, so the page shows people the
room they are actually in.

## Nothing is fetched at runtime

`livekit-client` is vendored under `assets/vendor/` and compiled into the binary like the page
itself. Nothing in a running deployment reaches out to a CDN, which means a host with no route to
the internet works, the content security policy above can forbid third-party scripts outright, and
no third party can change what the browser executes. The route carries the SDK version and is
served `immutable`, so upgrading it can never serve a stale cached copy. `assets/vendor/README.md`
says how to move to another version.

## Deployment

The page must be served over TLS: a browser hands out the microphone only in a secure context, and
a TLS page can only open `wss`. Put the demo and the SFU's `/rtc*` behind one TLS origin, so the
certificate the visitor accepted covers both. `deploy/` in the repository root does exactly that,
and its README covers the case that actually bites — a host with a public IP and no domain name.
Media does not pass through the proxy; it is UDP straight to the host, and the SFU advertises that
address itself through `rtc.advertise_addresses`.

## Who is speaking

The SFU does not send `SpeakersChanged` yet, so `RoomEvent.ActiveSpeakersChanged` never fires and a
speaking indicator driven by it would stay dark for ever. The page measures levels itself instead:
one `AnalyserNode` per participant, fed from the track's own `MediaStreamTrack`, sampled every
150 ms, RMS over the threshold marks that participant as speaking and holds it for half a second so
the dot does not flicker between syllables. Local and remote are measured the same way, which also
means the indicator keeps working unchanged once the server does start reporting speakers.

## Two decisions worth stating

**The form is dead until the module has loaded.** The name field and the Join button ship
`disabled` in the HTML and are enabled only after the SDK module resolves. Otherwise a click during
that window called `lk.createLocalAudioTrack` on `null`, and the resulting `TypeError` was reported
to the visitor as a microphone problem — sending them to the browser's permission settings to fix
something that had nothing to do with permissions.

**The AudioContext is created inside the click.** It used to be created in `watchLevel`, several
awaits after the submit — a permission prompt and a WebSocket connect later — where the user
activation that lets a context start has long expired. It would then sit `suspended` for the rest
of the session and no speaking indicator ever lit. It is now created and resumed synchronously in
the submit handler.

**A dropped signalling socket says so.** `livekit-client` reports
`ConnectionState.SignalReconnecting` when the signalling socket dies while the peer connection is
still up. This server has no session resume, so that reconnection cannot succeed; the page says
that plainly rather than leaving the pill on "live" while nothing can reach the server.

**Colliding names are disambiguated in the list.** The server keeps two people called Alice apart
by identity, but the list showed both as "Alice". When a display name appears more than once, each
one is suffixed with the last four characters of its identity — `Alice (a1b2)` — and a name that
appears once is left alone.

**One page, not a redirect to `/room`.** The name never enters a URL or the browser history, and
more importantly the click that submits it is a user activation: `getUserMedia` and the playback
of remote audio both happen inside that same gesture. After a redirect the room page loads with no
activation at all, and Safari in particular then refuses to start audio until something else is
clicked. `?room=<name>` still picks the room, so a link to a specific room is a link people can share.

**Identities are `<slug>-<8 hex>`.** The token's `sub` is the participant identity and the SFU
displaces an existing participant when the same identity joins again, so two people typing "Alice"
would otherwise kick each other out in turn. The name is slugged to ASCII (`Ann-Marie` becomes
`ann-marie`, a name with no ASCII letters becomes `guest`) and a random suffix is appended. The
typed name is carried separately in the `name` claim, so both people still show up as "Alice".
