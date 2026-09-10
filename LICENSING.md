# Licensing

How this server would be sold to somebody who runs it themselves: they install it, they paste in a
key, and what the key says decides what the server does.

Nothing here is built yet. This is the shape, the decisions already taken, and the things that have
to be decided before any of it is written.

## What a licence is

A short signed string. Ed25519, our private key on our side, the public key compiled into the
binary. It carries who it was issued to, when it stops being valid, and what it turns on:

```json
{
  "customer": "MCN",
  "issued": "2026-09-10",
  "expires": "2027-09-10",
  "graceDays": 14,
  "features": ["recording", "events", "data"]
}
```

The server reads it out of the configuration — inline as `license.key`, or from a path as
`license.file`, because a key pasted into a Kubernetes Secret and a key in a file on a VM are both
things people actually do — checks the signature, and never asks anybody anything. That last part
is the requirement everything else follows from: this is installed in other people's networks,
some of which do not reach the internet at all, and a licence that needs to phone home is a
licence that fails in the deployments most likely to pay for one.

It also means we cannot revoke a key that has already been issued. The expiry is the only clock,
so licences are issued for a term and reissued, rather than issued once and switched off later.

## What it gates

**Features, not counts.** A tier is a set of names in `features`, and a name that is not in the set
is a part of the server that does not start:

| Feature | What it turns on | Where it is decided today |
|---|---|---|
| `recording` | Recording a meeting to Ogg and uploading it | `AppState::start`, where `Recording` is built |
| `events` | Webhooks and the Kafka producer, and the outbox behind them | `events::start` |
| `data` | Data channels, and the chat that stands on them | `publisher_data(&config.data)` |
| `bwe` | Bandwidth estimation, allocation and pacing | `subscriber_bwe(&config.video.bwe)` |

Every one of those is already a single call that returns something or `None`, and the whole of the
enforcement is that the licence gets a say in that answer. Nothing has to be threaded through the
media path, and nothing on the hot path learns what a licence is.

A licence that does not name a feature leaves the server behaving exactly as a deployment that
switched it off in its own configuration — which is a shape the code already has and already
tests. The difference is one line in the log and one field in `/healthz`, so an operator who
configured recording and does not get it can find out why in the first place they look, rather
than in a support thread.

Counts — concurrent participants, concurrent rooms, nodes — were considered and are not the first
version. They are the honest way to price an SFU, because they are what actually costs money to
run, and they should be added when there is a second thing to sell. Features are what a first
customer can be told in a sentence.

## What happens when it is wrong

Softly, and never at the expense of a meeting somebody is in the middle of.

**No licence, or a signature that does not verify** — the server refuses to start, naming the
setting. This is a configuration mistake, it happens on a deployment rather than during a call,
and a server that started anyway would be a server nobody ever bought a licence for.

**Expired** — everything keeps working for `graceDays`, with a line in the log on every start and a
field in `/healthz` saying so. After the grace runs out, meetings that are running stay up and new
joins are refused with a reason that says the licence expired, not "unauthorized". The room service
keeps answering, so the platform above can still see what happened and close its own books.

**A feature that is not licensed** — it does not start, it says so once at startup, and `/healthz`
carries which features are on. A room that asks to be recorded on a server without the feature is
recorded by nobody, and the meeting record says the recording was refused rather than leaving a
gap somebody finds a month later.

The rule behind all three: a licence problem is the operator's problem, and the people in a call
did not cause it and cannot fix it. Ending their meeting to make a commercial point is the one
thing this must never do.

## What it cannot do, said plainly

The customer has the binary. Anybody with a disassembler and an hour can cut the check out, and no
amount of obfuscation changes that — it only makes our own build slower and our own crashes harder
to read.

So this is a commercial instrument, not a security boundary. What it buys is that going over what
was agreed has to be a deliberate act rather than an accident, and that the act is legible
afterwards. That is enough for the contracts this would be sold under, and anything more expensive
is money spent against an attacker who was never going to pay anyway.

Two things follow. The private key never goes near a server or a repository — it lives with
whoever issues licences, and the tool that mints them is a separate program, not a subcommand of
the thing customers run. And a leaked private key is a rebuild, not a patch: the public key is
compiled in, so rotating it means a new release and reissuing every live licence, which is an
argument for keeping the key somewhere boring and offline rather than for making it clever.

## What it would take

| Piece | Effort |
|---|---|
| `crates/license`: parse, verify, expiry and grace, with its own tests | half a day |
| Configuration block, startup refusal, daily re-read so a renewed key needs no restart | half a day |
| The four feature gates, each at the call that already decides | half a day |
| `/healthz` and the log lines: customer, expiry, grace, which features are on | two hours |
| The minting tool, in a repository of its own | half a day |
| Prose: this file rewritten as documentation, and the operator's half in the README | half a day |

About two and a half days, and none of it is on the media path.

## Decided

- Ed25519, verified offline, public key in the binary.
- Features rather than counts, in the first version.
- No licence at all is a refusal to start; an expired one is a grace period and then refused joins;
  an unlicensed feature is a feature that does not start.
- A meeting in progress is never ended by any of this.

## Still to decide

1. **The feature list.** The four above are the natural seams in today's code. Whether the tiers
   are those four, or bundles of them, is a pricing question rather than a code one — the list is
   one array, and adding a name to it is a line.
2. **What a licence is issued against.** Nothing today ties a key to a machine. Binding it to a
   hostname or a node id makes copying one key across installations visible, and makes a legitimate
   migration a support ticket. Worth doing only if licences are expected to leak between customers.
3. **How long a term is, and how a renewal reaches the server.** A file on disk that an operator
   replaces is the simplest thing that works; anything better needs the phone-home this design
   deliberately does not have.
4. **What the trial is.** A build with no licence refuses to start, which leaves nothing to try. A
   time-limited licence issued by us solves it without any code; a built-in trial mode does not,
   and is a second code path that has to be maintained forever.
