# mediacore-front

A single-page room. Someone opens `/r/<room>`, types a name, and hears everyone else in that room.
A room is an address: the URL is what you paste to a colleague, and reloading it comes back to the
same room rather than to the front page.
Audio only by default; from inside the room you can choose the microphone, choose which noise
suppressor runs -- ours, the browser's, or none -- change the rest of the browser's audio
processing, share a screen, and tick one box in the lobby to publish the camera with simulcast.
Calls start on our own denoiser, and every one of those choices is remembered by the browser that
made it. Behind the **Bench** button in the control bar is the bandwidth bench — per remote
track, which layer is arriving and what it costs, next to the browser's own view of the link. The
binary serves the page and mints access tokens; it never carries media and never talks to the SFU
itself. Point it at an OpenID Connect provider and it stops asking for a name: visitors are sent
there, come back signed in, and type only the room — see "Signing people in" below. That is a
switch, and it is off unless an issuer is configured.

```
mediacore-front
```

## Environment

| Variable | Required | Default | Meaning |
|---|---|---|---|
| `MEDIACORE_API_KEY` | yes | | API key, becomes `iss` in the token. Must match a key in the SFU's `keys:` map. |
| `MEDIACORE_API_SECRET` | yes | | Shared secret the token is signed with. At least 32 characters. Never leaves the process. |
| `MEDIACORE_FRONT_WS_URL` | yes | | What the browser is told to connect to, e.g. `wss://rooms.example.com`. `ws://` or `wss://` followed by a bare host; a path, a userinfo or a second scheme is refused, because those are what a `MEDIACORE_PUBLIC_HOST` holding a URL turns into. |
| `MEDIACORE_FRONT_ROOM` | no | `demo` | The room `/` offers, and the namespace every other room hangs off. `/r/standup` reaches `demo.standup`; `/` and `/r/demo` are the same room. Letters, digits, `-`, `_`, `.`. |
| `MEDIACORE_FRONT_PORT` | no | `8080` | TCP port for the page and the token endpoint. |
| `MEDIACORE_FRONT_OIDC_ISSUER` | no | | **The switch.** Set it and the page signs people in against that provider; leave it unset and nothing below is read, no sign-in route exists, and the page is exactly what it was. An `https://` URL, a realm path allowed, no query. |
| `MEDIACORE_FRONT_OIDC_CLIENT_ID` | with the issuer | | The client registered with the provider. |
| `MEDIACORE_FRONT_OIDC_CLIENT_SECRET` | with the issuer | | The client secret. Read from the environment, never written anywhere, and rendered `<redacted>` by `Debug` exactly as `MEDIACORE_API_SECRET` is. No minimum length: the provider chose it, not this process. |
| `MEDIACORE_FRONT_PUBLIC_URL` | with the issuer | | The origin a browser reaches this front end at, e.g. `https://10.251.10.201:5400`. An origin: no path, no query. The redirect URI (`<this>/auth/callback`) and the post-logout URI (`<this>/`) are derived from it, and both have to match what is registered with the provider to the character. |
| `MEDIACORE_FRONT_OIDC_SCOPES` | no | `openid profile email` | What to ask the provider for. `offline_access` is deliberately not in the default: nothing here ever spends a refresh token. |
| `MEDIACORE_FRONT_SESSION_TTL` | no | `43200` | How long a session lasts, in seconds, between 60 and 30 days. |

Anything required and missing, a secret that is too short, a URL that is not a WebSocket URL, a
port that is not a number and a room name with a space in it are all refused at startup with the
name of the variable in the message. The process does not start half-configured, and that includes
the sign-in: an issuer with no client id beside it is refused by name rather than starting a process
that serves the page, redirects nobody and fails at the first press of Join. `Config`'s `Debug` is
written by hand and renders both `api_secret` and `oidc.client_secret` as `<redacted>`, so neither
can reach a log through a stray `{config:?}`.

`RUST_LOG` is honoured; without it the crate logs at `info`.

## Endpoints

| Route | Purpose |
|---|---|
| `GET /` | the page, offering the configured room -- or, with signing in switched on and no session, a `302` to the provider |
| `GET /r/<room>` | the same page, addressed to a room -- for every room name, including one that cannot be a room |
| `GET /app.css`, `GET /app.js` | the page's assets, compiled into the binary |
| `GET /media.js` | the capture side: microphone, processing, device choice, screen share; no DOM |
| `GET /bench.js` | reads the browser's WebRTC statistics; no DOM |
| `GET /readout.js` | draws the video tiles and the numbers; no statistics |
| `GET /prefs.js` | what this browser remembers between visits; no DOM, no storage of its own |
| `GET /room.js` | reading and writing a room's address; no DOM, no opinion about what a room name may contain |
| `GET /denoise.js` | our denoiser: the support probe, the audio graph, the overload watch |
| `GET /denoise-probe.js` | the two AudioWorklet processors that measure the node; loaded by `addModule`, not imported |
| `GET /denoise-gate.js` | wraps the vendored processor so it says when its wasm is live; loaded by `addModule` *before* it |
| `GET /NOTICE.txt` | the third-party licences of everything under `assets/vendor/`; linked from the page |
| `GET /vendor/livekit-client-<version>.esm.js` | the browser SDK, also compiled into the binary |
| `GET /vendor/rnnoise-<version>.wasm`, `-simd-<version>.wasm` | RNNoise, `application/wasm`, `immutable` |
| `GET /vendor/rnnoise-worklet-<version>.js` | the worklet that drives it |
| `GET /api/config` | the lobby's configuration; `?room=<name>` asks whether that name can be a room, and the answer is `roomError`. Also `signIn` and `signedInAs` |
| `POST /api/token` | `{"name": "...", "room": "..."}` in, `{"url","token","room","identity","name"}` out. With signing in on, `name` is ignored and a session is required |
| `GET /auth/callback` | where the provider comes back to. **Only registered when the switch is on**; a 404 otherwise |
| `GET /auth/logout` | ends the session here and at the provider. `?returnUrl=` is validated against `MEDIACORE_FRONT_PUBLIC_URL` |
| `GET /healthz` | `ok` |

A refused request answers `400` with `{"error": "..."}`; that string is what the visitor is shown.

Every response carries `Content-Security-Policy: default-src 'none'; script-src 'self'
'wasm-unsafe-eval'; … connect-src 'self' <MEDIACORE_FRONT_WS_URL>`, plus
`X-Content-Type-Options: nosniff` and `Referrer-Policy: no-referrer`. The policy is built from the
configured WebSocket URL at startup, so the one cross-origin connection the page is allowed is the
signalling socket and nothing else.

`'wasm-unsafe-eval'` is there for the denoiser and buys exactly one thing: WebAssembly compilation.
It is not `'unsafe-eval'`, it does not allow inline script, and it does not widen where a script may
come from -- that is still `'self'` and nothing else. Chrome refuses `WebAssembly.instantiate`
outright without it, so the alternative was not a tighter policy but a dead feature.
`the_policy_buys_wasm_and_nothing_else` in `src/http.rs` fails if anything else is ever added
alongside it.

## A room is an address

`/r/standup` is where a room lives. Opening it goes to that room, reloading it comes back to the
same room, and it is the link somebody pastes into a chat window -- so it carries the room name and
nothing else. No token, no ticket, no display name: a token is minted per load and lives in memory
for the length of the connection, and a URL is written down and forwarded.

Four decisions, and the reasons they went that way.

**`/r/<room>` shows the lobby with the room filled in; it does not join.** Two reasons, and either
would be enough. A browser will not hand over a microphone without a gesture, and the click that
submits the lobby is that gesture -- it is what lets the `AudioContext` start and what Safari in
particular wants before it will play the room's audio, and after a load with no activation at all
there is nothing to spend. And arriving in a call with a live microphone without having pressed
anything is its own failure, the one that ends in "I did not know I was in". So a link costs one
press of Join, and the lobby knows which room it is for.

**The address bar catches up at the moment there is a room to catch up with.** Joining rewrites the
URL to the room that was actually joined -- with `replaceState`, not `pushState`, because Back would
otherwise leave the URL naming the lobby while the page is still in the room, and nothing here
listens for `popstate`. Leaving keeps it: the lobby is offering that room, F5 reaches the same
lobby, and rejoining costs the press it should. A room somebody is still typing is not an address
and does not get one.

**The name in the roster header and the tab title is the name a person says.** The tab is
`standup · mediacore`, room first, because four tabs of the same call are four identical entries in
a tab strip and the first two centimetres are all a squeezed tab shows. The header carries
`standup`, which is what the URL says and what a person would repeat out loud; the composed room
the SFU knows, `demo.standup`, is in the footer beside the socket and the identity, for whoever is
reading a room list at the same time. Both are derived from the room the token came back naming
rather than from what was typed, so the page can never be titled after a room it is not in.

**A room name that cannot be one is refused in the lobby, with the reason, before anything is
joined.** `/r/not%20a%20room` serves the page like any other name -- a 404 would answer a mangled
link with a dead end, and somebody who was *sent* that link cannot edit it. The page reads the room
out of its own URL, asks `/api/config?room=...` about it on the request it was making anyway, and
prints the answer: *a room name may only contain letters, digits, `-`, `_` and `.`*. The name stays
in the field, because that is what has to be corrected, and the refusal clears the moment somebody
starts correcting it. There is no second notion of a valid room name in the browser: `clean_room`
in `src/name.rs` is the only one, and the page asks it rather than keeping an opinion that would
drift from it the first time either moved.

`?room=<name>` was how a room was named before rooms had URLs, and links carrying it are in
people's chat histories. It still works and is corrected on arrival: `/?room=standup&video=1`
becomes `/r/standup?video=1`, keeping the parameters that are instructions for this visit.

## The token endpoint can only reach its own rooms

`POST /api/token` mints a join token for anyone who asks, without a password, a session or a
rate limit — there is no account system here, and the link to a room is the whole of the
invitation. It therefore must not be able to address a room outside its own namespace, or putting
it on an SFU that is also serving something real would hand every visitor a token for that
something.

So a `room` in the request body is **namespaced, not honoured**: it is checked for charset and
length and then prefixed with `MEDIACORE_FRONT_ROOM` and a `.`, and the composed name is length-
checked again. `/r/standup` with the default configuration reaches `demo.standup`. Nothing a caller
can put in that field escapes the `demo.` prefix — `.` and `-` are the only punctuation the charset
allows, and neither can walk up a namespace.

The one name that is not composed is the namespace itself: `demo` reaches `demo`, not `demo.demo`.
That is not a convenience, it is what makes the address work. `/` and `/r/demo` name the room
everyone lands in, and a reload has to come back to the room the first load joined; composing there
would put the second one somewhere else and split the room in two. It widens nothing: `demo` is the
room a body with no `room` field has always been given, so the set a caller can reach is what it
always was — this room, or one under it, and no third thing. `room_for` in `src/name.rs` is that
rule, and `nothing_a_caller_can_ask_for_reaches_another_namespace` states it as the property rather
than as a composition.

Always issuing the configured room would have been simpler, and was rejected: a shareable link to a
side room is most of what makes the page usable in front of a group, and it costs one prefix to
keep it safe. The response's `room` field carries the composed name, so the page shows people the
room they are actually in.

## Signing people in

Off by default, and off is not a degraded mode: with `MEDIACORE_FRONT_OIDC_ISSUER` unset there is no
provider, no session store worth the name, no `/auth/*` route to find, and the lobby asks for a name
the way it always has. That is deliberate. The stand this was built for has to stay quick to check,
and a self-signed certificate on a callback URL makes every round trip a chore.

With it set, `/` and `/r/<room>` answer a browser with no session with a `302` to the provider, and
the page comes back with the room still in the address and the name already known.

### The round trip

```
  GET /r/standup                     no session, and a person arriving
    -> build the redirect first, then remember
       { return_to: "/r/standup", nonce, pkce verifier } under a random `state`
    -> Set-Cookie: __Host-mediacore_login=<secret>   (one per browser, reused)
    -> 302 <provider>/authorize?response_type=code&response_mode=query&client_id=…
             &redirect_uri=<public>/auth/callback&scope=…&state=…&nonce=…
             &code_challenge=S256(verifier)&code_challenge_method=S256

  GET /auth/callback?code=…&state=…
    -> the `state` names a login this process started
    -> the login cookie proves this is the browser the sign-in left from
    -> and only then is the login spent
    -> POST <provider>/token   (server to server, client_secret_post, code_verifier)
    -> validate the id token; ask userinfo for what it did not carry
    -> Set-Cookie: __Host-mediacore_session=<id>.<hmac>
    -> 302 /r/standup
```

