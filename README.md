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

There is a server API as well as a signalling one. `livekit.RoomService` — the Twirp service LiveKit's
own server SDKs and `lk` speak — answers at `POST /twirp/livekit.RoomService/<Method>` in JSON or
protobuf, so a backoffice that already drives LiveKit drives this without a line changed. Rooms can
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

Both wires are generated from them. The signalling socket carries the types as protobuf; the server
API carries the same types as protobuf or as protobuf's JSON mapping, whose serialisers are generated
beside the Rust ones from the same descriptors. Neither wire is written by hand, so neither can drift
from the `.proto`.

## Server API

`livekit.RoomService` is where a backoffice, a bot or `lk` talks to the server rather than to a room.
It is Twirp: `POST /twirp/livekit.RoomService/<Method>`, one body in, one body out, no streaming.
All of it is also a page with buttons on it, at [`/swagger`](#the-page-at-swagger), generated from
the same protobuf descriptors the wire is.

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
`/twirp/livekit.RoomService/…` on whatever origin served the page, so the two have to be one origin
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
- `POST /twirp/livekit.RoomService/<Method>` — the server API, described under
  [Server API](#server-api)

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
| `crates/transport` | ICE, DTLS-SRTP, RTP intake and egress, one task per peer connection |
| `crates/sfu` | packet forwarding: sequence and timestamp rewriting, keyframe gating, layers |
| `crates/client` | a programmable client that publishes real RTP, used by the tests |
| `crates/server` | HTTP and WebSocket endpoints, the `livekit.RoomService` server API and its `/swagger` page, binary |
| `crates/front` | the browser front end: one page, an audio room, a token endpoint |

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
