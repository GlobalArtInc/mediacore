const TOTAL = Symbol("inbound-total");
const VIDEO = Symbol("inbound-video");

const RUNGS = [
  { label: "low", ceiling: 200 },
  { label: "mid", ceiling: 400 },
  { label: "high", ceiling: Number.POSITIVE_INFINITY },
];

const RID_LABELS = { q: "low", h: "mid", f: "high" };

export const NO_SUBSCRIBER =
  "the vendored client exposes no subscriber connection to read statistics from";
export const NO_PUBLISHER =
  "the vendored client exposes no publisher connection to read statistics from";
export const NOT_PUBLISHING = "no video is being published";
export const FIRST_SAMPLE = "waiting for a second sample to measure a rate against";
export const NOT_REPORTED = "this browser does not report the value";
export const NO_TRACK_STATS =
  "the browser reported no inbound statistics for this track";
export const NO_FRAMES = "no frame has been decoded yet";
export const NO_PACKETS = "no packet arrived during the last sample";
export const NO_CANDIDATE_PAIR = "the browser has not settled on a candidate pair yet";
export const NO_INCOMING_ESTIMATE =
  "this browser does not estimate what it can receive, so watch the video rate instead";
export const NO_VIDEO_ARRIVING = "no video stream is being received at all";

export function layerFor(height) {
  if (typeof height !== "number" || !Number.isFinite(height) || height <= 0) {
    return null;
  }
  const rung = RUNGS.find((entry) => height <= entry.ceiling);
  return rung ? rung.label : null;
}

export function labelForRid(rid) {
  if (typeof rid !== "string" || rid === "") return null;
  return RID_LABELS[rid] || rid;
}

export function createBench(room) {
  const history = new Map();
  const layers = new Map();

  async function read() {
    const reading = {
      at: Date.now(),
      link: {
        estimate: null,
        arriving: null,
        video: null,
        rtt: null,
        sending: null,
        note: null,
        pairNote: null,
        estimateNote: null,
        videoNote: null,
        sendingNote: NO_PUBLISHER,
      },
      tracks: new Map(),
      local: [],
      localTracks: new Map(),
      localNote: null,
    };

    const subscriber = connectionOf(room, "subscriber");
    if (!subscriber) {
      reading.link.note = NO_SUBSCRIBER;
    } else {
      const report = await subscriber.getStats();
      readLink(reading.link, report, history);
      reading.tracks = await readRemote(room, report, history, layers);
    }

    const publisher = connectionOf(room, "publisher");
    if (publisher) {
      const report = await publisher.getStats();
      readOutgoing(reading.link, report);
      readLocal(reading, report, history);
    } else {
      reading.localNote = NO_PUBLISHER;
    }

    return reading;
  }

  function forget(key) {
    history.delete(key);
    layers.delete(key);
  }

  function reset() {
    history.clear();
    layers.clear();
  }

  return { read, forget, reset };
}

function connectionOf(room, role) {
  const manager = room && room.engine ? room.engine.pcManager : null;
  const transport = manager ? manager[role] : null;
  if (!transport) return null;

  const connection = transport._pc;
  if (!connection || typeof connection.getStats !== "function") return null;
  if (connection.connectionState === "closed") return null;
  return connection;
}

function readLink(link, report, history) {
  const pair = selectedPair(report);
  if (pair) {
    link.estimate = finite(pair.availableIncomingBitrate);
    if (link.estimate === null) link.estimateNote = NO_INCOMING_ESTIMATE;
    const rtt = finite(pair.currentRoundTripTime);
    link.rtt = rtt === null ? null : rtt * 1000;
  } else {
    link.pairNote = NO_CANDIDATE_PAIR;
  }
  link.arriving = aggregate(report, history, TOTAL, null);
  link.video = aggregate(report, history, VIDEO, "video");
  if (link.video === null) {
    link.videoNote = hasInboundVideo(report) ? FIRST_SAMPLE : NO_VIDEO_ARRIVING;
  }
}

function hasInboundVideo(report) {
  let found = false;
  report.forEach((entry) => {
    if (entry.type === "inbound-rtp" && entry.kind === "video") found = true;
  });
  return found;
}

function readOutgoing(link, report) {
  const pair = selectedPair(report);
  if (!pair) {
    link.sendingNote = NO_CANDIDATE_PAIR;
    return;
  }
  link.sending = finite(pair.availableOutgoingBitrate);
  link.sendingNote = link.sending === null ? NOT_REPORTED : null;
}

