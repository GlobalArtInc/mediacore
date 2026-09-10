# Interop: mediacore against somebody else's code

Everything under `crates/*/tests` drives this server with code from this
repository. That is the right way to test what the answers mean, and it is
structurally incapable of testing the one property the server actually
advertises: that a client nobody here wrote can read them.

So this directory runs the built binary against LiveKit's own two clients, at
pinned versions:

* the `lk` CLI — mints its own tokens, builds `/twirp/livekit.RoomService/…`
  inside generated code, and prints a `twirp error <code>` or exits non-zero;
* `livekit-server-sdk` for Node — the same generated Twirp client an integrator
  uses, plus `WebhookReceiver`, which is the receiver most deployments already
  have in front of their LiveKit webhooks.

Five different ways for a divergence to surface, and none of them reachable from
a test that raises the router in-process: a token minted by their code, a request
built by their generator, an answer fed to their parser, a refusal caught as
their error type, and a webhook checked by their verifier.

## Running it

```sh
interop/run.sh
```

That builds `mediacore-server` in release, fetches the pinned `lk` into
`interop/.cache`, runs `npm ci`, starts a webhook receiver, starts the server on
`127.0.0.1:7880` against `config.yaml`, drives it from both clients, and stops
everything on the way out — including on a failure, a `Ctrl-C` or a `kill`.

Useful while working on it:

```sh
MEDIACORE_BIN=target/release/mediacore interop/run.sh   # skip the build
MEDIACORE_KEEP=1 interop/run.sh                         # keep the run directory
```

A failing run tails the server and receiver logs before it exits, because they
are the only witness to what the other side actually sent.

Nothing may already be answering `127.0.0.1:7880` or the receiver's port when
the run starts, and both processes it starts have to still be running when it
finishes — otherwise it stops and says so. The two are one check from two sides:
a mediacore left over from an interrupted run, or from another checkout, answers
`/healthz` exactly as well as this one does, and the server that lost the bind
dies in a log a passing run deletes. Without them the job reports every
assertion passed against a binary it never started, which is the one thing an
interop gate must not do.

## What is in here

| | |
| --- | --- |
| `run.sh` | the whole run, and the only supported way in. Both pins live at the top of it. |
| `config.yaml` | the server as this job runs it. The key and the secret are test credentials and look like it. |
| `interop.mjs` | three modes — `receiver`, `api`, `webhooks` — run in that order by `run.sh`. |
| `package.json` | exact versions, no `^` and no `~`, with `package-lock.json` beside it. |

`interop.mjs` is one file in three modes rather than three files because the
webhook receiver has to outlive both phases: the `lk` phase makes and unmakes a
room too, and `config.yaml` sets `attempts: 1`, so an event posted at a port
nobody is listening on is a hole nobody would ever see. The receiver goes up
first, writes down what it made of each delivery, and the `webhooks` mode reads
that back after both phases have run.

Two npm dependencies:

* `livekit-server-sdk` — the thing under test.
* `@livekit/protocol`, at the exact version the SDK itself pins. The SDK is a
  *server* SDK and carries no signalling client, but `ListParticipants`,
  `GetParticipant`, `UpdateParticipant`, `MutePublishedTrack` and
  `RemoveParticipant` assert nothing at all against an empty room. So the job
  opens one signalling socket and publishes one track through LiveKit's own
  generated `SignalRequest`/`SignalResponse` — which is more interop, not less.
  No media crosses it: mediacore answers the join and the `AddTrack` before any
  SDP is exchanged, there is no peer connection, no ICE and no RTP. RTC ports on
  a runner are their own story.

## What is asserted

On parsed types and field values, never on a 200. A server that dropped a field,
spelled it in snake_case or filled it with something their parser reads as zero
answers 200 to every call in the file, so what is asserted is what their parser
hands a caller: `Room.creationTime` *is* a positive `bigint`, `Room.emptyTimeout`
*is* the number that was asked for. Those are the two types an integrator's own
code compares against, and comparing a `bigint` with a `number` in JavaScript
quietly does nothing.

Which JSON shape carried them is a different question and not one this directory
can answer: `@livekit/protocol` takes a quoted string or a bare number for either
field and normalises to the type in the `.proto`, so the wire form is invisible
from behind their SDK. It is pinned in `crates/server/tests/roomservice.rs`,
which reads the raw body — an `int64` travels as a JSON string, and an SDK that
parsed it as a number would lose precision.

What `lk` prints is parsed, never grepped. protobuf-go puts one space or two
after each colon in its JSON output, picked from a hash of the binary it is
linked into so that nobody can treat that output as a wire format: the pinned
darwin build prints two and the linux build prints one. An assertion on that
text says which release archive was downloaded and nothing about this server —
and would have gone red on the job's first run under CI, blaming mediacore for
the platform.

Refusals are asserted as typed Twirp errors — code, HTTP status and the text
that says why — because `fetch` rejecting, a socket closing, and a 501 whose body
their parser cannot read all look identical to a caller that only checks that
the promise rejected, and only one of the three is this server answering the
question.

Refusals are the correct answer and are asserted as such:

* `ForwardParticipant` and `MoveParticipant` move a participant between nodes,
  and mediacore is one node → `unimplemented`, 501;
* `CreateRoom` carrying `egress` or `agents` promises a recording that will not
  be made and an agent that will not join → `unimplemented`, 501, naming the
  field;
* `SendData` with `destinationSids` → `unimplemented`, 501.

## What it found

**The `Authorization` header on a webhook carries a `Bearer ` scheme, and
LiveKit's receiver will not take it.** This is the finding the job was built to
catch, and it is why the job is red.

`crates/events/src/webhook.rs` sends `Authorization: Bearer <jwt>`. LiveKit
sends the bare JWT — `webhook/consts.go` has `authHeader = "Authorization"` and
`webhook/url_notifier.go` does `r.Header.Set(authHeader, token)` — and both
their Go verifier and the JS `WebhookReceiver` put that header value straight
into a JWT verify. Handed `Bearer eyJ…`, `jose` rejects it before it looks at
the signature:

```
JWSInvalid: JWS Protected Header is invalid
```

Every delivery in a run fails this way, and every one of them verifies once the
six characters are removed — so the signing key, the claims and the base64
SHA-256 of the body are all correct and the framing is not. The module's own
doc comment says a deployment already receiving LiveKit's webhooks receives
ours "without touching its receiver"; today it does not. The receiver in this
directory tries the header both ways and reports which, so the failure says
which half is broken rather than leaving somebody to find that out by hand.

**Webhook deliveries carry no `User-Agent`.** LiveKit's notifier sets
`User-Agent: LiveKit`. Nothing in the verification path reads it, so this is
noted in the transcript rather than asserted — but a receiver, or a proxy in
front of one, is entitled to filter on it.

**Egress, ingress, SIP and agent dispatch are not routed at all.** They are
services of their own rather than methods of `RoomService`, so a call from
`EgressClient` lands on no route and comes back as a bare 404. Their SDK still
turns it into a typed `TwirpError` — a caller sees a refusal rather than a
crash — but it carries no Twirp code and no message, because there is no
handler to write a Twirp body. The job asserts today's answer exactly and notes
the missing code. A Twirp `bad_route` body would be a better answer and is not
free: it means routing four more paths in order to refuse them, which is a
decision about the served surface rather than a bug in it.
