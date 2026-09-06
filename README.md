# mediacore

A selective forwarding unit written in Rust. The signalling protocol is compatible with LiveKit
clients, so existing `livekit-client` SDKs for web, iOS and Android connect to this server without
modification. Our own Rust and TypeScript SDKs are planned as additional clients speaking the same
wire protocol.

## Status

Early. Two participants in a room can see and hear each other: media published to the server is
forwarded to everyone who is subscribed to it, a simulcast publisher's layers are picked apart so
every subscriber gets the one it can use, and — once `video.bwe.enabled` is turned on — the layer a
subscriber gets is the one its own link can carry.

| Milestone | Scope | State |
|---|---|---|
| M0 | Protocol profile, vendored protobufs, workspace | done |
| M1 | Signalling, auth, room and participant state | done |
| M2 | Publisher transport, RTP intake, NACK, PLI | done |
| M3 | Subscriber transport, packet forwarding | done |
| M4 | Simulcast, layer selection, dynacast | done |
| M5 | Bandwidth estimation, allocation and pacing | done, off by default |
| M6 | Data channels, permissions, own SDKs | not started |
| M7 | Node registry, signal relay, TURN, drain | not started |
| M8 | Metrics, load and interop testing | not started |

A client that connects today receives a `JoinResponse`, sees other participants join and leave,
answers pings, is displaced when the same identity joins again, and is disconnected with a stated
reason after 60 seconds of silence. It can add a track, offer, complete ICE and DTLS against the
server's ice-lite agent, and send SRTP that the server receives, decrypts and accounts for. Other
participants see the published track appear, change and go away in their participant updates.

Received RTP is forwarded. A participant gets a second peer connection — the subscriber transport —
the first time it actually needs one; the server is the offering side there, sends the offer as
`SignalResponse.Offer`, takes the client's `SignalRequest.Answer` and trickles candidates in both
directions. Subscription intent is desired state: a reconcile pass compares what a participant wants
against what it has and adds or removes one media section at a time. A client joins with
`auto_subscribe` on by default and is subscribed to every track it is permitted to receive,
including tracks published later. Every packet is rewritten into a sequence number and timestamp
space of its own per subscriber, so two subscribers of one track never share numbering, and an
unsubscribe followed by a resubscribe continues the space the subscriber already had rather than
jumping. A keyframe request from any subscriber becomes a PLI to the publisher, throttled to one
every 500 ms and aimed at the layer that subscriber is on, and a new subscription asks for one as
soon as there is a layer arriving to ask. A muted publisher is never asked at all, and neither is a
paused subscription; asking is bounded to 20 tries for one need, so a publisher that has gone quiet
is not poked for ever. The count starts again whenever something changes that asking could fix: a
new target layer, an unmute, a resume, or the publisher restarting its stream. LiveKit throttles the
same way on the receiver side (`PLIThrottleConfig`, 500 ms for the low layer and a second for the
others) and bounds its gratuitous requests by a 30 s key frame seeder that stops as soon as a
keyframe arrives; its mute path stops the requester because a muted forwarder reports no layer to
ask on.

The subscriber is told the stream is active with `StreamStateUpdate`, and refusals come back as
`SubscriptionResponse`. The publisher is told its track has a subscriber with `TrackSubscribed`.

A simulcast publisher declares its encodings in `AddTrackRequest.layers`; those layers are carried in
`TrackInfo` and then kept current. Each negotiated rid becomes a spatial layer — by the quality the
publisher declared for it, falling back to the well known `q`/`h`/`f` and `2`/`1`/`0` ladders and
then to the order the rids were offered in — and a stream tracker watches which of them are actually
arriving. What the publisher declared is kept as one ladder, and both directions are read off it: the
quality a layer is reported and asked for under, and the layer a subscriber's `quality` request lands
on. A publisher that only sends `MEDIUM` and `HIGH` therefore has its medium encoding at the bottom
rung, and a subscriber asking for `MEDIUM` is put on that rung rather than on a middle layer that
does not exist. LiveKit derives both directions from the same declared layers as well
(`SpatialLayerToVideoQuality` and `VideoQualityToSpatialLayer`). A quality below everything published
is answered by the smallest rung and one above it by the largest. When the declaration cannot be
lined up with the rids at all — a different number of nameless layers than rids offered — nothing is
guessed: the declaration is logged and dropped for the rids it cannot name, and those layers keep the
well known ladder with no size. A rid the publisher did name is always believed, however many layers
it declared. A track that carries no rid is one layer described by the top rung declared for it.

