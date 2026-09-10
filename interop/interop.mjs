#!/usr/bin/env node
/**
 * mediacore, driven by LiveKit's own server SDK.
 *
 * Everything this file asserts, `crates/server/tests/roomservice.rs` already
 * asserts more thoroughly and in far less time. The one thing it cannot assert
 * is the thing this file exists for: that the answers are readable by code
 * nobody here wrote. A token minted by their `AccessToken`, a request built by
 * their generator, a response fed to their `Room.fromJson`, a refusal caught as
 * their `TwirpError`, and a webhook checked by their `WebhookReceiver` — those
 * are five different ways for a divergence to show up, and none of them can be
 * reached from a test that speaks to the router in-process.
 *
 * So every assertion here is on a PARSED TYPE and a field value. A 200 proves
 * nothing: a server that dropped a field, spelled it in snake_case, or filled it
 * with something their parser reads as zero answers 200 to every call in this
 * file, and what is checked instead is what their parser hands a caller —
 * `creationTime` as a `bigint` that is a real timestamp, `emptyTimeout` as the
 * number that was asked for.
 *
 * Not the JSON shape underneath it: `@livekit/protocol` takes a quoted string or
 * a bare number for either field and normalises to the type in the .proto, so no
 * assertion made through their SDK can see which one the server wrote. That is
 * pinned in `crates/server/tests/roomservice.rs`, where the raw body is there to
 * look at.
 *
 * Three modes, because one process has to own the webhook receiver for the
 * whole run — the `lk` phase makes events too, and an event posted at a port
 * nobody is listening on is an event this job never sees:
 *
 *   receiver  an HTTP server that verifies each POST and writes down what it
 *             found; started before mediacore and stopped after it
 *   api       the room service, through livekit-server-sdk
 *   webhooks  the assertions over what the receiver wrote down
 *
 * `run.sh` runs all three in that order and is the only supported way in.
 */

import { createServer } from 'node:http';
import { appendFileSync, readFileSync } from 'node:fs';
import { setTimeout as sleep } from 'node:timers/promises';

import {
  AccessToken,
  AgentDispatchClient,
  DataPacket_Kind,
  EgressClient,
  EncodedFileOutput,
  IngressClient,
  ParticipantInfo,
  ParticipantInfo_State,
  Room,
  RoomAgentDispatch,
  RoomCompositeEgressRequest,
  RoomEgress,
  RoomServiceClient,
  SipClient,
  TrackInfo,
  TrackSource,
  TrackType,
  TwirpError,
  WebhookReceiver,
} from 'livekit-server-sdk';

// The signalling wire is not part of livekit-server-sdk -- it is a server SDK,
// and joining a room is a client's job -- so the frames come from the protocol
// package the SDK itself depends on, at the version it pins. A participant has
// to exist before ListParticipants, UpdateParticipant, MutePublishedTrack and
// RemoveParticipant assert anything at all, and hand-rolling protobuf here to
// avoid the import would throw away the half of the interop that matters: these
// are LiveKit's own generated encoders and decoders, aimed at our signalling
// socket.
//
// No media crosses it. The socket carries a join and an AddTrack, both of which
// mediacore answers before any SDP is exchanged; there is no peer connection,
// no ICE and no RTP. RTC ports on a runner are their own story.
import {
  AddTrackRequest,
  SignalRequest,
  SignalResponse,
} from '@livekit/protocol';

const URL_ = process.env.LIVEKIT_URL ?? 'http://127.0.0.1:7880';
const KEY = process.env.LIVEKIT_API_KEY ?? 'interopkey';
const SECRET = process.env.LIVEKIT_API_SECRET ?? '';
const WEBHOOK_PORT = Number(process.env.MEDIACORE_INTEROP_WEBHOOK_PORT ?? 7899);
const WEBHOOK_LOG = process.env.MEDIACORE_INTEROP_WEBHOOK_LOG ?? 'webhooks.jsonl';

/** The room the `lk` phase makes and unmakes, so its events can be recognised. */
const CLI_ROOM = process.env.MEDIACORE_INTEROP_CLI_ROOM ?? 'interop-cli';
/** The room this file makes and unmakes. */
const SDK_ROOM = process.env.MEDIACORE_INTEROP_SDK_ROOM ?? 'interop-sdk';
/** Who joins `SDK_ROOM` over the signalling socket. */
const IDENTITY = 'dana';

