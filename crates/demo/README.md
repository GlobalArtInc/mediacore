# mediacore-demo

A single-page room. Someone opens the URL, types a name, and hears everyone else in the room.
Audio only by default; from inside the room you can choose the microphone, choose which noise
suppressor runs -- the browser's, ours, or none -- change the rest of the browser's audio
processing, share a screen, and tick one box in the lobby to publish the camera with simulcast. Behind the **Bench** button in the control bar is the bandwidth bench — per remote
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
| `GET /denoise.js` | our denoiser: the support probe, the audio graph, the overload watch |
| `GET /denoise-probe.js` | the two AudioWorklet processors that measure the node; loaded by `addModule`, not imported |
| `GET /denoise-gate.js` | wraps the vendored processor so it says when its wasm is live; loaded by `addModule` *before* it |
| `GET /NOTICE.txt` | the third-party licences of everything under `assets/vendor/`; linked from the page |
| `GET /vendor/livekit-client-<version>.esm.js` | the browser SDK, also compiled into the binary |
| `GET /vendor/rnnoise-<version>.wasm`, `-simd-<version>.wasm` | RNNoise, `application/wasm`, `immutable` |
| `GET /vendor/rnnoise-worklet-<version>.js` | the worklet that drives it |
| `GET /api/config` | the default room and the public WebSocket URL, for the lobby |
| `POST /api/token` | `{"name": "...", "room": "..."}` in, `{"url","token","room","identity","name"}` out |
| `GET /healthz` | `ok` |

A refused request answers `400` with `{"error": "..."}`; that string is what the visitor is shown.

Every response carries `Content-Security-Policy: default-src 'none'; script-src 'self'
'wasm-unsafe-eval'; … connect-src 'self' <MEDIACORE_DEMO_WS_URL>`, plus
`X-Content-Type-Options: nosniff` and `Referrer-Policy: no-referrer`. The policy is built from the
configured WebSocket URL at startup, so the one cross-origin connection the page is allowed is the
signalling socket and nothing else.

`'wasm-unsafe-eval'` is there for the denoiser and buys exactly one thing: WebAssembly compilation.
It is not `'unsafe-eval'`, it does not allow inline script, and it does not widen where a script may
come from -- that is still `'self'` and nothing else. Chrome refuses `WebAssembly.instantiate`
outright without it, so the alternative was not a tighter policy but a dead feature.
`the_policy_buys_wasm_and_nothing_else` in `src/http.rs` fails if anything else is ever added
alongside it.

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
| `/denoise.js` | 24 436 | **every load** |
| `/vendor/rnnoise-worklet-0.4.0.js` | 64 389 | only once it is switched on |
| `/denoise-probe.js` | 11 372 | only once it is switched on |
| `/denoise-gate.js` | 3 928 | only once it is switched on |
| **every load** | **181 670** | |
| **once it is switched on** | **261 359** | |

Most of it is not conditional. The wasm is fetched on **every** page load whether or not anybody
turns the denoiser on, because the gate that decides whether to offer the option at all validates
the binary rather than assuming it, and a gate that assumed would be the dishonest kind. The plain
build (152 656 bytes) is fetched instead of the SIMD one on a browser without SIMD; never both.

For comparison the page already carries `/vendor/livekit-client-2.22.2.esm.js` at 1 365 927 bytes.
Against that, the denoiser adds about an eighth on every load and about a fifth once somebody turns
it on. The "about 150 KB" this file used to quote was the wasm alone, and it called the rest
conditional: a load that uses the denoiser fetches 261 359 bytes, seventy per cent more than that,
and 181 670 of them are fetched whether anybody uses it or not.

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
file sitting in the repository does not. `cargo test -p mediacore-demo` fails if the notice stops
being compiled in, stops being served, stops naming any of the four, or stops carrying the licence
texts.


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
like one. The page can put a real denoiser in front of the encoder instead: **RNNoise**, compiled to
WebAssembly, driven from an `AudioWorklet`.

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

Three gates, and every one of them lands on the browser's own suppressor rather than on a broken
track or on silence.

1. **Before it is offered.** The page probes once at load: Web Audio, `AudioWorklet`, WebAssembly,
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

992 is measured, not derived. `cargo test -p mediacore-demo` drives the vendored worklet and the
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

## The interface

The room is the shape a voice chat has settled into, and nothing more: a persistent list of who is
present down the left, the people or the shared screen in the middle, and one bar of controls along
the bottom holding mute, the microphone chooser, the noise-suppression control, screen share,
camera, the bench and leave. The noise control is one button naming the engine that is running —
pressing it steps through the values on offer — and the audio menu behind the chooser holds the
same setting as a radio group with a sentence about each. When somebody is sharing a screen it takes the main area and the participants
shrink to a strip beneath it; when nobody is, the participants fill it. Two people with the same
name keep the last four characters of their identity beside it, as they always have.

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
`aria-checked` wherever it toggles; the noise engines are a `radiogroup`, and the one this machine
cannot run is disabled with the reason on hover rather than silently doing nothing. Escape closes
the audio menu and returns focus to the button that opened it. `prefers-reduced-motion` turns the speaking pulse off.

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

**The corner of a shared screen**, outside the bench and always on while a screen is on the stage:
`frameWidth×frameHeight` and the bitrate differenced over the sample, from the `inbound-rtp` entry
for that track, or from the `outbound-rtp` entries joined to that track through `media-source` when
the share is your own.

**The microphone denoiser panel**, when ours is in the path — described in the denoiser section
above.

## Testing it

`cargo test -p mediacore-demo` runs everything, and the split between the two halves is deliberate.

**The Rust tests are statements about bytes**: that each artefact is served, with its version in the
path and the right content type, that the module graph the page imports resolves to routes this
binary answers, that the `.wasm` files really are WebAssembly modules, that the licence notice is
compiled in and served and names every project it covers, and that the content security policy still
forbids what it should. A test that a file *contains a phrase* is not a statement about behaviour and
none is written here: a substring survives every change that matters, so it can only ever fail for
the wrong reason.

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
- `stage.js`, which is where the stage's decisions live precisely so they can be argued with here:
  that a pinned share survives the room being reordered under it, that a share ending names whose it
  was, that a browser with no fullscreen and one that refuses and one that silently ignores the
  request all land on the path that fills the window, and that the corner badge prints a half it has,
  nothing at all when it has neither, and never `0×0`.
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