function selectedPair(report) {
  let transport = null;
  report.forEach((entry) => {
    if (entry.type === "transport" && transport === null) transport = entry;
  });

  if (transport && transport.selectedCandidatePairId) {
    const pair = report.get(transport.selectedCandidatePairId);
    if (pair) return pair;
  }

  let fallback = null;
  report.forEach((entry) => {
    if (entry.type !== "candidate-pair" || fallback !== null) return;
    if (entry.nominated || entry.state === "succeeded") fallback = entry;
  });
  return fallback;
}

function aggregate(report, history, key, kind) {
  let bytes = 0;
  let timestamp = null;
  let seen = false;

  report.forEach((entry) => {
    if (entry.type !== "inbound-rtp") return;
    if (kind !== null && entry.kind !== kind) return;
    seen = true;
    const received = finite(entry.bytesReceived);
    if (received !== null) bytes += received;
    const at = finite(entry.timestamp);
    if (at !== null && (timestamp === null || at > timestamp)) timestamp = at;
  });

  if (!seen || timestamp === null) return null;

  const current = { bytes, timestamp };
  const previous = history.get(key);
  history.set(key, current);
  return bitrate(current, previous, "bytes");
}

async function readRemote(room, report, history, layers) {
  const inbound = indexInbound(report);
  const readings = new Map();
  if (!room || !room.remoteParticipants) return readings;

  for (const participant of room.remoteParticipants.values()) {
    for (const publication of participant.trackPublications.values()) {
      const track = publication.track;
      if (!track || !track.mediaStreamTrack) continue;

      let stats = inbound.get(track.mediaStreamTrack.id) || null;
      if (!stats) stats = await fromReceiver(track);

      readings.set(
        publication.trackSid,
        trackReading(participant, publication, track, stats, history, layers),
      );
    }
  }

  return readings;
}

function indexInbound(report) {
  const inbound = new Map();
  report.forEach((entry) => {
    if (entry.type !== "inbound-rtp") return;
    if (typeof entry.trackIdentifier !== "string") return;
    inbound.set(entry.trackIdentifier, entry);
  });
  return inbound;
}

async function fromReceiver(track) {
  const receiver = track.receiver;
  if (!receiver || typeof receiver.getStats !== "function") return null;

  let report;
  try {
    report = await receiver.getStats();
  } catch (error) {
    console.warn("the receiver refused to report statistics", error);
    return null;
  }

  let found = null;
  report.forEach((entry) => {
    if (entry.type === "inbound-rtp" && found === null) found = entry;
  });
  return found;
}

function trackReading(participant, publication, track, stats, history, layers) {
  const kind = publication.kind === "video" ? "video" : "audio";
  const reading = {
    sid: publication.trackSid,
    identity: participant.identity,
    kind,
    streamState: track.streamState || "unknown",
    muted: Boolean(publication.isMuted),
    bitrate: null,
    loss: null,
    packets: null,
    width: null,
    height: null,
    fps: null,
    jitter: null,
    freezes: null,
    layer: null,
    changes: 0,
    lastChange: null,
    note: stats ? null : NO_TRACK_STATS,
  };

  if (!stats) return reading;

  const key = publication.trackSid;
  const current = {
    bytes: finite(stats.bytesReceived),
    packets: finite(stats.packetsReceived),
    lost: finite(stats.packetsLost),
    timestamp: finite(stats.timestamp),
  };
  const previous = history.get(key);
  history.set(key, current);

  reading.bitrate = bitrate(current, previous, "bytes");
  const received = change(current, previous, "packets");
  const lost = change(current, previous, "lost");
  reading.packets = received;
  if (received !== null && lost !== null && received + lost > 0) {
    reading.loss = lost / (received + lost);
  }

  reading.width = finite(stats.frameWidth);
  reading.height = finite(stats.frameHeight);
  reading.fps = finite(stats.framesPerSecond);
  reading.freezes = finite(stats.freezeCount);
  const jitter = finite(stats.jitter);
  reading.jitter = jitter === null ? null : jitter * 1000;

  if (kind === "video") {
    reading.layer = layerFor(reading.height);
    applyLayerChange(layers, key, reading);
  }

  return reading;
}