`TrackInfo.layers` reports the layers that are arriving, each with the SSRC it arrived on, and the
declared ladder is what is reported until the first packet lands. A layer that goes quiet is
dropped from the list and re-appears when it comes back, so a subscriber is never offered a quality
nobody is sending.

Every subscriber picks its own layer. `UpdateTrackSettings` is now acted on in full: `disabled`
pauses the stream, a non-zero `width`/`height` picks the smallest layer whose shortest side covers
90% of what was asked for, and `quality` is used when no size is given — the same precedence the
reference uses. Settings that arrive before the subscription is bound are remembered and applied when
it binds. Nothing is answered back: like the reference, the server sends no reply to
`UpdateTrackSettings`, only a `StreamStateUpdate` when the paused state actually changes.

A layer switch never breaks the stream the subscriber already has. Packets of the layer being left
keep being forwarded until the new layer produces a keyframe, the first packet of the new layer
continues the subscriber's own sequence number and timestamp space, and a keyframe request goes to
the publisher naming the rid that subscriber needs rather than the track — asking the wrong layer for
a keyframe is the failure this milestone is most exposed to. A layer that is not arriving is never
asked; the request falls to the layer that is, which is the same overshoot the selector makes. The
500 ms throttle is counted per layer. Downgrades are immediate; an upgrade that comes from a
layer re-appearing is held for `video.layer_upgrade_hold_ms` (2 s by default) so a flapping layer
cannot pull a subscriber back and forth. A quality a subscriber asks for itself is never held.

