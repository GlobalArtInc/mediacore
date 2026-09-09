# Roadmap

What is worth building next, why, and what it costs. Every item was checked against this tree
before it was written down: each one carries the file and line that proves the gap is real, and
anything that turned out to be already implemented was struck out rather than kept as a plan.
Estimates are in this project's own units — a milestone here has closed in about a working day —
so "a day" means a day of work, not a sprint.

M0 through M5 are done and M5.5 (meeting records, events, audio recording and upload) with them;
M6, M7 and M8 are not started. See [Status](README.md#status). Nothing below contradicts that
table: it is the work between the milestones and inside them, ordered by what it costs against
what it returns.

## The next three

1. **Stop the node on SIGTERM.** Every deploy today loses the tail of what was recorded and closes
   live meetings as `Ending::Restart`. One day, on bricks that already exist.
2. **A `/metrics` handler.** The numbers are already counted; they have no history and nothing
   alerts on them. One day, after which everything else on this page becomes measurable.
3. **An honest `active_recording`.** A room can be recorded today without one client being told.
   A day and a half, and it is the blocker on turning recording on anywhere else.

Straight after those: **data channels**. M6 does not begin without them, and `SendData` and
`PerformRpc` keep answering 501 until they land.

## Operations: the node stops lying

Two days of work that pay for themselves on the first deploy, and make everything else on this
page measurable. Today the only way to learn that the recorder is gone or the store is away is to
`curl /healthz` by hand and read the line with your eyes.

### A Prometheus `/metrics` handler over a shared snapshot

Lift the number-gathering out of `healthz` into one snapshot function so the two handlers cannot
drift, and serve the same quantities in the Prometheus text format: counters for RTP and forwarded
packets and bytes, `dropped_audio_packets`, `abandoned_recordings` and events; gauges for rooms,
participants, publishers, tracks, subscriptions, `unwritten_sessions`, `owed_sessions`,
`undelivered_events`, `stuck_recording_work`, and for the states themselves (store up/down,
recorder on/stopped/off, bwe on/off). No room, participant or track labels — that is cardinality
this server cannot afford. Leave `/healthz` byte for byte as it is: the compose healthcheck and the
tests are pinned to it. The `ServiceMonitor` and the alert rules are a separate commit in gitops;
there is no chart in this repository.

The comments in the code already call some of these counters alertable, and `abandoned_recordings`
and `stuck_recording_work` were written precisely so a rare failure would be visible — which today
it is not, unless somebody happens to curl at the right minute. Prometheus and Telegram alerting
already run on prod1; what is missing is the source.

*Effort:* a day for `/metrics` and the snapshot, half a day in gitops.
*Depends on:* nothing; it opens M8 and gives M7 its observability.
*Evidence:* `crates/server/src/http.rs:79-202` — the whole of healthz is one `format!` into one
line, with the comments at :139-146 and :152-157 naming the alertable counters; a grep for
prometheus, metrics or opentelemetry across the repository returns nothing.

### Shut down on SIGTERM: `/readyz` to 503, release the rooms, then flush and exit

Catch SIGTERM through `tokio::signal::unix`, not only `ctrl_c`. A `draining` flag on `AppState`:
while it is raised a new `/readyz` answers 503 and join is refused — leave `/healthz` alone, it is
the liveness probe in compose and almost certainly in the chart. Then release the rooms through the
existing `Registry::rooms`/`delete`, but send the participants a `Leave` carrying `can_reconnect`
and the reason `SERVER_SHUTDOWN` (the value exists in the protocol and is used nowhere), close the
meetings as `Ending::Closed` with the time they actually ended, wait with a ceiling measured in
seconds, and only then `sessions.flush` and exit. In gitops: a readiness probe on `/readyz`, a
`preStop` hook and `terminationGracePeriodSeconds`.

There is no lossless deploy at all right now. `main.rs` listens for `ctrl_c` only, kubelet sends
SIGTERM, and the process is started through `exec` — PID 1 without a handler ignores the signal
outright, so the pod sits there until SIGKILL. The consequences are exactly the ones the code
promises in prose to prevent: the flush does not run and the last minutes of a meeting are lost,
participants get a bare socket close, and the reaper later closes those meetings as
`Ending::Restart`, so every deploy corrupts the statistics. Catching the signal is not enough on
its own: axum waits for every connection to close and a signalling socket lives for ever.

*Effort:* a day of Rust with tests, plus a small change to the manifests in gitops.
*Depends on:* nothing, every brick is in the tree; it is the precondition for any multi-replica
work in M7.
*Evidence:* `crates/server/src/main.rs:72-75` — `shutdown()` waits on `ctrl_c` alone and
`sessions.flush` sits behind it; `Dockerfile:50` and `deploy/start-sfu.sh:98` — `exec`, so the
process is PID 1; `crates/session/src/lib.rs:169-174` — `Ending::Restart`, "Nobody watched this
meeting end"; `DisconnectReason::SERVER_SHUTDOWN` appears in the protobufs and nowhere else.

## The picture stops freezing when a layer changes

The only group here that fixes what participants see right now. VP8 is what our own front end
publishes, and the whole of M4 was built for a layer switch that a VP8 subscriber experiences as a
freeze.

### Rewrite the VP8 payload descriptor across a switch

Parse the VP8 descriptor (RFC 7741) on the way in and rewrite `PictureID`, `TL0PICIDX` and `KEYIDX`
on the way out, by the same offset scheme already applied to sequence numbers and timestamps: take
an offset when the source changes, hold it on `SourceState`, and reassemble the leading bytes of
the payload in `ForwardedPacket` — a copy for video VP8 only, where today there are no copies at
all. Carry the codec into `ForwarderConfig` from the source. Handle the cases where there is
nothing to rewrite or rewriting is not allowed: the I bit cleared, L and K cleared, padding. Take
the parser values from `keyframe.rs` so the crate reads the descriptor in one place. Check the
retransmission path: a resent packet must get the same offset it got the first time.

The README already lists this as the first gap in M4. After a splice the RTP timestamps and
sequence numbers are continuous, but `PictureID` and `TL0PICIDX` jump, backwards included, because
they are another encoder's counters; a libwebrtc receiver matches frames by `PictureID` and can
discard the keyframe itself and everything after it — the picture stands still until the receiver
asks for a new one.

*Effort:* a day or two. While temporal layers are not selected and frames are not dropped, the
offset per source is constant, which is the same arithmetic as `shift()`, not a history of deltas.
*Depends on:* nothing; the parser is reused by temporal layers.
*Evidence:* `README.md:405-406`; `crates/sfu/src/forwarder.rs:118-160` — `translate` touches only
seq and timestamp; `crates/sfu/src/packet.rs:76` — the payload is the same `Arc<[u8]>`; greps for
`picture_id`, `tl0` and `keyidx` come back empty.

### Temporal layers for VP8 and VP9: select and drop by TID

Pull the TID and the layer-sync bit out of the descriptor walk that already exists in
`keyframe.rs`, add a temporal index to `InboundPacket` beside the spatial one, split the measured
rate in the tracker by the (spatial, temporal) pair, and teach `Allowance` to cap both axes. A step
down the temporal axis needs no keyframe and no `LayerLatch` — drop the packets above the allowed
TID, with a `DropReason` of their own. Require the pyramid to be recognised first (several frames
with TID > 0 in a row), otherwise behave exactly as today. Check the screenshare profile separately:
libwebrtc builds a different pyramid for screen content.

The allocator's smallest move today is a whole spatial rung — roughly a factor of three, and a wait
for a keyframe. On a real link that is a see-saw. Temporal layers give a step of one and a half to
two, immediately and without a keyframe.

*Effort:* a day or two; the parser is already there.
*Depends on:* worth doing alongside turning `video.bwe.enabled` on — until the flag is on, nothing
but the allocator drives the second axis.
*Evidence:* `README.md:377` and `:405` — "temporal layers are still not selected";
`crates/sfu/src/layer.rs:1-27` — the layer type is `SpatialLayer(u8)` and nothing else;
`selector.rs:27-33`; `alloc.rs:68-79` — arrays of `LAYER_COUNT`.

## Recordings you can trust, and can use

Recording is the one subsystem that already writes other people's data into S3, and three things in
it cost about a day each: people recorded without being told, ciphertext written as if it were
audio, and no way to hand a file over or delete it.

### An honest `active_recording`

The source of truth is the listener in `crates/server/src/recording.rs`, where the server has read
the policy on `SessionStarted` and called `recorder.session_started`: the same place raises a "this
meeting is being recorded" flag on the room and clears it on `SessionFinished` and on the recorder
dying. `Room::to_proto` reports that flag in `active_recording` — OR it with the egress participant
if you want to keep LiveKit's semantics — and a change broadcasts `RoomUpdate` down the same path
as `set_metadata`. On the front end it is one subscription to `RoomEvent.RecordingStatusChanged`, a
permanent "this meeting is being recorded" badge beside the state pill, and a mark in the tab
title. Leave a pre-join warning out of scope: the front end has no RoomService client, the room does
not exist until somebody arrives, and the honest answer would almost always be "unknown". Add a
paragraph to the README saying that `active_recording` here means recording by the SFU itself.

The flag is computed as "is there a participant of kind Egress in this room", and mediacore never
creates an egress participant — the SFU records by itself. So no `livekit-client` ever receives
`RecordingStatusChanged`, and the page says nothing: the word metadata does not occur in `app.js`
at all. Any API caller can set `sessionPolicy.record` on a room and record people who will never be
told, and in many jurisdictions being told is what makes the recording lawful.

*Effort:* a day on the server with tests and README, half a day on the front end.
*Depends on:* nothing; do the server half first and the front end becomes one subscription.
*Evidence:* `crates/room/src/lib.rs:878`; `crates/front/assets/app.js:718-734` — the complete list
of subscriptions, without this one; the vendored `livekit-client.esm.js:34040` already emits the
event from `activeRecording`.

### Do not write Ogg out of ciphertext

`open_tap` refuses to open a tap when `encryption != None`, and does not stay quiet about it: a
WARN plus a mark on the meeting record, through the same mechanism the recorder already uses to
report a tap opened for nothing. Beside it, an `encryption: required` mode on the room, arriving in
the same JSON metadata as `sessionPolicy`, with a new `PublishError` variant for the reason (the
existing `NotAllowed` carries a `TrackSource` and does not fit). Do not add an `off` mode or an
"unrecordable meeting" column: the first protects nothing and the second is a migration for a case
that does not happen yet. Do not add a field to the Room proto — this project deliberately does not
carry its own settings over the wire.

The `encryption` field arrives from the track and is stored, and no check reads it — four mentions
in the whole repository, all in one file. A LiveKit client with E2EE on publishes Opus, the tap
opens (only kind, mime, source and `room.records()` are consulted), and the writer reads the Opus
TOC straight out of ciphertext: what lands in S3 is a file lying about its channels and its length,
and nobody finds out until somebody tries to listen. Our own front end does not enable E2EE, but
`livekit-client` is vendored here, and the day somebody turns it on this breaks silently.

*Effort:* a day and a half — the refusal is half a day, the required mode with tests is another day.
*Depends on:* nothing; sensible to do in one pass with the fix to `writer.rs:452`, both are about
the recording telling the truth.
*Evidence:* `crates/room/src/track.rs:104` and `:142-143` — the field exists and is filled;
`crates/server/src/peer.rs:555-584` — `open_tap` knows nothing about it;
`crates/recording/src/writer.rs:501,640` — the TOC is read from the payload.

### Screen-share audio as its own recording, labelled by source

Add `source` to `TrackSpec` and to `Recording`, a `source` column in a new migration (after the
shape of migration 2, defaulting to `microphone` for existing rows), and carry it through the
sidecar with a serde default, through the INSERT and SELECT, into the `recording_ready` event and
into the answer from `GET /api/sessions/{id}/recordings`. Let `ScreenShareAudio` through the filter
in `open_tap` and rewrite the doctrine comment and the README paragraph beside it. The `source`
field is mandatory rather than decorative, and the behaviour sits behind
`recording.screen_share_audio`, off by default: otherwise a consumer that does not read the new
field gets a file containing a video's soundtrack under a participant's name and puts it into a
transcript as that person's words. Worth taking the option of counting segments by
(session, participant, source) so the tracks cannot interleave.

Screen-share audio is published and forwarded, but it is absent from the recording entirely and
nothing says it was ever there: a meeting where somebody played a clip is stored as a set of
voices. The reason given in the comment for refusing it is right — do not mix content with a
person's voice — but it forbids mixing, not recording, and a separate file does not break it.

*Effort:* a day, a dense one: changes in five crates, a migration, a flag, README and tests.
*Depends on:* better after the `writer.rs:452` fix — continuous system audio on somebody else's
clock is exactly the input where "the timestamp went backwards" shows up more often than a
microphone.
*Evidence:* `crates/server/src/peer.rs:568-571` — the filter is `Microphone|Unknown`;
`crates/front/assets/media.js:884-940` — the front end publishes system audio with
`systemAudio: include`; `ScreenShareAudio` appears nowhere in `crates/recording` or
`crates/session`.

### A signed link to a recording

Lift the signature calculation out of `fn authorization` into a function of its own and build
`presign` on top of it: a query signature with `X-Amz-Algorithm`/`Credential`/`Date`/`Expires`,
`SignedHeaders=host` and `UNSIGNED-PAYLOAD`, plus a second known-answer test after the existing
one. Make the endpoint `GET /api/sessions/{id}/recordings/{recording}` answering 302 rather than
`/api/recordings/{id}`: then the room resolves through the same code, the `reads()` grant applies
unchanged, and no new store method is needed at all. Build the link from configuration; no S3
client and no `AppState` change. Keep the TTL short and configurable, and cut the key out of the
logged `Location`, or the signature leaks into the Loki access logs and works from there until it
expires. If the object has not been uploaded yet, refuse honestly rather than signing a key that
does not exist.

A recording can be found but not fetched: `GET /api/sessions/{id}/recordings` returns the bucket
and the object key, and neither the server nor the front end can hand over the file — so an
integrator has to be given the bucket credentials, which is access to every recording of every
room instead of one.

*Effort:* a day, most of it the signature and its test rather than the endpoint.
*Depends on:* nothing. The payoff is narrow while our own platform collects recordings with bucket
credentials, and appears when they have to be shown to a third party or from a browser.
*Evidence:* `crates/recording/src/s3.rs:5-8` — "It does not list, copy, delete, tag, presign…";
`crates/server/src/sessions.rs:30-39` — four paths in total; `s3.rs:307` — `authorization` is
already parameterised by method and path.

### Deleting recordings, and retention behind a flag

Manual deletion first: a DELETE in the S3 client on top of the already parameterised signature, a
third `delete` stage in the queue (the `stage` column has no CHECK constraint, so the schema does
not break), `DELETE /api/sessions/{id}/recordings` behind the same `roomAdmin` guard that already
covers reading, and a DELETE of the session itself, cascading through attendance, recordings and
work rows. Order it softly: the object first, then the row. Retention by
`recording.retention_days` is the second half, behind a flag, never deleting by default, with a dry
run counting "how many would have gone" on `/healthz` before it is switched on. The local file is
barely involved: `disk::forget` already removes it after upload and `sweep_up` collects the
remainder — only `keep_local: true` is left.

Nothing in this project deletes an object or a row, so there is no answer to give to a request for
erasure. And the data is not only the audio: the room metadata and the participant attributes the
platform above stored byte for byte, and the name and identity in the attendance. A bucket
lifecycle rule does not help — it removes the object and leaves the row pointing at nothing. The
"the bucket grows for ever" argument is weak (Opus is a few megabytes an hour); the real one is
that there is nothing to delete with.

*Effort:* a day and a half for manual deletion, another day for retention with the dry run.
*Depends on:* nothing; document that a consumer of `recording_ready` which kept the bucket and key
will get a 404 afterwards.
*Evidence:* `crates/recording/src/s3.rs:5-8`; `crates/config/src/lib.rs:412` — no key for a
lifetime; `crates/session/src/recording.rs:139-148` — `Stage` has exactly two variants;
`README.md:776-779` — `find -mtime` covers the sessions directory only.

### A manifest for a closed meeting in the bucket

Write `<prefix>/<session>/manifest.json`: the room, the session id, the start and end of the
meeting, the participants with sid, identity and name and their presence intervals — including the
ones who recorded nothing — and the list of files with key, `offset_ms`, `duration_ms`,
`sample_rate` and `channels`. The data comes from `SessionStore::get` and the recordings that are
already there; the S3 client needs a `put` of bytes with `application/json`, since today's `put`
only takes a file from disk as `audio/ogg`. The trigger is not "after every announce": the last
participant's announce happens before the session closes on the grace period, and the manifest
would be left saying `endedAtMs: null`. It needs a "session closed and every recording announced"
condition. Attributes and metadata only behind an explicit flag, off by default.

To reconstruct a meeting from the bucket you have to call the API and stitch sids to file names —
whoever was handed the bucket for transcription has neither the speakers' names nor a timeline. The
value is narrower than it sounds, since `GET /api/sessions` exists and is documented, but it is
real: a self-describing bucket that survives the loss of the database, and a roster of the people
who contributed no audio.

*Effort:* a day and a half to two days; the serialisation and the PUT are hours, the rest is a new
trigger on the session, a migration for it, a flag and tests.
*Depends on:* nothing; the `source` field from the screen-share work should land in the manifest at
the same time.
*Evidence:* a grep for manifest under `crates/` finds only the upstream `livekit_egress.proto`;
`s3.rs` `key_for` — the key is `<session>/<sid>-<segment>.ogg`, and no participant name reaches the
bucket.

## The LiveKit protocol: stop swallowing what clients send

The core of M6 and the neighbouring holes of the same class: a client sends a request, the server
logs "not implemented yet" and answers nothing, or the protocol declares a message the server never
builds. Data channels come first — M6 does not begin without them — and everything else here costs
between hours and a day.

### Data channels: receive, check the grant, distribute, and turn `SendData` on

Carry `Event::ChannelOpen`/`ChannelData`/`ChannelClose` from `peer.rs` into new `PeerEvent`
variants and add a command that writes into a channel. On the server, parse the payload as
`mediacore.DataPacket`, check `can_publish_data` — take it from the participant in `crates/room`,
not from the token, because `UpdateParticipant` can revoke it — and deliver to
`destination_identities` or to the whole room, with a ceiling on packet size and on rate. Write the
packets back into the client's publisher channel: `livekit-client` listens to its lossy and
reliable channels on the publisher connection, and that path does not touch SDP at all. A channel
in the subscriber offer is a second step and not sufficient on its own: that transport is created
lazily and a listener with no subscriptions never has one. Then `Support::Refused` becomes `Served`
for `SendData`, the front end stops minting `can_publish_data: false`, and the whole thing sits
behind a configuration flag shaped like `video.bwe.enabled`.

Chat, reactions, a raised hand and any state exchange are physically impossible today.
`livekit-client` opens two data channels as soon as it connects, str0m negotiates them, and
everything that arrives lands in one match arm and is dropped. Do not sell this as "chat is
coming" — there is no chat UI — but `SendData` over Twirp and reception in third-party LiveKit SDKs
start working the day it lands.

*Effort:* a day for receive and distribute with tests, half a day for the flag, the limits and the
README.
*Depends on:* nothing. The types are generated from the vendored protos, the grant is already
computed and stored on the participant, and SCTP in str0m 0.23 is not behind a feature.
*Evidence:* `crates/transport/src/peer.rs:1653-1659` — channel events swallowed by one arm
returning true; `crates/server/src/rooms.rs:196-202` — "SendData needs a data channel, and
mediacore does not carry data packets yet"; `crates/front/src/http.rs:520`.

### Honest `LeaveRequest.action` instead of a permanent DISCONNECT

Not "revive the unused `reconnect()`" but move the decision into the `leave()` constructor itself:
instead of a hard `can_reconnect: false` with `Action::Disconnect`, a table from reason to action.
RECONNECT for `ConnectionTimeout`, `MediaFailure` and an `UnknownReason` where a transport failed to
come up; DISCONNECT for `ROOM_DELETED`, `PARTICIPANT_REMOVED`, `DUPLICATE_IDENTITY` and a refusal at
the door. Do not set RESUME anywhere until resume actually exists — a client told RESUME goes into
`resumeConnection` with an ICE restart and falls over anyway. The change belongs in `leave()` rather
than in `signal.rs`, because `Participant::disconnect` goes through the same place and one edit also
fixes the publisher-transport failure path.

Today every departure, including the sixty-second timeout and a media failure, reaches the client
as "we have parted for good", and `livekit-client` reads exactly this field when deciding whether
to come back. The field matters not on a real network drop — there the frame never arrives and the
client reconnects on `onClose` — but when the socket is alive: a sleeping tab that missed its pings,
a media failure, a transport that failed to start. All of those get a verdict where an invitation
to return belongs.

*Effort:* hours. One table, no rewritten tests, two or three new ones.
*Depends on:* nothing; when resume lands, the table gets revisited to move the timeout and
`MediaFailure` onto RESUME — leave that written in it.
*Evidence:* `crates/room/src/lib.rs:2122-2129` — the single constructor hard-codes `Disconnect`,
with `reconnect()` right beside it at :2131, already used in `subscriber.rs:655`;
`livekit_rtc.proto:371-386`.

### `UpdateMetadata` over the signalling channel

Handle `signal_request::Message::UpdateMetadata`: check `can_update_own_metadata` on the
participant, bound the size of name, metadata and attributes with a new setting (shaped like
`sessions.max_opaque_bytes`, one shared ceiling for the profile is reasonable), assemble a
`ProfileUpdate` and apply it through `room.update_participant`, which already broadcasts
`ParticipantUpdate` from the inside. Answer the `request_id` with `RequestResponse::Ok`, or
`NOT_ALLOWED` with a reason; the plumbing for that already exists on the `AddTrack` path. The merge
rules for attributes — a removal by empty value, and "a field not named is a field not touched" —
are already implemented in `Participant::apply` and do not need writing again.

There is no handler, so `setName`, `setMetadata` and `setAttributes` fall into the "signal request
is not implemented yet" arm: no answer, the call times out, and the room learns nothing. Fairly:
`can_update_own_metadata` defaults to false and our own front end does not grant it, so this is a
hole for an external platform that turns the right on rather than a live breakage. Attributes on
the meeting record are snapshotted once on arrival, so self-editing cannot forge those — only the
live roster is exposed.

*Effort:* half a day. The machinery is there; the new part is a match arm, a grant check, a config
field and a couple of tests.
*Depends on:* nothing; a rate limit belongs here naturally, since every call broadcasts to the room.
*Evidence:* `crates/server/src/signal.rs:322-330` and `:434`; `crates/room/src/lib.rs:1162`.

### Server-side speaker detection and `SpeakersChanged`

Read `audio_level` and `voice_activity` in the `PeerEvent::Rtp` arm, where the packet is already in
hand; combine them in a detector with an adaptive room noise floor and hysteresis on the way in and
out; hold the loudness state on the room and broadcast a `SpeakerInfo` list sorted by loudness
about every 200 ms through `Room::broadcast`. The extmap does not need pinning — str0m uses the
standard map, which has `ssrc-audio-level`; the work is confirming the extension actually arrives
from Chrome. This needs a room-level ticker (after the shape of `Caretaker`) or one elected
broadcaster: every existing ticker lives in a single participant's task, and a room of ten would
otherwise send ten messages per tick. The sid in `SpeakerInfo` is the participant's, not the
track's.

The message is declared in the protocol and built nowhere, so a client that relies on it gets a
permanently dark border. Today every browser measures everyone else's audio itself — CPU on each
participant, and a different answer per participant to "who is talking". Server-side loudness is
what a large room needs to pick the loudest N, what gives a speaker priority on the top layer, and
what future SDKs will have no meter of their own for. Adaptive thresholds only: the front end has
already walked the constant-threshold road and written down where it ends.

*Effort:* a day for the server side; moving the front end onto server levels is a separate day.
*Depends on:* nothing; the visible effect in production only arrives with the front-end change,
which nobody has asked for yet.
*Evidence:* `crates/sfu/src/packet.rs:117` — extensions are deliberately preserved, yet
`audio_level` does not occur once in `crates/server` or `crates/room`;
`crates/room/src/lib.rs:2091` and `:2115` — the only two mentions of `SpeakersChanged` in the tree.

### Connection quality: compute it, send `ConnectionQualityUpdate`, show the badge

Fold `PeerReport` (RTT, loss in both directions, queue depth and age) and the ingress and egress
reports (jitter, NACKs, PLIs, per-layer loss) into one score per participant — separately for the
up and down side, taking the worse, with hysteresis and a hold — and broadcast it through the
existing path. Compute on report arrival; a timer is only needed to declare LOST. Fill in `score`,
not just the enum. On the front end: a subscription to `ConnectionQualityChanged` and a badge on
the tile and in the roster row — the vendored client already parses the message and keeps
`participant.connectionQuality`. Drop the idea of a client-side fallback that estimates quality
from the bench while the server is silent: a browser cannot see the other party's uplink, and a
second opinion about quality is precisely the disease the front end has already documented in
itself.

Everything the server knows about a participant's link goes into `tracing::trace!`. A client cannot
show "poor connection", and support cannot tell a bad participant from a bad server except by logs
with trace enabled. Finding out whose link is falling apart means opening a debug panel with a
table of rates — a tool for somebody debugging the SFU.

*Effort:* a day on the server, a couple of hours on the front end.
*Depends on:* nothing, though the right thresholds come off a load bench that does not exist yet,
so the first ones will be by eye.
*Evidence:* `crates/room/src/lib.rs:2092` and `:2115` — the message is named and never built;
`crates/server/src/peer.rs:190-229` — three `tracing::trace!` calls in a row.

### Publisher permissions: `SubscriptionPermission` as a third gate in `can_subscribe`

Keep `all_participants` and the publisher's list of `TrackPermission` on the participant and consult
it inside `Room::can_subscribe` — the single gate that all three sources already pass through
(auto-subscribe, the subscriber's own gate, and the admin `UpdateSubscriptions`). The three-way
conflict does not need inventing: intent and gate are already separated in this architecture, and a
publisher's permission is a third gate, stricter than anyone's intent, the admin included. What has
to be written is the method that returns who is affected, the removal of intent through the existing
`remove_desired_subscriptions` (without it a live forward never breaks), the
`SubscriptionPermissionUpdate` broadcast, and careful separation of sid from identity given
suffixes like `bob#7`. If the budget has to be cut, start with an honest refusal instead of silent
swallowing — that is an hour.

A publisher has no way to limit who listens: the right exists on the token at the door and in the
admin call, and a request from a client falls into the "not implemented yet" arm — the message is
not even named in the log. The shape of the hole is the unpleasant kind: the client believes it has
restricted its audience and everybody hears it.

*Effort:* a day, most of it tests on gate precedence and on breaking a live forward.
*Depends on:* nothing, but the demand is hypothetical — our front end does not send this, the
platform uses the admin API, and our own client needs the sending side added to have something to
test against.
*Evidence:* `crates/server/src/signal.rs:322-330`; `crates/client/src/signal.rs:359-365` and
`tests/subscriber.rs:810-825` — our client parses a reply the server never sends;
`crates/room/src/lib.rs:1318`.

## The network does not forgive: blocked UDP and broken sockets

One failure here has no workaround at all today — a person is in the room, the roster is there, and
there is silence — and two reconnection items that only pay off together.

### ICE over TCP for networks where UDP is blocked

A new module in `crates/transport`: a passive `TcpListener` on its own port, RFC 4571 framing,
demultiplexing the first STUN by ufrag through the same `Router`, writing back into the same
connection through a bounded queue, a connection limit and an idle timeout — the port accepts
anyone before the STUN check, which is new DoS surface. This is not a listener bolted on the side:
the transport core has to become protocol-aware — a `proto` field on `Datagram`, the hard-coded
`Protocol::Udp` removed, an egress abstraction instead of a direct `socket.send_to`, a pair of local
addresses per protocol. Add the candidate through a builder with `tcptype passive`, not
`Candidate::host(addr, "tcp")`; without it Chrome ignores the candidate. Plus `rtc.tcp_port` in the
configuration (the same shape LiveKit uses), the port in the deployment and on the perimeter, and
edits to both READMEs — including the sentence "A TCP rule for that port does nothing at all",
which stops being true.

The server knows exactly one kind of candidate: UDP host. A client on a corporate network or hotel
wifi completes signalling, enters the room, sees the roster and hears nothing — the exact failure
`deploy/README.md` calls the hardest to diagnose. Honestly: networks that cut UDP often allow only
80 and 443, and 443 in a single-host deployment belongs to Caddy, so this cures a large part of the
population and not all of it; the remainder is TURN in M7.

*Effort:* two or three days of code and tests, half a day of deployment and README; it touches the
hot loop in `peer.rs`, so the existing transport tests need a regression run.
*Depends on:* nothing; a protocol-aware egress is the same fork TURN will need, so M7 gets cheaper
afterwards.
*Evidence:* `crates/server/src/media.rs:39-49` and `crates/transport/src/peer.rs:693` — everywhere
`Candidate::host(addr, "udp")`; `crates/transport` has no substantive mention of TCP; str0m 0.23
supports `TcpType::Passive` and `Protocol::Tcp` out of the box.

### Hold the session across a broken socket: `ReconnectResponse` and `SyncState`

Half of the resume scaffolding already stands and should not be touched: parsing `reconnect=1&sid`,
returning the previous identity with protection against claiming somebody else's session, and the
`Leave` carrying `action=RECONNECT`. The work is three things: move `PublisherSession` and
`SubscriberSession` out of the websocket task into an owner that survives the disconnect, with the
sink swapped and a short grace closed on every path (Leave, `RemoveParticipant`, `DeleteRoom`, the
end of the room, the timeout) — that is the ownership rebuild and the expensive part; answer with
`ReconnectResponse` and keep the previous sid (an existing test asserts the opposite and will need
rewriting); and accept `SyncState`, reconciling subscriptions and track state with what the client
remembers.

Resume is not implemented and the code says so in the log: "resume is not implemented, rejoining as
a new session". Moving from wifi to LTE, a proxy timeout or a five-second dip becomes a full
rejoin — a new sid, new peer connections, the whole room seeing somebody leave and arrive, and the
screen share cut. The `SyncState` the client sends at exactly that moment is discarded, so
subscriptions come back blind. Fairly: "a deploy becomes a pause instead of a drop" is not true with
one in-memory replica — held transports die with the process. Resume closes exactly one scenario:
the client's socket went away and the server is alive.

*Effort:* a day or two of code and tests plus a live browser run, since the join path is rewritten;
and that is on top of the ICE restart below.
*Depends on:* the server half of ICE restart, as a precondition rather than a wish.
*Evidence:* `crates/server/src/signal.rs:83-88` and `:322-330`;
`crates/proto/protobufs/livekit_rtc.proto:269` — `ReconnectResponse` is built nowhere;
`PublisherSession`/`SubscriberSession` are local variables of the socket task at `signal.rs:141-142`.

### The server half of ICE restart on the subscriber transport

Add an `ice_restart` flag to `Negotiation` and honour it when a negotiation command is handled, so
the server can emit an offer that restarts ICE. On the subscriber transport, drive it from the
existing `PeerEvent::IceConnectionState(Disconnected)` — today only a debug log — with a debounce,
instead of waiting out the twenty-second grace, respecting the renegotiation queue and never firing
while an offer is outstanding. The publisher half needs no work: accepting an offer with a new ufrag
works and is covered end to end.

This is not a roaming fix in itself — a NAT rebind already survives on its own, since the server is
ice-lite and the router learns the new address by ufrag. The value is elsewhere: `livekit-client`
restarts ICE only on its publisher and expects the server to restart the subscriber, which its code
states outright. So the server-side restart is mandatory for resume, and pointless without it.

*Effort:* half a day.
*Depends on:* plan it with resume; on its own it has no visible effect.
*Evidence:* `crates/transport/src/peer.rs:1800` and `tests/session.rs:282` — `ice_restart(true)` is
called from tests only; the word does not occur in `crates/server`; `README.md:411`.

## Moderation and traces

Two cheap pieces the first platform to embed this core will want: close the window on a removed
participant's token, and be able to answer "who threw this person out at 14:32".

### Refuse a removed participant at the door: a revocation list by (room, identity)

Keep a revocation list in memory on the room registry: (room, identity) to a timestamp until which
a join is refused; `remove_participant` writes an entry, `verify_join` on `/rtc` and
`/rtc/validate` consults it after the signature check and answers 401; an entry lives no longer than
the maximum token lifetime and is swept by TTL. Pick one of the two levels and name it honestly: a
ban by person (simple, no `iat` needed), or true `revokeTokenTs` semantics — but then `iat` has to
be added to `Claims` and minted first, because there is no `iat` in the token at all and `nbf` is
always zero, so a literal check would reject every token issued today. Leave persistence in Postgres
out of scope: a restart ends every meeting anyway.

The README admits it: there is no revocation list, `revokeTokenTs` is accepted and ignored, and a
removed participant's token stays valid until it expires — six hours by default. One correction to
the motivation: after removal the client receives a `Leave` with `can_reconnect=false` and does not
come back on its own (there is a test), so the protection is against a person who rejoins
deliberately, not against automation.

*Effort:* a day for the ban by person; a day and a half to two with real `revokeTokenTs`, and that
is a token-format change for everybody embedding this.
*Depends on:* the `revokeTokenTs` variant is blocked on adding `iat` to `Claims` and to minting.
*Evidence:* `README.md:592-593`; `crates/server/src/rooms.rs:609-632`;
`crates/auth/src/lib.rs:146-152`; `revoke_token_ts` is read nowhere.

### The caller in the logs and in the events of moderation calls

Not a stored audit table with an endpoint, but two cheap things. First, pass `Claims` into the
places where success is already logged (deleting a room, muting a track, updating a participant) and
add `iss` and `sub`; write the same lines for `remove_participant`, `update_room_metadata` and
`update_subscriptions`, where nothing is written at all. prod1 logs go to Loki, so "who touched this
room at 14:32, and with which key" is closed by that. Second, add an optional `actor` block (key,
identity, method) to `Event` and fill it on events produced by a moderation call — a webhook
consumer gets the answer where it already listens, and no new event type is needed. Put external
strings through the existing escaping, or someone else's string breaks the log parse.

A successful call is either not logged or logged without who pressed the button; a refusal is logged
with service, method and code, and also without the caller. For a platform embedding this core, that
is support's first question. A stored journal is deliberately out of scope: now that the core has
stopped knowing about accounts, it could only record the key name, and the platform making the call
knows its own user better.

*Effort:* a day with tests.
*Depends on:* nothing; add the `actor` field now rather than changing the event shape later, when
moderation arrives from the client side.
*Evidence:* `crates/server/src/rooms.rs:345-366` — only the refusal is logged;
`crates/room/src/lib.rs:1192` — "participant updated" with no caller; a grep for `.iss` under
`crates/server` is empty.

## The front end, for people rather than for debugging

Polish on the production page: visible to people, moves nothing in the core.

### Camera and output device selection, switched without reconnecting

Enumerate beyond the microphone — today only `audioinput` is taken. Switch output through
`switchActiveDevice("audiooutput")` and the `audioOutput` option on the Room; the vendored client
distributes the `sinkId` to existing and future elements itself, and the unsupported case should be
detected with `supportsSetSinkId` rather than a browser list (Safari and all of iOS do not support
it; Firefox has since 116). Publish the camera with an explicit `deviceId` and `facingMode` on
phones, and change it live through `restartTrack`, as the microphone already does. Do not treat the
plumbing as second class: unnamed labels before permission is granted, `devicechange` handled with a
rollback. Two new fields in `prefs.js`, changes to the browser module tests, and paragraphs in the
front-end README.

The camera opens whatever the browser feels like: `createLocalVideoTrack` gets a resolution and no
`deviceId`. Audio output cannot be chosen at all, so with a headset plugged in the sound can still
play out of the laptop speakers — the first thing anyone trips over on a real call.

*Effort:* a day, half of it tests and README prose in the style used there.
*Depends on:* nothing; the one place a front-end change can touch the server is that replacing the
camera track rebuilds the simulcast ladder, which is worth a run against production.
*Evidence:* `crates/front/assets/media.js:357`; `app.js:851`; `setSinkId`, `videoinput` and
`audiooutput` occur only in the vendored bundle.

### A device check in the lobby, before joining

Add a pre-join device check gated behind an explicit click: the gesture is needed both for
`getUserMedia` and for `AudioContext`, and starting automatically on load contradicts a decision the
front end has already written down. The microphone half is nearly free — `openMicrophone` is already
called before `connect`, is already idempotent, publishes the same track, and its error is already
shown in the lobby; what is missing is a button, a meter on the already-open track, and a microphone
select. The camera half is real work: there is no camera in `media.js` as a concept, and
`startCamera` requires a live room, so a preview means lifting the camera lifecycle out of the room,
handing the track to `publishTrack` without recreating it, and releasing it in the same handler as
leaving the page — otherwise the light stays on for somebody who changed their mind.

The join form is a room field, a name field and a camera checkbox. What the camera shows, a person
first sees inside the room, in front of other people.

*Effort:* a day and a half to two days; "a day" is only true if it is cut down to the microphone.
*Depends on:* the camera half effectively needs the device-selection work above.
*Evidence:* `crates/front/assets/index.html` — `#join-form` is room, name, checkbox, button;
`app.js:743` — `watchLevel` is called inside `connect`.

### Localisation of the interface, starting with Russian

Move the strings into dictionaries, pick the language from `Accept-Language` with a manual switch
remembered in `prefs.js`, and set `lang` on the root. The text is not in the markup: roughly three
hundred long literals across six JS modules plus about thirty nodes in `index.html`. Invert the
server half relative to the obvious approach: codes with parameters are more expensive and weaker
here — there are about fifteen distinct messages in total (the `ApiError` variants, the name and
room errors, five phrases on the sign-in pages), the server has `Accept-Language` on every request,
and the page shows the text it was given as it is. Dictionaries on the server are simpler; introduce
codes only where the page substitutes its own parameters. Design for the fact that some of the text
is not ours — browser messages, device labels, state names — and the page will be mixed.

The page is entirely in English, including the long explanations in the audio menu and the bench,
which are the reason to show it at all. For meetings held in Russian that is a barrier in the most
visible place, and there is no localisation machinery of any kind: no dictionaries, no language
choice, no attribute.

*Effort:* a day for the machinery plus one or two days for the Russian text itself, which is an
editor's work rather than a programmer's.
*Depends on:* sensible after M6 — permissions and data channels bring new text, or it gets
translated twice.
*Evidence:* `crates/front/assets/index.html:2` — `<html lang="en">`; greps for i18n, locale and
translat find only `translateX` in CSS.

## Instruments

Things that improve nothing by themselves, but without which neither the node's capacity, nor
compatibility, nor the right to turn bandwidth estimation on can be demonstrated. The bench is worth
building after `/metrics`, or there is no telling whether the server or the bots ran out first.

### An interop job in CI: the `lk` CLI and `livekit-server-sdk` against the built server

A separate job in `ci.yml`: build the binary, start it with a config (key, port, webhooks to a local
receiver), run a pinned release of the `lk` CLI (token create, room create/list/delete, list
participants, update metadata, mute) and a short Node script on a pinned `livekit-server-sdk`,
including a webhook signature check through their `WebhookReceiver`. Compare answers through parsed
types rather than a 200, and refusals as typed Twirp errors rather than a transport failure. Leave
media alone; RTC ports on a runner are their own story. Stay inside the served surface: egress,
ingress, SIP and agents are `Support::Refused` and that is not a failure.

Compatibility with other people's SDKs is a declared property of this server — it is why the
`livekit.RoomService` alias and the lowerCamelCase protobuf JSON exist. The shape of the answers is
already covered densely by our own tests, so this adds exactly the one thing missing: validation by
somebody else's code — a token minted by `lk`, a request built by their generator, and their own
webhook signature check. That is the class of divergence you otherwise hear about from an
integrator rather than from a build.

*Effort:* a day for the job, plus an unknown tail to fix whatever it finds.
*Depends on:* nothing; pin both the `lk` release and the npm version, or the build goes red on
upstream's schedule.
*Evidence:* `.github/workflows/ci.yml` — fmt, clippy, `cargo test`, and nothing else;
`crates/server/tests/roomservice.rs:21-47` — the router is raised in-process and driven by our own
code; greps for the `lk` CLI, `livekit-server-sdk` or interop are empty.

### A `mediacore-load` binary: bots on the client crate, and receive timestamps

Two parts, and the first is not free. Instrument reception: put the arrival time into
`ReceivedPacket` and into the reports — today the client measures neither jitter, nor time to first
packet, nor layer-switch latency, only counters. Then the binary in `crates/client` (clap is already
in the workspace, plus token minting or a "bring your own" flag): N bots across M rooms, each
publishing synthetic Opus and simulcast VP8 and subscribed to the others, a summary from
`TrackReport` plus the new timings in CSV, two scenarios — fixed load for a duration, and a ladder
that prints the ceiling it found — plus CPU and RSS of the bots themselves, so it is visible who ran
out first. Drop the claim that this unblocks `video.bwe.enabled`: the README asks for a run over a
constrained link, and a crowd of bots on a fat loop constrains nobody's link.

The production replica has no known ceiling: how many participants and tracks a node holds cannot be
stated, and the capacity model from M8 has nothing to stand on. The client crate can publish and
count, but it is a library — there is no way to run it outside `cargo test`.

*Effort:* a day and a half for the binary and the scenarios, half a day for the timestamps and the
tests that compare whole reports.
*Depends on:* half-blind without `/metrics`. Run it against a separate instance only.
*Evidence:* `crates/client/Cargo.toml` — no `[[bin]]` section; `crates/client/src/receiver.rs:12-20`
— `ReceivedPacket` carries no arrival time.

### The client as an SDK: feeding encoded frames in, and packets out

Publishing: a source trait (the synthetic one becoming an implementation) and a command through
which the caller hands over ready Opus or VP8. This is more than swapping a source: it needs a
second scheduling mode instead of the catch-up timer, the keyframe request surfaced outward (today
it is absorbed inside the synthetic source), frames addressed by layer under simulcast, and `Copy`
dropped from `PublishSource`. Receiving: a command that yields `ReceivedPacket` into an mpsc — the
smaller half, since payloads already arrive through the report window, but batched and with the tail
lost. Two examples: publishing an Ogg file (which needs a small page demuxer — nothing in this
repository reads Ogg) and recording a received track through the public `ogg`/`opus` modules rather
than the writer, which is private and tied to a tap and a meeting.

The client today is an instrument: it can send synthetic noise and count packets. Nothing can be
built on it — not a bot, not an announcement player, not a receiver for speech recognition — and the
Rust SDK promised in M6 comes down to exactly these two places. Everything else it needs —
signalling, ICE/DTLS, simulcast, subscriptions, statistics — is already in the crate.

*Effort:* a day or two.
*Depends on:* nothing, and it unblocks nothing running: this is groundwork for M6. The synthetic
source must stay interchangeable — the M2 to M5 tests rest on it.
*Evidence:* `crates/client/src/transport.rs:633` and `:681` — `SyntheticSource` is the only way
payload appears; `subscriber.rs:21-27`; `receiver.rs:6` — a 128-packet window inside the report.

## Expensive and far away

### Video recording without decoding

Do not take this as one item. The order is: (1) recording deletion and retention — video grows the
bucket by one or two orders of magnitude and there is nothing to delete with; (2) split the queue
budget and the writer thread by track kind — today there is one shared limit and one thread per
process, and the first screen recording starts starving the audio of rooms already running;
(3) only then VP8 into WebM with the layer pinned: depacketisation into frames, assembly into a
container, the same sender-clock arithmetic already proven on Ogg, starting at a keyframe. The tap
has to learn to tell layers apart: the tap table is keyed by mid, and under simulcast one mid
carries three streams with different bases. H.264 into fragmented MP4 is a separate pass with its
own depacketiser, SPS/PPS collection and its own recovery procedure for an unfinished file. Keyframe
detection for VP8 and H.264 is already written.

There is no video recording at all: the tap opens for `audio/opus` only. A meeting with a screen
share is stored as a set of voices and what was shown cannot be recovered. This is the one
after-the-meeting item that cannot be handed to an external service. Soberly: the result is not "a
recording of the meeting" you open and watch, but N files per track that still have to be assembled
with an external ffmpeg — self-sufficient for a screen share, not for a grid of cameras. An explicit
step outside the M6-M8 plan.

*Effort:* two to three weeks for the first codec, and that is with the queue already split; the
second codec costs the same again.
*Depends on:* retention and the split queue limits, both far cheaper and worth doing on their own
even if video never happens.
*Evidence:* `crates/server/src/peer.rs:563-567` — everything but `audio/opus` is filtered out;
`crates/recording` holds only `ogg.rs` and `opus.rs`; greps for webm, mp4, matroska, ivf and
depacket are empty.

## Build, configuration and documentation

Smaller than the work above and cheaper than all of it, and none of it is in any milestone.

- **The `kafka` feature compiles nowhere.** `ci.yml` runs `fmt`, `clippy --workspace --all-targets`
  and `cargo test --workspace`, all with default features, and `Dockerfile:47` builds without
  features too. `crates/events/src/lib.rs:48` gates a 147-line module with zero tests, and
  `README.md:1624-1632` gives the two commands that would cover it — commands CI does not run. The
  declared delivery path rots quietly and is absent from the production image. The fix is two lines
  in `ci.yml`, and it needs a C toolchain for `rdkafka-sys`.
- **`build.yml` is not gated on `ci.yml`.** It triggers on push to main with no `needs`, so a red
  main still builds and pushes an image to Harbor. And the `Dockerfile` is built on no pull request
  at all — a broken cargo-chef recipe is discovered after the merge.
- **No `cargo audit` or `cargo deny`, no dependabot, no `SECURITY.md`.** For a server exposed to the
  internet with str0m, reqwest, rdkafka and a vendored 1.3 MB `livekit-client` bundle, nothing
  watches for CVEs.
- **No `LICENSE`** in the root, while `Cargo.toml:8` says `license = "Apache-2.0"` and the vendored
  protos carry Apache headers. `crates/front/assets/vendor` has its `NOTICE.txt`; the repository has
  nothing.
- **No `[profile.release]`** at all: the default sixteen codegen units and no LTO. Free percentage
  points for a forwarding hot loop — and exactly the thing that cannot be measured until the load
  bench exists.
- **`rtc.use_external_ip` is a setting that does nothing.** `crates/server/src/media.rs:166-170`
  logs a WARN saying so, `README.md:410-412` admits it, and `config-sample.yaml:374` prints it
  without one word of comment, in a file where every other setting gets a paragraph.
- **The `rtc:` block in `config-sample.yaml` (lines 367-376) is the only uncommented one**, and it
  is incomplete: `region`, `node_id` (`crates/config/src/lib.rs:51-52`) and
  `rtc.negotiation_timeout` (`lib.rs:670`) are read — `signal.rs:456-457` puts region and node id
  into `ServerInfo` — and are not in the sample. The test at `lib.rs:1579` checks that the sample
  parses, not that it is complete.
- **The server does not check the length of an API key secret.** `Config::validate`
  (`crates/config/src/lib.rs:974`) only asserts that the key list is non-empty, while the front end
  refuses to start on a secret under 32 bytes (`crates/front/src/config.rs:31`). `README.md:1472`
  says `cp config-sample.yaml config.yaml`, and the sample carries a working, publicly known
  `devkey` secret that mints any `roomAdmin` token. Neither a length check nor a refusal of the
  sample value exists.
- **There is no admission control or rate limiting anywhere in the workspace.** A grep for
  rate_limit, governor or 429 finds only the `Code::ResourceExhausted → 429` mapping in
  `crates/server/src/twirp.rs:98,325` — an error code produced nowhere. There is no ceiling on rooms
  per node, no limit on signalling connections, no limit on token minting (the front end's
  `/api/token` without OIDC is open to anyone), and no rate limit on Twirp calls. `max_participants`
  is per room and `MAX_PUBLISHED_TRACKS` is per participant.
- **Logs are unstructured**, so the caller-in-the-logs item above lands in text rather than fields:
  `crates/server/src/main.rs:66-70` uses `tracing_subscriber::fmt()` with no `.json()`, no OTLP and
  no config key. And dependency logs are silenced by default — the filter is `mediacore={level}`, so
  without an explicit `RUST_LOG` no warning or error from str0m, axum, hyper or reqwest is visible
  at all, which makes ICE and DTLS diagnosis in production blind.
- **The binary does not know which build it is.** `crates/proto/src/lib.rs:41` reports the workspace
  version `0.1.0` in `JoinResponse.server_version`, in `ServerInfo.version` and from
  `mediacore --version`, while image tags are `<timestamp>-<sha>`. There is no way to tell which
  commit a running node holds.
- **SIGTERM is unhandled in the front end too** (`crates/front/src/main.rs:43-46`), and it runs
  three replicas in production; the drain item above covers the SFU only.
- **Rolling back a release with a migration is undefined.** `README.md:797-799` says there is no
  down migration, and migration 4 is forward-only. Production is on Flux, where a rollback is a
  `git revert` of the image tag — and what to do then is written nowhere. At minimum a README
  paragraph and the rule that a migration must survive the previous binary; at most a compatibility
  check in CI.
- **TLS to Postgres is not implemented** — `sslmode=require` is rejected at startup
  (`README.md:818-820`), which rules out any managed Postgres and any network where the database is
  not trusted.
- **`/rtc/v1` is not implemented** (`README.md:413-416`), so a recent `livekit-client` makes a failing
  request on every connection before falling back to `/rtc`.
- **`UpdateTrackSettings.fps` and `priority` are accepted and ignored** (`README.md:406`).
  `priority` stings: the allocator reasons about priority in `BweConfig`
  (`config-sample.yaml:415`), and the subscriber's declared priority never reaches it.
- **Unsubscribing parks the m-line instead of renegotiating** (`README.md:408-410`), so SDP sections
  accumulate through a long meeting with churn, and a subscriber does not renegotiate on a codec
  change.
- **There are no `track_published` / `track_unpublished` events.** `crates/events/src/lib.rs:103-111`
  has exactly seven names without them, while the signalling replies `TrackPublished` and
  `TrackUnpublished` are sent (`signal.rs:347`, `peer.rs:606`). A LiveKit webhook consumer learns
  that somebody joined but not that they started publishing.
- **The README contradicts itself about recording.** `README.md:640` says "Nothing here reads
  `recordingEnabled`: recording is egress, and there is none yet", while recording is implemented and
  documented at `README.md:1399+` and the real key is `sessionPolicy.record`
  (`crates/room/src/lib.rs:124,159`). An integrator reading the metadata section straight through
  will set `recordingEnabled: true` and get nothing.
- **The production deployment shape is undocumented.** Greps for kubernetes, helm, k8s and chart
  across `README.md` and `deploy/README.md` are empty, while `build.yml` says "Builds the image the
  Helm chart installs" and production is prod1: chart 0.6.x, hostNetwork, one replica, PVCs. Only
  the single-host compose stack is documented — which directly obstructs the SIGTERM item above,
  since it says "a change to the manifests in gitops" and what probes, ports and
  `terminationGracePeriodSeconds` the chart must set is written nowhere.

## Considered and rejected

Kept here so they are not reopened from scratch. Each was checked against the tree and turned down
for the reason given, not for lack of interest.

| Proposal | Why not |
|---|---|
| `subscriber_primary`: create the subscriber transport at join | An offer is built from a change list and str0m emits nothing for an empty one, so it needs a data channel that does not exist. And the payoff is defined through resume, which is not implemented. It belongs inside M6, not beside it. |
| SVC on one SSRC: AV1 dependency descriptor and the VP9 descriptor | Real, but no client here sends such a track (both publish paths are pinned to VP8), and it puts a second selector axis where the first is unfinished. |
| Incoming bandwidth estimation towards the publisher | The premise is wrong — the publisher transport already sends RTCP and the browser's own send-side estimator acts on it — and the ingress has no pacer backlog, the only evidence this project accepts for a congestion verdict. |
| Act on subscriber REMB instead of logging it | Same defect as the str0m estimate: bounded by what we choose to send. And the path is dead — the server forces transport-cc on every m-line, so browsers never send REMB. |
| Meeting search across rooms | `identity` names a person inside one room, not a person: `alice#2` exists and `bob#7` is a legitimate identity that must never be parsed. The endpoint would answer the question wrongly. |
| Embedding the front end into another platform (external token source, allowed origins, branding) | Already solved another way: LiveKit compatibility means a client with somebody else's token issuer works as it is. |
| Moderation buttons in the front end | The server half exists and is genuinely unreachable from a browser, but the caller is the platform above with its admin API, not this page. |
| A node registry and room-to-node binding in Postgres | Puts join on the Postgres path, breaking the recorded invariant that a database being down stops neither startup nor a call. |
| Tenant = API key: room namespacing and visibility | Isolation that does not isolate — the prefix is checked at the Twirp door while a join token walks straight into a live room. |
| Quotas and throttling per key | Half of it already exists per room and per participant; the rest is admission control that belongs above the core (and see the rate-limiting note in the section above). |
| The `roomRecord` grant, declared and never checked | Zero value while the flag is off and cannot be switched on without an external change. |
| A bot participant subscribing to room audio for recognition | The README argues this shape down explicitly, and the recorder already produces the audio. |
| A TypeScript package for our own endpoints | Four GETs with no request bodies and one type cast. |
| WHIP ingress | Two of its three claimed benefits — headless publishing and a load source — are `crates/client`. |
