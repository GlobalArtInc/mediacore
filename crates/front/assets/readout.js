import {
  FIRST_SAMPLE,
  NOT_REPORTED,
  NO_FRAMES,
  NO_PACKETS,
  NOT_PUBLISHING,
} from "/bench.js";

const MISSING = "—";
const FLASH_MS = 2500;

const VIDEO_ROWS = [
  { key: "bitrate", label: "bitrate" },
  { key: "loss", label: "loss" },
  { key: "fps", label: "fps" },
  { key: "jitter", label: "jitter" },
  { key: "freezes", label: "freezes" },
  { key: "stream", label: "stream" },
];

export function formatBitrate(bps) {
  if (bps === null || bps === undefined) return null;
  if (bps >= 1_000_000) return `${(bps / 1_000_000).toFixed(2)} Mb/s`;
  return `${Math.round(bps / 1000)} kb/s`;
}

export function formatPercent(fraction) {
  if (fraction === null || fraction === undefined) return null;
  const percent = fraction * 100;
  return `${percent < 1 ? percent.toFixed(2) : percent.toFixed(1)} %`;
}

export function formatMillis(millis) {
  if (millis === null || millis === undefined) return null;
  if (millis < 1) return `${millis.toFixed(2)} ms`;
  return millis < 10 ? `${millis.toFixed(1)} ms` : `${Math.round(millis)} ms`;
}

export function formatDecibels(db) {
  if (db === null || db === undefined) return null;
  return `${db.toFixed(1)} dBFS`;
}

export function formatChange(db) {
  if (db === null || db === undefined) return null;
  return `${db > 0 ? "+" : ""}${db.toFixed(1)} dB`;
}

export function formatCount(value) {
  if (value === null || value === undefined) return null;
  return String(Math.round(value));
}

export function write(node, text, reason) {
  if (!node) return;
  if (text === null || text === undefined) {
    node.textContent = MISSING;
    node.title = reason || NOT_REPORTED;
    node.dataset.missing = "true";
    return;
  }
  node.textContent = text;
  node.removeAttribute("title");
  node.dataset.missing = "false";
}

export function describeAudio(reading) {
  if (!reading) return { text: null, reason: "nothing is arriving for this participant" };
  if (reading.note) return { text: null, reason: reading.note };

  const bitrate = formatBitrate(reading.bitrate);
  if (bitrate === null) return { text: null, reason: FIRST_SAMPLE };

  const loss = formatPercent(reading.loss);
  return {
    text: loss === null ? `${bitrate} · loss unknown` : `${bitrate} · loss ${loss}`,
    reason: null,
  };
}

export function createTiles(root) {
  const tiles = new Map();

  function sync(wanted) {
    const keys = new Set(wanted.map((entry) => entry.key));

    for (const [key, tile] of [...tiles]) {
      if (keys.has(key)) continue;
      if (tile.track) tile.track.detach().forEach((element) => element.remove());
      tile.node.remove();
      tiles.delete(key);
    }

    for (const entry of wanted) {
      let tile = tiles.get(entry.key);
      if (!tile) {
        tile = build(entry);
        tiles.set(entry.key, tile);
        root.appendChild(tile.node);
      }
      tile.who.textContent = entry.label;
      attach(tile, entry.track);
    }

    root.hidden = wanted.length === 0;
  }

  function paint(reading) {
    for (const [id, tile] of tiles) {
      if (tile.local) {
        paintLocal(tile, reading);
      } else {
        paintRemote(tile, reading.tracks.get(id));
      }
    }
  }

  function clear() {
    sync([]);
  }

  return { sync, paint, clear };
}

function build(entry) {
  const node = document.createElement("li");
  node.className = "tile";
  node.dataset.key = entry.key;

  const frame = document.createElement("div");
  frame.className = "frame";

  const head = document.createElement("div");
  head.className = "tile-head";

  const who = document.createElement("span");
  who.className = "who";

  const badge = document.createElement("span");
  badge.className = "badge";
  badge.textContent = MISSING;

  head.append(who, badge);

  const tile = {
    node,
    frame,
    who,
    badge,
    local: Boolean(entry.local),
    track: null,
    values: new Map(),
    rungs: null,
    trail: null,
  };

  node.append(frame, head);

  if (tile.local) {
    const rungs = document.createElement("ul");
    rungs.className = "rungs";
    node.appendChild(rungs);
    tile.rungs = rungs;
  } else {
    const readout = document.createElement("dl");
    readout.className = "readout";
    for (const row of VIDEO_ROWS) {
      const cell = document.createElement("div");
      const term = document.createElement("dt");
      term.textContent = row.label;
      const value = document.createElement("dd");
      value.textContent = MISSING;
      cell.append(term, value);
      readout.appendChild(cell);
      tile.values.set(row.key, value);
    }
    const trail = document.createElement("p");
    trail.className = "trail";
    trail.textContent = "no layer change seen yet";
    node.append(readout, trail);
    tile.trail = trail;
  }

  return tile;
}