function applyLayerChange(layers, key, reading) {
  const seen = layers.get(key) || { layer: null, changes: 0, lastChange: null };

  if (reading.layer !== null && seen.layer !== null && reading.layer !== seen.layer) {
    seen.changes += 1;
    seen.lastChange = { from: seen.layer, to: reading.layer, at: Date.now() };
  }
  if (reading.layer !== null) seen.layer = reading.layer;

  layers.set(key, seen);
  reading.changes = seen.changes;
  reading.lastChange = seen.lastChange;
}

function readLocal(reading, report, history) {
  const rungs = [];
  const live = new Set();

  report.forEach((entry) => {
    if (entry.type !== "outbound-rtp" || entry.kind !== "video") return;

    const key = `local:${entry.id}`;
    live.add(key);
    const current = { bytes: finite(entry.bytesSent), timestamp: finite(entry.timestamp) };
    const previous = history.get(key);
    history.set(key, current);

    const limitation =
      typeof entry.qualityLimitationReason === "string" &&
      entry.qualityLimitationReason !== "none"
        ? entry.qualityLimitationReason
        : null;

    rungs.push({
      rid: typeof entry.rid === "string" ? entry.rid : null,
      source: typeof entry.mediaSourceId === "string" ? entry.mediaSourceId : null,
      label: labelForRid(entry.rid),
      width: finite(entry.frameWidth),
      height: finite(entry.frameHeight),
      fps: finite(entry.framesPerSecond),
      bitrate: bitrate(current, previous, "bytes"),
      limitation,
      active: typeof entry.active === "boolean" ? entry.active : null,
    });
  });

  for (const key of [...history.keys()]) {
    if (typeof key === "string" && key.startsWith("local:") && !live.has(key)) {
      history.delete(key);
    }
  }

  rungs.sort((left, right) => order(left.rid) - order(right.rid));
  reading.local = rungs;
  reading.localTracks = groupLocalTracks(report, rungs);
  reading.localNote = rungs.length === 0 ? NOT_PUBLISHING : null;
}

/**
 * Which outbound stream belongs to which local track. `outbound-rtp` never says
 * so itself; it names a `media-source`, and that is the entry carrying the
 * `trackIdentifier`. Without both there is no honest way to tell a screen share
 * apart from a camera going out beside it, so a rung the browser did not join
 * up is left out rather than added to whichever track was asked about.
 */
function groupLocalTracks(report, rungs) {
  const identifiers = new Map();
  report.forEach((entry) => {
    if (entry.type !== "media-source") return;
    if (typeof entry.trackIdentifier !== "string") return;
    identifiers.set(entry.id, entry.trackIdentifier);
  });

  const tracks = new Map();
  for (const rung of rungs) {
    const identifier = rung.source === null ? undefined : identifiers.get(rung.source);
    if (identifier === undefined) continue;

    const found = tracks.get(identifier) || {
      width: null,
      height: null,
      fps: null,
      bitrate: null,
    };

    // The tallest rung is the one a viewer can be asked about, and its width,
    // height and frame rate are read off that same rung so the three cannot
    // come from three different encodings.
    if (rung.height !== null && rung.width !== null) {
      if (found.height === null || rung.height > found.height) {
        found.width = rung.width;
        found.height = rung.height;
        found.fps = rung.fps;
      }
    }
    if (rung.bitrate !== null) {
      found.bitrate = found.bitrate === null ? rung.bitrate : found.bitrate + rung.bitrate;
    }

    tracks.set(identifier, found);
  }

  return tracks;
}

function order(rid) {
  const index = ["q", "h", "f"].indexOf(rid);
  return index === -1 ? 9 : index;
}

function bitrate(current, previous, field) {
  const bytes = change(current, previous, field);
  if (bytes === null) return null;
  const window = span(current, previous);
  if (window === null) return null;
  return (bytes * 8) / window;
}

function change(current, previous, field) {
  if (!previous) return null;
  const now = current[field];
  const then = previous[field];
  if (now === null || then === null) return null;
  const delta = now - then;
  return delta >= 0 ? delta : null;
}

function span(current, previous) {
  if (!previous || current.timestamp === null || previous.timestamp === null) return null;
  const window = (current.timestamp - previous.timestamp) / 1000;
  return window > 0 ? window : null;
}

function finite(value) {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}
