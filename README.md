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
answers pings, and is disconnected with a stated reason after 60 seconds of silence. It can add a
track, offer, complete ICE and DTLS against the server's ice-lite agent, and send SRTP that the
server receives, decrypts and accounts for. Other participants see the published track appear,
change and go away in their participant updates.

Rooms exist because somebody made them, not only because somebody walked in. A room can be created
before anybody arrives and carries settings of its own — metadata, participant limit, empty and
departure timeouts — which override the `room` block for that room alone. Creating a room that is
already there returns the one that is there and changes nothing about it, so a caller retrying a
request cannot rewrite a room people are already sitting in. Creating one that exists only because
somebody walked into it is the other case: that room took the server defaults because nobody had
asked for anything, so the request wins — the settings apply, the room stops closing itself when it
empties, and whoever is already inside is told the new metadata in a `RoomUpdate`. That last one is
asked of a token that names the room, because `roomCreate` names none and settling a meeting that is
already running should not be something a server-wide token does by accident; see
[Server API](#server-api). With `room.auto_create` off, joining a room nobody created is refused,
which is what makes it possible for only the rooms an operator knows about to exist. A room somebody
created outlives the last person to leave it; a room that exists only because somebody walked in is
closed when they walk out, as before. Deleting a room is not dropping a
map entry: everyone in it is sent a `Leave` naming `ROOM_DELETED` first, and removing one participant
sends it `PARTICIPANT_REMOVED` and tells the rest of the room the same reason, on the same path a
displaced session already used.

A created room does not live for ever either. `empty_timeout` is how long it is kept before anybody
has ever joined it and `departure_timeout` how long after the last person leaves — LiveKit's own
division, and both are enforced rather than only reported. A room that exists because an arrival
opened it still goes the moment they walk out, which is a departure timeout of zero; either timeout
set to zero means such a room is never collected and `DeleteRoom` is the only way to be rid of it.

A session can also end by policy rather than by emptiness. `room.min_humans` is how many **people** a
room needs; when fewer than that are left, the room ends `room.end_grace` seconds later and whoever
is still in it is disconnected with `ROOM_CLOSED`. Somebody arriving inside the grace period calls it
off. A recorder or an agent is not a person — the `recorder` and `agent` grants, and a token's `kind`
claim, become `ParticipantInfo.kind`, and a room holding nothing but those ends on schedule, which is
the difference between this and waiting for the room to be empty. The grace only ever starts on the
way down: a room that has not yet had `min_humans` people in it is waiting for them, not losing them.
The policy is off at `min_humans: 0`, so upgrading changes nothing, and per-room settings can turn it
on for one room without turning it on for the server. Switching a policy off — `minHumans: 0` through
`UpdateRoomMetadata` — takes any countdown already running with it.

Both timeouts and the policy hang off one clock. The registry sweeps itself every
`room.sweep_interval_ms`, a second by default, whether or not anybody is connected, and that is the
only thing in the server that ends a room nobody is touching. It has to be a clock of its own,
because the grace is armed by the departure that drops a room below its floor and the room can empty
completely before it runs out: driven from a participant's session, the sweep would then have nobody
left to drive it, and the deadline would sit in the past until somebody opened the room the next
morning — whose own first tick would end the meeting and disconnect *them* with `ROOM_CLOSED`. A
grace period is a promise to the people who were in the room, so a room counting one down is not
idle: the twenty-second departure timeout does not collect it out from under a ninety-second grace.
It ends when it said it would, and with nobody in it there is nobody to disconnect.

**A meeting can be asked about after it has happened.** A room is a place; a session is a meeting
held in it. A room created over the API outlives its meetings and holds one after another, and a room
that exists only because somebody walked in goes when they leave — while what happened in it does
not. A session starts when a room reaches the number of people it needs and ends when it falls below
and the grace period runs out, and its record carries an id, the room, a start, an end and everybody
who was there with the times they arrived and left. `GET /api/rooms/<room>/sessions`,
`GET /api/rooms/<room>/session` and `GET /api/sessions/<id>` answer for it; see
[Sessions](#sessions).

None of that waits for an operator to opt in. With `room.min_humans: 0` — the default, and what the
deployment runs — the policy is off and the floor is one person, so a meeting is still one stretch of
a room being occupied, from the first person arriving to the room emptying. `room.end_grace` still
applies to it, which is what makes somebody whose network blinked one meeting with a gap in it rather
than two; `end_grace: 0` ends a meeting the instant its room empties, for a deployment that would
rather have that.

Records are on disk, one JSON file per meeting under `sessions.directory`, written as the meeting
goes on rather than emitted once it is over. An event is a notification that state changed and never
the only copy of the fact, so a consumer that was down at half past six has to be able to come back
and ask what it missed — and records that live in memory die with the process and take that guarantee
with them. What a restart costs is the *end* of a meeting that was still running: nobody observed it,
so the record is closed at the last arrival or departure written into it and says
`"endedBy": "restart"` rather than pretending to a time it does not know.

**And it can be heard about as it happens.** Six events — `room_started`, `room_finished`,
`participant_joined`, `participant_left`, `session_started` and `session_finished` — go to whoever
has asked to be told: an emitter inside the process, webhooks in LiveKit's shape and with LiveKit's
signing, and a Kafka topic behind a Cargo feature that is off by default. An event is written down
before it is attempted and struck off only once every destination has taken it, so an endpoint that
is away for two minutes costs latency rather than two minutes of events. Nothing about any of it is
on the path somebody joining a room walks: emitting opens no file, makes no request and waits for
nothing. Nothing is configured by default, either — with no endpoint and no broker there is no
outbox, no task and no cost. See [Events](#events).

One person can be in a room twice. `livekit-client` keys its remote participants by identity
throughout, so two participants sharing one identity are invisible to every client, ours included;
the server therefore gives the second connection an identity of its own — `alice`, `alice#2`,
`alice#3` — keeps the display name exactly as the token minted it, and tells the client which
identity it got in the `JoinResponse`, which is where a client learns who it is anyway. The suffix
is skipped past any identity already in the room, so a token that literally says `alice#2` keeps it.
That is `room.duplicate_identity: suffix`, the default; `displace` throws the earlier session out
with `DUPLICATE_IDENTITY`, which is LiveKit's own behaviour and still available. Under either rule a
client that reconnects naming the session it had — `reconnect=1&sid=…`, which `livekit-client` sends
on its own — takes that session's identity back rather than arriving as a second person and leaving
its own ghost behind; a claim on a session belonging to another identity is ignored.

The suffix is what a client is told, not what the server reasons with. Every connection remembers the
identity its token asked for, and that — an exact comparison, never a parse — is what "the same
person" means, both for a reconnect taking a session back and for a moderator naming somebody.
`#` followed by digits is not a reserved namespace: `bob#7` is an identity a backoffice may
legitimately mint, and participant sids are broadcast to the whole room in every roster, so reading
the ordinal back out of the identity string would let anyone holding a plain `bob` token reconnect
into the real `bob#7`'s session and evict them with `DUPLICATE_IDENTITY`. Names are matched, never
taken apart.

What suffixing costs is worth stating, because it is a limit rather than a bug. Displacement used to
be how half-dead sessions were cleaned up. A client that loses its network and comes back without
`reconnect=1&sid=` — a second device, a rejoin button, an SDK that gave up and started over — now
leaves its old session sitting in the room until the 60-second signal timeout collects it, and
returns as `alice#2`. `RemoveParticipant` covers this because it names the person and takes every
connection they have; `GetParticipant`, `UpdateParticipant`, `MutePublishedTrack` and
`UpdateSubscriptions` each name one connection, so for that minute a backoffice muting `alice` gets
`not_found` while alice is the only person in the room. Shortening the timeout would disconnect
people whose network is merely slow, so this is documented rather than closed;
`room.duplicate_identity: displace` is there for a deployment that would rather have LiveKit's rule
and its ghost-free rejoin.

There is a server API as well as a signalling one. `mediacore.RoomService` — LiveKit's room service,
method for method, which its own server SDKs and `lk` speak — answers at
`POST /twirp/mediacore.RoomService/<Method>` in JSON or protobuf, and also at
`POST /twirp/livekit.RoomService/<Method>`, so a backoffice that already drives LiveKit drives this
without a line changed. Rooms can
be created, listed and deleted; participants can be listed, read, renamed, given or refused
permission, muted, subscribed and unsubscribed on their behalf, and removed. The three methods that
only make sense across several nodes — `ForwardParticipant`, `MoveParticipant`, `PerformRpc` — and
`SendData`, which needs a data channel we do not have yet, answer `unimplemented` rather than
pretending. The whole surface, the grants each method wants and the parts that behave differently
from LiveKit are under [Server API](#server-api).

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
Known gaps: session resume (`reconnect=1`) falls back to a full join — the returning client takes
back the identity of the session it names, but nothing else about it — the `/rtc/v1` signalling path a
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

They are copied byte for byte, upstream's `package livekit;` included, and `crates/proto/build.rs`
renames that package to `mediacore` on the compiled descriptors before a line of Rust is generated.
So the types are `mediacore_proto::mediacore::ParticipantInfo` rather than `…::livekit::…`, and the
room API is served at `/twirp/mediacore.RoomService/` — both because the descriptors say so, and
neither because anything rewrites a name on the way out. Re-vendoring stays a straight copy with a
diff against upstream that is honest, which is the whole reason the rename is not in the `.proto`
text; `crates/proto/protobufs/README.md` says so where somebody dropping a new file in will see it.
Nothing about the bytes changes: protobuf sends field numbers, not names, and the one construct that
would put a fully-qualified type name on the wire, `google.protobuf.Any`, appears nowhere in the
schema. The Twirp URL was the only place the package was ever visible outside the process, which is
why that is the only place the old name survives.

Both wires are generated from them. The signalling socket carries the types as protobuf; the server
API carries the same types as protobuf or as protobuf's JSON mapping, whose serialisers are generated
beside the Rust ones from the same descriptors. Neither wire is written by hand, so neither can drift
from the `.proto`.

## Server API

`mediacore.RoomService` is where a backoffice, a bot or `lk` talks to the server rather than to a
room. It is Twirp: `POST /twirp/mediacore.RoomService/<Method>`, one body in, one body out, no
streaming. All of it is also a page with buttons on it, at [`/swagger`](#the-page-at-swagger),
generated from the same protobuf descriptors the wire is.

**It answers to a second name, and that one is not optional for LiveKit's SDKs.** Twirp builds its
path out of the protobuf package, and LiveKit's server SDKs and `lk` build it from *their* package,
in generated code, with no setting to point them elsewhere. So every method also answers at
`POST /twirp/livekit.RoomService/<Method>`. It is the same axum handler reading the same
`rooms::METHODS` table, so the two paths cannot come to disagree about which methods exist, what
grant each one wants or what a failure looks like; the only difference is that a `bad_route` names
whichever service the caller posted to, because telling somebody about a path they did not use hides
their typo behind a name they have never seen.

The alias is on by default and costs nothing to leave on:

```yaml
twirp:
  livekit_alias: true
```

Turn it off and `/twirp/livekit.RoomService/…` stops being routed at all — not refused with a Twirp
error, but a bare 404 with no body, which an SDK reports as a transport failure rather than as a
method that does not exist. Every LiveKit-generated client pointed at this server breaks at once and
has no setting they can change to recover; they would have to regenerate against our `.proto` or
drop to raw HTTP. Turn it off when nothing built against LiveKit calls this server any more and you
would rather the old name stop working loudly than linger.

A request carries `Content-Type: application/json` or `application/protobuf` and
`Authorization: Bearer <token>`, and the answer comes back in whichever the request used. The JSON is
protobuf's JSON mapping rather than a spelling of our own: field names in lowerCamelCase
(`maxParticipants`, `trackSid`), 64-bit integers as strings (`"creationTime": "1757155200"`), and a
field at its default value left out of the body altogether rather than sent as a zero. That is what
every LiveKit SDK writes and expects to read. A `charset` parameter on the content type is accepted,
which Twirp's own Go implementation does not do; anything else is refused.

A failure is a Twirp error, and always JSON whatever the request was — a caller we could not
understand cannot be assumed to read protobuf either:

```
HTTP/1.1 403 Forbidden
Content-Type: application/json

{"code":"permission_denied","msg":"this token is an admin of room `standup`, not of `payroll`"}
```

The status is the one the Twirp specification assigns the code, not one we chose: 400 for
`invalid_argument` and `malformed`, 401 for `unauthenticated`, 403 for `permission_denied`, 404 for
`not_found` and `bad_route`, 501 for `unimplemented`. An SDK turns the code back into a typed error
and everything between it and us decides from the status whether the call is worth trying again, so a
wrong one makes a permanent failure look retryable or the reverse. A method that does not exist, a
content type we do not speak and a request that is not a `POST` are all `bad_route`: in Twirp the
path and the content type together are what identify a call.

### What a token has to carry

An API token is signed with the same key as a join token and checked the same way, but the grant is a
different one — and a join token is worth nothing here, which is the point, because everybody in the
meeting holds one of those.

| Grant | Methods |
|---|---|
| `roomCreate` | `CreateRoom`, `DeleteRoom` |
| `roomList` | `ListRooms` |
| `roomAdmin`, scoped by `room` | everything else |

`roomAdmin` needs both halves. A token saying `{"roomAdmin": true, "room": "standup"}` is refused on
every other room with `permission_denied`, whether or not that room exists; without the scope an
admin token minted for one meeting would be a master key for the server. No grant stands in for
another: `roomCreate` builds rooms and does not moderate the people inside them.

A server API token has no identity on it. LiveKit's SDKs refuse to set one unless the grant is
`roomJoin`, so `sub` is simply absent from the token — it is not required here, and it is still
required at `/rtc`, where a participant without a name cannot join.

**What `roomCreate` and `roomList` reach.** Both are LiveKit's own semantics, kept as they are
because changing them would surprise every SDK written against LiveKit — but they are wider than
they look, and whoever mints these tokens should know what they are handing out.

- **`roomCreate` is not scoped by `room`, and it is also what `DeleteRoom` checks.** One token
  carrying it can create any room on the server and delete any room on the server, including a
  meeting in progress: everybody in it is disconnected with `ROOM_DELETED`. Whoever may bring a room
  into being may take it away again, and neither half asks which room. The one thing it cannot do is
  re-point a room somebody is already sitting in — see `CreateRoom` below.
- **`roomList` sees every room and the whole of every room record.** There is no filtering by the
  token's `room`, matching LiveKit's `EnsureListPermission`, and `Room.metadata` comes back with it.
  For a caller that keeps its meeting record in the metadata — which is where the session policy
  travels — that record is the metadata.

So an API token is a server credential and not a per-meeting one: mint them for services, keep them
away from anything a browser can reach, and give something that only needs one meeting a
`roomAdmin` token, which is the only grant here that is scoped.

### The methods

| Method | Grant | What it does here |
|---|---|---|
| `CreateRoom` | `roomCreate`, plus `room` to settle an occupied room | Creates the room, or adopts one an arrival opened; returns the room. Retried on a room that was already created, it changes nothing. |
| `ListRooms` | `roomList` | Every room, or the ones `names` asks for, sorted by name. |
| `DeleteRoom` | `roomCreate` | Everyone in it gets a `Leave` naming `ROOM_DELETED`. Deleting a room that is already gone succeeds. |
| `ListParticipants` | `roomAdmin` | Everyone in the room in the order they arrived, hidden participants included. |
| `GetParticipant` | `roomAdmin` | One participant by exact identity. |
| `RemoveParticipant` | `roomAdmin` | Removes the person, every connection of theirs; `PARTICIPANT_REMOVED` to them and to the room. |
| `MutePublishedTrack` | `roomAdmin` | Mutes or unmutes one track, tells the publisher's own client, and returns the track. |
| `UpdateParticipant` | `roomAdmin` | Renames, rewrites metadata and attributes, grants and revokes permission, hides and unhides. |
| `UpdateSubscriptions` | `roomAdmin` | Subscribes or unsubscribes somebody else's session on their behalf. |
| `UpdateRoomMetadata` | `roomAdmin` | Sets the room metadata and broadcasts a `RoomUpdate`. |
| `SendData` | — | `unimplemented`: there is no data channel yet. |
| `ForwardParticipant`, `MoveParticipant` | — | `unimplemented`: both move a participant between nodes, and this is one node. |
| `PerformRpc` | — | `unimplemented`: it calls a method on an agent over a data channel, and we have neither. |

A zero in a request is a field the caller left out. Proto3 cannot tell one from the other on a
scalar, and LiveKit reads them the same way, so `emptyTimeout: 0` on `CreateRoom` means "whatever the
server's default is" rather than "close this room the instant it empties". The consequence is that a
zero timeout cannot be asked for over this API at all; it has to come from the configuration file.

**`CreateRoom` on a room that is already there.** If it was created on purpose, it comes back
untouched: that call is a retry as often as it is a first attempt, and a retry must not rewrite a
room people are sitting in. `UpdateRoomMetadata` is how a live room's metadata and session policy
move. If the room exists only because an arrival opened it, the request wins and the room is settled
— it took the server defaults because nobody had asked for anything — but only for a token that also
names the room in its `room` claim. `roomCreate` alone names no room, and re-pointing the settings
and the session policy of a meeting that is already running is not something a server-wide token
should do in passing. Without that scope the existing room comes back as it is, with a 200 and its
own metadata in the answer, so a caller comparing what it sent against what it got can see that its
request was not applied.

**Fields we cannot honour are refused, not dropped.** `egress` asks for the meeting to be recorded,
`agents` for an agent to be dispatched into it, `roomPreset` for a bundle of settings kept on the
server, `replayEnabled` for a replay, `tags` for something to be stored and read back later. None of
those exist here, and a 200 to a request for a recorded meeting is exactly how a caller comes to
believe a meeting is being recorded when it is not — so `CreateRoom` answers `unimplemented` (501)
and the message names the fields it could not do. Nothing is created. `nodeId`, `minPlayoutDelay`,
`maxPlayoutDelay` and `syncStreams` are accepted and ignored on purpose: they are hints about node
placement and playout timing on a server with one node, not promises about the meeting.

**`RemoveParticipant` names a person, not a socket.** One person can hold two connections here —
`155592` and `155592#2` — so removing one and leaving the other would answer a moderator's button by
leaving the person in the room, still audible. Removing `155592` therefore removes every connection
that identity opened. A caller who wants exactly one names the suffixed identity, and gets exactly
that one. Every other participant method matches the identity exactly, because each of them reads or
changes a single participant and quietly picking one of several would be worse than a `not_found` the
caller can do something about — and `ListParticipants` reports the identity each connection actually
has, so there is always an exact name to use. `revokeTokenTs` is accepted and ignored: we do not keep
a revocation list, and a removed participant's token stays valid until it expires.

**`UpdateParticipant` is a patch.** A field left empty is a field the caller did not mention, so
renaming somebody does not blank their metadata. Attributes merge key by key and an empty value
deletes the key, which is LiveKit's own rule. An update that changes nothing announces nothing, so a
backoffice resending the same profile on every render does not flood the room.

Permission is the half that does more than store a flag, and a moderator control that half works is
worse than one that is missing:

- Taking `canPublish` away unpublishes the tracks the participant may no longer send, tells their own
  client with `TrackUnpublished`, **and drops the route from the forwarder** — the same call the
  client-initiated unpublish makes. Without that last step the publisher is still connected, so their
  RTP keeps arriving, keeps being forwarded to every subscriber the route already had, and keeps
  being counted, for the rest of their session.
- Taking `canSubscribe` away drops the intent behind their subscriptions, and clears their session's
  memory of what it has already auto-subscribed to — so giving the permission back re-subscribes
  them, instead of leaving them looking at a blank screen until they reload the page.
- Making somebody hidden reaches the room, not only themselves. Everybody else is told the
  participant disconnected, which is the only update that takes somebody out of a client's roster,
  and their tracks are dropped from everybody's subscriptions. Otherwise they stay on every screen
  for the rest of the meeting — including after they leave, because the room says nothing about a
  hidden participant departing.

**`UpdateSubscriptions` is checked whole before any of it is applied.** A track sid the room does not
have is `not_found` and a track the participant may not receive — their own, or anything at all once
`canSubscribe` is gone — is `invalid_argument`, and in both cases nothing is changed, so a caller is
never left guessing which half landed. What it writes is intent; the participant's own session
notices on its next reconcile, within a few seconds, and negotiates from there. Unsubscribing is not
undone by auto-subscribe: the refusal is recorded on the participant rather than in their session's
bookkeeping, so it holds whether the call arrives before that session first looked at the room or
after. Asking for the track again is how somebody takes a refusal back.

**The session policy travels in the room metadata.** `CreateRoomRequest` has no field for
`min_humans` or `end_grace` — LiveKit has no such policy — so the only way one can reach us through
an unmodified server SDK is inside the metadata the caller is already sending. A room record shaped

```json
{"sessionPolicy": {"minHumans": 2, "endGraceMs": 90000}, "recordingEnabled": true}
```

creates a room that ends ninety seconds after it drops below two people. `endGraceMs` is milliseconds
and rounds up to whole seconds; either half may be left out and falls back to the `room` block.
Metadata that is not JSON, or JSON with no `sessionPolicy` in it, is left completely alone — it is
the caller's opaque blob and it round-trips unchanged either way. `UpdateRoomMetadata` reads it
again, so a policy can arrive after the room did; metadata carrying no policy leaves the policy where
it is rather than silently dropping it. Nothing here reads `recordingEnabled`: recording is egress,
and there is none yet.

### Sessions

A **session** is one meeting; a **room** is the place it happened in, which outlives it and holds the
next one — so `/sessions` is plural on purpose. These three are plain `GET`s and not Twirp: LiveKit
has no session record, so there is no generated client anywhere that would call them and nothing to
be compatible with. They are on the `/swagger` page all the same, under **Sessions**, because that
page is where somebody integrating against this server looks.

| Endpoint | Answers |
|---|---|
| `GET /api/rooms/<room>/sessions` | `{"sessions": [ … ], "total": n}`, newest first, one page at a time; an empty list for a room that has never held one. |
| `GET /api/rooms/<room>/session` | `{"session": { … }}` or `{"session": null}`. No meeting in progress is an answer and not a failure, so it is 200 either way and a poller has one case to handle. |
| `GET /api/sessions/<id>` | `{"session": { … }}`, or `not_found` (404). |

They read the record rather than the registry, so a room that no longer exists still answers for the
meetings it held.

**Grants.** `roomList` reads every meeting on this server: it already returns every room and the
whole of every room record, metadata included, so a lock here would be a lock on a door with no wall.
`roomAdmin` reads the meetings of the room its `room` claim names, which is what lets a backoffice
hand out a token for one meeting and show that meeting's history without handing over the run of the
server. `roomCreate` reads nothing — it makes rooms and deletes them, and has never been a way to
look inside one, which is why `ListParticipants` refuses it too.

A refusal on the two endpoints that name a room is `permission_denied` (403); the caller supplied the
name, so being told they may not read it tells them nothing they did not know. A session the token
may not read is `not_found` (404) instead, byte for byte the same answer as an id that names nothing,
because telling a token scoped to one room that some id is real but forbidden is all it needs to walk
the id space and learn who met where.

**The list is paged.** `?limit=` (100 by default, 1000 at most; a larger number is capped rather than
refused) and `?offset=`, counting back from the most recent, with `total` on every answer so a caller
handed a full page knows whether there is another. A room a service has been making a meeting in
every morning for two years holds thousands of them, and one response carrying all of them would be
built by reading thousands of files with the lock that every arrival on this server also needs. The
other two endpoints answer with one meeting and take no window.

```
$ curl -s -H "Authorization: Bearer $TOKEN" \
    "http://127.0.0.1:7880/api/rooms/standup/sessions?limit=1"
{"total":37,"sessions":[{
  "id": "SE_3f9a1c04b7de",
  "room": "standup",
  "startedAtMs": 1757155200000,
  "endedAtMs": 1757156740000,
  "endedBy": "grace",
  "participants": [
    {"sid": "PA_a1b2c3d4e5f6", "identity": "alice", "name": "Alice", "kind": "standard",
     "joinedAtMs": 1757155188000, "leftAtMs": 1757156650000},
    {"sid": "PA_9f8e7d6c5b4a", "identity": "bob", "name": "Bob", "kind": "standard",
     "joinedAtMs": 1757155200000, "leftAtMs": 1757156650000}
  ]
}]}
```

Times are milliseconds since the epoch, as JSON numbers — these bodies are ours and not protobuf's,
so an `int64` is not wrapped in quotes here the way it is on the room service. One entry per
**stretch** of attendance rather than per person: somebody who dropped out and came back is two
entries, which is what makes the gap answerable at all, and one person's two tabs are two entries
under `alice` and `alice#2`. `kind` is `standard`, `egress` or `agent`, and only `standard` counts
towards the number of people a room needs — a recorder in the room does not make a meeting, and it is
in the record anyway, because "was this meeting recorded" is a question somebody asks a month later.

`joinedAtMs` can be earlier than `startedAtMs`: the first person into a room with a floor of two was
waiting for the second, and how long they waited is worth knowing. It is never earlier than the
previous meeting in that room ended, however long ago the connection actually arrived — a recorder
that sat through both halves of a room's evening has one arrival time and two meetings to appear in,
and carrying that one time into the second record would make its stretch cover the whole of the
first. One `sid`'s stretches never overlap, in one record or across two, so adding them up is the
thing it looks like it is. `endedAtMs` can be later than the
last `leftAtMs` by the length of the grace period, which is the window in which the meeting was still
theirs to come back to. `endedBy` is `grace` when nobody came back before it ran out — the ordinary
end — `closed` when the room was deleted under it, and `restart` when the server was still holding
the meeting when it stopped, in which case `endedAtMs` is the last moment the record knows about, a
lower bound and never a guess above it.

**Where they live.**

```yaml
sessions:
  directory: /var/lib/mediacore/sessions
```

One file per meeting, named by its id, rewritten in place as the meeting goes on: flushed and then
renamed over itself, so a reader — including the next start of this server — never sees half of one.
The directory is created at start and the server refuses to start if it cannot be, because a server
that answers with an empty list because it never managed to open its own directory is a failure
nobody discovers until they are asked what happened last week. There is no setting that turns
recording off. Nothing deletes a record either, and that is why the layout is one self-describing
file per meeting rather than a database: `find /var/lib/mediacore/sessions -mtime +90 -delete` is a
real answer for whoever wants a retention policy, and it costs nothing to whoever does not. In a
container the directory belongs on a volume — `deploy/docker-compose.yml` mounts one — or the history
ends at the last release.

**Written by a thread of its own, and what that costs.** A membership change is recorded in memory
and handed to a writer thread; nobody joining a room ever waits for a disk. What a *crash* costs on
top of the usual is a freshness window the width of that hand-off: a record may come back missing its
last few arrivals and departures, and a meeting that began inside the window may be missing
altogether. It never comes back saying something that did not happen — each file is written whole,
flushed and renamed, so a record on disk is always a state the meeting really passed through — and a
`SIGINT` waits for the queue, so a planned stop loses nothing. The trade is deliberate and this way
round: the flush used to happen on the thread that was letting somebody into a room, under a lock
shared by every room on the server, so one busy disk delayed joining everywhere at once. Freshness
measured in milliseconds is worth less than that.

**What it costs in memory.** An index of every meeting — id, room, start — and the full record only
of the meetings running, the ones waiting to be written, and the ones the disk refused. A record
nobody is at is read back from its file when somebody asks for it, so a server two years into a
deployment holds a few hundred thousand short strings rather than a few hundred thousand records.

`/healthz` carries `sessions=`, `live_sessions=`, `resident_sessions=`, `owed_sessions=` and
`unwritten_sessions=`. The last is the one worth an alert: a record that could not be written is
still served from memory until the process ends and is gone after that, and there is nowhere else an
operator would find out — it comes back down if a later write of the same record lands, so a disk
that hiccuped once is not reported for ever as a meeting that is going to be lost. `owed_sessions=`
is the freshness window: records recorded and not yet on the disk, normally zero, and however long it
stays up is what a crash would cost. `resident_sessions=` is the memory bound above, and an operator
watching it climb beside a flat `live_sessions=` is watching a leak.

### The page at `/swagger`

Everything above is on the server itself, at `/swagger`, with a button next to every method.

```
cargo run -p mediacore-server -- --config config.yaml
open http://127.0.0.1:7880/swagger
```

**It is generated from the protobuf descriptors, not written.** `crates/proto/build.rs` already
compiles every `.proto` with protox to produce the wire types and their JSON codecs, and now keeps
the resulting `FileDescriptorSet` as `mediacore_proto::FILE_DESCRIPTOR_SET`. `openapi.rs` reads that
with `prost-reflect` and emits the OpenAPI document: the paths, the request and response schemas,
every field name in the spelling JSON uses, every enum member, and the field documentation — which
is the comments LiveKit wrote in the `.proto`, carried through because the descriptor set is kept
with `source_code_info` in it. Change a `.proto` and the page changes in the same build. A
hand-written specification is accurate for about a month, and the month ends without anyone noticing.

What the descriptors cannot say is which of those methods this server answers, which grant each one
insists on and which fields it refuses. That is `rooms::METHODS` and the three `CREATE_*` tables in
`crates/server/src/rooms.rs`, and they are not documentation: `serve` reads the same table to tell a
method it refuses from a method that does not exist, so a method cannot be documented as implemented
and answer 501. `crates/server/tests/swagger.rs` holds the rest against reality — every method in the
table is driven over a real socket and has to answer what the page claims, the three `CREATE_*` lists
together have to name every field `CreateRoomRequest` has, and every schema in the document has to
match the descriptor pool field for field, so a document that was edited by hand instead of generated
fails there.

**The page is served by the SFU, on the same origin as the API.** Try-it-out posts to
`/twirp/mediacore.RoomService/…` on whatever origin served the page, so the two have to be one origin
or every button is a cross-origin call to another port. The front end deploys on its own schedule
and does not have the descriptors; the SFU has both, which also means the page works unchanged over
an SSH tunnel to `127.0.0.1:7880` with no proxy in front of it. `deploy/Caddyfile` routes `/swagger*`
to the same upstream as `/twirp/*` for the same reason.

**Try-it-out is not a sandbox, and the page says so above the fold.** `DeleteRoom` really deletes the
room and hangs up on everybody in it.

**Authorize sends `Authorization: Bearer <token>`.** The document declares one security scheme per
grant — `roomCreate`, `roomList`, `roomAdmin` — and every operation names the one it needs, so the
requirement is in the document rather than only in prose beside it, and the authorize dialog reads as
the list of grants rather than as one anonymous "bearer". The cost of that is a reader who pastes a
token into the row an operation does not name, which would send no header at all and read as a broken
server; `swagger-init.js` fills the header from whichever row was filled in, so the answer is always
this server's judgement of the token.

**swagger-ui is vendored**, in `crates/server/assets/vendor/`, compiled into the binary and served
under a version-stamped path — the rule the front end already follows for the LiveKit SDK and
RNNoise. No CDN in the runtime path, a deployment with no route to the internet still gets the page,
and the page's own content security policy is `default-src 'none'; script-src 'self'`. Its licences,
and those of everything webpack built into that bundle, are in that directory's `NOTICE.txt` and
served beside it at `/swagger/NOTICE.txt`.

To move to another version:

```
V=<version>
curl -fsSLO https://registry.npmjs.org/swagger-ui-dist/-/swagger-ui-dist-$V.tgz
tar xzf swagger-ui-dist-$V.tgz
cp package/swagger-ui-bundle.js crates/server/assets/vendor/swagger-ui-bundle-$V.js
sed 's|/\*# sourceMappingURL=swagger-ui.css.map\*/||' package/swagger-ui.css \
  > crates/server/assets/vendor/swagger-ui-$V.css
```

Then bump the two paths and `UI_VERSION` in `crates/server/src/swagger.rs`, the two `<link>`/`<script>`
paths in `crates/server/assets/swagger.html`, and the version and licence list in that directory's
`NOTICE.txt`. `cargo test -p mediacore-server` fails if the version, the routes and the page disagree.


## Events

A record answers a question afterwards; an event is a notification that something changed. The two
are not alternatives and the order between them is the whole design: **nothing announced here is
ever the only copy of the fact**. An event to a consumer that was not listening is gone — that is
what an event is — and the answer to that is not a cleverer delivery, it is that the meeting is still
under `/api/rooms/<room>/sessions` and the room is still in `ListRooms` when the consumer comes back.
Everything below is built on being allowed to fail, loudly, without losing anything.

Six names. Four are LiveKit's, spelled the way LiveKit spells them:

| Event | When |
|---|---|
| `room_started` | a room came into being: created over the API, or opened by the first person to walk in |
| `room_finished` | a room went away: deleted, swept up empty, or thrown away with the last person to leave |
| `participant_joined` | somebody is in a room |
| `participant_left` | somebody is not, with LiveKit's `disconnectReason` on the participant |
| `session_started` | a meeting began: the room reached the number of people it needs |
| `session_finished` | a meeting ended: nobody came back before the grace period ran out, or the room was taken away under it |

The last two are for the thing LiveKit has no concept of. A room is a place and a session is the
meeting held in it, so `room_started` fires when the place is made — which for a room created the
night before is not remotely when the meeting began. A hidden participant produces its events like
anybody else: a recorder is invisible to the people in the room on purpose, and not to whoever is
billing for the recording.

### The shape on the wire

LiveKit's `WebhookEvent`, as `protojson` writes it, posted with LiveKit's headers and LiveKit's
signature — so a deployment already receiving LiveKit's webhooks receives these without touching its
receiver.

```
POST /hook HTTP/1.1
Content-Type: application/webhook+json
Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9…

{"event":"participant_joined",
 "room":{"sid":"RM_hycBMAjmt6Ub","name":"standup","creationTime":"1757155200",…},
 "participant":{"sid":"PA_a1b2c3d4e5f6","identity":"alice","name":"Alice",…},
 "id":"EV_1f0c9a2b7d34","createdAt":"1757155200"}
```

The nested objects are serialised by the codecs `crates/proto` generates from the schema, so they are
the same bytes the room service would answer with and cannot drift from them. `createdAt` is in whole
seconds and quoted, because protobuf's JSON mapping sends an `int64` as a string and a generated
LiveKit client refuses a number where it expects one — the same reason `creationTime` inside the room
is quoted too. Fields at their default value are left out rather than sent as nulls, again because
that is what `protojson` does.

The token is a short-lived JWT signed with the API secret, carrying the base64 SHA-256 of the body in
its `sha256` claim. A receiver verifies the signature to know the request came from this server and
the digest to know the body it read is the body that was signed; without the second half the first
half is a signature over nothing in particular. `events.webhooks.api_key` says which of the `keys`
signs, because the receiver reads that name out of the token to know which secret to check — and a
key this server does not have is refused at startup rather than producing requests nobody can verify.

**Where this differs from LiveKit, and it is the only place.** A `session_started` or
`session_finished` carries a `session` object, which LiveKit's message has no field for. A receiver
built on `protojson.Unmarshal` without `DiscardUnknown` will refuse to parse those two — and they are
two event names it has no case for anyway, so the extra field is only ever on the events such a
receiver was going to ignore. The four it does know are byte for byte what its own server would have
sent. The session events carry no `room`: a meeting outlives the room it happened in, so a `room`
field would be present on some of them and absent on others, which is a worse contract than never
having one. The room's name is inside the session, where it was true throughout.

### Three deliveries

**In process.** `Bus::listen` hands out a queue and every event goes into every queue. The queue is
unbounded, so emitting never waits for a consumer and nothing is ever dropped in front of one — which
is what a consumer inside this process needs and a network consumer cannot have. Recording will hang
off this later: a session starts, files open; a session ends, files close and are composed. It is
deliberately not a broadcast channel, because a broadcast channel that a consumer falls behind on
drops the oldest events and hands it a number, and "the first ninety seconds are missing" is not a
recording.

**Webhooks.** One queue and one worker per URL, so a slow endpoint delays only itself. Each attempt
is bounded by `events.timeout_ms`; a failure is retried `events.attempts` times with the wait
doubling from `events.backoff_ms` up to `events.max_backoff_ms`, and after that the event is
abandoned. Giving up is loud:

```
ERROR mediacore_events::delivery: an event was abandoned: nobody at this destination will hear
about it. What it was announcing is still on this server — a meeting under /api/sessions/<id>, a
room under ListRooms — and this line is the only notice that the announcement did not arrive
event=EV_1f0c9a2b7d34 kind=participant_left room="standup"
destination="https://backoffice.example/hook" attempts=5 reason="the endpoint answered 503"
```

Everything in that line that came from outside this process — the room's name, the endpoint, the
reason — goes through the same quoting `crates/front/src/log.rs` uses, for the same reason: a room
named with a newline in it would otherwise write a second line, and that second line reads as one
this binary wrote. The line that says what went missing is the last one that should be forgeable.

**Kafka**, behind a Cargo feature: `cargo build --features kafka -p mediacore-server`. Off and not
compiled by default, because `rdkafka` links librdkafka into the binary and wants a C toolchain in
whatever builds the image, and an operator who does not run a bus should pay for neither. A broker
named in the config on a build without the feature is **refused at startup** rather than ignored: a
bus that is silently not being fed is exactly the failure this is all for. The body is the same bytes
the webhooks post — one shape, two transports — keyed by room name, so one room's events share a
partition and are read in the order they happened. The producer is built without touching the
network, so a server configured with a broker that is not there yet starts, stays quiet, and fails
deliveries with a bounded timeout.

### The outbox

The part that is easy to skip and the reason the rest is worth building. Without it, an endpoint away
for two minutes costs every event in those two minutes, nothing retries them because nothing
remembers them, and no line anywhere says which ones they were.

An event is written to `events.directory` — one JSON file, flushed and renamed into place — **before**
the first attempt is made, and struck off only once every destination has taken it. A record names
the destinations that still owe it, so a broker that was down does not cause an hour of redelivery to
the webhook endpoint that took everything the first time.

**What a restart does to what is in it.** Reads it, puts it back in the order the events were emitted
in, and hands each one to the destinations that still owe it. An event may therefore arrive twice —
once before the process died and again after, if it died between the endpoint answering and the
record being struck off — which is why every event carries an `id` that never changes, LiveKit's own
field, for a consumer that wants to throw the second one away. At least once, and never at most once.

**What happens when it grows.** It cannot grow without bound. A record exists exactly while an event
is in a destination's queue, and those queues are bounded by `events.queue`, so the directory holds at
most `queue × destinations` files. When a queue is full the event is refused before it is recorded,
counted, and named in a log line, and the next event that destination does take carries `numDropped`
— LiveKit's field, LiveKit's meaning — so the consumer knows it has a hole rather than inferring one
from a gap in the timestamps. That trade is deliberate: a destination ten thousand events behind is
not going to catch up, and filling a disk on its behalf would take the server down with it.

### Where it can lose, plainly

* A destination that refuses every attempt is given up on. Counted in `undelivered_events`, named in
  the log line above.
* A destination too far behind to queue another event has that event thrown away. Same counter, its
  own log line, and reported as `numDropped` on the next one it takes.
* A crash in the window between an event being emitted and its outbox record being written loses that
  event. The window is one hand-off and one file write wide, and it is the price of the write not
  being on the join path.
* An outbox record that could not be written at all — a full disk — is delivered anyway and counted in
  `unwritten_events`. It will not be retried after a restart.
* A power cut can lose the rename of the last record written. Each is flushed before the rename, so a
  torn record is impossible, but the directory entry is left to the filesystem.

**How much of a fact goes with the notification.** For `session_started` and `session_finished`, and
for a `participant_joined` or `participant_left` that happened while a meeting was running, the
answer is none: the meeting is under `/api/sessions/<id>` with everybody's arrival and departure in
it, so a lost event costs a notification and nothing else. That is the whole reason the records exist
first.

It is not the answer for the other two, and this is worth knowing before a billing consumer is built
on them. **A room's existence is recorded nowhere durable**: a `room_started` or `room_finished` that
is dropped is the only copy of the fact, and a room created and deleted without a meeting in it
leaves nothing behind at all. Nor is an arrival or a departure that happened while no meeting was
running — which is every arrival before quorum under `min_humans ≥ 2`, and every recorder that joins
an empty room. `ListRooms` answers for rooms that still exist and cannot answer for one that has
gone. If those facts have to survive a consumer being away, they have to be counted from the
meetings, or the consumer has to be one that does not lose events.

### Nothing here sits on the hot path

```
Bus::emit ──▶ (unbounded) ──▶ dispatcher ──▶ outbox.record
 no I/O                        one task           │
 no waiting                                       ├─▶ (bounded) ─▶ webhook worker ─▶ settle
                                                  └─▶ (bounded) ─▶ kafka worker   ─▶ settle
```

`Bus::emit` takes a lock, pushes a shared pointer into each listener's queue and returns. It opens no
file, makes no request and never awaits, so it is safe to call from the middle of somebody joining a
room — which is where it is called from. The two blocking calls in the whole pipeline, both of them
the outbox flushing a file, go through `spawn_blocking`, so they land on the blocking pool rather than
on a runtime thread that is also carrying somebody's media.

The meeting record is the other thing on that path and it is off it too: `SessionStore` edits the
record in memory under its lock and wakes a thread of its own to write it, so the `fsync` and the
rename happen with no lock held and on nobody's join. `crates/session` measures that against this
machine's own `sync_all` rather than against a number of milliseconds, because the question is how
many disk flushes a caller waited for and the answer has to mean the same thing on an SSD and on a
loaded CI box.

`crates/server/tests/events.rs` is the test that would catch a regression: eight people join a real
server over WebSockets while the only configured webhook endpoint accepts connections and answers
nothing, with a five-second timeout and five attempts thirty seconds apart. Anything on the join path
that waited for a delivery turns that from milliseconds into seconds.
`crates/events/tests/delivery.rs` makes the same measurement on the emitter directly, and adds the
rest: a request a LiveKit receiver can verify, an endpoint refusing until it is let go, an event
still owed after the process that promised it has gone, and one destination that never answers not
holding up the one that does.

### Configuration

```yaml
events:
  directory: /var/lib/mediacore/events
  queue: 10000
  attempts: 5
  timeout_ms: 5000
  backoff_ms: 1000
  max_backoff_ms: 30000
  webhooks:
    urls:
      - https://backoffice.example/mediacore/events
    api_key: devkey
  kafka:
    brokers: kafka1:9093,kafka2:9093
    topic: mediacore
    properties:
      security.protocol: ssl
      ssl.ca.location: /etc/ssl/certs/kafka-ca.pem
```

`kafka.properties` is passed to librdkafka unread, which is where a real deployment's TLS and SASL
live; naming each setting in this server's own config would mean a release every time a broker wanted
one nobody had thought of. In a container `directory` belongs on a volume — `deploy/docker-compose.yml`
mounts one — or a restart during an outage loses exactly the events the outbox was holding.

`/healthz` carries four more fields. `events` is how many have been announced; `owed_events` is how
many are written down and not yet taken, which sits above zero while a destination is behind and
climbs while one is away; `undelivered_events` is how many nobody will ever get, and is the one worth
an alert; `unwritten_events` is outbox records that could not be written, which is durability lost
rather than delivery lost.

## Running

```
cp config-sample.yaml config.yaml
cargo run -p mediacore-server -- --config config.yaml
```

The config path also comes from `MEDIACORE_CONFIG`. Two sockets are opened: TCP `port` for
signalling and one UDP socket for all media.

The `room` block is the room lifecycle. `auto_create` decides whether joining a room nobody created
makes one. `min_humans` and `end_grace` are the session policy: how many people a room needs and how
long it has after it loses them, off at `min_humans: 0` and overridable per room by whoever creates
it — through the room's metadata, since `CreateRoomRequest` has no field for either. `duplicate_identity` is `suffix` — one person may be in a room twice, and the second connection
is given `alice#2` — or `displace`, LiveKit's rule, where the second connection throws the first out.
`empty_timeout` and `departure_timeout` are how long a room somebody created is kept with nobody in
it, before anybody has ever joined and after the last person leaves; both at zero mean it is kept
until `DeleteRoom`. `max_participants` is the limit on one room. All four are the defaults every room
starts from and any of them can be overridden per room by whoever creates it.
`sweep_interval_ms` is how often the server looks at rooms nobody is doing anything to: it is the
only clock the registry has, and both timeouts and the session policy are enforced from it, so
turning it up is turning off the only thing that ends a room by itself.

`sessions.directory` is where the record of every meeting is kept, one JSON file per meeting. It is
created at start, the server refuses to start if it cannot be, and there is no value that turns
recording off; in a container it belongs on a volume. See [Sessions](#sessions).

The `events` block is who is told what happens as it happens: `webhooks.urls` and `kafka.brokers` are
the destinations, `directory` is the outbox that holds what has been promised and not yet taken, and
`queue`, `attempts`, `timeout_ms`, `backoff_ms` and `max_backoff_ms` are how hard this server tries
before it gives up and says so. Nothing is started until a destination is named. See
[Events](#events).

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
- `GET /healthz` — room, participant, RTP and forwarding counters, how many meetings are recorded and
  running, how many events have been announced and how many of them nobody will ever get, whether
  bandwidth estimation is on and how many subscribers it is allocating for, and the ICE socket
  address
- `GET /rtc/validate?access_token=…` — token check, returns the identity
- `GET /rtc?access_token=…` — signalling WebSocket. `auto_subscribe=0` joins without subscribing
  to anything; it is on by default.
- `POST /twirp/mediacore.RoomService/<Method>` — the server API, described under
  [Server API](#server-api). Also answered at `/twirp/livekit.RoomService/<Method>` unless
  `twirp.livekit_alias` is turned off.
- `GET /api/rooms/<room>/sessions`, `GET /api/rooms/<room>/session`, `GET /api/sessions/<id>` — what
  happened in a room, as opposed to what is happening in it. See [Sessions](#sessions).

Tokens are standard LiveKit access tokens: HS256, `iss` is the API key, `sub` is the participant
identity, and the `video` claim carries the grants. Any LiveKit token generator works, including
`lk token create`. A token for the server API is the same token with a different grant and no `sub`
at all.

## Deployment

`cargo run` above is a development server on plain HTTP, which a browser will
not give a microphone to: `getUserMedia` needs a secure context, and a page that
is not one cannot open the signalling WebSocket either. Plain HTTP is enough to
run the tests and nothing else.

`deploy/` is the deployable shape — Caddy terminating TLS in front of the SFU
and of `crates/front`, a single-page audio room that serves the page and mints
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
cover nearly every silent room. `crates/front/README.md` documents the front end
itself, and the root `Dockerfile` builds both binaries.

## Layout

| Crate | Contents |
|---|---|
| `crates/proto` | protobuf definitions, generated types, and the descriptor set they came from |
| `crates/auth` | access tokens and grants |
| `crates/config` | YAML configuration |
| `crates/room` | rooms and their lifecycle, participants, tracks, subscription intent, signalling fan-out |
| `crates/session` | the record of what happened in a room: one meeting per file on disk, and the store that keeps them |
| `crates/events` | announcing what happened: the in-process emitter, the outbox, the webhook and Kafka deliveries |
| `crates/transport` | ICE, DTLS-SRTP, RTP intake and egress, one task per peer connection |
| `crates/sfu` | packet forwarding: sequence and timestamp rewriting, keyframe gating, layers |
| `crates/client` | a programmable client that publishes real RTP, used by the tests |
| `crates/server` | HTTP and WebSocket endpoints, the `mediacore.RoomService` server API and its `/swagger` page, binary |
| `crates/front` | the browser front end: one page, an audio room, a token endpoint, and an optional OpenID Connect sign-in |

`crates/session` knows nothing about rooms — it is handed a roster and works out what that means for
a meeting — which is why the dependency runs from `crates/room` to it and never back. It is also
where *when a meeting begins and ends* is decided, once: a room that armed a second countdown on a
second condition disagreed with it, and the shape of that disagreement was a record closed under
people still sitting in the room, so `Room::session_ends_at_ms` reads the store's deadline rather
than keeping one. Every change to a room's membership or its policy is handed over under the registry
lock, which is what stops an older view of a room landing on top of a newer one and what stops a
grace period expiring in the instant somebody walked back into it. `crates/events`
is the same shape: it is handed a room, a participant or a meeting and knows nothing about a
registry, so `crates/room` depends on it and it depends on neither. It knows nothing about
configuration either — the server maps the `events` block onto its own settings types, as it already
does for `RoomDefaults`.
`crates/room` owns all shared state and knows nothing about transport; `crates/transport` speaks
str0m types and knows nothing about the LiveKit protocol; `crates/sfu` is pure packet arithmetic
with no IO and no async; `crates/server` owns the sockets, the reconcile pass and the protocol
mapping between them. It owns the clocks too: `crates/room` has no timers of its own — the sweep
that ends sessions and collects empty rooms is handed the time, so a test can be the clock — and the
server runs it on a task for as long as the process is up.

## Development

```
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all
```

The protobuf definitions are compiled by `protox`, so no system `protoc` is needed.

The Kafka producer is behind a feature and is therefore not in any of those:

```
cargo test -p mediacore-events --features kafka
cargo clippy -p mediacore-server --features kafka --all-targets
```

That build wants a C toolchain, because `rdkafka-sys` compiles librdkafka. Everything else about
events — the emitter, the outbox, the webhooks — is in the default build and covered by the commands
above.