function attach(tile, track) {
  if (tile.track === track) return;

  if (tile.track) tile.track.detach().forEach((element) => element.remove());
  tile.track = track || null;
  if (!tile.track) return;

  const element = tile.track.attach();
  element.autoplay = true;
  element.playsInline = true;
  element.muted = true;
  tile.frame.replaceChildren(element);
}

function paintRemote(tile, reading) {
  if (!reading) {
    write(tile.badge, null, "this track is not subscribed");
    for (const value of tile.values.values()) {
      write(value, null, "this track is not subscribed");
    }
    return;
  }

  const missing = reading.note;
  if (reading.layer !== null && reading.width !== null && reading.height !== null) {
    write(tile.badge, `${reading.layer} · ${reading.width}×${reading.height}`);
  } else {
    write(tile.badge, null, missing || NO_FRAMES);
  }

  write(tile.values.get("bitrate"), formatBitrate(reading.bitrate), missing || FIRST_SAMPLE);
  write(
    tile.values.get("loss"),
    formatPercent(reading.loss),
    missing || (reading.packets === null ? FIRST_SAMPLE : NO_PACKETS),
  );
  write(tile.values.get("fps"), formatCount(reading.fps), missing || NOT_REPORTED);
  write(tile.values.get("jitter"), formatMillis(reading.jitter), missing || NOT_REPORTED);
  write(tile.values.get("freezes"), formatCount(reading.freezes), missing || NOT_REPORTED);
  write(tile.values.get("stream"), reading.streamState, missing || NOT_REPORTED);

  tile.node.dataset.paused = String(reading.streamState === "paused");
  paintTrail(tile, reading);
}

function paintTrail(tile, reading) {
  if (!reading.lastChange) {
    tile.trail.textContent =
      reading.changes === 0 ? "no layer change seen yet" : `${reading.changes} layer changes`;
    tile.node.dataset.changed = "false";
    return;
  }

  const age = Date.now() - reading.lastChange.at;
  const plural = reading.changes === 1 ? "change" : "changes";
  tile.trail.textContent =
    `${reading.changes} layer ${plural} · ` +
    `${reading.lastChange.from} → ${reading.lastChange.to} ${Math.round(age / 1000)} s ago`;
  tile.node.dataset.changed = String(age < FLASH_MS);
}

function paintLocal(tile, reading) {
  const rungs = reading.local;

  if (!rungs || rungs.length === 0) {
    write(tile.badge, null, reading.localNote || NOT_PUBLISHING);
    tile.rungs.replaceChildren(note(reading.localNote || NOT_PUBLISHING));
    return;
  }

  write(tile.badge, `${rungs.length} layers`);
  tile.rungs.replaceChildren(...rungs.map(rung));
}

function rung(entry) {
  const row = document.createElement("li");

  const name = document.createElement("span");
  name.className = "rung-name";
  name.textContent = entry.label || entry.rid || "single";

  const size = document.createElement("span");
  size.className = "rung-size";
  write(
    size,
    entry.width !== null && entry.height !== null ? `${entry.width}×${entry.height}` : null,
    NO_FRAMES,
  );

  const rate = document.createElement("span");
  rate.className = "rung-rate";
  write(rate, formatBitrate(entry.bitrate), FIRST_SAMPLE);

  row.append(name, size, rate);

  if (entry.limitation) {
    const limit = document.createElement("span");
    limit.className = "rung-limit";
    limit.textContent = `limited by ${entry.limitation}`;
    row.appendChild(limit);
  }

  row.dataset.idle = String(entry.active === false);
  return row;
}

function note(text) {
  const row = document.createElement("li");
  row.className = "rung-note";
  row.textContent = text;
  return row;
}