// ---------------------------------------------------------------------------
// Assertions
// ---------------------------------------------------------------------------

let checked = 0;
const failures = [];

/** One assertion. Never throws: a run reports every divergence it found, not
 * the first, because the second one is usually the one that explains it. */
function expect(what, condition, detail = '') {
  checked += 1;
  if (condition) {
    console.log(`  ok   ${what}`);
    return true;
  }
  failures.push(detail ? `${what}: ${detail}` : what);
  console.log(`  FAIL ${what}${detail ? `: ${detail}` : ''}`);
  return false;
}

function same(what, actual, wanted) {
  return expect(
    what,
    actual === wanted,
    `expected ${render(wanted)}, got ${render(actual)}`,
  );
}

function render(value) {
  if (typeof value === 'bigint') return `${value}n`;
  if (typeof value === 'string') return JSON.stringify(value);
  return String(value);
}

/**
 * A call that must come back as a typed Twirp error rather than as a transport
 * failure.
 *
 * The distinction is the whole point of asserting on refusals at all. `fetch`
 * rejecting, a socket closing, or a 501 arriving with a body their parser
 * cannot read all look the same to a caller who only checks that the promise
 * rejected -- and only one of the three is mediacore answering the question.
 */
async function refused(what, call, { code, status, mentions }) {
  let thrown;
  try {
    await call();
  } catch (error) {
    thrown = error;
  }

  if (!thrown) {
    return expect(what, false, 'the call succeeded, and it must not have');
  }
  if (!(thrown instanceof TwirpError)) {
    return expect(
      what,
      false,
      `a transport failure rather than a Twirp error: ${thrown.constructor.name}: ${thrown.message}`,
    );
  }

  let held = expect(`${what} is a typed Twirp error`, true);
  held = same(`${what} code`, thrown.code, code) && held;
  held = same(`${what} status`, thrown.status, status) && held;
  if (mentions !== undefined) {
    held =
      expect(
        `${what} says why`,
        thrown.message.includes(mentions),
        `expected the message to mention ${JSON.stringify(mentions)}, got ${JSON.stringify(thrown.message)}`,
      ) && held;
  }
  return held;
}

function heading(text) {
  console.log(`\n== ${text}`);
}

function report(phase) {
  console.log(
    `\n${failures.length ? 'FAILED' : 'passed'}: ${phase}, ${checked} assertions, ${failures.length} failed`,
  );
  for (const failure of failures) console.log(`  - ${failure}`);
  return failures.length === 0;
}

// ---------------------------------------------------------------------------
// The webhook receiver
// ---------------------------------------------------------------------------

/**
 * Somebody else's receiver, on somebody else's terms.
 *
 * `WebhookReceiver.receive(body, authHeader)` is exactly what LiveKit's own
 * documentation tells an integrator to call, and it is handed the `Authorization`
 * header VERBATIM -- because that is what an integrator's code does, and a job
 * that tidies the header up first would prove that mediacore signs correctly
 * while proving nothing about whether an unmodified LiveKit receiver takes what
 * mediacore sends.
 *
 * A second attempt with the `Bearer ` scheme removed is made only so that a
 * failure can say which half broke. A signature that verifies once the scheme is
 * gone is a framing bug worth one line of Rust; a signature that verifies neither
 * way is a signing bug worth a morning. The report should not make somebody find
 * that out by hand.
 */