Authorization code with PKCE `S256`, and nothing else. No implicit, no hybrid, no `response_type=token`,
whatever the provider says it also supports: the code comes back through the browser and is redeemed
from this process, so the credential worth stealing never touches a URL bar, a history entry or a
`Referer` header. `response_mode=query`, because the fragment is visible only to the page and the page
is exactly who should not see it.

**The room lives on this side of the wire, keyed by `state`.** Not in the redirect, not in a `next=`
parameter, not in a cookie: all three are the browser's to edit, and all three end as a redirect to
somewhere else wearing this host's name. What comes back carries `state` and nothing about where
anybody was going, so there is nothing to edit. The callback answers with a `302` rather than a page
that redirects itself, which means the address bar lands on the clean `/r/standup` and the back
button holds no entry with a `code` in it.

**The login cookie is not decoration.** Without it, anybody can hand you a working callback URL and
sign you quietly into *their* account, and then read whatever you type next. The cookie holds a
secret; the login holds its `sha256`; the callback compares them and refuses if it cannot.

**It is one cookie per browser, and the browser check comes before the spend.** Both of those were
the other way round and both were wrong. A cookie named after the `state` meant every signed-out page
load left a *different* one alive for ten minutes with nothing to clear it, so a browser made to
fetch this origin — `<img src="https://host/r/a">` on any page anywhere — collected them by the
hundred until its jar started evicting cookies that mattered. One name costs nothing: what the cookie
says is *which browser*, and two tabs signing in at once are the same browser, so they share it and
each keep their own `state`, nonce and room. It is cleared by the callback only once no other sign-in
in that browser is still relying on it. And spending the login before checking the cookie meant that
anybody who read a `state` — it is in the address bar while the provider's page is open, and in the
provider's access log afterwards — could end that sign-in with one fetch, holding nothing. A callback
that fails the browser check has redeemed no code, so it now leaves the login exactly where it found
it, and sets no cookie on the way out either.

### A flood of page loads cannot lock anybody out

Starting a sign-in costs an unauthenticated `GET /`, so the map of sign-ins in flight is capped at
4096 — and what happens *at* the cap is the whole of it. Dropping the oldest to make room dropped the
person who had been waiting longest at the provider, which is the person still typing a password;
four thousand requests take under a second from one host, so anybody could end any sign-in in flight,
and a sustained stream made signing in impossible for everyone. A full map now refuses the newcomer
instead, with **503** and a sentence saying to try again in a minute. A newcomer failing during a
flood is survivable; somebody thrown out halfway through is not.

Two things keep the map from filling in the first place. A request that is not a person arriving is
answered `401` and starts nothing: `Sec-Fetch-Dest` saying `image`, `iframe` or `empty`, a prefetch,
or a `HEAD` — which axum answers from the same handler and which is always a monitor, a crawler or a
link unfurler rather than somebody about to type a password. So the shape that needs no script at
all, an `<img src="https://host/r/a">` on a page somebody only had to look at, costs neither an entry
nor a cookie. A browser that sends no `Sec-Fetch-*` is otherwise trusted, the same way signing out
trusts one that sends no `Sec-Fetch-Site`: refusing it would break signing in for old browsers rather
than protect anything, and a scripted flood can set any header it likes anyway. And the redirect to
the provider is built *before* anything is remembered, so a provider having a bad ten minutes does
not leave the map full of sign-ins that never left this process.

`a_flood_of_anonymous_page_loads_does_not_lock_anybody_out` in `tests/oidc.rs` is the whole sequence:
somebody at the provider, four thousand page loads from somebody with no account, and the first
person's callback still landing on `/r/standup`.

### Nothing from outside writes a line of the log

A `tracing` field written with `%` goes into the line exactly as it came, so a value carrying a
newline writes *two* lines and the second one reads as one this binary wrote. It needed no account:
`GET /` for a `state`, then `/auth/callback?state=…&error=%0a…`, and the log grew a convincing
`signed somebody in subject=admin`. Everything from outside — a query parameter, a header, a `kid`, an
error carrying a string a provider chose — now goes through `quoted` in `src/log.rs` on its way to a
field: quoted so the reader can see where somebody else's words start and stop, escaped for anything
that starts a line or reorders one, and cut at 200 characters with a `…` so a megabyte of `?error=`
costs the log a line rather than a megabyte. Printable text outside ASCII is left alone, because a
display name is `Роман` far more often than it is an attack.

### The ID token is where implementations get this wrong

An ID token is a signed statement about who somebody is. A validator that skips a check turns it
into an unsigned one, so every check has a test in `tests/oidc.rs` that breaks exactly that check and
watches the sign-in be refused:

| check | what it stops |
|---|---|
| signature, against the provider's JWKS | a token anybody minted |
| **`alg` is `RS256`, decided here and never read off the token** | the classic forgery: name `HS256`, and get a validator to verify the provider's *public* key — which anybody can fetch — as an HMAC secret. `alg: none` goes the same way |
| the key is chosen by `kid` | a token signed with a key that is in the set for something else |
| `iss` | tokens from any other provider |
| `aud`, plus `azp` whenever the token carries one | a real token minted for another client on the same provider (OpenID Connect 3.1.3.7, both steps: `azp` is verified when it is present, not only when there are several audiences) |
| `exp`, with a minute of leeway | a token from last week |
| `nonce`, against the one this login issued, compared whole | a real token from another sign-in, replayed into this one |

The JWKS is cached for an hour. A token naming a `kid` that is not in it triggers **one** refetch, and
not one per token: a stream of invented `kid`s would otherwise be a way to make this server hammer the
provider. A refetch that fails keeps the keys already held — a provider having a bad minute should not
stop people signing in with keys that have not changed. The discovery document is treated the same way,
and its `issuer` must equal the configured one (RFC 8414 §3.3), which is also what catches a trailing
slash or an `http`/`https` mix-up in configuration. Every endpoint the document names has to be an
`http` or `https` URL, **including the two optional ones** — `userinfo_endpoint` and
`end_session_endpoint` are the ones that produce an outbound request carrying an access token and a
`Location` a browser follows. One door described as `javascript:` refuses the whole document rather
than being quietly dropped: it is either broken or hostile, and neither is a document to take the
other four values from.

`userinfo` is asked afterwards for anything the ID token did not carry — plenty of providers put only
`sub` in the token, and a roster of UUIDs is not a room. It is allowed to fail: the signed token
already said who this is. What it is not allowed to do is disagree, and a `sub` that does not match
refuses the whole sign-in (OpenID Connect 5.3.2).

### Identity is `sub`, the name is `name`

The LiveKit identity every token carries is the provider's `sub`. Display name is `name`, else
`preferred_username`, else `email`, else the `sub` — because a roster entry with no text at all is
worse than an ugly one. **Never the email as the identity**: an address changes hands and a `sub` does
not, and an identity is what a room is keyed by.

The `sub` also travels a second time, as a `subject` attribute on the join token. mediacore keeps a
participant's attributes on the attendance row of the meeting record and reads none of them — that is
where a platform built on it puts its own idea of who somebody is, and this front end is one such
platform. A guest who typed a name into the box has no `sub` and sends no attribute.

Whatever the directory returns goes through the same `clean_name` a typed name does and is cut to the
same 40 characters. A provider is not a source of short printable strings, and that name goes into a
token and onto everybody's screen.

One consequence worth knowing: a LiveKit identity is unique within a room, so **one person joining the
same room twice replaces their own first participant** rather than sitting beside it. That is the right
answer for a call — the alternative is somebody's abandoned tab in the roster with a live microphone in
it — but it is a change from the anonymous path, where two people typing `Alice` get two seats.

### The session, and the cookie

`__Host-mediacore_session=<id>.<hmac>`, `HttpOnly`, `Secure`, `SameSite=Lax`, `Path=/`. The browser
holds an opaque id and nothing else — not the `sub`, not the roles, not the ID token — and everything
about a person stays in this process, keyed by 256 bits nobody can guess. The signature is HMAC-SHA256
over the id with a key made at startup, checked in constant time, so a garbled or invented cookie is
refused without a lookup. The page's JavaScript has no use for the value and `HttpOnly` means it could
not read it if it did.

`__Host-` is why the front end has to be reached over TLS with the switch on: a browser refuses the
prefix without `Secure`, and refuses `Secure` outside a secure context. `http://localhost` counts as
one, so local development works; a plain-http LAN address does not, and no cookie will be set at all.

`SameSite=Lax` rather than `Strict`: arriving from the provider's redirect is a top-level cross-site
navigation, and `Strict` would drop the cookie exactly there. `Lax` still means a cross-site `POST`
carries no session, so nothing on another origin can mint a token as anybody.

**The store is memory, and a restart signs everybody out.** One process, one stand: there is no second
replica for a login to be finished on, and a session that survived a restart would have to be written
somewhere this crate has nowhere to write. It costs a person one press of the provider's
already-signed-in page. It costs nobody in a call anything at all — see below.

### The token endpoint stops being anonymous

With the switch on, `POST /api/token` needs a session and **reads the `name` field and discards it**.
That is the entire point of having signed anybody in: a browser that can still name itself here is a
browser that can join as somebody else. The name and the identity both come out of the session; the
room is the one thing the body is still believed about, and it is namespaced exactly as before. No
session is `401` with a sentence the lobby prints.

With the switch off it behaves precisely as it did: a name on trust, no session, no cookie.

Either way the response carrying the token is sent `Cache-Control: no-store`, as is `/api/config`
once it starts saying who is signed in. No cache stores a `POST` response unasked, so this is a
header nothing currently relies on — which is the reason to set it rather than to leave it out, since
the rule that makes it safe lives in somebody else's software and a proxy in front is somebody else's
software.

### Signing out

`GET /auth/logout` closes the session here and then sends the browser to the provider's
`end_session_endpoint` with `id_token_hint`, because ending only this side leaves the provider's own
session standing and the next press of Join signs the same person back in without a prompt — which
reads as the button not working.

The `post_logout_redirect_uri` handed to the provider is always `<MEDIACORE_FRONT_PUBLIC_URL>/`,
because that is the value registered with it and a provider refuses any other. `?returnUrl=` is
therefore for the local half — where somebody lands when the provider publishes no
`end_session_endpoint` or cannot be reached — and it is validated against this front end's own origin
first: a redirect that forwards anywhere is a phishing link wearing this host's name.

A cross-site request to it is refused (`Sec-Fetch-Site`), so another page cannot sign somebody out with
an `<img>` tag. A browser that sends no such header is trusted, because refusing it would break signing
out rather than protect it.

### What happens when a session ends mid-call

**Nothing.** The LiveKit token was minted at the moment of joining, carries its own six-hour lifetime,
and is the SFU's to judge; the media connection is between the browser and the SFU and this process is
not in it. Nothing on the page re-checks a session while a call is running — `/api/config` is fetched
once at load and `/api/token` once at join — so an expiring session cannot drop anybody. It is felt at
the *next* page load, which redirects to the provider, and at the next press of Join, which answers
`401` with "this sign-in has ended. Reload the page to sign in again."
`a_session_that_ends_does_not_end_a_call` in `tests/oidc.rs` is that property.

### What happens when the provider is unreachable

Signing in stops; nothing else does. A page asked for with no session answers **503** with a sentence
saying the provider could not be reached and that nothing is wrong with the account — rather than a
blank page, a redirect loop, or a lobby that would fail at Join. A callback that cannot redeem its code
answers the same way. Sessions already open keep working, because nothing about them needs the
provider; calls already running are untouched for the reason above. Every request out has a ten-second
timeout, and a timeout, a refused connection, an untrusted certificate and a name that does not resolve
are all one answer to whoever is looking at the page — the distinction is in the log.

If the host reaches the outside through a proxy, `HTTPS_PROXY` and `NO_PROXY` are honoured.

### The gap: roles are read and not enforced

The provider's `roles` claim is read into the session and **nothing consults it**. With the switch on,
anybody the provider will sign in can enter any room whose name they know — signing in says who
somebody is, and this front end has no rule about who may be where.

That gap is deliberate rather than overlooked. There is no model here of which rooms exist or who owns
one, and inventing a rule — "a room named after a role needs that role" — would be a guess that is
worse than the gap, because it would look like an answer. When there is a real rule it goes next to
`room_for` in `src/name.rs`, which is already the one place that decides which room a request reaches.

## Nothing is fetched at runtime

`livekit-client` and the RNNoise build are vendored under `assets/vendor/` and compiled into the
binary like the page itself. Nothing in a running deployment reaches out to a CDN, which means a
host with no route to the internet works, the content security policy above can forbid third-party
scripts outright, and no third party can change what the browser executes. Every one of those
routes carries the version of what it serves and is sent `immutable`, so upgrading can never serve
a stale cached copy. `assets/vendor/README.md` says how to move to another version, and records the
size and sha256 of each file so what is in the tree can be checked against what was published.

### What the denoiser costs the page

