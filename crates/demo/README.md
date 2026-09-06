# mediacore-demo

A single-page room. Someone opens the URL, types a name, and hears everyone else in the room.
Audio only by default; from inside the room you can choose the microphone, change the browser's
audio processing, share a screen, and tick one box in the lobby to publish the camera with
simulcast. Behind the **Bench** button in the control bar is the bandwidth bench — per remote
track, which layer is arriving and what it costs, next to the browser's own view of the link. The
binary serves the page and mints access tokens; it never carries media and never talks to the SFU
itself.

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
| `GET /media.js` | the capture side: microphone, processing, device choice, screen share; no DOM |
| `GET /bench.js` | reads the browser's WebRTC statistics; no DOM |
| `GET /readout.js` | draws the video tiles and the numbers; no statistics |
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


## Query parameters

| Parameter | Default | Effect |
|---|---|---|
| `?room=<name>` | the configured room | joins `<configured>.<name>`; see the namespacing section above |
| `?video=1` | off | ticks the camera box in the lobby, so joining publishes the camera as well as the microphone |
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

The three audio settings are constraints passed to `getUserMedia`, so the processing is the
browser's own and happens before Opus encoding. The SFU forwards encoded packets and never decodes
them, which is why nothing here can be changed after the fact:

| Setting | What it does to the audio | What it costs |
|---|---|---|
| `echoCancellation` | subtracts what the speakers are playing from what the microphone hears | needs a reference of the output; can duck the near end during double-talk |
| `noiseSuppression` | attenuates steady background noise — fans, traffic, hiss | thins quiet speech and can leave watery artefacts on music |
| `autoGainControl` | rides the level towards a target loudness | pumps the noise floor up between phrases |
| `voiceIsolation` | Chrome's stronger, model-based separation of voice from everything else | Chrome only; heavier, and it removes anything that is not a voice, music included |

`voiceIsolation` is on the list because `livekit-client` turns it on by default; leaving it out of
the interface would mean claiming noise suppression is off while a second suppressor is still
running. What the browser actually applied is read back from `MediaStreamTrack.getSettings()` and
reported as unknown where the browser reports nothing.

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

## The interface

The room is the shape a voice chat has settled into, and nothing more: a persistent list of who is
present down the left, the people or the shared screen in the middle, and one bar of controls along
the bottom holding mute, the microphone chooser, the noise-suppression switch, screen share, camera,
the bench and leave. When somebody is sharing a screen it takes the main area and the participants
shrink to a strip beneath it; when nobody is, the participants fill it. Two people with the same
name keep the last four characters of their identity beside it, as they always have.

What the page will not do is look calm about something it cannot see:

- **Speaking is measured, not announced.** This SFU does not send `SpeakersChanged`, so an
  indicator driven by the server's opinion would stay dark for ever. Every ring on this page comes
  from an `AnalyserNode` on the track's own audio; the section below says how.
- **A participant nobody can measure reads as unknown**, with a dashed outline and a sentence on
  hover: a track that is published but not subscribed, one the server paused for bandwidth, a
  microphone that was never opened, or a browser that gave the page no audio context at all. None of
  those are shown as `listening`.
- **A muted microphone reads as muted on every page**, because the mute travels as a track event and
  each page paints it from the publication rather than from a guess.
- **Every control says what it will do and reports what happened.** A refused microphone says which
  setting to change; a screen picker closed without a choice says so, and says that no browser
  distinguishes that from a blocked permission; a failed publish names the source the token would
  have needed. The noise-suppression button carries what the browser *reports* it applied, which is
  not always what was asked for, and says `asked for on` where the browser reports nothing.

Every control is a real `button`, `input` or `select`, reachable and operable from the keyboard, with
a visible focus ring, an accessible name that changes with its state, and `aria-pressed` /
`aria-checked` wherever it toggles. Escape closes the audio menu and returns focus to the button that
opened it. `prefers-reduced-motion` turns the speaking pulse off.

The bench is one press away rather than always on, and while it is open **it owns the video**: the
bench tiles attach the tracks and the stage does not. A track attached in two places is a track
either renderer can detach from under the other, so only one of them ever holds it.

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
Not the demo's HTTP port, and not the signalling port: signalling is TCP on 7880, and shaping it
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

Changing a processing setting or switching input device replaces the local `MediaStreamTrack` behind
the publication, and a meter left on the old one would report silence for ever. The capture module
announces the swap as `microphonetrack`, and the page re-attaches the analyser to the new track on
that event. Where no analyser could be built at all — no `AudioContext`, or a track that is not
subscribed — the participant is painted as unknown rather than as quiet.

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