function receiver() {
  const rx = new WebhookReceiver(KEY, SECRET);

  const server = createServer((request, response) => {
    // run.sh polls this before it starts mediacore. A receiver that came up
    // second would miss room_started, and with attempts: 1 in config.yaml there
    // is no retry behind it to catch the miss.
    if (request.method === 'GET' && request.url === '/ready') {
      response.writeHead(200, { 'content-type': 'text/plain' });
      response.end('ready');
      return;
    }

    const chunks = [];
    request.on('data', (chunk) => chunks.push(chunk));
    request.on('end', async () => {
      const body = Buffer.concat(chunks).toString('utf8');
      const authorization = request.headers.authorization;
      const record = {
        at: new Date().toISOString(),
        path: request.url,
        contentType: request.headers['content-type'] ?? null,
        userAgent: request.headers['user-agent'] ?? null,
        hasAuthorization: typeof authorization === 'string' && authorization.length > 0,
        verbatim: await verify(rx, body, authorization),
        stripped: await verify(rx, body, authorization?.replace(/^Bearer\s+/i, '')),
      };
      appendFileSync(WEBHOOK_LOG, `${JSON.stringify(record)}\n`);

      // Always 200. A receiver that refuses what it could not verify would put
      // mediacore into its retry loop and turn one divergence into five
      // identical lines in the log.
      response.writeHead(200, { 'content-type': 'text/plain' });
      response.end('ok');
    });
  });

  server.listen(WEBHOOK_PORT, '127.0.0.1', () => {
    console.log(`webhook receiver listening on 127.0.0.1:${WEBHOOK_PORT}, writing ${WEBHOOK_LOG}`);
  });

  for (const signal of ['SIGTERM', 'SIGINT']) {
    process.on(signal, () => {
      server.close(() => process.exit(0));
    });
  }
}

async function verify(rx, body, authorization) {
  if (!authorization) return { ok: false, error: 'no Authorization header' };
  try {
    const event = await rx.receive(body, authorization);
    return {
      ok: true,
      event: event.event,
      id: event.id,
      room: event.room?.name ?? null,
      participant: event.participant?.identity ?? null,
      createdAt: String(event.createdAt ?? ''),
    };
  } catch (error) {
    return { ok: false, error: `${error.constructor.name}: ${error.message}` };
  }
}

// ---------------------------------------------------------------------------
// The signalling socket
// ---------------------------------------------------------------------------

/**
 * One participant, joined and publishing, with nothing negotiated.
 *
 * mediacore registers a participant when the socket opens and publishes a track
 * when it is asked to, both before any offer is exchanged -- so this is enough
 * to give the four participant methods something real to act on, and it is the
 * whole of the media path this job touches.
 */
async function joined(room, identity) {
  const token = new AccessToken(KEY, SECRET, { identity, name: 'Dana Interop', ttl: '10m' });
  token.addGrant({ roomJoin: true, room, canPublish: true, canSubscribe: true });

  const socket = new WebSocket(
    `${URL_.replace(/^http/, 'ws')}/rtc?access_token=${await token.toJwt()}` +
      '&protocol=17&sdk=interop&version=0.0.0&auto_subscribe=0',
  );
  socket.binaryType = 'arraybuffer';

  const waiting = new Map();
  const arrived = [];
  socket.onmessage = (message) => {
    const response = SignalResponse.fromBinary(new Uint8Array(message.data));
    arrived.push(response);
    const settle = waiting.get(response.message.case);
    if (settle) {
      waiting.delete(response.message.case);
      settle(response.message.value);
    }
  };

  const next = (kind) =>
    Promise.race([
      new Promise((settle) => waiting.set(kind, settle)),
      sleep(10_000).then(() => {
        throw new Error(
          `no ${kind} arrived on the signalling socket; saw ${arrived.map((r) => r.message.case).join(', ') || 'nothing'}`,
        );
      }),
    ]);

  await new Promise((settle, fail) => {
    socket.onopen = settle;
    socket.onerror = () => fail(new Error(`the signalling socket refused ${URL_}/rtc`));
  });

  const join = await next('join');

  const publish = next('trackPublished');
  socket.send(
    new SignalRequest({
      message: {
        case: 'addTrack',
        value: new AddTrackRequest({
          cid: 'interop-audio',
          name: 'interop-audio',
          type: TrackType.AUDIO,
          source: TrackSource.MICROPHONE,
        }),
      },
    }).toBinary(),
  );
  const published = await publish;

  return { socket, join, track: published.track };
}

// ---------------------------------------------------------------------------
// The room service
// ---------------------------------------------------------------------------