Bytes on the wire, `wc -c` on the files this binary serves. Nothing here is compressed: the router
sets no encoding, so these are what a browser downloads.

| Route | Bytes | When |
|---|---:|---|
| `/vendor/rnnoise-simd-0.4.0.wasm` | 157 234 | **every load** |
| `/denoise.js` | 24 434 | **every load** |
| `/vendor/rnnoise-worklet-0.4.0.js` | 64 389 | when the node goes into a call |
| `/denoise-probe.js` | 11 372 | when the node goes into a call |
| `/denoise-gate.js` | 3 928 | when the node goes into a call |
| **every load** | **181 668** | |
| **once the node goes in** | **261 357** | |

None of it is conditional in the way it reads. The wasm is fetched on **every** page load, whether
or not anybody ever joins, because the gate that decides whether ours can run at all validates the
binary rather than assuming it, and a gate that assumed would be the dishonest kind. The other
three are fetched when the node is built — which, now that ours is the default, is every call
whose microphone opens on a machine that can run it. The plain build (152 656 bytes) is fetched
instead of the SIMD one on a browser without SIMD; never both.

For comparison the page already carries `/vendor/livekit-client-2.22.2.esm.js` at 1 365 927 bytes.
Against that, the denoiser adds about an eighth on a page load that goes no further, and about a
fifth on an ordinary call. The "about 150 KB" this file used to quote was the wasm alone, and it
called the rest conditional: a call that runs the denoiser fetches 261 357 bytes, seventy per cent
more than that, and 181 668 of them are fetched by a browser that only ever looks at the lobby.

`the_page_weight_in_the_readme_is_the_page_weight` in `src/http.rs` fails if any of those numbers
stops matching the bytes this binary serves.

The weights are inside the `.wasm`. There is no model file to fetch, from us or from anyone.

### Licences

Everything under `assets/vendor/` is compiled into the binary, and so is
`assets/vendor/NOTICE.txt`, which is served at `/NOTICE.txt` and linked from the page footer. Four
projects, four licences: `livekit-client` (Apache-2.0), `@sapphi-red/web-noise-suppressor` (MIT,
the AudioWorklet wrapper), `@shiguredo/rnnoise-wasm` (Apache-2.0, the emscripten build whose bytes
are actually shipped) and `xiph/rnnoise` (BSD-3-Clause, the algorithm and the weights). Apache-2.0
section 4(d) and the MIT permission notice both require that notice to travel with the thing; a
file sitting in the repository does not. `cargo test -p mediacore-front` fails if the notice stops
being compiled in, stops being served, stops naming any of the four, or stops carrying the licence
texts.


## Query parameters

| Parameter | Default | Effect |
|---|---|---|
| `?room=<name>` | the configured room | the old way of naming a room, still honoured and rewritten to `/r/<name>` on arrival; see "A room is an address" above |
| `?video=1` | off | ticks the camera box in the lobby, so joining publishes the camera as well as the microphone; `?video=0` unticks it, and either one outranks what the box was left at last time |
| `?res=180\|360\|720` | `720` | capture resolution for the camera |
| `?simulcast=0` | simulcast on | publishes a single encoding instead of a ladder |
| `?bench=1` | off | opens the bandwidth bench straight away, instead of waiting for the Bench button |

`?res` decides how many rungs there are, and the rule is `livekit-client`'s, not ours: three
encodings (`q` 320×180, `h` 640×360, `f` 1280×720) only when the capture is at least 960 px on its
long side. `?res=360` publishes two rungs, `?res=180` publishes one. Run the bench at `?res=720`
unless you are deliberately testing a shorter ladder, and use the same `?res` for every publisher
in a run or the numbers are not comparable between them.

## What the token grants

Every token this server mints carries the same four publish sources: `microphone`, `camera`,
`screen_share` and `screen_share_audio`. The SFU enforces that list, so a source missing from it is
a `NotAllowed` refusal at publish time rather than a client-side check.

`camera` is granted so the camera button works after joining audio-only — the case worth testing,
because it is the one where the SFU has to discover bandwidth it never had a reason to measure.
`screen_share` and `screen_share_audio` are two sources for one feature: the video of the screen and
the tab or system sound the browser hands over with it. Nothing else is granted; `can_publish_data`
stays off, and the list lives in one place, `PUBLISH_SOURCES` in `src/http.rs`.

## The capture module

`assets/media.js` owns everything the page captures: the microphone, the browser's audio
processing, the choice of input device, and the screen share. It touches no DOM and imports nothing
of ours — the interface hands it the already-loaded `livekit-client` namespace, calls its methods,
and renders `state()`. The header of the file documents the API.

`echoCancellation` and `autoGainControl` are constraints passed to `getUserMedia`, so that
processing is the browser's own and happens before Opus encoding. The SFU forwards encoded packets
and never decodes them, which is why nothing here can be changed after the fact:

| Setting | What it does to the audio | What it costs |
|---|---|---|
| `echoCancellation` | subtracts what the speakers are playing from what the microphone hears | needs a reference of the output; can duck the near end during double-talk |
| `autoGainControl` | rides the level towards a target loudness | pumps the noise floor up between phrases |

Noise suppression is not on that list. It is one setting with three values — off, the browser's own,
or ours — and the section below says why.

What the browser actually applied is read back from `MediaStreamTrack.getSettings()` and reported as
unknown where the browser reports nothing.

Changing any of them, or switching input device, restarts the underlying `MediaStreamTrack` and
replaces it on the existing sender: the publication and every subscription survive, and no
renegotiation happens. While the microphone is muted the change is held instead — restarting a
muted track would re-open the device and light the browser's recording indicator — and applied on
unmute; `state().microphone.pendingWhileMuted` says so.

Device names are only revealed by the browser after the microphone has been granted once. Before
that the list is real but unnamed, and the module labels the entries `Microphone 1`, `Microphone 2`
with `named: false` and a note saying why. A `devicechange` that removes the chosen microphone falls
back to the system default and says so, rather than leaving a dead track.

The screen share is published as `screen_share`, never as a camera: the server keys its stream
tracker and its bandwidth policy off the source, and a screen holding still must not be read as a
layer that stopped arriving. It is published single-layer VP8, capped at 1080p15. Screen audio comes
only when the browser offers it — Chrome and Edge share tab or system sound only when the box in the
picker is ticked, Firefox and Safari not at all — and the module says the share is silent instead of
pretending otherwise. A picker closed without a choice and a blocked permission are the same
`NotAllowedError` in every browser, so the module reports the pair rather than guessing between
them, and a share stopped from the browser's own bar unpublishes and reports itself the same as one
stopped from our button.

## The denoiser

The browser's own noise suppression is a lightweight attenuator for stationary noise, and it sounds
like one. The page puts a real denoiser in front of the encoder instead: **RNNoise**, compiled to
WebAssembly, driven from an `AudioWorklet`. It is what a call starts on — `DEFAULT_NOISE_ENGINE` in
`assets/media.js` — and the browser's own is one press away in the same control, for anybody who
wants it back.

It runs in the browser and it has to. The SFU forwards encoded packets and never decodes them, so
denoising on the server would make it a transcoder — roughly a core per participant, a decode and
re-encode generation loss, and more delay than the node itself costs. Here it runs once, on the
machine that owns the microphone, before Opus ever sees the signal.

### One suppressor, not two

Two suppressors in series sound worse than either alone, so noise suppression is **one control with
three values** — `off`, the browser's own, ours — and never two switches. `noiseSuppression` and
`voiceIsolation` are what "the browser's own" means: they are derived from the choice and handed to
`getUserMedia`, and both are off whenever ours is running. Nothing in the interface offers them
separately, and `media.setProcessing()` refuses to be handed either.

**Echo cancellation is not part of the choice and stays on.** The canceller needs the render-side
reference in the capture clock domain, which a worklet in a Web Audio graph does not have. Chrome's
own capture chain puts its noise suppressor *after* its echo canceller, and our node goes in exactly
that slot: by the time a track reaches `createMediaStreamSource` the echo has already been
cancelled.

### How it goes in and out mid-call

Through `livekit-client`'s `TrackProcessor`, which is the same mechanism the device chooser already
relies on. `LocalTrack.mediaStreamTrack` returns the processor's `processedTrack` when one is
attached, so the publication is untouched, no subscriber is dropped and nothing renegotiates.
`restartTrack` re-initialises the attached processor against the new capture track, so a device
change carries the node across rather than rebuilding it.

Four things have to be true at the moment the engine changes, and they are one design rather than
four fixes.

**Nothing is published until the node is denoising.** The vendored processor compiles its
WebAssembly asynchronously from inside its own constructor, and until that finishes its render
callback returns without writing to the output at all. A worklet output block starts zero-filled, so
a `processedTrack` handed to the sender before then carries silence — for a few hundred milliseconds
if the compile succeeds, and *for ever* if it does not, with nothing to tell the two apart from
outside. `denoise-gate.js` goes into the worklet scope first and wraps `registerProcessor`, so the
class the vendor registers is subclassed on its way in: the subclass fills the output with `NaN`
before delegating and looks at it afterwards, which is a statement about what the node did with the
audio rather than about the vendor's internals. Until the first written block the output is cleared
to silence and `init` does not resolve; after it, the wrapper stops touching the audio. A node that
writes nothing for five seconds of real input reports that instead, and a failed attach leaves the
call on the browser's own suppressor.

**The engine field only ever names the suppressor that is running.** Turning ours on attaches the
node first, while the browser is still suppressing, and relaxes the constraints second: that leaves
a few hundred milliseconds with both running, which is merely worse audio, against no suppression at
all if the attach fails. But the constraint change is the moment ours *replaces* the browser's, and
until it lands both are in series — which is the exact failure this control exists to prevent. So
the engine is recorded only once that change has landed. If it does not, the node comes back out and
the field goes back to what it was. Going the other way, ours is stopped before the constraints
change, never after, so a dead worklet is never left holding the capture; if that change then fails,
ours is out and the browser's has not come back, and the field reads `off`, which is what the
capture is.

**Changing the suppressor does not re-open the microphone.** It is two constraints and nothing else,
so it goes through `applyConstraints` on the device already open. A full `restartTrack` is for a
change of input device, a track the browser ended, or a browser that refuses the constraint outright.
Toggling the engine used to re-open the capture on the way in and again on the way out — a level jump
and a gap in the audio, twice, for a change the device never needed to hear about. When a restart is
genuinely needed the constraints go on again explicitly afterwards, because `LocalTrack.restart`
passes the device and, when no device was named, asks `getUserMedia` for `audio: true` and drops
everything else — which is how a page that asked for one channel ends up publishing two.

**What the browser says it did is read off the capture device.** `LocalTrack.mediaStreamTrack` is
the *processor's* output whenever ours is attached, and a track out of a
`MediaStreamAudioDestinationNode` carries none of these flags in `getSettings()`. Reading that one
answers "not reported" exactly when the claim matters most, and could never contradict us. The page
reads `getSourceTrackSettings()`, which is the capture device.

One more, on the way out: the graph's sink is set to one channel. A `MediaStreamAudioDestinationNode`
is stereo by default and up-mixes a mono graph into it, so a page that asked `getUserMedia` for one
channel would publish two of the same thing.

`livekit-client` also refuses a processor on a local audio track that has no `AudioContext`, and it
only gives a track one when the track was made through a `LocalParticipant` or when it is published.
Every track this page opens is opened before the room exists, so the first join gets a context from
the publication and a second join — a fresh track, never published — gets none. The page lends the
track the context it already keeps for the level meters; without that, rejoining a room dropped the
denoiser and nobody was told.

While the microphone is muted both are held and applied on unmute, for the same reason every other
capture change is: restarting a muted track re-opens the device and lights the browser's recording
indicator behind somebody who believes they are muted.

### Degrading honestly

Ours being the default is only defensible because of the three gates below: it means a page that
loads on a machine nobody has tested puts a WebAssembly node in the capture path before anybody has
asked for anything, and every way that can go wrong has to end on the browser's own suppressor
without a person doing anything about it. The default decides which suppressor is *tried* first and
nothing else. It costs a page load nothing either — the wasm is fetched on every load whichever
engine is the default, because the first gate validates the binary rather than assuming it.

Three gates, and every one of them lands on the browser's own suppressor rather than on a broken
track or on silence.

