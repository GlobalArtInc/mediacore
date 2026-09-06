/**
 * What one browser remembers between visits: the noise suppressor, the
 * microphone, the name typed in the lobby, whether the camera box was ticked,
 * and the two processing switches beside them. Preferences, and nothing else.
 *
 * **Nothing the server minted is ever written here.** Not the token, not the
 * identity, not the room it was for. Those are issued per visit and expire;
 * a copy of one in `localStorage` outlives the call it was for, is readable by
 * every script this origin ever runs, and buys the person nothing they do not
 * get from pressing Join again. A device id and a display name are preferences
 * about this browser, which is a different kind of thing.
 *
 * Nothing here touches the DOM, and nothing here reaches for `localStorage` on
 * its own: the store is handed in, so `node --test` can hand it one that
 * throws. That is not an exotic case. A browser set to block site data throws
 * on the *accessor* — reading `window.localStorage` at all — before any method
 * is called; a private window can return null happily from `getItem` and then
 * throw from `setItem` when its small quota fills. A page that white-screens in
 * a private window is worse than one that forgets, so every path through this
 * file ends in a value the page can render.
 *
 * It imports nothing, so the behaviour suite can load it straight off disk
 * where the page's absolute `/module.js` paths do not resolve. What is on offer
 * — which suppressors exist, which processing keys are settable — is therefore
 * handed in rather than imported from `media.js`, which is also what makes the
 * interesting case testable: a stored value that was one of the choices when it
 * was written and is not one any more.
 *
 *     const store = storageOf(globalThis);
 *     const prefs = readPrefs(store, { engines: NOISE_ENGINES, processing: PROCESSING_KEYS });
 *     writePrefs(store, { ...prefs, noise: "browser" });
 */

/**
 * One key, one JSON object, written whole whenever anything in it changes. A
 * scattering of keys written from wherever is a storage surface nobody can read
 * as a whole, and this one is five fields.
 *
 * The version is in the key and not inside the blob, and every read validates
 * field by field. A version number can only say which shape wrote a value; it
 * cannot say whether the value is still one of the choices this page offers,
 * which is the failure that actually happens — a suppressor that was withdrawn,
 * a microphone that was unplugged. Validation answers both, so validation is
 * what runs. The `v1` is there for the one thing it cannot do: a future layout
 * that means something different by the same field names is written under
 * another key and never reads this one at all.
 */
export const PREFS_KEY = "mediacore.front.v1";

/**
 * Every field null, and deliberately not a set of defaults. What the page does
 * when nothing was remembered is the page's own business — the capture module
 * owns the default suppressor, the markup owns the unticked checkbox — and a
 * second copy of those here is how the two drift apart.
 */
export const NOTHING_REMEMBERED = Object.freeze({
  name: null,
  camera: null,
  noise: null,
  microphone: null,
  processing: null,
});

export const MICROPHONE_GONE =
  "The microphone this browser was set to use is not connected any more, so this call is on " +
  "whatever the system calls the default input. Choose another one in the audio menu.";

export const STORAGE_REFUSED =
  "This browser will not let the page remember anything, so these settings last until you " +
  "reload. A private window and a browser set to block site data both do this.";

/** Long enough for any name a person types, short enough that a hostile blob is not a payload. */
const NAME_LIMIT = 200;

const FIELDS = ["name", "camera", "noise", "microphone", "processing"];

/**
 * The store, where this browser has one and will admit to it. Reading the
 * property is itself what throws when site data is blocked, which is why this
 * is a function called inside a try rather than a constant somebody evaluates
 * at import time and takes the whole module down with.
 */
export function storageOf(scope) {
  try {
    const store = (scope || globalThis).localStorage;
    return store && typeof store.getItem === "function" ? store : null;
  } catch {
    return null;
  }
}

/**
 * What this browser remembers, already validated against what is on offer now.
 * Never throws, and never returns a partial object: the page can read every
 * field of the answer without checking anything first.
 */