async function api() {
  const svc = new RoomServiceClient(URL_, KEY, SECRET);

  // Whatever a previous run left behind. DeleteRoom on a room that is already
  // gone is not a failure, which is the documented behaviour and also what
  // makes this safe to call before anything exists.
  await svc.deleteRoom(SDK_ROOM);

  heading('CreateRoom');
  const created = await svc.createRoom({
    name: SDK_ROOM,
    emptyTimeout: 600,
    departureTimeout: 45,
    maxParticipants: 12,
    metadata: '{"interop":"created"}',
  });
  expect('CreateRoom answers a Room their parser accepts', created instanceof Room);
  same('Room.name', created.name, SDK_ROOM);
  expect('Room.sid is set', created.sid.length > 0, `got ${JSON.stringify(created.sid)}`);
  // The two lines an integrator's own code would trip over: a 64-bit field
  // reaches them as a bigint and a uint32 as a number, and code that compares
  // either against the wrong one silently does nothing. Which JSON shape the
  // server wrote is invisible from here -- their parser takes a quoted string
  // and a bare number for both -- and is asserted where the raw body is, in
  // crates/server/tests/roomservice.rs.
  expect(
    'Room.creationTime decodes as an int64',
    typeof created.creationTime === 'bigint' && created.creationTime > 0n,
    `got ${typeof created.creationTime} ${render(created.creationTime)}`,
  );
  same('Room.emptyTimeout', created.emptyTimeout, 600);
  same('Room.departureTimeout', created.departureTimeout, 45);
  same('Room.maxParticipants', created.maxParticipants, 12);
  same('Room.metadata', created.metadata, '{"interop":"created"}');
  same('Room.numParticipants', created.numParticipants, 0);

  // An unscoped roomCreate token -- which is the only kind livekit-server-sdk
  // mints for CreateRoom -- gets the existing room back untouched, and the
  // metadata in the answer is how a caller sees that nothing was applied.
  const again = await svc.createRoom({ name: SDK_ROOM, metadata: '{"interop":"ignored"}' });
  same('CreateRoom on an existing room returns the same sid', again.sid, created.sid);
  same('CreateRoom on an existing room applies nothing', again.metadata, created.metadata);

  heading('ListRooms');
  const listed = await svc.listRooms([SDK_ROOM]);
  same('ListRooms honours the name filter', listed.length, 1);
  expect('ListRooms answers Rooms their parser accepts', listed[0] instanceof Room);
  same('ListRooms reports the sid', listed[0].sid, created.sid);
  same('ListRooms reports the metadata whole', listed[0].metadata, created.metadata);
  const everything = await svc.listRooms();
  expect(
    'ListRooms with no filter includes the room',
    everything.some((room) => room.name === SDK_ROOM),
    `saw ${everything.map((room) => room.name).join(', ') || 'nothing'}`,
  );

  heading('UpdateRoomMetadata');
  const remetadata = await svc.updateRoomMetadata(SDK_ROOM, '{"interop":"updated"}');
  expect('UpdateRoomMetadata answers a Room', remetadata instanceof Room);
  same('UpdateRoomMetadata applies the metadata', remetadata.metadata, '{"interop":"updated"}');
  same('UpdateRoomMetadata keeps the sid', remetadata.sid, created.sid);

  heading('A participant, over the signalling socket');
  const { socket, join, track } = await joined(SDK_ROOM, IDENTITY);
  same('JoinResponse names the room', join.room?.name, SDK_ROOM);
  same('JoinResponse names the participant', join.participant?.identity, IDENTITY);
  expect(
    'AddTrack is answered with a TrackInfo',
    track instanceof TrackInfo && track.sid.length > 0,
    `got ${render(track?.sid)}`,
  );
  same('the published track is audio', track.type, TrackType.AUDIO);

  heading('ListParticipants and GetParticipant');
  const roster = await svc.listParticipants(SDK_ROOM);
  same('ListParticipants finds one', roster.length, 1);
  expect('ListParticipants answers ParticipantInfos', roster[0] instanceof ParticipantInfo);
  same('ParticipantInfo.identity', roster[0].identity, IDENTITY);
  same('ParticipantInfo.state', roster[0].state, ParticipantInfo_State.ACTIVE);
  same('ParticipantInfo carries the published track', roster[0].tracks.length, 1);
  expect(
    'ParticipantInfo.joinedAt decodes as an int64',
    typeof roster[0].joinedAt === 'bigint' && roster[0].joinedAt > 0n,
    `got ${typeof roster[0].joinedAt} ${render(roster[0].joinedAt)}`,
  );

  const one = await svc.getParticipant(SDK_ROOM, IDENTITY);
  same('GetParticipant matches the roster', one.sid, roster[0].sid);
  same('GetParticipant reports the name from the token', one.name, 'Dana Interop');
  same('GetParticipant reports canPublish', one.permission?.canPublish, true);

  heading('UpdateParticipant');
  const updated = await svc.updateParticipant(SDK_ROOM, IDENTITY, {
    metadata: '{"seat":"front"}',
    name: 'Dana Updated',
  });
  expect('UpdateParticipant answers a ParticipantInfo', updated instanceof ParticipantInfo);
  same('UpdateParticipant applies the metadata', updated.metadata, '{"seat":"front"}');
  same('UpdateParticipant applies the name', updated.name, 'Dana Updated');
  expect(
    'UpdateParticipant moves the version on',
    updated.version > roster[0].version,
    `expected more than ${roster[0].version}, got ${updated.version}`,
  );

  heading('MutePublishedTrack');
  const muted = await svc.mutePublishedTrack(SDK_ROOM, IDENTITY, track.sid, true);
  expect('MutePublishedTrack answers a TrackInfo', muted instanceof TrackInfo);
  same('the muted track is the one asked for', muted.sid, track.sid);
  same('the track comes back muted', muted.muted, true);
  const unmuted = await svc.mutePublishedTrack(SDK_ROOM, IDENTITY, track.sid, false);
  same('the track comes back unmuted', unmuted.muted, false);

  heading('SendData');
  // Broadcast, and it reaches nobody: a data packet goes out over each
  // participant's own data channel, and the participant this job joined has a
  // signalling socket and no peer connection. The assertion is that the call is
  // accepted anyway, which is the documented behaviour -- a channel that has not
  // opened is not waited for -- and that the SDK's `nonce` and empty
  // `destinationSids` do not upset the decoder at this end.
  await svc.sendData(SDK_ROOM, new TextEncoder().encode('interop'), DataPacket_Kind.RELIABLE, {
    topic: 'interop',
  });
  expect('SendData to the whole room is accepted', true);

  heading('Refusals, as typed Twirp errors');
  await refused(
    'GetParticipant for somebody who is not there',
    () => svc.getParticipant(SDK_ROOM, 'nobody'),
    { code: 'not_found', status: 404, mentions: 'nobody' },
  );
  await refused('CreateRoom with no name', () => svc.createRoom({ name: '' }), {
    code: 'invalid_argument',
    status: 400,
    mentions: 'name',
  });
  // Naming somebody is a different promise from broadcasting: a caller who
  // addressed a packet and was told nothing went wrong would believe it arrived.
  await refused(
    'SendData to an identity with no data channel open',
    () =>
      svc.sendData(SDK_ROOM, new TextEncoder().encode('interop'), DataPacket_Kind.RELIABLE, {
        destinationIdentities: [IDENTITY],
      }),
    { code: 'not_found', status: 404, mentions: 'listening' },
  );
  await refused(
    'SendData with destinationSids',
    () =>
      svc.sendData(SDK_ROOM, new TextEncoder().encode('interop'), DataPacket_Kind.RELIABLE, {
        destinationSids: ['PA_nobody'],
      }),
    { code: 'unimplemented', status: 501, mentions: 'destinationSids' },
  );
  await refused(
    'a token signed with the wrong secret',
    () => new RoomServiceClient(URL_, KEY, `${SECRET}-wrong`).listRooms(),
    { code: 'unauthenticated', status: 401 },
  );

  // The methods the table marks `Support::Refused`. A single node cannot move a
  // participant to another node, and answering 501 to say so is the correct
  // answer -- what is being asserted is that it arrives as a refusal their SDK
  // can read the code out of, rather than as a 501 with a body it cannot parse.
  await refused(
    'ForwardParticipant',
    () => svc.forwardParticipant(SDK_ROOM, IDENTITY, 'elsewhere'),
    { code: 'unimplemented', status: 501, mentions: 'single node' },
  );
  await refused('MoveParticipant', () => svc.moveParticipant(SDK_ROOM, IDENTITY, 'elsewhere'), {
    code: 'unimplemented',
    status: 501,
    mentions: 'single node',
  });

  // The CreateRoom fields that promise something this server has not got. A
  // recording that will never be made and an agent that will never join must be
  // refused rather than accepted and ignored, and the refusal has to name the
  // field or the caller is left guessing which of the six they sent was the
  // problem.
  await refused(
    'CreateRoom asking for egress',
    () =>
      svc.createRoom({
        name: `${SDK_ROOM}-egress`,
        egress: new RoomEgress({
          room: new RoomCompositeEgressRequest({
            roomName: `${SDK_ROOM}-egress`,
            output: { case: 'file', value: new EncodedFileOutput({ filepath: '/tmp/interop.mp4' }) },
          }),
        }),
      }),
    { code: 'unimplemented', status: 501, mentions: 'egress' },
  );
  await refused(
    'CreateRoom asking for an agent',
    () =>
      svc.createRoom({
        name: `${SDK_ROOM}-agents`,
        agents: [new RoomAgentDispatch({ agentName: 'interop' })],
      }),
    { code: 'unimplemented', status: 501, mentions: 'agents' },
  );

  heading('The services this server does not carry');
  // Egress, ingress, SIP and agent dispatch are not methods of RoomService --
  // they are services of their own, and mediacore routes none of them. What an
  // SDK gets is a 404 that its Twirp layer still turns into a typed error, so a
  // caller sees a refusal rather than a crash; it carries no Twirp code, because
  // there is no handler to write a Twirp body. That is what is asserted here:
  // today's answer, exactly. `README.md` in this directory says what a
  // `bad_route` body would add and why it is not free.
  for (const [what, call] of [
    ['ListEgress', () => new EgressClient(URL_, KEY, SECRET).listEgress({})],
    ['ListIngress', () => new IngressClient(URL_, KEY, SECRET).listIngress({})],
    ['ListSIPInboundTrunk', () => new SipClient(URL_, KEY, SECRET).listSipInboundTrunk()],
    ['ListAgentDispatch', () => new AgentDispatchClient(URL_, KEY, SECRET).listDispatch(SDK_ROOM)],
  ]) {
    let thrown;
    try {
      await call();
    } catch (error) {
      thrown = error;
    }
    expect(
      `${what} is refused as a typed error rather than a transport failure`,
      thrown instanceof TwirpError && thrown.status === 404,
      thrown
        ? `${thrown.constructor.name} status=${thrown.status} code=${thrown.code}`
        : 'the call succeeded, and this server carries no such service',
    );
    console.log(`  note ${what} carries no Twirp code: code=${String(thrown?.code)}`);
  }

  heading('RemoveParticipant');
  await svc.removeParticipant(SDK_ROOM, IDENTITY);
  await settled(() => svc.listParticipants(SDK_ROOM), (roster) => roster.length === 0);
  same('the room is empty after RemoveParticipant', (await svc.listParticipants(SDK_ROOM)).length, 0);
  socket.close();
  await refused(
    'RemoveParticipant for somebody who has gone',
    () => svc.removeParticipant(SDK_ROOM, IDENTITY),
    { code: 'not_found', status: 404, mentions: IDENTITY },
  );

  heading('DeleteRoom');
  await svc.deleteRoom(SDK_ROOM);
  const afterwards = await svc.listRooms();
  expect(
    'DeleteRoom takes the room off the list',
    !afterwards.some((room) => room.name === SDK_ROOM),
    `saw ${afterwards.map((room) => room.name).join(', ')}`,
  );
  await svc.deleteRoom(SDK_ROOM);
  expect('DeleteRoom on a room that is already gone is not a failure', true);

  return report('the room service through livekit-server-sdk');
}