1. **Before it is used at all.** The page probes once at load: Web Audio, `AudioWorklet`, WebAssembly,
   whether this browser will run a graph at 48 kHz — RNNoise is a 48 kHz model, and at any other
   rate it is not the thing we tested — and whether the binary can actually be fetched from this
   server and validated. The rate is part of that gate rather than a surprise at attach time, for
   the same reason as everything else in it: finding out once somebody has chosen the option is how
   an option that does nothing gets offered.

   If any of it fails, **the one-press control skips the value** — pressing it cycles through the
   two that work and can never land on something that does nothing — and **the menu keeps the row,
   disabled, with the reason written under it** rather than in a tooltip nobody on a touchscreen
   will see. Removing the row outright was rejected: a radio group that quietly loses an option
   tells you nothing, and the reason is the useful part.

   Since ours is also the default, that probe is what the **default itself** falls back from: the
   engine goes to the browser's the moment the answer comes back, before anybody has joined
   anything. A page naming ours as the engine over the same row greyed out underneath as not on
   offer here would be arguing with itself, and a remembered choice of ours on a machine that
   cannot have it lands in exactly the same place.
2. **While it runs.** The two worklets either side of the node measure it, twice a second.
3. **When it is losing.** Three triggers, and the node comes out on any of them: the processor is
   stopped, the capture restarts with the browser's suppression back on, and a notice says why.
   - **The render thread falling behind real time** — the audio it actually emitted against the
     wall clock it took, sustained above 125 % for three seconds. This is the one that works
     everywhere, because it is measured on the main thread, which always has a clock. On this
     machine the healthy ratio measured 0.999–1.001 across a run, so the threshold has a great deal
     of room.
   - **Half of real time spent inside the node** for three seconds, where the render thread has a
     clock to measure its own time with. Against the wall clock: see *real time used* below for why
     the audio-clock denominator cannot report this at all.
   - **A worklet that throws**, immediately, because a dead worklet passes silence.

   **It does not re-arm itself.** A suppressor that switched on and off mid-sentence would be worse
   than either state, so coming back is a decision a person makes; the menu goes on saying it
   happened.

### What it will and will not do

RNNoise is a 2018 model and shows it. It is a real step up on steady broadband and tonal noise —
fans, air conditioning, hiss, road noise — at full 48 kHz, so it does not band-limit the voice before
Opus, and it costs a fraction of one core.

**It delays the signal by 992 samples — 20.7 ms at 48 kHz.** That is the number that decides whether
a call feels responsive, and it is not RNNoise's data-sheet figure: the model's own algorithmic
latency is 10 ms, and the vendored AudioWorklet wrapper buffers another 512 samples on top of it to
turn 128-sample render quanta into 480-sample frames. Both are in what the room hears; only the
first is in the data sheet.

992 is measured, not derived. `cargo test -p mediacore-front` drives the vendored worklet and the
vendored `.wasm` this repository ships, through the same tap-and-probe chain the page uses, and
fails if the answer moves — see *measures what the vendored worklet and the vendored wasm really
add* in `tests/browser-modules.test.mjs`. The bench panel measures the same thing in the browser,
against whatever the machine is really doing, and reports it in the **added delay** cell. What the
browser adds either side of the node — between the capture track and the graph, and between the
graph and the sender — is not in that figure and no browser reports it.

For scale, ITU-T G.114 puts the one-way mouth-to-ear budget for a comfortable conversation at
150 ms, and the encoder and the network have to come out of the same budget. 20.7 ms is a real slice
of it, which is why it is on the panel rather than in a footnote.

It does **not** remove other people's voices. Nobody's colleague at the next desk, nobody's café,
nobody's television. That is background voice cancellation, RNNoise was never trained for it, and
neither is any other option that can be shipped without negotiating a licence. It also treats music
as noise, thins whispered or distant speech, and gives up in a heavily reverberant room. If the
complaint is "I can hear the person next to me", this does not fix it and nothing free does.

### Measuring it instead of claiming it

The bench has a **Microphone denoiser** panel, and every number in it is measured on the render
thread that runs the node, over the window the reading covers:

| Cell | What it is |
|---|---|
| engine | which suppressor this page put in the path |
| level in / level out | rms in dBFS of the signal either side of the node |
| change | the difference between them, which is the node's effect on this window |
| added delay | how far the node sets the signal back, in the running graph |
| cost per 10 ms | the node's own time per RNNoise frame |
| real time used | that cost against the wall clock, not against the audio clock |
| worst block | the most expensive single block in the window |
| keeping up | the audio the node emitted against the wall clock it took |
| fallback | whether the machine gave up, and why |

The cost is the wall clock between `mediacore/denoise-tap` returning and `mediacore/denoise-probe`
being entered inside the same render quantum. Those two processors sit either side of the denoiser
and nothing else does, so that gap is the denoiser's own time plus the render thread's dispatch
between two nodes. It is the same measurement the overload watch acts on.

The added delay is found by correlating the probe's newest block of output against the tap's recent
history and taking the lag with the strongest normalised peak. The search runs a few lags per render
quantum so it never costs a whole block, and it reports nothing at all — rather than a plausible
number — when there is not enough signal either side to line up. It is the one figure here that
cannot be reasoned out of the graph: the vendored worklet buffers internally and does not say by how
much.

**`real time used` divides by the wall clock on purpose.** The obvious denominator — the audio time
the measured quanta cover — cannot report an overload: it is the audio device's clock, it advances
at real time whatever the render thread is doing, and when the thread starts losing, that
denominator shrinks in step with the numerator and the fraction sits still. The same goes for
`keeping up`, which now counts the blocks the node actually emitted rather than reading the
context's frame counter, for exactly the same reason.

### The clock the render thread does not have

`performance` is not part of the AudioWorklet specification, and **Chrome does not expose it**
— measured in Chrome 148 on macOS: inside `AudioWorkletGlobalScope`, `typeof performance` is
`"undefined"`. `AudioContext.renderCapacity` does not exist in that build either. `Date.now` is
there, and it ticks in whole milliseconds.

So the report says which clock it had:

- **`performance`** — the cost is measured per block, and the worst-block figure means something.
- **`date`** — the report is marked coarse. The cost is the total number of whole milliseconds the
  node was found to be inside a block across the window; the tick boundary falls where it likes, so
  summed over hundreds of blocks that total estimates the real one rather than inventing it. The
  bench prefixes those cells with a tilde and the tooltip says what they are. A single block's cost
  and a worst-block figure cannot be recovered from a one-millisecond tick, so both are reported as
  absent — **not as a rounded-down zero**.
- **`none`** — no cost at all, and the cells say so.

The keeping-up figure needs none of this. It is the audio the node emitted, counted in the blocks it
was asked for, against the wall clock between two readings on the main thread, and it is what the
fallback watches first.

**A number that could not be measured is never filled in with a plausible one**, which is the same
rule the rest of this page runs on.

### What it measured here

One run on an Apple Silicon Mac, Chrome 148, SIMD build, against synthetic low-frequency noise fed
through the real graph — not a data sheet, and not a promise about anyone else's machine:

| | |
|---|---|
| level in | −9 dBFS |
| level out | −48 to −53 dBFS |
| change | **−48 to −54 dB** |
| cost per 10 ms frame | ~0.06–0.22 ms (estimated from the coarse clock) |
| keeping up | 0.999–1.001 |

The `real time used` row is missing from that table on purpose: the run above measured it against
the audio-clock window, and the panel now divides by the wall clock. In a healthy run the two are
the same 500 ms, which is why the old figure was not wrong — and also why it could never have
reported a machine that was losing. It has not been re-measured under the new denominator, so no
number is quoted for it.

The added delay is not in that table either, because it is not a property of that machine: the
992 samples above are measured against the shipped worklet and the shipped `.wasm` in `node --test`,
and the panel measures the same thing in the browser.

Broadband white noise at the same level came out only 1.4 dB down. That is not a fault in the
wiring; it is the model. RNNoise was trained on the noise rooms actually have.

Two things the panel cannot tell you, and does not pretend to. The levels are our node's input and
output, not the room's: whatever the operating system did to the signal before `getUserMedia`
handed it over is already in "level in". And `getSettings()` reporting `noiseSuppression: false` is
the browser's claim, not a measurement — on some platforms hardware or OS suppression runs whatever
the constraint says. The menu labels that line "the browser says" for exactly that reason.

## What this browser remembers

A person who chooses the mediacore denoiser, picks the headset they actually use and types their
name should not have to do it again on the next reload. Those choices are kept in `localStorage`
under **one key, `mediacore.front.v1`**, as one JSON object, written whole whenever any of it
changes:

| Remembered | Put back by |
|---|---|
| the noise suppressor | `media.setNoiseEngine()`, the call the row in the menu makes |
| the microphone's `deviceId` | `media.selectMicrophone()`, once the browser will say the device is still there |
| echo cancellation and automatic gain | `media.setProcessing()` |
| the name typed in the lobby | the field, cut to the length this server will accept |
| the room this browser last joined | the room field at `/`, unless the URL names one, which always wins |
| the camera box in the lobby | the checkbox, unless `?video=` says otherwise for this visit |

**Nothing the server minted is ever written.** Not the token and not the identity. A token expires,
a copy of one on disk outlives the call it was issued for, and every script this origin ever runs
can read it — and all it would save anybody is one press of Join. The list of fields that may be
stored is a single line in `assets/prefs.js`, and a test fails if it grows one.

Some things are deliberately **not** remembered, and each one is a decision rather than an
oversight. **Muted or not**: this page has no "join muted" control, so restoring it would mean
arriving silent with nothing in the lobby having said so — the failure being "nobody could hear me
for the first minute". **The bandwidth bench**: it is a diagnostic view that takes every video onto
its own tiles, `?bench=1` already opens it for the people who want it that way, and a room that
opens into a panel of statistics looks broken. **The tile in the middle and the pinned share**:
both name somebody who was in the room at the time, which is not a fact about this browser.

The room changed sides when rooms got addresses. It used to be "that is the link, not a
preference"; it is now both. It is not a credential and never was — it is in the URL bar of the tab
that joined it and in the link that was pasted to get there, and it opens nothing on its own, since
a token still has to be minted for it per load. What remembering buys is that somebody who joins
`standup` every morning is offered `standup` when they arrive at `/` with nothing else to go on. A
link outranks it every time, so a remembered room can never put anybody somewhere their link did
not say.

A remembered choice is not treated as truer than a chosen one. It goes back in through the same
calls the controls make, on the far side of the same denoiser probe, so a machine that cannot run
ours lands on the browser's own exactly as it would if somebody had pressed the row by hand — and
the reason is under the row in the menu either way.

**A remembered `deviceId` is not a promise.** The device can be gone, the same microphone can come
back under a different id after a replug, and the browser hides ids and labels alike until the
microphone has been granted once. So `chooseMicrophone()` gives three answers and the page needs all
three: the device is on the list, so ask for it; the list can answer and it is not on it, so use
the default and say so; or the browser has not said yet — ask for it anyway and be ready, because
`getUserMedia` given an exact device that is not there fails the whole call for it, in Chrome as a
`NotFoundError` that reads exactly like having no microphone at all. That last one is why the
capture module drops the device and asks again rather than reporting "no microphone was found" to
somebody with one plugged in.

**Storage is input, not truth, and it is allowed to throw.** Every read and every write is wrapped,
including the accessor itself: a browser told to block site data throws on `window.localStorage`
before a method is called, and a private window can answer `getItem` happily and then throw from
`setItem` when its small quota fills. All of those paths end with the page rendering and the
settings lasting until the tab is closed; the first refused write says so once, and then stops
saying it.

Values are **validated on read**, field by field, and a field nothing can be done with comes back as
"nothing was remembered about that" without costing the other four. That is the check that matters,
and a version number could not have done it: a version says which shape wrote a value, not whether
the value is still one of the choices the page offers — a suppressor that was withdrawn, a
processing key that is no longer settable, a name of a length this server would refuse. The `v1` in
the key is there for the one thing validation cannot do, which is a future layout that means
something different by the same field names: it is written under another key and never reads this
one.

## A pass from somewhere else

This page mints its own token on every load, which is the right shape for a page that is its own
product: it asks for a name, the server signs it, and nobody outside decides who anybody is. It is
the wrong shape for a page a platform sends people to. There the platform already knows who this
is — an account, a contract, a role — and none of that survives a token this page invented for
itself.

So a pass is accepted from the address:

```
https://meet.example.com/r/standup#token=eyJhbGciOiJIUzI1NiIs…
```

With one there, nothing is minted, nothing is asked and nothing is pressed. The name field and the
room field both go — who this person is was decided by whoever signed the pass, and the room is
written into it — the lobby becomes a sentence naming both, and the page joins on arrival.

The button used to be there to buy a gesture, and neither thing that wants one is worth stopping
for any more. A microphone refused without a gesture is no longer a refusal of the room: the person
lands as a listener, and `Turn on mic` is itself the gesture. Audio playback held back by the
browser is what the `Enable audio` banner has always been for, and it is already the path everybody
who reloads into a room walks.

**After the hash, and nowhere else.** A fragment is never sent to a server: it stays out of access
logs, out of the `Referer` header sent to whatever the page loads, and out of a proxy's history. A
`?token=` is in all three. Accepting both would mean the safe shape is the one nobody uses, so only
one is read.

