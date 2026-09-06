/**
 * The stage's own decisions, kept out of the DOM so `node --test` can argue
 * with them: which share the stage is holding, whether this browser will hand
 * the page fullscreen at all, and what may honestly be printed in the corner of
 * a shared screen.
 *
 * Nothing here imports anything, on purpose: the behaviour suite loads this
 * file straight off disk, where the page's absolute `/module.js` paths do not
 * resolve.
 */

export const NO_FULLSCREEN_API =
  "this browser offers no fullscreen to anything but a video element";
export const FULLSCREEN_NOT_ALLOWED = "this document is not allowed to go fullscreen";
export const REFUSED_WITHOUT_REASON = "this browser refused without saying why";
export const FULLSCREEN_IGNORED =
  "this browser took the request and did not go fullscreen";

export const NO_FRAME_SIZE = "this browser has not reported a frame size for this track";
export const NO_BITRATE = "this browser has not reported a bitrate for this track yet";

/**
 * iOS Safari is the ordinary case rather than the exotic one: it has no
 * `requestFullscreen` on an element, only on a video, and taking the video
 * fullscreen would hand the screen to the system player and take the call
 * controls away with it. So a browser that will not give the page fullscreen is
 * told to fill the window instead, and the reason travels with the answer.
 */
export function fullscreenPlan(api) {
  const support = api || {};
  if (!support.request) return { mode: "fallback", reason: NO_FULLSCREEN_API };
  if (support.enabled === false) {
    return { mode: "fallback", reason: FULLSCREEN_NOT_ALLOWED };
  }
  return { mode: "native", reason: null };
}

/**
 * An embedded web view, and a window the platform will not let go fullscreen,
 * answer `requestFullscreen()` by resolving and doing nothing at all. Believing
 * the promise there leaves the page in a state whose Escape key belongs to a
 * browser that never took the screen, so what the document says afterwards is
 * what counts.
 */
export function afterRequest(taken) {
  if (taken) return { mode: "native", reason: null };
  return { mode: "fallback", reason: FULLSCREEN_IGNORED };
}

export function afterRefusal(error) {
  const detail = (error && (error.message || error.name)) || "";
  return { mode: "fallback", reason: detail === "" ? REFUSED_WITHOUT_REASON : detail };
}

/**
 * How long a stage holds a share that has stopped arriving while the link is
 * up. A publisher moving from a window to a whole screen stops one capture and
 * starts another, and the gap in between is theirs; tearing the viewer out of
 * fullscreen for it is the very complaint this view was written against.
 */
export const SHARE_GRACE_MS = 4000;

/**
 * What the stage shows next, given what it was already holding. A share that is
 * still being published stays on the stage whatever else the room is doing --
 * who is speaking is not an input here, and that is the whole point of somebody
 * sharing.
 *
 * An empty list is not by itself the news that a share ended. A room that is
 * reconnecting has dropped every remote participant and told us nothing about
 * any of them, and a publisher switching surface mints a new track. So the
 * stage holds what it had -- without end while the link is down, and for
 * `SHARE_GRACE_MS` while it is up -- and only then says the share is over.
 */
export function nextShare(pinned, shares, world) {
  const list = Array.isArray(shares) ? shares : [];
  const held = pinned && pinned.key ? pinned : null;
  const link = (world && world.link) || "connected";
  const now = world && typeof world.now === "number" ? world.now : 0;

  if (held) {
    const kept = list.find((entry) => entry.key === held.key);
    if (kept) return holding(kept, "kept");

    // The same person, a different track: they changed what they are sharing,
    // which is one share continuing and not one ending and another beginning.
    const again = held.identity
      ? list.find((entry) => entry.identity && entry.identity === held.identity)
      : undefined;
    if (again) return holding(again, "resumed");
  }

  if (list.length === 0) {
    if (!held) return over("none", null);
    if (link === "gone") return over("ended", null);
    if (link !== "connected") return waitingOn(held, null, null);

    const since = typeof held.since === "number" ? held.since : now;
    const left = SHARE_GRACE_MS - (now - since);
    if (left > 0) return waitingOn(held, since, left);

    return over("none", held.label || held.key);
  }

  const first = list[0];
  return { ...holding(first, held ? "switched" : "first"), gone: held ? held.label || held.key : null };
}

function holding(entry, outcome) {
  return {
    pinned: {
      key: entry.key,
      identity: entry.identity || null,
      label: entry.owner || entry.label || entry.key,
      since: null,
    },
    key: entry.key,
    entry,
    outcome,
    gone: null,
    waitFor: null,
  };
}

function waitingOn(held, since, waitFor) {
  return {
    pinned: { ...held, since: typeof since === "number" ? since : null },
    key: held.key,
    entry: null,
    outcome: "waiting",
    gone: null,
    waitFor,
  };
}

function over(outcome, gone) {
  return { pinned: null, key: null, entry: null, outcome, gone, waitFor: null };
}

export function resolutionOf(reading) {
  if (!reading) return null;
  const width = positive(reading.width);
  const height = positive(reading.height);
  if (width === null || height === null) return null;
  return `${width}×${height}`;
}

/**
 * The corner badge. Each half is printed only if the browser reported it, and
 * when it reported neither there is no badge at all: an empty rectangle with a
 * dash in it answers "I cannot see it" no better than the dash did.
 */
export function shareBadge(reading, formatBitrate) {
  const resolution = resolutionOf(reading);
  const rate =
    reading && typeof reading.bitrate === "number" && Number.isFinite(reading.bitrate)
      ? formatBitrate(reading.bitrate)
      : null;

  const parts = [];
  if (resolution !== null) parts.push(resolution);
  if (rate !== null) parts.push(rate);

  let note = null;
  if (resolution !== null && rate === null) note = NO_BITRATE;
  if (resolution === null && rate !== null) note = NO_FRAME_SIZE;

  return {
    resolution,
    bitrate: rate,
    note,
    text: parts.length === 0 ? null : parts.join(" · "),
  };
}

/**
 * The size the browser reports for the share on the stage moves for reasons
 * that are the publisher's -- a window swapped for a whole screen -- and for
 * reasons that are nobody's, the encoder scaling itself down under congestion
 * among them. This cannot tell those apart, so it does not claim to: it says
 * what the size is now and what it was, and only when the change is large
 * enough to be worth a line at all.
 */
export const SIZE_STEP = 0.2;

export function sizeChange(previous, current) {
  if (!previous || !current) return null;
  if (previous.key === null || previous.key !== current.key) return null;
  if (!previous.resolution || !current.resolution) return null;
  if (previous.resolution === current.resolution) return null;
  if (!worthSaying(previous.area, current.area)) return null;

  return {
    from: previous.resolution,
    to: current.resolution,
    text: `The shared screen is now ${current.resolution} (it was ${previous.resolution}).`,
  };
}

export function frameArea(reading) {
  const width = positive(reading && reading.width);
  const height = positive(reading && reading.height);
  if (width === null || height === null) return null;
  return width * height;
}

function worthSaying(before, after) {
  if (typeof before !== "number" || typeof after !== "number") return true;
  if (before <= 0 || after <= 0) return true;
  return Math.min(before, after) / Math.max(before, after) <= 1 - SIZE_STEP;
}

function positive(value) {
  if (typeof value !== "number" || !Number.isFinite(value)) return null;
  return value > 0 ? value : null;
}