/** Polls until a call answers what is wanted, because RemoveParticipant tears a
 * session down on another task and a roster read a millisecond later would race
 * it. */
async function settled(call, wanted, attempts = 50) {
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    if (wanted(await call())) return true;
    await sleep(100);
  }
  return false;
}

// ---------------------------------------------------------------------------
// What the receiver wrote down
// ---------------------------------------------------------------------------

function webhooks() {
  const lines = readFileSync(WEBHOOK_LOG, 'utf8')
    .split('\n')
    .filter((line) => line.trim().length > 0)
    .map((line) => JSON.parse(line));

  heading('Webhooks, through their WebhookReceiver');
  if (!expect('mediacore posted something', lines.length > 0, `${WEBHOOK_LOG} is empty`)) {
    return report('webhooks');
  }
  console.log(`  note ${lines.length} deliveries were received`);

  expect(
    'every delivery carries LiveKit\'s webhook content type',
    lines.every((line) => line.contentType === 'application/webhook+json'),
    `saw ${[...new Set(lines.map((line) => line.contentType))].join(', ')}`,
  );
  expect(
    'every delivery carries an Authorization header',
    lines.every((line) => line.hasAuthorization),
  );

  // The single most valuable assertion in the job: their receiver, handed the
  // header as it arrived, the way an integrator's code hands it over.
  const rejected = lines.filter((line) => !line.verbatim.ok);
  const salvaged = rejected.filter((line) => line.stripped.ok);
  expect(
    'their WebhookReceiver verifies every delivery',
    rejected.length === 0,
    rejected.length === 0
      ? ''
      : `${rejected.length} of ${lines.length} were rejected, first: ${rejected[0].verbatim.error}` +
        (salvaged.length === rejected.length
          ? '. Every one of them verifies once the `Bearer ` scheme is taken off the ' +
            'Authorization header, so the signature and the sha256 digest are right and the ' +
            'framing is not: LiveKit sends the bare JWT (protocol/webhook/url_notifier.go, ' +
            '`r.Header.Set(authHeader, token)`) and both their Go verifier and their ' +
            'WebhookReceiver put the header value straight into a JWT verify. ' +
            'crates/events/src/webhook.rs::authorization returns `format!("Bearer {token}")`.'
          : ''),
  );

  const verified = lines.map((line) => (line.verbatim.ok ? line.verbatim : line.stripped)).filter((v) => v.ok);
  const named = (event, room) => verified.some((v) => v.event === event && v.room === room);

  // Both phases of the run are in this file, which is the reason the receiver
  // outlives them both: the `lk` phase makes and unmakes a room too, and an
  // event it fired at a port nobody was listening on would be a hole nobody saw.
  expect(`room_started arrived for ${CLI_ROOM}`, named('room_started', CLI_ROOM));
  expect(`room_finished arrived for ${CLI_ROOM}`, named('room_finished', CLI_ROOM));
  expect(`room_started arrived for ${SDK_ROOM}`, named('room_started', SDK_ROOM));
  expect(`room_finished arrived for ${SDK_ROOM}`, named('room_finished', SDK_ROOM));
  expect(
    `participant_joined arrived for ${IDENTITY}`,
    verified.some((v) => v.event === 'participant_joined' && v.participant === IDENTITY),
  );
  expect(
    `participant_left arrived for ${IDENTITY}`,
    verified.some((v) => v.event === 'participant_left' && v.participant === IDENTITY),
  );

  expect(
    'every event carries an id',
    verified.every((v) => typeof v.id === 'string' && v.id.length > 0),
  );
  console.log(`  note events seen: ${[...new Set(verified.map((v) => v.event))].sort().join(', ')}`);

  // LiveKit's notifier sets one, and a receiver -- or a proxy in front of one --
  // is entitled to filter on it. Not a failure: nothing in their verification
  // path reads it.
  const agents = [...new Set(lines.map((line) => line.userAgent))];
  console.log(`  note User-Agent on the deliveries: ${agents.map((a) => String(a)).join(', ')}`);

  return report('webhooks');
}

// ---------------------------------------------------------------------------

const mode = process.argv[2];

if (!SECRET) {
  console.error('LIVEKIT_API_SECRET is empty; run this through run.sh');
  process.exit(2);
}

switch (mode) {
  case 'receiver':
    receiver();
    break;
  case 'api':
    process.exit((await api()) ? 0 : 1);
    break;
  case 'webhooks':
    process.exit(webhooks() ? 0 : 1);
    break;
  default:
    console.error(`usage: interop.mjs <receiver|api|webhooks>, got ${JSON.stringify(mode)}`);
    process.exit(2);
}