**And it is wiped as it is read.** A pass in a link is a credential — whoever holds the link is that
person in that room until it expires — so the fragment is removed from the address before anything
else runs. What is left in the address bar, in the history entry, in a bookmark and in a screenshot
of any of them is a link that opens the right room and lets nobody in by itself.

Wiping it would also lose it on a reload, dropping somebody who pressed F5 back into a lobby with
nothing in it, so the tab keeps a copy in `sessionStorage` — never in `localStorage`, for the reason
written at the top of `prefs.js`: a token that outlives the visit it was minted for buys nobody
anything. The copy dies when the tab closes, and pressing Leave forgets it on the spot, because
leaving is a decision and a reload after one should land in the lobby rather than walk straight back
in. A browser set to block site data throws on the accessor itself; the page opens anyway and a
reload there asks for a new link.

Nothing here verifies the signature. That is the server's business and it checks it when the socket
opens. What the page reads out of the claims is who somebody is about to be, and two refusals it can
make early and legibly: a pass that has expired, and a pass for a different room than the address
names. Both would otherwise arrive as a socket that closed without a word, which looks like a broken
server rather than an old link.

A deployment that signs people in is bypassed by a pass, deliberately: it was signed with the same
secret this server verifies by, so whoever minted it is as trusted as this page's own `/api/token`.
If that is not wanted, do not hand the secret out.

## The interface

The room is the shape a voice chat has settled into, and nothing more: a persistent list of who is
present down the left, the people or the shared screen in the middle, and one bar of controls along
the bottom holding mute, the speaker mute, the microphone chooser, the noise-suppression control,
screen share, camera, the bench and leave. The noise control is one button naming the engine that is running —
pressing it steps through the values on offer — and the audio menu behind the chooser holds the
same setting as a radio group with a sentence about each. When somebody is sharing a screen it takes the main area and the participants
shrink to a strip beneath it; when nobody is, the participants fill it. Two people with the same
name keep the last four characters of their identity beside it, as they always have. That is the
shape on a window with room for it; "On a phone" below is the same room on a screen 375 pixels
across, which is where most of these calls are actually taken.

### Joining without a microphone

The join asks for the microphone and then carries on whatever the answer was. It did not always: a
refusal used to end the join in the lobby with the sentence about the padlock and nothing else,
which let one browser permission decide whether somebody could be in the room at all. That is the
wrong thing for it to decide. Hearing the room, seeing who is in it, reading and writing the chat,
watching a shared screen and the bench are all of the room except one half of one control, and none
of them needs a capture device. The person this happened to is not an edge case — a machine with no
microphone, a laptop where the device is already held by something else, somebody who pressed Block
on a prompt weeks ago and does not connect the two — and telling them to fix the padlock before
they may hear a meeting that has already started is a page putting its own precondition ahead of
what they came for.

Nothing on the server ever depended on the device. **The token is minted per join and always grants
`microphone`** — see "What the token grants" — so a listener holds exactly the same grant as
everybody else and is one `publishTrack` away from talking. `connect()` publishes only when there
is something to publish; a publication that fails *with* a microphone in hand is still fatal,
because that one is the SFU refusing a source the token names, which is a different failure and
worth stopping the join for.

**The microphone button becomes the way to a microphone rather than a dead control.** It says
`Turn on mic`, and pressing it asks the browser again and publishes what comes back onto the
connection that is already up: nobody is unsubscribed, nothing renegotiates beyond the one added
track, and the room sees somebody who was quiet start talking. Leaving it disabled — which is what
the page used to do — is the worst of both, because the person most likely to press it is the
person who has *just* come back from the padlock having allowed the device, and the page would
answer them with a greyed-out button and no way back short of leaving the room and joining it
again. The press is also the only mechanism available: `getUserMedia` puts its question inside a
user gesture and nowhere else, so a page that retried on a timer would be asking a question the
browser will not ask, and one that retried on every render would spend the call being refused in
silence.

**The reason is kept, not shown once.** The refusal `media.js` wrote — what happened and where the
padlock that undoes it is — is said once on arrival as one line, with this page's half after it:
the room refused nobody, and there is a button. Notices time out, so the same sentence then lives
on the button's own tooltip for as long as there is no microphone, which is where somebody who has
been away fixing a permission will look for it. To a screen reader the button reports itself
*pressed* while there is no microphone: a browser that has not handed one over is exactly as far
from the room as a muted one, and a control claiming otherwise would be telling somebody they can
be heard.

For anybody who does have a microphone nothing about this is visible. The one opened mid-call goes
through the same `openMicrophone()` as the one opened at the join — the same denoiser probe, the
same node attach, the same fallback to the browser's suppressor — and mute, the device chooser, the
changes held until you unmute and the speaker mute are all untouched.

### Putting one thing in the middle

Press a tile and that one thing fills the stage, with everything else in a strip down the right that
scrolls. Press it again, or press Escape, and the room goes back to the grid it was. Anything on the
stage can go there — a camera, a shared screen, and a participant with no camera as their avatar
tile, large, because the point of choosing somebody is *I want to look at this one*, which is as
true of a name as it is of a picture.

A shared screen keeps the precedence it has: it takes the stage on its own as soon as it starts,
without anybody pressing anything, and putting a person in the middle beside it **never** throws it
away. It moves to the top of the strip, one press from coming back. When the person in the middle
leaves, the screen is back in the middle where it was.

The choice is the viewer's, and the room cannot take it: it survives a re-render, a reorder, the
bench being opened and closed over the top of it, and a publisher swapping a window for a whole
screen — a share is chosen as *the share* and never as a track, so the new track that swap mints
carries on in the middle. It is let go of only when what was being looked at has gone: the person
left, or the screen ended. A room that is reconnecting has dropped every remote participant without
saying a word about any of them, so while the link is down the choice is held rather than concluded
from, exactly as the shared screen is.

**Fullscreen outranks it.** The roster, the tiles and the strip are all behind a fullscreen picture,
so a person in the middle under it is in the middle of nothing anybody can see. The choice is held
rather than dropped, the control that sets it is taken off the picture while fullscreen is up, and
leaving fullscreen puts the viewer back exactly where they were. Escape follows the same order it
reads in: the audio menu first, then the fullscreen this page drew itself, then the middle — one
press each, so coming out of a picture never rearranges the room behind it in the same breath.

**Every tile is a real control.** The picture is a `button`: in the tab order, answering Enter and
Space, carrying `aria-pressed`, with an accessible name that says what pressing it does — `Focus
Bob`, and `Exit focus on Bob` for the one already there. The name and the state underneath stay
*outside* the button, because a button's own name is what a screen reader reads instead of its
contents, and wrapping the whole tile would have swapped "Bob, speaking" for the name of the action.
They are still selectable and still carry the sentence on hover that says why a state is unknown.
The shared screen has the same control as a pill in its corner beside `Fullscreen`, because it is
the one tile that cannot answer a plain click: a double click on the picture is already fullscreen,
and a click bound beside it would fire on the way there.

Nothing is hidden by this layout, so nothing is detached for it: every tile is in the strip, showing
who is speaking, which is how you know which one is worth pressing. **Narrower than 64rem** a column
beside the picture leaves neither of them worth looking at, so the strip goes along the bottom and
scrolls sideways inside itself — the bottom rather than the top because it is beside the control
bar, and so under the thumb that is already there. That is its own breakpoint and not the one where
the roster stops being a column: a window can be too narrow for two columns on the stage and still
wide enough for the roster beside them. Nothing anywhere scrolls the page sideways.

Only one node ever moves in the document to do any of this — the tile in the middle, out of the
strip and back. `#share` and `#cards` stay where they are written and the grid places them, because
a video element taken out of the document is a video the browser is entitled to pause. The tile that
does move takes the keyboard's focus with it and is handed it back, so pressing a tile does not send
the next Tab to the top of the page.

### The shared screen

A shared screen is the main view for as long as it is being shared, and nothing displaces it. It is
not moved aside by whoever is speaking and not re-picked on every render: the stage pins the share
it is holding by track, and only lets go when that share ends. Somebody sharing a screen is asking
the room to look at one thing, and a view that swaps to the loudest person is the failure this is
written against.

The room allows two screens at once. Both appear as a row of buttons under the picture naming who
is sharing, the stage shows the one you picked, and it stays picked until you pick another or it
ends. If the one you were watching ends while somebody else is still sharing, the stage moves to
what is left and says whose ended and whose you are now watching.

A share that stops arriving is not the same thing as a share that ended, and the stage will not
confuse them. A room that is reconnecting has dropped every remote participant and told us nothing
about any of them, so while the link is down the stage holds what it had, says nothing, and leaves
the fullscreen view standing — the red alert underneath is already saying what happened, and a line
blaming the person whose picture stopped would be blaming them for the viewer's own connection. A
room that has ended closes the fullscreen view and still names nobody. While the link is up the
stage waits four seconds before concluding a share is over, because a publisher swapping a window
for a whole screen stops one capture and starts another, and the gap in between is theirs. That
swap arrives as a **new track from the same publisher**, which the stage reads as one share
continuing rather than one ending and a stranger beginning: the picture carries on and the
fullscreen view is not torn down for the most ordinary thing a sharer does.

**Fullscreen hides the roster and the tiles**, which were the only two places the page showed who
was talking and whether the link was up, so the stage carries its own line for both: the connection
pill and a list of who is speaking, under the picture, visible only in fullscreen because the room
already says both elsewhere. The tiles behind the picture are detached while it is up — a video
decoded into something `display: none` buys the share nothing.

**Fullscreen** is one button on the tile — `Fullscreen` / `Exit fullscreen`, in the tab order, with
`aria-pressed` and an accessible name that says which state pressing it leaves you in — a double
click on the picture, and Escape to come back. It is asked for on the **room**, not on the video:
the control bar is inside that element, so muting, switching microphone, choosing the other screen
and leaving the call all stay on the screen with the picture. A video element taken fullscreen on
its own would take none of them along.

Three answers are possible to that request and all three are handled:

| What the browser does | What the page does |
|---|---|
| grants it | the browser's own fullscreen, and its own Escape |
| has no `requestFullscreen` on an element, or forbids it to this document | the same layout, pinned over the window, and Escape handled by the page |
| resolves the request and stays where it was | the same, because what the document says afterwards is what counts |

The second row is iOS Safari, where nothing but a video element goes fullscreen and taking the
video would hand the screen to the system player and the controls with it. The window-filling path
is not a lesser fallback: it is the same markup and the same controls, laid out the same way, and
the only difference is who owns the Escape key. Whichever path was taken is said in the notice
line, with the browser's own reason in it.

The states that end a fullscreen say why, rather than leaving somebody looking at a dead rectangle:
the share ending closes it and names who stopped, another share still running moves the view there
instead, and opening the bench — which takes the video onto its own tiles — closes it too. Leaving
the room closes it.

**In the corner of the picture** is what actually arrived: the frame size and the bitrate this
browser reports for that track, sampled once a second from the same `getStats()` the bench reads,
with a tooltip saying whether that is what is arriving or, for your own share, what is going out.
If the browser reports only one of the two, only that one is shown and the tooltip says the other is
missing; if it reports neither there is no badge at all. Nothing there is inferred from what was
published: a 1080p15 layer that is arriving as 1280×720 says 1280×720.

When that size changes the change is said out loud, as what it is and what it was, and with no
claim about why. The page cannot tell a publisher swapping a window for a whole screen from their
encoder scaling itself down under congestion — both arrive as a different `frameWidth` — so it does
not pretend to: the line reads *the shared screen is now 1280×720 (it was 1920×1080)*. Two readings
a second apart have to agree before the change counts, and a step smaller than a fifth of the area
is not worth a line at all, because the page has one notice slot and mute, the denoiser and the
microphone chooser all speak through it too.

Telling one local track's outbound stream from another's needs the `media-source` entry that
carries the `trackIdentifier`; a stream the browser does not join to a track is left out of the
reckoning rather than added to whichever track was asked about, which is what keeps a camera going
out beside a share from moving the share's number.

What the page will not do is look calm about something it cannot see:

- **Speaking is measured, not announced.** This SFU does not send `SpeakersChanged`, so an
  indicator driven by the server's opinion would stay dark for ever. Every ring on this page comes
  from an AudioWorklet on the track's own audio, measuring every block; the section below says how.
- **A participant nobody can measure reads as unknown**, with a dashed outline and a sentence on
  hover: a track that is published but not subscribed, one the server paused for bandwidth, a
  microphone that was never opened, a browser that gave the page no audio context at all, or a meter
  that has stopped reporting. None of those are shown as `listening`.