Dynacast closes the loop back to the publisher. The union of what every subscriber of a track wants,
closed downwards, is the set of layers the publisher is asked to send, and it is sent as
`SubscribedQualityUpdate` — always three entries, `LOW` through `HIGH`, enabled up to the highest
layer wanted, as the reference sends them. The timing is deliberate and asymmetric: **enabling is
immediate**, so a layer is asked for before a subscriber needs it and never after, while
**disabling waits `video.dynacast_pause_delay_ms`** (5 s by default, LiveKit's `dynacast_pause_delay`)
so a subscriber changing its mind twice does not stop and restart the publisher's encoder. Demand
that returns inside the delay cancels the pause and nothing is sent at all. A track nobody has
subscribed to yet goes through the same disable path, so a publisher that no one is watching is told
to stop after one delay rather than immediately. On the way in, a disabled layer also has its NACKs
suppressed on the publisher transport, and a re-enabled one is asked for a keyframe on that rid the
moment it is turned back on. Only that direction restarts the stream tracker for a layer: a layer
being paused is still arriving for a while, and forcing it to look stopped would make the reported
layer list bounce. A decision counts as applied only once the publisher has actually been told; an
announcement that could not be delivered — a full or closed signal queue, a publisher that has left —
leaves the decision uncommitted and is made again on the next pass, so the publisher cannot be left
disagreeing with the server. Announcements always carry the state committed at the moment they are
sent, so two decisions taken back to back cannot reach the publisher out of order.

Bandwidth estimation, allocation and pacing are behind `video.bwe.enabled`, off by default. Turned
on, every **subscriber** transport is built with str0m's congestion controller, which is also what
installs its leaky-bucket pacer; publisher transports get neither and keep sending only RTCP. The
estimate is str0m's and the pacing and probing are str0m's — we write neither — but the estimate as
str0m exports it is a three-second moving average with five percent hysteresis, which is a fine
budget and a useless alarm. So the reading of the link is ours, and it turns on one question: what
counts as evidence that the link got worse, as opposed to evidence that we chose to send less, or
that a lossy link is merely lossy?

Only one observable answers it. A backlog in the pacer's send queue is the difference between what we
handed the transport and what the transport got out, and nothing else we can measure has that
property. The estimate does not: it is bounded by what we actually put on the wire, so it falls when
we cut on purpose and it falls on an idle link, and str0m's loss controller drags it down on a link
that is fast but lossy — a fifth of packets lost pins it near `100 kbps / (loss − 0.05)` whatever the
link is really worth. The loss ratio does not either: a link that drops a fifth of everything drops a
fifth whether we offer a hundred kilobits or five megabits, so loss is a fact about the link and not
about our offer. So **the queue is the only signal that raises pressure**; the estimate and the
weighted loss ratio are recorded, logged and used to size and corroborate, and neither can start a
congestion verdict on its own.

That leaves a three-state interpreter (`Clear` / `Hold` / `Congested`) sampled once a second. One
sample of backlog holds; two consecutive congest; three consecutive samples of relief return a hold
to clear. Relief is unanimous: a signal under pressure the queue does not corroborate is discounted
to indeterminate rather than laundered into recovery, so a drained queue beside a fifth of the
packets dropped is not three samples of a link that got better. Both non-clear states are claims that need renewing: a hold nothing escalated within
`5 s` lapses back to clear, and a congestion claim no backlog has renewed for `5 s` lapses too. The
doors in are strict and consecutive in both cases, the doors out are the same lapse in both cases,
and there is no second, weaker way into congestion — a link that dips on alternating samples cycles
between clear and hold, where the budget below keeps working, rather than being escalated by
evidence the front door would have refused.

There is a committed budget in every state, not only under congestion, and on a clear link it is the
primary control loop rather than a formality. A subscriber's first budget is `initial_kbps` — the
same number str0m's estimator is built with, so the allocator and the pacer start from one story and
a subscriber joining a slow link is capped from its first allocation instead of being handed
everything for a second. From there the budget tracks the estimate, floored at `min_budget_kbps`: up
without argument, and down only when the new figure is a real reduction against what we are actually
spending — under `commit_percent` of it — which is what stops a quiet moment, whose estimate is low
merely because we are sending little, from costing the room a quality step. Following the estimate
down on a clear link is not a concession; it is what keeps our offer inside what the pacer will
drain, and therefore what stops the backlog that would otherwise be read, correctly, as congestion.
The first cut is sized from the fast evidence that triggered it — the unsmoothed estimate and what is
actually leaving the socket, whichever is lower — never from the three-second average, which lags by
a whole rung. A cut that did not help is cut again every `max(2s, 5·rtt)` from what is actually
leaving, capped by the budget already in force. What proves a cut did not work is not what proved
the link was congested in the first place. The door in stays the backlog alone; but a bottleneck with
a shallow buffer answers an offer it cannot carry by dropping rather than by queueing, so a cut whose
symptom is loss would otherwise be made once and never repeated. The recut therefore asks for a
backlog **or** for weighted loss still over its pressure threshold — evidence that survives the cut —
and never for a falling estimate, which is bounded by what we chose to send and falls precisely
because the cut worked. Loss still cannot start a verdict: it can only decline to certify that one
the queue started has been discharged, and since nothing but a backlog renews the congestion claim, a
link that is merely lossy runs out of claim and lapses back to clear.

What the budget buys is decided per subscriber, over the tracks that subscriber actually receives.
Audio is never enrolled: it is never capped, never paused, and its bitrate is reserved off the top
before video is priced, with a floor of `video.bwe.audio_reserve_kbps` so a track whose owner is
silent still books the room it needs to speak. A subscription that cannot carry audio at all —
publisher muted, or the subscriber's own `disabled` — books nothing, so a large room does not reserve
a floor per participant for audio nobody is sending. Video is allocated layer-major — every track gets its
base rung before any track gets its second — in priority order, with a screen share outranking a
camera and a single-layer screen share paid in full first because it has no rung to fall back on.
Every layer is priced from what actually arrives from the publisher, windowed and referenced to the
wire rather than the payload, with the publisher's declared `VideoLayer.bitrate` and then a static
ladder behind it, so a layer that is momentarily unmeasured is never unallocatable. The wire
overhead is calibrated per subscriber at runtime from the gap between the bytes str0m put on the
socket and the RTP payload it counted.

The verdict reaches the forwarder as a second ceiling, never as a replacement for what the
subscriber asked for: an allowance caps the preference and can never raise it, so a subscriber that
asked for the small picture keeps getting the small picture, and a subscriber whose link recovers
returns to the layer it originally asked for without asking again. A bandwidth pause is a separate
flag from the subscriber's own `disabled`, and both go through the one door that keeps sequence
numbers continuous — a track paused for bandwidth and resumed continues the sequence space it had,
exactly as a client-disabled one does. `wanted_layer` deliberately ignores the allowance, so one
subscriber on bad wifi never pulls the publisher's encoder down and never degrades the room; the
cost is publisher uplink we do not forward, and the benefit is that recovery needs no renegotiation.
With `allow_pause` false — the default, and the reference's default — a squeeze steps down and, at
the bottom rung, overruns the estimate rather than blanking the picture. That overrun is bounded by
`video.bwe.max_overrun_percent`: refusing to pause is not a licence to hand the pacer two seconds of
queue with no move left, so past the bound the lowest-priority tracks are paused anyway, and the
highest-priority track keeps its base rung whatever the bound says. With `allow_pause` true the
allocator pauses at the first rung it cannot afford instead. Either way the subscriber is told, with
`StreamStateUpdate` carrying `PAUSED` and later `ACTIVE`, which is the one message the reference
sends for this and the only one we send. A mute is not a bandwidth pause and stays silent, as it
always did. A track with no rung published at or below the subscriber's own ceiling is not a
bandwidth decision at all: the allocator has nothing to choose between, so it leaves the selector to
overshoot to the lowest rung there is, exactly as it did before any of this existed, and books that
rung's price against the pool. Pricing it at zero would have been the same lie in the other
direction — the bits leave the socket either way. A track that *becomes* that way while it is holding
a cap — the last rung under its ceiling stops arriving and the tracker drops it — gives the cap back
on the spot, congested or not. Letting go is not a raise and must not wait for a clear link: the pool
is already being charged the rung the selector really forwards, and the alternative is a subscriber
capped for as long as the congestion lasts on a decision the allocator can no longer make.

Recovery is deliberately slower than reaction. Going down happens on the tick it is decided; going
up needs the link to have been clear for `video.bwe.recovery_hold_ms`, raises at most one track per
allocation tick, no track twice inside `video.bwe.recovery_step_ms`, and only when the new rung's
price fits in the budget beside what every other track is actually granted right now. Measuring that
room against a hypothetical in which every track had already been upgraded is what makes an
affordable upgrade look unaffordable forever, and one track per tick means no track ever moves; each
raise is re-measured against what the earlier ones actually took, so the allowance handed out on the
third tick knows about the two granted before it. A rung this subscriber has never been granted asks
for fifteen percent more than its price on top of that, and that margin is not a timidity tax:
prices are measured on ingress in payload bytes plus a calibrated header constant, they carry no
retransmission and none of str0m's padding, so a rung that books at ninety-one percent of the budget
really spends more than that, and a rung nobody here has paid for yet is a price we have never
checked against this link. A rung the subscriber already held — anything at or below the one it was
forwarding when the dip cut it down — asks for the room and nothing more. It is not speculation:
the link carried it, at a price we have since watched it pay. Charging the margin there would pin a
link one rung below what it can carry for ever, because the wish handed to str0m's prober is the
ladder itself, and an estimate that reaches the ladder is the most a link worth exactly the ladder
will ever report — no margin over that price could ever be met. Lifting a bandwidth pause is not
speculation either — the allocation pass has
already proved the track fits inside the budget — so a paused track resumes as soon as there is room
for it, without waiting for a return to `Clear`. The unconstrained wish — what the subscriber would take if
the link allowed it — is pushed to str0m as the desired bitrate so its prober has something to aim
at; without that call the estimate can only ever follow what we already send, and a subscriber that
once dropped to the base layer would never come back. That failure is invisible on a loopback and
permanent in production, which is why it has its own test.

Known gaps in M5: temporal layers are still not selected, so the smallest move available is a whole
spatial rung; the congestion signal is one second granular where the reference computes queuing
delay per packet group, so a capacity cliff is detected in about two seconds rather than a few
hundred milliseconds; retransmissions and str0m's padding are not attributed to any track's price;
and an audio-only subscriber never probes, because with our codec set nothing on its transport can
carry padding — the first video subscription restarts the estimator, which is what rescues it, once
per transport and not again every time a camera is republished. The
flag stays off by default until the constrained-link bench in the M5 design has been run.

Two more, found by verification and left deliberately. The fifteen percent of headroom an upgrade
must clear is inert on the ordinary path: a track remembers the highest rung it has ever been
granted, and that memory is seeded at construction from the rung the forwarder sends before the first
allocation tick, which is the top one, so the carve-out meant for restoring a rung after a dip applies
to every climb. Making the seed honest reintroduces the defect it was written to fix, because the wish
handed to str0m's prober is the ladder itself: on a link worth exactly the top rung's price a
multiplicative margin over that price is unreachable, since the estimate never exceeds what we send.
The way out is to probe above the ladder rather than at it, which is what probing is for, and it is a
change to make deliberately, with the four allocator tests that encode the present premise rewritten
alongside it. Second, only a backlog in the send queue starts a verdict; the estimate and the loss
ratio size cuts and decline to certify that a cut worked, but neither can originate one. That is the
intended rule, and its cost is that a bottleneck which drops rather than queues is detected later than
one that queues.

`JoinResponse.subscriber_primary` stays `false`. The subscriber transport is created lazily, so a
client that treats it as the primary connection and waits for it before reporting itself connected
would wait until it has something to receive. Turning it on means offering the subscriber transport
eagerly at join time, which belongs with session resume in a later milestone.

Known gaps in M4: temporal layers are not selected, only spatial ones; the VP8 payload descriptor is
not rewritten across a layer switch, which a browser subscriber may notice as a stall on a VP8 track;
`UpdateTrackSettings.fps` and `priority` are accepted and ignored; and with `video.bwe.enabled` off,
layer choice is driven by what the subscriber asks for and nothing else. Known gaps in M3: an
unsubscribe stops the flow and parks the media section rather than renegotiating it away, so a
resubscribe reuses it; and the subscriber never renegotiates on codec changes. Known gaps in M2: no
ICE restart, no receive-side estimation towards publishers, and `rtc.use_external_ip` is accepted
but not implemented.
Known gaps: session resume (`reconnect=1`) falls back to a full join, the `/rtc/v1` signalling path a
recent client tries first is not implemented so the client falls back to `/rtc`, and requests outside
the implemented subset are logged and ignored.

Verified against an unmodified `livekit-client` 2.22.2 in a real browser: two participants join,
one publishes audio and video, the other subscribes, and media arrives. Audio forwarded one packet
for one, with nothing lost. The browser only turns simulcast on above a small resolution, so the
simulcast tests drive `crates/client`, which publishes the encodings deliberately.

## Protocol profile

The server implements a subset of the LiveKit signalling protocol and advertises protocol version
17. Requests outside the subset are logged and ignored rather than answered with an error, so a
client that sends them stays connected. The vendored `.proto` files under
`crates/proto/protobufs` are copied from `livekit/protocol` and are the single source of truth for
the wire format.

## Running

```
cp config-sample.yaml config.yaml
cargo run -p mediacore-server -- --config config.yaml
```

The config path also comes from `MEDIACORE_CONFIG`. Two sockets are opened: TCP `port` for
signalling and one UDP socket for all media.

`video.dynacast_pause_delay_ms` is how long a layer nobody wants is left running before the publisher
is told to stop it, and `video.layer_upgrade_hold_ms` is how long a layer that re-appears must keep
arriving before a subscriber is moved back up onto it. Both only delay taking something away or
moving up; nothing that a subscriber needs now waits on them.

`video.bwe` is the bandwidth block, off by default and documented field by field in
`config-sample.yaml`: `initial_kbps` is what the estimator starts from and what the allocator spends
until the first estimate replaces it, and `max_desired_kbps` the
ceiling on what it is asked to probe for, `min_budget_kbps` the floor a committed budget never falls
below, `audio_reserve_kbps` what is taken off the top for each audio subscription,
`allocation_interval_ms` how often the budget is spent, `allow_pause` whether a squeeze may pause a
track before it overruns and `max_overrun_percent` how far the overrun may go when it may not,
track instead of overrunning, `overhead_bytes` the starting point for the per-packet wire overhead,
and `recovery_hold_ms` / `recovery_step_ms` / `queue_pressure_ms` the hysteresis. Turning `enabled`
on changes egress timing for every subscriber for the life of the process: str0m's pacer replaces
the null one in the same switch that creates the estimator.

The media socket binds `rtc.bind_address` on `rtc.udp_port`, or on the first free port in
`rtc.port_range_start..=rtc.port_range_end` when `udp_port` is `0`, or on an ephemeral port when the
range is `0` as well. The address put into ICE candidates is the first of: `rtc.advertise_addresses`
(an ip, or an `ip:port` to override the bound port), `rtc.node_ip`, the socket's own address when
`rtc.use_socket_address` is set and the bind address is not a wildcard, and otherwise the address of
the interface holding the default route. A wildcard bind never advertises the wildcard.

Endpoints:

- `GET /` — service name
- `GET /healthz` — room, participant, RTP and forwarding counters, whether bandwidth estimation is
  on and how many subscribers it is allocating for, and the ICE socket address
- `GET /rtc/validate?access_token=…` — token check, returns the identity
- `GET /rtc?access_token=…` — signalling WebSocket. `auto_subscribe=0` joins without subscribing
  to anything; it is on by default.

Tokens are standard LiveKit access tokens: HS256, `iss` is the API key, `sub` is the participant
identity, and the `video` claim carries the grants. Any LiveKit token generator works, including
`lk token create`.

## Deployment

`cargo run` above is a development server on plain HTTP, which a browser will
not give a microphone to: `getUserMedia` needs a secure context, and a page that
is not one cannot open the signalling WebSocket either. Plain HTTP is enough to
run the tests and nothing else.

`deploy/` is the deployable shape — Caddy terminating TLS in front of the SFU
and of `crates/demo`, a single-page audio room that serves the page and mints
tokens. Three containers, one host, one URL to send people:

```
cd deploy
cp .env.example .env
$EDITOR .env
docker compose up -d --build
```

**[deploy/README.md](deploy/README.md)** is the full account: what each variable
does, how to get a certificate on a host with no domain name (use
`<ip>.sslip.io`, not the bare IP), and how to tell apart the three failures that
cover nearly every silent room. `crates/demo/README.md` documents the demo app
itself, and the root `Dockerfile` builds both binaries.

## Layout

| Crate | Contents |
|---|---|
| `crates/proto` | protobuf definitions and generated types |
| `crates/auth` | access tokens and grants |
| `crates/config` | YAML configuration |
| `crates/room` | rooms, participants, tracks, subscription intent, signalling fan-out |
| `crates/transport` | ICE, DTLS-SRTP, RTP intake and egress, one task per peer connection |
| `crates/sfu` | packet forwarding: sequence and timestamp rewriting, keyframe gating, layers |
| `crates/client` | a programmable client that publishes real RTP, used by the tests |
| `crates/server` | HTTP and WebSocket endpoints, binary |
| `crates/demo` | the browser demo: one page, an audio room, a token endpoint |

`crates/room` owns all shared state and knows nothing about transport; `crates/transport` speaks
str0m types and knows nothing about the LiveKit protocol; `crates/sfu` is pure packet arithmetic
with no IO and no async; `crates/server` owns the sockets, the reconcile pass and the protocol
mapping between them.

## Development

```
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all
```

The protobuf definitions are compiled by `protox`, so no system `protoc` is needed.