export function readPrefs(store, offer) {
  let raw = null;
  try {
    raw = store ? store.getItem(PREFS_KEY) : null;
  } catch {
    return NOTHING_REMEMBERED;
  }

  if (typeof raw !== "string" || raw === "") return NOTHING_REMEMBERED;

  try {
    return sanitize(JSON.parse(raw), offer);
  } catch {
    return NOTHING_REMEMBERED;
  }
}

/**
 * Stored values are input, not truth. Anything that is not the shape this
 * version writes — a blob edited by hand, half a write, a field an older page
 * wrote and this one no longer means, a suppressor that has since been taken
 * out of the list — comes back as "nothing was remembered about that". One
 * field at a time, so one unreadable value never costs the other four.
 */
export function sanitize(raw, offer) {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return NOTHING_REMEMBERED;

  const engines = listOf(offer && offer.engines);
  const settable = listOf(offer && offer.processing);

  const name = typeof raw.name === "string" ? raw.name.trim().slice(0, NAME_LIMIT) : "";
  const processing = {};
  if (raw.processing && typeof raw.processing === "object") {
    for (const key of settable) {
      if (typeof raw.processing[key] === "boolean") processing[key] = raw.processing[key];
    }
  }

  return {
    name: name === "" ? null : name,
    camera: typeof raw.camera === "boolean" ? raw.camera : null,
    noise: engines.includes(raw.noise) ? raw.noise : null,
    // The system default is remembered as nothing at all, because that is what
    // it is: not a device, but the absence of a choice of one.
    microphone:
      typeof raw.microphone === "string" && raw.microphone !== "" ? raw.microphone : null,
    processing: Object.keys(processing).length === 0 ? null : processing,
  };
}

/**
 * Writes the five fields and drops the ones nothing is remembered about, so the
 * blob on disk is always the shape this version writes and never accumulates
 * whatever a caller happened to be holding.
 *
 * It answers whether it landed instead of throwing. There is nothing the page
 * can do about a full quota or a browser that refuses, and a setting that will
 * not be remembered is not a reason to interrupt a call.
 */
export function writePrefs(store, prefs) {
  if (!store) return { saved: false, reason: STORAGE_REFUSED };

  const blob = {};
  for (const field of FIELDS) {
    const value = prefs ? prefs[field] : null;
    if (value !== null && value !== undefined) blob[field] = value;
  }

  try {
    store.setItem(PREFS_KEY, JSON.stringify(blob));
    return { saved: true, reason: null };
  } catch {
    return { saved: false, reason: STORAGE_REFUSED };
  }
}

/**
 * A remembered `deviceId` is not a promise. The device can be gone; the same
 * physical microphone can come back under a different id after a replug or a
 * browser restart; and the browser hides ids and labels alike until the
 * microphone has been granted once, so on a first visit the list often cannot
 * answer the question at all.
 *
 * Three answers, and the page needs all three to behave differently:
 *
 *   kept      it is on the list — ask for it
 *   gone      the list can see, and it is not on it — say so, use the default
 *   unproven  the browser has not said yet — ask for it anyway, and be ready
 *             for the open to fail, because `getUserMedia` given an exact
 *             device that is not there fails the whole call for it, and on some
 *             browsers as a `NotFoundError` that reads like having no
 *             microphone at all
 *
 * A list of entries with empty ids is the browser declining to say, not an
 * empty list of devices, and the two must not be read the same way.
 */
export function chooseMicrophone(remembered, devices) {
  if (typeof remembered !== "string" || remembered === "") {
    return { deviceId: null, outcome: "none" };
  }

  const known = (Array.isArray(devices) ? devices : []).filter(
    (device) => device && typeof device.deviceId === "string" && device.deviceId !== "",
  );

  if (known.length === 0) return { deviceId: remembered, outcome: "unproven" };
  if (known.some((device) => device.deviceId === remembered)) {
    return { deviceId: remembered, outcome: "kept" };
  }
  return { deviceId: null, outcome: "gone" };
}

function listOf(value) {
  return Array.isArray(value) ? value.filter((entry) => typeof entry === "string") : [];
}