- **A muted microphone reads as muted on every page**, because the mute travels as a track event and
  each page paints it from the publication rather than from a guess.
- **Every control says what it will do and reports what happened.** A refused microphone says which
  setting to change; a screen picker closed without a choice says so, and says that no browser
  distinguishes that from a blocked permission; a failed publish names the source the token would
  have needed. The noise-suppression button carries what the browser *reports* it applied, which is
  not always what was asked for, and says `asked for on` where the browser reports nothing.

Every control is a real `button`, `input` or `select`, reachable and operable from the keyboard, with
a visible focus ring, an accessible name that changes with its state, and `aria-pressed` /
`aria-checked` wherever it toggles; the noise engines are a `radiogroup`, and the one this machine
cannot run is disabled with the reason on hover rather than silently doing nothing. Escape closes
the audio menu and returns focus to the button that opened it. `prefers-reduced-motion` turns the speaking pulse off.

The bench is one press away rather than always on, and while it is open **it owns the video**: the
bench tiles attach the tracks and the stage does not. A track attached in two places is a track
either renderer can detach from under the other, so only one of them ever holds it.

### On a phone

A phone is not a narrow desktop. It is 375 pixels across, a thumb rather than a cursor, a window
whose height changes as the address bar comes and goes, and a strip along the bottom that belongs to
the operating system rather than to the page. Every answer below is a question of size and pointer
that the stylesheet asks for itself -- nothing is sniffed, nothing is remembered, and a window
dragged narrow on a desktop gets exactly what the phone gets.

**A phone is two windows, and the second one is not narrow.** Upright it is 375 or 390 pixels
across, which any question about width finds. Turned sideways the same phone is 844 pixels across,
or 932, or 956 -- wider than a good many laptop windows -- and 390 pixels tall. So the page asks
about width *or* about height: narrower than 52rem, or shorter than 30rem while still narrower than
60rem. Without the second half a phone held sideways goes back to the desktop layout, and the
desktop layout in a window 390 pixels tall is a 17.5rem roster column and a control bar wrapped onto
two rows. The 60rem bound is what keeps a short, wide desktop window out of a layout drawn for a
thumb.

**The bar keeps four controls and `More` holds the rest.** Eight of them across 375 pixels is 352
pixels of button before a single gap between them, at the 44 each that a finger needs, and both ways
of keeping all eight are worse than moving four: drawn smaller they cannot be hit, and scrolled
sideways Leave is something you go looking for at the moment you wanted to leave. So the row is the
microphone, the camera, `More` and Leave -- Leave at the far end with a gap of its own, because the
one control here that cannot be undone should not be beside the one that is pressed most. Behind
`More`, as full-width rows carrying the names they never had room for beside a cursor, are the audio
menu, the noise control, screen share and the bench. Nothing is dropped, and nothing is more than
one press further away than it was. `More` is a width and nothing else, and it is the one question
here that stays a pure width: the same phone held sideways has room for all eight at once, so
sideways there is no `More` and all eight are in the row.

**Which four is not an arbitrary choice.** The microphone and Leave are what a call is; the camera
is the one thing on a phone that is reached for during it. Screen share is the one that moves,
because no phone browser hands a web page a screen to capture at all -- on iOS and on Android the
control is there and disabled, saying so, which is what this page does everywhere rather than hiding
what a browser will not do. The noise control moves and loses nothing, because the menu behind
`More` carries the same setting as a radio group with a sentence about each engine.

**The roster is one line**: the room's name, the connection pill, and a count that is a button. A
17.5rem column is half a phone screen, and the strip it used to become across the top was 40dvh --
a third of the screen, spent on a list the tiles underneath are already showing, with who is
speaking marked on both. What only the roster says stays on that line, and the pill is the whole
reason: on a phone it is the only thing on the page that says the call is still up.

**The list comes over the stage rather than pushing it down.** Pressing the count opens it as a
sheet under the line, with the room's own address and the licence notice at the bottom of it --
both of which were simply unreachable on a phone before, because the footer holding them was the
first thing a narrow window turned off. It comes over the stage because a picture that jumps when
somebody counts heads is a picture whose Leave button has moved by the time they reach for it.

**The audio menu is a sheet along the bottom**, where the thumb already is, with the way out of it
kept on the screen while the panel behind it scrolls. It is fixed to the window rather than hung off
the bar, because a panel anchored to something that moves with the address bar arrives with its head
off the top of the screen. It has a backdrop, and the backdrop covers the control bar as well: a
sheet somebody can reach past to press Leave is a sheet somebody will reach past to press Leave, and
under a sheet drawn over the stage every tile is a button that answers a press.

**Every viewport height is a `dvh` and the bar is inset for the home indicator.** `100vh` is not the
height of a phone's window: it is the height that window has once the address bar has scrolled away,
and this page does not scroll, so the address bar never goes and a bar measured in `vh` spends the
call behind it. `env(safe-area-inset-bottom)` is the other half -- the page asks for the whole
display with `viewport-fit=cover`, so without the inset Leave is drawn under the strip a swipe
leaves the browser by, and the press that ends the call is the press that hides it. Sideways, the
same is true of the notch at one end: the roster's line, the stage, the bar and everything drawn in
a corner of a fullscreen picture are held off it by `env(safe-area-inset-left)` and its opposite.

The insets are on the ordinary rules and not only inside the phone's own block, because a tablet held
sideways is 1024 pixels across -- too wide for any question this file asks about phones -- with the
same strip along the bottom of it. `env()` is zero on a screen that has no such strip, so a desktop
pays nothing for them.

**A finger has no hover, and mobile Safari answers `:hover` anyway.** It sets it on a tap and leaves
it set until something else is tapped, and every hover on this page lights a border -- which is what
a control that is *on* looks like here. So the four rules that answer a hover are inside a question
about whether this pointer has one, and a press gets its own answer instead. Two gestures are turned
off on the controls and nowhere else: the double-tap, because a second press inside a third of a
second is a zoom and muting twice quickly is an ordinary thing to do, and the long press, because a
magnifying glass over the word "Leave" is not what that press was for. Pinch is untouched.

**Sideways is the case that matters most**, because sideways is how a shared screen is watched, and
it is 375 to 440 pixels of height with a browser's own bar already in some of them. There the room's
line is as thin as it can be read at and the bar gives up the air around it -- and, more than either,
**the room turns through ninety degrees with the phone**. Upright the stage is a column: a picture
with faces underneath it. Sideways that does not fit and never did -- a stage with about 260 pixels
in it cannot hold a 16:9 picture and a row of faces, which want more than 370 between them, so one
of the two goes over the edge. So sideways they stand side by side, which is what the 500 spare
pixels of width were for: the picture takes the height the window has and whatever width 16:9 makes
of that, and the faces are a column beside it. Focus does the same thing for the same reason -- the
strip that runs along the bottom of a narrow window goes back to being a column on the right, because
that strip's whole argument was about width, and sideways width is the one thing there is plenty of.

**Which way a picture is measured is a question about which axis runs out first**, and it has two
answers here because the phone has two shapes. Upright the width runs out: the picture is drawn to
the width it has and its height follows. Sideways the height runs out, so the picture is given the
height and its width follows. Getting it the wrong way round does not clip anything -- it letterboxes
instead, and the frame's border, the size badge and the two corner buttons stay around the whole box
rather than on the picture, which is how you can see at a glance that it is wrong.

**The rest of the stage is the shape it was.** Grid, share, focus and both fullscreens are the same
four layouts reached by the same conditions; nothing is added and nothing is taken away. What changes
is the size of what is in them: two tiles across instead of one, and the pills in the corner of a
picture keeping their names for a screen reader and losing them for the eye, because two words each
is a third of the width of a shared screen on a phone laid over the part somebody is trying to read.

**The bench stays.** It is a table of numbers and the temptation is to call it a desktop thing, but
it is the one view that answers *why is this call bad*, and a call is bad on a phone more often than
it is anywhere else. It needs nothing: the link panel is two columns instead of five, the tiles are
one, and every one of them scrolls inside the stage rather than putting a sideways scrollbar on the
page. Nothing on this page ever does that, at any width, and there is a test that says so.

## What the page shows

**The link panel**, in the bench view, five numbers straight from `RTCPeerConnection.getStats()` on
the client's own transports, sampled once a second:

| Cell | Source |
|---|---|
| browser estimate in | `availableIncomingBitrate` on the subscriber transport's selected candidate pair |
| arriving now | every `inbound-rtp` `bytesReceived` on that transport, summed and differenced over the sample |
| video arriving | the same, over the video `inbound-rtp` entries only |
| round trip | `currentRoundTripTime` on the same candidate pair |
| browser estimate out | `availableOutgoingBitrate` on the publisher transport |

Video is measured apart from audio because the failure this bench exists to catch — a subscriber the
SFU has capped or paused and never restores — leaves audio arriving exactly as before. A total that
still reads a few tens of kilobits per second when every tile has frozen is not a signal, it is
camouflage; the video cell goes to zero and says so.

The two estimates are the *browser's* opinion, not the SFU's. That is the point: the SFU's
allocator runs off str0m's send-side estimate, and the only independent check available from
outside the server is what the receiving browser thinks the same link is worth. Chrome fills in
`availableIncomingBitrate` only while it is running a receive-side estimator; when it does not, the
cell shows an em dash whose tooltip says so and points at the video rate instead, which is the
number that actually answers whether the link came back. Nothing on this page
invents a number, and nothing shows a zero for a value that was absent — a zero on the page is a
zero the browser reported.

**A tile per remote video track.** The badge names the layer and the exact decoded frame size, for
example `mid · 640×360`. The frame size is measured; the layer name is *inferred* from the frame
height (≤ 200 px low, ≤ 400 px mid, above that high), because the SFU tells the client which track
is paused but never which spatial layer it chose. Underneath: bitrate, loss over the sample, frames
per second, jitter, the browser's freeze count, and the stream state — `paused` there is the
`StreamStateUpdate` the SFU sends when it pauses a track for bandwidth, so a bandwidth pause is
visible as itself and not as a track that merely stopped.

Under that, the tile counts layer changes and says how long ago the last one was
(`3 layer changes · high → mid 4 s ago`), and its border goes green for a couple of seconds when
one happens. That is the thing to watch during a step change.

**A tile for your own camera**, one row per simulcast rung, read from `outbound-rtp`: the rid, the
encoded size, the measured bitrate and — when the browser is holding a rung down — the
`qualityLimitationReason`. A rung the browser has stopped sending is dimmed. This is where dynacast
shows itself: if the SFU stops asking for a rung, the publisher stops encoding it and the row goes
quiet, which matters because one congested subscriber must never do that to the room.

**A line per remote participant** in the list, with the arriving bitrate and loss of their audio.

**The corner of a shared screen**, outside the bench and always on while a screen is on the stage:
`frameWidth×frameHeight` and the bitrate differenced over the sample, from the `inbound-rtp` entry
for that track, or from the `outbound-rtp` entries joined to that track through `media-source` when
the share is your own.

**The microphone denoiser panel**, when ours is in the path — described in the denoiser section
above.

## Testing it

`cargo test -p mediacore-front` runs everything, and the split between the two halves is deliberate.

**The Rust tests are statements about bytes**: that each artefact is served, with its version in the
path and the right content type, that the module graph the page imports resolves to routes this
binary answers, that the `.wasm` files really are WebAssembly modules, that the licence notice is
compiled in and served and names every project it covers, and that the content security policy still
forbids what it should. A test that a file *contains a phrase* is not a statement about behaviour and
none is written here: a substring survives every change that matters, so it can only ever fail for
the wrong reason.

**The sign-in is tested against a provider that only exists inside the test**, in `tests/oidc.rs`:
an axum router on a loopback port serving a discovery document, a JWKS, a token endpoint and a
userinfo endpoint, with the test driving the round trip the way a browser would — follow the `302`,
read `state` and `nonce` out of the URL the browser was sent to, mint the ID token that provider
would have minted, carry the cookies back, and see where it lands. Nothing there reaches the network.
That shape is what makes the interesting failures reachable, and most of the file is one of them at a
time: each ID-token check broken on its own, a callback in the wrong browser, a callback replayed, a
`state` this server never issued, a provider that is down, a provider describing itself as somebody
else, a userinfo response about somebody else, a `returnUrl` pointing off this origin, and a stream of
tokens naming keys that never existed. `nothing_of_the_client_secret_escapes` is the one that says the
secret reaches exactly one place, which is the body of the token request.

**The behaviour is tested in `tests/browser-modules.test.mjs`**, run through
`tests/browser_modules.rs`, which shells out to `node --test`. It needs no package manager, no
bundler and no browser, and it drives the real modules:

- `media.js` against a fake `livekit-client` modelled on the real one — `mediaStreamTrack` returning
  the processor's output, `setProcessor` refusing a track with no audio context, `restart` dropping
  every constraint but the device. It proves the settings round-trip into the constraints the
  browser is given, that choosing our engine turns the browser's two suppressors off and choosing
  any other turns them back on, that echo cancellation is never turned off, that a change made while
  muted is held until unmute, that a failed constraint change never leaves two suppressors running,
  that changing the suppressor does not re-open the capture device, that rejoining a room keeps the
  denoiser, and that a denoiser which cannot be probed, cannot attach, or cannot keep up leaves the
  call on the browser's own suppressor rather than on nothing. The screen share is driven the same
  way: that the video goes up with the screen-share ceiling rather than as a camera, that the sound
  is published beside it when the browser offers it and said to be missing when it does not, that a
  closed picker and a blocked permission are reported as the pair the browser makes of them, and
  that a share stopped from the browser's own bar unpublishes and reports itself.
- `prefs.js`, on a `Storage` that holds a value, one that throws on the way in and one that throws
  on the way out: that a blob written by one visit is the choices that visit made, that a token
  handed to it is not written down, that a suppressor which is no longer on offer comes back as
  nothing without costing the five fields beside it, that a room is kept for its shape and not
  judged for its validity, that a blob of the wrong shape entirely is not
  a broken page, and that a remembered microphone the browser can see is gone is never asked for —
  while one it will not say anything about is asked for, and the capture module opens the default
  and says so when it turns out to be missing.
- `room.js`, where a room's address is read and written: that the room comes out of `/r/<room>`
  and out of a legacy `?room=` with the canonical URL to correct it to, that a percent sequence
  which will not decode is shown rather than thrown, that the other parameters of a visit survive
  the correction, that a link outranks a remembered room and a remembered room outranks the
  server's own, and that a spoken name survives the round trip through the room the SFU knows and
  back.
- `stage.js`, which is where the stage's decisions live precisely so they can be argued with here:
  that a pinned share survives the room being reordered under it, that a share ending names whose it
  was, that a browser with no fullscreen and one that refuses and one that silently ignores the
  request all land on the path that fills the window, and that the corner badge prints a half it has,
  nothing at all when it has neither, and never `0×0`. The middle is decided in the same file and
  argued with the same way: that the layouts reached without pressing anything are the ones that
  were there before there was a middle, that a screen starting joins the strip instead of displacing
  somebody out of it, that somebody leaving is let go of and a reconnect is not, that fullscreen
  takes the stage without forgetting what was in the middle, and that pressing the tile already
  there puts it back — which is the only way out on a phone, where there is no Escape key.
- `bench.js` against a fake `RTCStatsReport`, on the join that attributes an outbound stream to a
  local track: a screen share and a camera going out together keep their own numbers, a stream the
  browser did not join to a track is left out, and one sample yields no rate.
- `denoise-probe.js` in a `vm` context carrying the globals an `AudioWorkletGlobalScope` carries,
  driven a quantum at a time: the cost it reports is the time it was actually given, a block that
  overran is counted as one, a window with no audio reports no level rather than a very quiet one,
  with no `performance` on the render thread it reports having no clock instead of a cost of zero,
  and a delay it was given comes back out of the correlation search to the sample.
- **the vendored RNNoise worklet itself, on the vendored `.wasm`**, in the same kind of context.
  That is where the gate is proved: before the wasm is live the node writes nothing and the gate
  says nothing, after it the gate says so exactly once, and a compile that never finishes is
  reported as a stall rather than passing silence for ever. It is also where the 20.7 ms of added
  delay quoted above is measured, through the same tap-and-probe chain the page builds.

It needs `node` on the path. Without it the Rust test fails and says so, rather than passing
without having run anything.

The bench reads all of this off the two transports the vendored `livekit-client` builds, which is
not part of that library's public surface. It is pinned to the vendored copy; if the SDK is
upgraded and those fields move, every cell reports that it has no connection to read rather than
quietly reporting nothing at all.

Loss is reported as `lost / (lost + received)` over one sample, and is blank rather than `0 %` when
no packet arrived at all in that window — a silent DTX track and a track that is being dropped are
not the same thing and the page will not conflate them.

The client runs with `adaptiveStream: false`, so the browser never asks for a different quality
itself. Every layer change you see on a tile was the SFU's decision. Turning adaptive streaming on
would make the page measure the client instead.

## Running the bench against a constrained link

Shape the **SFU host's egress on the UDP media port**, `rtc.udp_port`, which defaults to **7882**.
Not the front end's HTTP port, and not the signalling port: signalling is TCP on 7880, and shaping it
kills the session for reasons that have nothing to do with congestion. Egress from the SFU is the
direction that matters — it is the direction the allocator spends.

Keep the bottleneck queue shallow, around 50 packets. A deep queue turns the run into a latency
test: the pacer hides behind it, nothing is lost, and the estimator sees a slow drift instead of a
cliff. If the SFU runs in a container, shape inside its network namespace or on its veth; shaping
the host bridge shapes every peer at once and the "one subscriber shaped, one not" run becomes
impossible.

### Linux

```
IFACE=eth0
sudo tc qdisc add dev $IFACE root handle 1: htb default 20
sudo tc class add dev $IFACE parent 1: classid 1:10 htb rate 1500kbit burst 15k
sudo tc class add dev $IFACE parent 1: classid 1:20 htb rate 1000mbit
sudo tc qdisc add dev $IFACE parent 1:10 handle 10: netem delay 40ms limit 50
sudo tc filter add dev $IFACE protocol ip parent 1:0 prio 1 u32 \
  match ip protocol 17 0xff match ip sport 7882 0xffff flowid 1:10
```

Change the ceiling in place, which is how you produce a step:

```
sudo tc class change dev $IFACE parent 1: classid 1:10 htb rate 800kbit burst 15k
```

Add loss without a ceiling, for the wifi impersonation:

```
sudo tc qdisc change dev $IFACE parent 1:10 handle 10: netem delay 20ms loss 20% limit 50
```

Tear it all down:

```
sudo tc qdisc del dev $IFACE root
```

### macOS

```
sudo dnctl pipe 1 config bw 1500Kbit/s delay 40ms queue 50
printf 'dummynet out proto udp from any port 7882 to any pipe 1\n' | sudo pfctl -f - -e
```

Step it:

```
sudo dnctl pipe 1 config bw 800Kbit/s delay 40ms queue 50
```

Loss without a ceiling:

```
sudo dnctl pipe 1 config bw 5Mbit/s plr 0.2 delay 20ms queue 50
```

Tear it down:

```
sudo pfctl -d
sudo dnctl -q flush
```

`pfctl -f -` replaces the whole ruleset, so on a machine with rules of its own, add the `dummynet`
line to that ruleset instead of piping a one-line file at it.

### A run

Three publishers at `?video=1&res=720` and one subscriber behind the shaper. Two browser profiles
on one machine will do for the publishers; the subscriber should be the shaped host. Ten minutes
each, and write down the git rev and the shaper settings — a run nobody recorded cannot be compared
against the next one.

| # | Do this | The page should show |
|---|---|---|
| 1 | steady 1500 kbit/s | "arriving now" settles near 1500 kbit/s; every tile picks a rung and stops; under two layer changes per minute per track; loss under 1 %; round trip within 100 ms of the unshaped baseline |
| 2 | step 5000 → 800 kbit/s | a tile's badge steps down within about two seconds, its border flashes, loss is back under 1 % within five; the freeze counters stay put |
| 3 | step 800 → 5000 kbit/s | at least one tile climbs back within thirty seconds |
| 4 | no ceiling, 20 % loss | no tile reports `paused`, and the badges settle once and then do not move |
| 5 | shape one subscriber, leave a second unshaped | the unshaped subscriber's badges never move, and every rung on each publisher's own tile stays alive |
| 6 | join with `?video=0`, press Start camera after a minute | video reaches at least the mid rung within ten seconds |
| 7 | no shaping at all, ten minutes | no `paused`, and fewer than two layer changes per minute per track |

Scenario 4 does not measure a five megabit link, and it is not supposed to. str0m's loss controller
caps its own estimate at `instant_upper_bound_bandwidth_balance / (loss - 0.05)`, which at twenty
percent loss is about 666 kbit/s no matter how fast the link is, and its pacer paces at that number
whatever the allocator thinks. So the run is passed by the badges settling at a rung that fits inside
666 kbit/s within the first couple of seconds and then never moving, and by the state never reaching
`congested` — not by the badges staying at 720p. Fighting the estimate instead of honouring it is
what fills the pacer's queue, and a full queue is the one thing this system reads as congestion.
`the_bench_wifi_scenario_is_a_smaller_budget_and_not_a_congested_link` in
`crates/server/src/bandwidth.rs` is this scenario driven off a model of that link, so a regression
shows up without a shaper; the shaped run is still what proves the model.

The failure worth learning to recognise is scenario 1 going wrong quietly: **round trip climbing
into the hundreds of milliseconds while loss stays at zero and no tile changes layer**. That is not
a healthy link, it is the pacer's queue filling — the SFU is over-subscribing and the allocator has
not reacted. Loss is the loud failure; standing delay with no loss is the silent one, and it is why
the round-trip cell is on the panel at all.

Cross-check anything surprising in `chrome://webrtc-internals`, which shows the same counters this
page samples, plus the graphs. Every em dash on the page carries a tooltip saying why the value is
missing; hover it before assuming the bench is broken.

## Deployment

The page must be served over TLS: a browser hands out the microphone only in a secure context, and
a TLS page can only open `wss`. With signing in switched on there is a third reason — the session
cookie is `__Host-`-prefixed and a browser refuses that outside a secure context, so on plain http
no cookie is set at all and nobody can stay signed in. `http://localhost` counts as secure; a plain
LAN address does not. Put the front end and the SFU's `/rtc*` behind one TLS origin, so
the certificate the visitor accepted covers both. `deploy/` in the repository root does exactly
that, and its README covers the case that actually bites — a host with a public IP and no domain
name. Media does not pass through the proxy; it is UDP straight to the host, and the SFU advertises
that address itself through `rtc.advertise_addresses`.

## Who is speaking

The SFU does not send `SpeakersChanged` yet, so `RoomEvent.ActiveSpeakersChanged` never fires and a
speaking indicator driven by it would stay dark for ever. The page measures levels itself instead.

### What it used to do, and why the roster lied

An `AnalyserNode` per participant, `fftSize` 512, read from a `setInterval` every 150 ms, and
anything whose RMS came out above 0.02 was called speech. Both halves of that were wrong.

**The threshold could not be right.** 0.02 is −34 dBFS. Measured on the subscriber side, on a
continuous undistorted tone arriving at 49 kb/s with no loss: RMS 0.0239 read `speaking`, 0.0197 read
`listening`, 0.0100 read `listening`. A quiet or distant speaker lives permanently below that line
while everybody in the room can hear them, which is the complaint this started from — *I can hear
them and the roster says they are listening.* Lowering the constant does not fix it, it inverts it: a
room with a fan beside a large condenser microphone sits at −40 dBFS with nobody in it, so a
threshold low enough for the quiet speaker paints every row `speaking` for ever. That is the worse
lie of the two, because a roster that always says yes cannot be caught being wrong.

**And it judged a sliver.** 512 samples is 21.3 ms in a 24 kHz context, read once every 150 ms: 7 to
14 per cent of the audio was ever looked at, and *which* seven per cent was decided by wherever the
timer happened to land. Chrome throttles a background tab's timers to about once a second, taking it
to two per cent. Measured A/B on one loud continuous source: the foreground tab said `listening`
0 % of the time and the background tab 67 %. The page changed its mind about who was talking
depending on which window had focus.

Two things that were **not** the cause, and were ruled out with numbers before anything was changed:
`createMediaStreamSource` on a remote track reads exactly what arrives (a remote meter and the
publisher's own local meter agreed to three decimal places), and the half-second hold behaves
correctly across gaps.

### What it does now

**`level-meter.js`, an AudioWorklet, measures every sample of every block** and posts an RMS every
50 ms. The render thread is the only place that sees every block by construction: it is never
throttled by tab visibility because it is feeding a sound card in real time, and it cannot miss a
block without the audio itself breaking up. The alternative considered was a much larger analyser
window plus a rolling peak kept between polls, which loses nothing *within* a poll but still runs on
a main-thread timer — so a browser that decides to run that timer once a second still decides how
much of the audio is judged. The measurement is complete by construction rather than by luck, and
the main thread's job shrinks to reading messages. The node's output goes to a
`MediaStreamAudioDestinationNode` whose stream is attached to nothing: a destination is what makes
the graph pull the node, and the one destination it must never be pulled towards is the speakers,
where everybody is already audible once through their own `<audio>` element.

**Silencing the speaker is `muted` on every one of those elements, and it takes the microphone with
it.** Talking into a room you cannot hear is the position nobody means to be in, so pressing it
mutes the microphone as well, and pressing it again puts the microphone back where it was found
rather than where it would be convenient — somebody who was already muted stays muted. It is a
decision about this browser: nothing is unsubscribed, nothing stops arriving, and the room is told
only the mute, which it would have been told anyway.

Three reasons it is not a volume of zero, each enough on its own. iOS Safari ignores writes to
`HTMLMediaElement.volume`, so on a phone the room would stay audible. A detached element goes back
into the SDK's pool and is handed to the next person's track with its volume intact, so a zero
leaks on to somebody who never asked for it. And a participant's volume does not cover screen-share
audio, which arrives on a track of its own. Muting the elements covers all three, and it is already
the idiom here — the shared screen and the video tiles are muted the same way.

The part that leaks if it is forgotten is the arrivals. `Track.attach` writes `muted = false` every
time it runs, so a participant who joins after the button was pressed, a resubscription after a
reconnect, and `room.startAudio()` behind the playback banner each hand the room back at full
volume. All three go through one funnel, and a test asserts on each of them by name.

What it deliberately does not do is save bandwidth. Unsubscribing would: it would also turn every
roster row into `not subscribed`, put out the speaking rings, and cost a renegotiation per track on
the way back, so a button pressed to get some quiet would take the room's own display with it. The
meter is untouched for the same reason — it runs off the media stream track rather than the element,
so a deafened person still sees who is talking.

The worklet is registered, fed and stopped the same way the denoiser's two are — `addModule` once
per context, a `"stop"` message on the way out so the processor returns `false` and is collected.

**`voice.js` decides, and it asks two questions rather than one.** How loud is this against the room,
and how long has it lasted. Level alone cannot answer it at any threshold, because a keystroke and
the first syllable of a word are the same height for fifty milliseconds — a key measured thirty
decibels over a quiet room — and the only thing that tells them apart is that fifty milliseconds
later the key has stopped and the syllable has not. A floor-relative test with no duration in it
paints somebody taking notes as a talker for the whole meeting.

So speech is a level `ATTACK_OVER_FLOOR_DB` (9 dB) over the room that has occupied `ONSET_MS`
(200 ms) of the last `ONSET_WINDOW_MS` (400 ms) — a duty cycle of one half, where speech runs at
about seven tenths and a key every four hundred milliseconds runs at one eighth. It stops being
speech below `RELEASE_OVER_FLOOR_DB` (6 dB), two levels rather than one so a voice sitting on the
line settles on an answer instead of strobing, and it is held for half a second so the dot does not
flicker between syllables. The occupancy is measured as a share of the window rather than as an
unbroken run because syllables are 120 to 260 ms long and the dips between them are real: a rule
wanting a fifth of a second of *unbroken* loudness cannot be started by a short syllable at all, and
measured, it cost ordinary speech seventeen points of coverage.

**The room is a median, not a minimum.** Four seconds, kept in eight half-second buckets, each
holding the quietest level of its own half second. Minima *within* a bucket are what find the gaps in
speech, so the estimate lands on the room and not on the voice. A median *across* buckets is what
refuses a dropout: the previous shape of this took the minimum of the buckets, which is one number
decided by one bucket, so a single half second in which the track carried nothing rewrote the room
and every real sound for the next quarter of a minute was tens of decibels "above" it. A median
cannot be moved by anything occupying less than half the window. Both halves are needed; either one
alone gets one of the two cases wrong.

Three rules sit around that:

- **The floor may move nine decibels a second, in either direction.** Rooms change over seconds; a
  fan spins up, a window opens. Anything faster is the *track* changing, and the estimate does not
  follow it. It was previously rate-limited only upwards, on the reasoning that a level below the
  floor is direct evidence the room is quieter — and it is, but evidence about the track is not
  evidence about the room. The limit is what keeps a voice the far end has compressed flat: a speaker
  with two decibels of modulation and no breath for eight seconds reads `speaking` 99 % of the time
  with the limit and 31 % without it, and that is the case the test stands on. Its cost is that a fan
  which *starts* mid-call reads as somebody talking until the floor catches up — the two seconds the
  buckets need plus the jump divided by nine, measured at 3.8 s for a twenty-decibel jump and 5.9 s
  for forty, bounded and self-correcting.
- **Nothing at or below `QUIETEST_ROOM_DBFS` (−80 dBFS) is a room.** A microphone in a room, however
  still, is measuring air, a preamplifier and a converter. A level under that line is the track not
  carrying the room — a hardware-muted headset whose samples are zeros, an Opus DTX gap, a jitter
  buffer that ran dry, our own denoise gate before the wasm is live — and none of those are evidence
  about how loud the room is, so none of them reach the estimate. This is the rule that makes a
  dropout free: fifty milliseconds, a second or a minute of digital silence all cost zero seconds of
  `speaking` afterwards, where the old shape charged between ten and eighteen seconds for the
  shortest of them.
- **Nothing below `QUIETEST_SPEECH_DBFS` (−65 dBFS) is speech**, however far above the floor it sits.
  With the line above in place this only bites in one corner — a room sitting just over the bottom of
  what counts as a room, where a −68 dBFS tick is eleven decibels "above" it — but it is the only
  absolute statement left and the corner is real.

Everything in the gate except staleness runs on the audio clock the reports carry, not on the wall
clock, because that is the time the sound happened in. A main thread that reads twenty reports in one
burst therefore reaches exactly the same answer as one that read them fifty milliseconds apart, which
is the property that makes a background tab agree with a foreground one. The one place wall time
overrules it is a gap in the reports longer than the staleness limit: the answer was withdrawn out
loud while that gap lasted, so the hold is dropped rather than resumed, and a tab coming back after a
minute does not spend half a second showing a syllable that ended before it left.

**What it deliberately does not do.** A room whose own level swings six decibels every few seconds —
traffic through a window rather than a fan — still reads `speaking` about 15 % of the time; a four
second window cannot estimate a room that changes materially inside four seconds. A meter that
attaches in the middle of somebody's monologue, on a microphone compressing their gaps to under nine
decibels, reads `listening` until they pause — measured, a two-second pause recovers it and a
one-second pause does not, where the shape this replaced needed a pause longer than the whole
four-second window. Both are the safe direction: a missed speaker is a mistake somebody can see, and
a permanent `speaking` is not.

### A meter that stopped says so

A reading nobody has refreshed for a second is withdrawn: the participant goes to `unknown` with the
sentence saying how long ago the meter last reported, the same treatment a track nobody is subscribed
to gets. A browser that will not run the worklet at all is told so too, and is *not* quietly demoted
to a main-thread meter — that is the meter this page just stopped believing.

The reasons are the participant's own where the reason is. A worklet that dies on one person's track
says so about that person; only a module the browser refuses to install at all is reported about the
browser, because only that one is true of the browser. And a meter still being built says *that*,
rather than the page claiming it was given no audio context during the second the `addModule` round
trip takes — which is a claim about a browser that had just handed one over.

`visibilitychange` repaints when the tab comes back, and resumes an `AudioContext` the browser
suspended while it was away. Nothing about who is speaking changed while the tab was hidden — the
audio thread went on measuring, which is the whole reason the meter is on it — but what was on
screen was painted from readings that have since aged.

Changing a processing setting or switching input device replaces the local `MediaStreamTrack` behind
the publication, and a meter left on the old one would report silence for ever. The capture module
announces the swap as `microphonetrack`, and the page re-attaches the meter to the new track on that
event. Installing a worklet is asynchronous and a device change can replace a track twice before the
first attach finishes, so each attempt carries a token and one that finds a newer attempt in its
place disposes of what it built.

Local and remote are measured the same way, which also means the indicator keeps working unchanged
once the server does start reporting speakers. "The same way" means *what is published*: a local
track with a processor on it sends `processor.processedTrack`, and that is the track the meter is put
on. Measuring the capture instead would judge your own row on audio nobody else receives — RNNoise
takes most of the keyboard and the room tone out before any remote meter sees them — so your row and
everybody else's row for you could disagree for the whole call, with both of them right.

### The roster and the stage agree

Each repaint describes every participant once and hands that one description to the roster, the
tiles and the line across the shared screen. The line used to read the speaking set directly, with no
mute check in it, so somebody who muted mid-word was named as talking on the one view that hides the
roster saying otherwise, for as long as the hold lasted.

A participant who leaves without their track being unsubscribed first now takes their `<audio>`
element with them. `onTrackUnsubscribed` detaches on the way out and does not fire for somebody who
simply left, so every departure used to leave an element behind in `#audio` holding a dead stream for
the rest of the call.

The numbers are arguable rather than assertable: `voice.js` is a leaf module the behaviour suite
drives directly, so `node --test crates/front/tests/browser-modules.test.mjs` puts a −40 dBFS speaker
in a −60 dBFS room, a −30 dBFS fan in an empty one, a minute of uninterrupted speech and the same
audio delivered in one-second bunches, and asserts what the roster says about each.

### Measured in a browser

Driven in Chrome against a local SFU, with `HTMLMediaElement.prototype.muted` and `volume` redefined
as accessors that ignore writes and the microphone replaced by an oscillator through a
`MediaStreamAudioDestinationNode` — so the real device is never opened and nothing can reach the
speakers. Levels below are what the meter measured, not what was asked for.

| what was published | what the meter measured | the roster |
|---|---|---|
| the quiet speaker: syllables at RMS 0.0199 (−34 dBFS) in a −55 dBFS room | `−34.0 dBFS, 21.1 dB above this room's own −55.0 dBFS` | `speaking` from 464 ms, 100 % after |
| a monologue: syllables at −27 dBFS, 15 dB dips, 31 s | level −27.1 dBFS, floor settled at −42.0 dBFS | `speaking` from 346 ms, **100 %** for the whole 31 s |
| a loud speaker over a −40 dBFS fan | level −25 dBFS, floor −40.0 dBFS | `speaking` throughout |
| **somebody typing**: 50 ms of key at −30 dBFS every 400 ms, −70 dBFS room | keys −30.1 dBFS, floor held at −70.0 dBFS — the keys are **40 dB over the room** | `listening`, **0 %** speaking over 19 s |
| 50 ms and 1 s of digital silence, in −40, −50 and −60 dBFS rooms | level −100.0 dBFS, floor unmoved in all six | `listening`, **0 s** of speaking afterwards |
| a fan starting, −60 → −40 dBFS | floor climbed −60.0 → −40.0 | `speaking` for 3.0 s, then `listening` |
| a fan starting, −60 → −20 dBFS | floor climbed −60.0 → −20.0 | `speaking` for 6.0 s, then `listening` |
| a meter attached mid-utterance | floor found the dips at −42.0 dBFS | `speaking` from 401 ms, 100 % after |

The six digital-silence rows are the ones worth staring at. Under the shape this replaced, a *single*
fifty-millisecond window of silence bought between 10.7 and 17.6 seconds of `speaking` in a room
where nobody had spoken, because the floor fell to its clamp in one report and then had to climb back
at three decibels a second. They now cost nothing at all: a level that far down is not a room.

The typing row is the other one. Those keys are forty decibels over the floor — no threshold
separates them from a voice — and the reason the row says `listening` is that they last fifty
milliseconds.

The whole run happened in a tab Chrome was throttling. A control `setInterval(…, 150)` — the old
meter's own cadence — ran in the same page throughout and fired **1.1 times a second** instead of the
6.6 it manages with the tab in front. That is the throttling the old `AnalyserNode` meter died of,
which took it from judging 14 % of the audio to 2 %, and the roster's answers above were measured
under it.

Two things this run does *not* show, and both are known: a room whose own level swings six decibels
every few seconds still reads `speaking` about 15 % of the time, and the hover text on a row is a
snapshot from the last repaint rather than a live reading, because the roster is repainted when the
answer changes rather than twenty times a second per participant.

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

**One page, and `/r/<room>` is an address rather than a redirect.** The room has a URL and the
display name does not: the name never enters a URL or the browser history, and the click that
submits it is a user activation that `getUserMedia` and the playback of remote audio both happen
inside. That is also why following a link to a room lands in the lobby with the room filled in
instead of joining on arrival — a page that loaded with no activation at all has nothing to spend,
and Safari in particular then refuses to start audio until something else is clicked. One page, one
gesture, and a link that names the room. See "A room is an address" above.

**Identities are `<slug>-<8 hex>`.** The token's `sub` is the participant identity and the SFU
displaces an existing participant when the same identity joins again, so two people typing "Alice"
would otherwise kick each other out in turn. The name is slugged to ASCII (`Ann-Marie` becomes
`ann-marie`, a name with no ASCII letters becomes `guest`) and a random suffix is appended. The
typed name is carried separately in the `name` claim, so both people still show up as "Alice".
