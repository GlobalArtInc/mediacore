import {
  FIRST_SAMPLE,
  NOT_REPORTED,
  NO_SUBSCRIBER,
  createBench,
} from "/bench.js";
import {
  createTiles,
  describeAudio,
  formatBitrate,
  formatChange,
  formatDecibels,
  formatMillis,
  formatPercent,
  write,
} from "/readout.js";
import {
  DEFAULT_NOISE_ENGINE,
  DIRECT_PROCESSING_KEYS,
  NOISE_ENGINES,
  createMedia,
} from "/media.js";
import {
  MICROPHONE_GONE,
  NOTHING_REMEMBERED,
  chooseMicrophone,
  readPrefs,
  storageOf,
  writePrefs,
} from "/prefs.js";
import {
  addressedRoom,
  composedRoom,
  offeredRoom,
  plainRoom,
  roomPath,
} from "/room.js";
import { createDenoiser } from "/denoise.js";
import { LEVEL_METER_PROCESSOR, REPORT_EVERY_MS } from "/level-meter.js";
import { createVoiceGate } from "/voice.js";
import {
  afterRefusal,
  afterRequest,
  frameArea,
  fullscreenPlan,
  nextFocus,
  nextShare,
  shareBadge,
  sizeChange,
  stagePlan,
} from "/stage.js";
import {
  KEEP_LINES,
  NOT_ALLOWED,
  afterArrival,
  canSend,
  clockOf,
  lineFrom,
  unreadLabel,
} from "/chat.js";
import {
  claimsOf,
  forgetPass,
  passFrom,
  passProblem,
  recallPass,
  rememberPass,
  whoFrom,
} from "/pass.js";

const SDK_URL = "/vendor/livekit-client-2.22.2.esm.js";

// Served from this binary, with the build's version in the path, so upgrading
// it can never hand a browser a stale cached copy of the old one.
const RNNOISE_WASM_URL = "/vendor/rnnoise-0.4.0.wasm";
const RNNOISE_SIMD_WASM_URL = "/vendor/rnnoise-simd-0.4.0.wasm";
const RNNOISE_WORKLET_URL = "/vendor/rnnoise-worklet-0.4.0.js";
const DENOISE_GATE_URL = "/denoise-gate.js";
const DENOISE_PROBE_URL = "/denoise-probe.js";
const LEVEL_METER_URL = "/level-meter.js";

/**
 * How often the page checks whether a meter has stopped reporting. Nothing here
 * decides who is speaking -- `voice.js` did that when the last report arrived,
 * and every repaint asks it again -- this only exists so that a meter which
 * went quiet is *repainted* as unknown rather than sitting on its last answer
 * until something else happens to cause a render.
 */
const METER_SWEEP_MS = 500;
const BENCH_INTERVAL_MS = 1000;
const NOTICE_MS = 12000;
const TOGGLE_GUARD_MS = 350;
const PRESETS = { 180: "h180", 360: "h360", 720: "h720" };
const SYSTEM_DEFAULT = "";

const PROCESSING_ROWS = [
  {
    key: "echoCancellation",
    label: "Echo cancellation",
    note:
      "Subtracts what the speakers are playing from what the microphone hears. It needs the " +
      "browser's own reference of the output in the capture clock, which nothing on this page " +
      "can supply, so it stays the browser's job whichever suppressor is running.",
  },
  {
    key: "autoGainControl",
    label: "Automatic gain",
    note: "Rides the level towards a target loudness between phrases.",
  },
];

// One control, three values. Two suppressors in series sound worse than either
// alone, so the browser's noiseSuppression and voiceIsolation are not offered
// beside ours: they are what "the browser's own" means, and they are both off
// whenever ours is running.
/**
 * The two a person may choose between. The browser's own suppressor is not
 * here on purpose: it is no longer an option, it is the floor this page lands
 * on when ours cannot run -- the probe says no, the worklet will not attach, or
 * it overruns the render thread. Leaving it in the menu invited somebody to
 * pick the weaker of the two by hand; taking it out of the rollback would leave
 * a machine that cannot run ours with nothing at all, which is worse than
 * either. `effective` under the choices says which one is really in the path,
 * so a fallback is never silent.
 */
const NOISE_ROWS = [
  {
    engine: "off",
    label: "Off",
    short: "off",
    note: "Nothing attenuates the room. Worth a listen before judging either of the other two.",
  },
  {
    engine: "mediacore",
    label: "mediacore (RNNoise)",
    short: "mediacore",
    note:
      "RNNoise, running in this page as WebAssembly between the microphone and the encoder, and " +
      "what a call starts on unless you choose otherwise. Steady noise -- fans, hiss, traffic -- " +
      "comes out further down than the browser manages. It does not remove other people's " +
      "voices, and it treats music as noise.",
  },
];

const DENOISE_ROWS = [
  { key: "engine", label: "engine" },
  { key: "levelIn", label: "level in" },
  { key: "levelOut", label: "level out" },
  { key: "change", label: "change" },
  { key: "delay", label: "added delay" },
  { key: "frame", label: "cost per 10 ms" },
  { key: "load", label: "real time used" },
  { key: "realtime", label: "keeping up" },
  { key: "worst", label: "worst block" },
  { key: "fallback", label: "fallback" },
];

const DENOISE_OFF =
  "our denoiser is not in the capture path, so there is nothing here to measure";

const NO_AUDIO_CONTEXT =
  "this browser gave the page no audio context, so nothing here can tell whether they are speaking";
const NO_LEVEL_METER =
  "this browser will not run the level meter on its audio thread, and a meter on the main thread " +
  "is one the browser throttles away in a background tab, so nothing here can tell whether they " +
  "are speaking";
const METER_STOPPED =
  "the level meter on this participant's audio stopped on the audio thread, so nothing here can " +
  "tell whether they are speaking";
const METER_ATTACHING = "a level meter is being put on this audio, and has not measured it yet";
const METER_NOT_YET =
  "no level meter has been put on this audio yet, so whether they are speaking is not something " +
  "this page can say";
const NOT_SUBSCRIBED =
  "the server has not sent this track to this browser, so nothing here can tell whether they are speaking";
const NOT_PUBLISHED =
  "the microphone is open in this browser but the server has not accepted a publication for it";

const el = {
  lobby: document.getElementById("lobby"),
  lobbyRoom: document.getElementById("lobby-room"),
  lobbyStatus: document.getElementById("lobby-status"),
  lobbyError: document.getElementById("lobby-error"),
  form: document.getElementById("join-form"),
  name: document.getElementById("name"),
  join: document.getElementById("join"),
  withVideo: document.getElementById("with-video"),
  room: document.getElementById("room"),
  roomName: document.getElementById("room-name"),
  roomCount: document.getElementById("room-count"),
  roomState: document.getElementById("room-state"),
  roomError: document.getElementById("room-error"),
  roster: document.getElementById("roster"),
  peopleToggle: document.getElementById("people-toggle"),
  peopleCount: document.getElementById("people-count"),
  participants: document.getElementById("participants"),
  rosterSheet: document.getElementById("roster-sheet"),
  peopleTab: document.getElementById("people-tab"),
  peopleUnread: document.getElementById("people-unread"),
  chatTab: document.getElementById("chat-tab"),
  chatUnread: document.getElementById("chat-unread"),
  chat: document.getElementById("chat"),
  chatLog: document.getElementById("chat-log"),
  chatEmpty: document.getElementById("chat-empty"),
  chatForm: document.getElementById("chat-form"),
  chatText: document.getElementById("chat-text"),
  chatSend: document.getElementById("chat-send"),
  chatNote: document.getElementById("chat-note"),
  stage: document.getElementById("stage"),
  spotlight: document.getElementById("spotlight"),
  cards: document.getElementById("cards"),
  share: document.getElementById("share"),
  shareFrame: document.getElementById("share-frame"),
  shareStats: document.getElementById("share-stats"),
  shareFocus: document.getElementById("share-focus"),
  shareFocusLabel: document.getElementById("share-focus-label"),
  shareFullscreen: document.getElementById("share-fullscreen"),
  shareFullscreenLabel: document.getElementById("share-fullscreen-label"),
  shareWho: document.getElementById("share-who"),
  shareLive: document.getElementById("share-live"),
  shareState: document.getElementById("share-state"),
  shareSpeaking: document.getElementById("share-speaking"),
  shareTabs: document.getElementById("share-tabs"),
  bench: document.getElementById("bench"),
  tiles: document.getElementById("tiles"),
  estimate: document.getElementById("link-estimate"),
  arrivingRate: document.getElementById("link-arriving"),
  videoRate: document.getElementById("link-video"),
  rtt: document.getElementById("link-rtt"),
  sending: document.getElementById("link-sending"),
  notice: document.getElementById("notice"),
  noticeText: document.getElementById("notice-text"),
  noticeDismiss: document.getElementById("notice-dismiss"),
  enableAudio: document.getElementById("enable-audio"),
  audioMenu: document.getElementById("audio-menu"),
  audioSettings: document.getElementById("audio-settings"),
  audioClose: document.getElementById("audio-close"),
  micSelect: document.getElementById("mic-select"),
  micNote: document.getElementById("mic-note"),
  processing: document.getElementById("processing"),
  noiseChoices: document.getElementById("noise-choices"),
  noiseNote: document.getElementById("noise-note"),
  noiseEffective: document.getElementById("noise-effective"),
  denoise: document.getElementById("denoise"),
  mute: document.getElementById("mute"),
  muteLabel: document.getElementById("mute-label"),
  deafen: document.getElementById("deafen"),
  deafenLabel: document.getElementById("deafen-label"),
  noise: document.getElementById("noise"),
  noiseSub: document.getElementById("noise-sub"),
  shareScreen: document.getElementById("share-screen"),
  shareLabel: document.getElementById("share-label"),
  camera: document.getElementById("camera"),
  cameraLabel: document.getElementById("camera-label"),
  benchToggle: document.getElementById("bench-toggle"),
  bar: document.getElementById("bar"),
  barMore: document.getElementById("bar-more"),
  leave: document.getElementById("leave"),
  audio: document.getElementById("audio"),
  footUrl: document.getElementById("foot-url"),
  joinRoom: document.getElementById("join-room"),
  nameField: document.getElementById("name-field"),
  roomField: document.getElementById("room-field"),
  passNote: document.getElementById("pass-note"),
  passRoom: document.getElementById("pass-room"),
  passWho: document.getElementById("pass-who"),
  signedIn: document.getElementById("signed-in"),
  signedInName: document.getElementById("signed-in-name"),
};

const query = new URLSearchParams(location.search);
const wantsSimulcast = query.get("simulcast") !== "0";

/**
 * Which room this tab was opened at. `/r/standup` is the room's own address --
 * it is what a reload comes back to and what somebody pastes to a colleague --
 * and a `?room=` from before rooms had one still names a room, so it is
 * honoured and then quietly corrected.
 */
const arrival = addressedRoom(location.pathname, location.search);
/**
 * A pass minted by whatever sent this person here, taken out of the address and
 * removed from it in the same breath.
 *
 * Wiped before anything else runs, because from here on it is a credential
 * this tab holds rather than one the address carries: out of the address bar,
 * out of a screenshot of it, out of the history entry, and out of what a
 * bookmark would keep.
 */
const addressed = passFrom(location.hash);
if (addressed) {
  history.replaceState(null, "", location.pathname + location.search);
  rememberPass(sessionStorage, addressed);
}
const pass = addressed ?? recallPass(sessionStorage);
const passClaims = pass ? claimsOf(pass) : null;

/**
 * What this browser is allowed to remember, and where. The offer travels with
 * the read because a stored value is only worth restoring while it is still one
 * of the choices this page makes -- see `prefs.js`.
 */
const store = storageOf(globalThis);
const ON_OFFER = { engines: NOISE_ENGINES, processing: DIRECT_PROCESSING_KEYS };

let prefs = NOTHING_REMEMBERED;
let storageComplained = false;

/**
 * Something the page worked out before anybody joined. The notice slot is
 * inside the room, so a sentence shown at load is painted into a panel nobody
 * is looking at and has timed out by the time they are.
 */
let arrivalNotice = null;

/**
 * Whether the lobby is showing a refusal of the room the link named. It is
 * about the link, so it is cleared the moment somebody starts correcting the
 * field rather than at the next press of Join -- a page still refusing a name
 * nobody is asking for any more is a page arguing with itself.
 */
let linkRefused = false;

let lk = null;
let media = null;
let settings = {
  room: "demo",
  wsUrl: "",
  maxNameLength: 40,
  maxRoomLength: 64,
  roomNamespaceSeparator: ".",
  roomError: null,
  signIn: false,
  signedInAs: null,
};
let room = null;
let cameraTrack = null;

/**
 * One entry per participant whose microphone this page is measuring: the graph
 * that feeds the meter, and the gate in `voice.js` that turns its reports into
 * an answer. `state` is only the last answer that was *painted*, and it exists
 * so the sweep knows whether a repaint is owed -- the answer itself is always
 * asked for fresh.
 */
const meters = new Map();

/**
 * Meters being built. Installing the worklet module is asynchronous, and a
 * device change can replace a track twice before the first attach finishes, so
 * each attempt carries a token and a finished attempt that is no longer the
 * current one throws away what it built.
 */
const attaching = new Map();
let attachCount = 0;

/**
 * Why this page can put no meter on anything, once it knows -- which is a claim
 * about the *browser*, so it is only ever set by the one thing that is true of
 * the browser rather than of a track: a worklet module that will not install.
 * One dead worklet on one participant used to set this for everybody and never
 * clear it, so every meterless row for the rest of the session was told the
 * browser would not run a level meter, which was not true of any of them.
 */
let meterFault = null;
let meterModule = null;

/**
 * Why one participant has no meter, when the reason is theirs alone. Cleared
 * when a meter is attached to them again, because a device change is exactly
 * how a dead worklet gets replaced by a live one.
 */
const meterFailures = new Map();
const arriving = new Map();
const cards = new Map();
const switches = new Map();
const noiseChoices = new Map();
const denoiseCells = new Map();
const pending = { mute: false, share: false, camera: false, noise: false };
let meterSweep = null;
let audioContext = null;
let bench = null;
let benchTimer = null;
let lastReading = null;
let tiles = null;
let showBench = false;
/**
 * How much of what was said has not been read, and which of the column's two
 * panels is showing. There is no message array beside them: the panel's own
 * markup is the state, which is what keeps a room event from rebuilding the
 * log and taking the scroll position and the live region's announcements with
 * it.
 */
let unread = 0;
let panel = "people";
/**
 * Whether this browser is refusing to hear the room, and what the microphone
 * was doing before it started refusing.
 *
 * The pair is one state rather than two flags: going deaf takes the microphone
 * with it, because talking into a room you cannot hear is the position nobody
 * means to be in, and coming back has to put the microphone where it was found
 * rather than where it would be convenient. Somebody who was already muted
 * stays muted.
 */
let deafened = false;
let mutedBeforeDeafened = false;
/**
 * Why this browser has no microphone, in the room it is nonetheless in.
 *
 * It is kept rather than said once and forgotten. The sentence names the
 * padlock, and the person reading it is usually the person who has just come
 * back from the padlock wanting to know what to press now -- by which time a
 * notice that timed out is of no use to anybody. It hangs on the one control
 * the answer belongs to, and it goes the moment a microphone opens.
 */
let micRefusal = null;
let pinnedShare = null;
let stageShare = null;
let stageFocus = null;
let shareSignature = "";
let deviceSignature = "";
let sharedElement = null;
let sharedTrack = null;
let noticeTimer = null;
let fullscreen = "off";
let shareSize = { key: null, resolution: null, area: null };
let shareSeen = null;
let shareTimer = null;
let fullscreenToggledAt = 0;

start();

async function start() {
  tiles = createTiles(el.tiles);
  prefs = readPrefs(store, ON_OFFER);

  // A link that names the camera is an instruction for this visit and outranks
  // what the box was left at last time; `?video=0` is such an instruction too.
  el.withVideo.checked = query.has("video")
    ? query.get("video") === "1"
    : prefs.camera === true;
  showBench = query.get("bench") === "1";

  // A link that named its room in the query still names it. The address bar is
  // put right on arrival rather than on joining, because it is wrong now: this
  // load is already about that room, and a reload of the corrected URL reaches
  // the same one.
  if (arrival.canonical) history.replaceState(null, "", arrival.canonical);

  el.form.addEventListener("submit", onJoin);
  el.joinRoom.addEventListener("input", onRoomTyped);
  el.mute.addEventListener("click", onMute);
  el.deafen.addEventListener("click", onDeafen);
  el.noise.addEventListener("click", onCycleNoise);
  el.audioSettings.addEventListener("click", () => toggleMenu());
  el.audioClose.addEventListener("click", () => {
    toggleMenu(false);
    el.audioSettings.focus();
  });
  el.micSelect.addEventListener("change", onSelectMicrophone);
  el.shareScreen.addEventListener("click", onShareScreen);
  el.shareFocus.addEventListener("click", () => onFocus({ kind: "share", key: null }));
  el.shareFullscreen.addEventListener("click", onToggleFullscreen);
  el.shareFrame.addEventListener("dblclick", onToggleFullscreen);
  document.addEventListener("fullscreenchange", onFullscreenChange);
  el.camera.addEventListener("click", onCamera);
  el.benchToggle.addEventListener("click", onBenchToggle);
  el.barMore.addEventListener("click", () => toggleTray());
  el.peopleToggle.addEventListener("click", () => togglePeople());
  el.peopleTab.addEventListener("click", () => showPanel("people"));
  el.chatTab.addEventListener("click", () => showPanel("chat"));
  el.chatForm.addEventListener("submit", onChatSubmit);
  el.leave.addEventListener("click", () => leave("You left the room."));
  el.enableAudio.addEventListener("click", onEnableAudio);
  el.noticeDismiss.addEventListener("click", () => clearNotice());
  document.addEventListener("keydown", onKeydown);
  document.addEventListener("pointerdown", onPointerDown);
  document.addEventListener("visibilitychange", onVisibilityChange);
  window.addEventListener("pagehide", () => {
    if (room) room.disconnect();
  });

  buildSwitches();
  buildNoiseChoices();
  paintFocusControl();
  paintFullscreenControl();
  buildDenoiseReadout();
  paintBenchToggle();
  paintControls();
  paintChat();

  settings = await loadSettings(arrival.room);
  el.name.maxLength = settings.maxNameLength;
  el.joinRoom.maxLength = settings.maxRoomLength;
  // `maxLength` only binds what a person types, so a longer name out of storage
  // would sit in the field and be refused by the server on submit.
  if (prefs.name && !settings.signIn) {
    el.name.value = prefs.name.slice(0, settings.maxNameLength);
  }
  paintSignIn();
  paintPass();
  offerRoom();
  el.footUrl.textContent = settings.wsUrl;

  if (!window.isSecureContext || !navigator.mediaDevices) {
    block(
      "This page is not a secure context, so the browser will not hand over the microphone. " +
        "Open the page over https, or over http://localhost while developing.",
    );
    return;
  }

  try {
    lk = await import(SDK_URL);
  } catch (error) {
    console.error("the livekit-client module failed to load", error);
    block(
      "The page could not load the client library it needs. Reload the page; if it keeps " +
        "happening this server is serving a broken build.",
    );
    return;
  }

  media = createMedia(lk, {
    // livekit-client will not take a processor on a track with no audio
    // context, and a track opened before the room exists has none. The page
    // already keeps one for the level meters; lending it that one is what keeps
    // the denoiser alive across a leave and a second join.
    audioContext: ensureAudioContext,
    denoiser: createDenoiser({
      wasmUrl: RNNOISE_WASM_URL,
      simdWasmUrl: RNNOISE_SIMD_WASM_URL,
      workletUrl: RNNOISE_WORKLET_URL,
      gateUrl: DENOISE_GATE_URL,
      probeUrl: DENOISE_PROBE_URL,
    }),
  });
  media.on("change", onMediaChange);
  media.on("microphonetrack", onMicrophoneTrack);
  media.on("screensharestopped", onScreenShareStopped);
  media.on("notice", (entry) => showNotice(entry.text));
  media.on("denoise", paintDenoise);

  paintControls();
  ready();

  // Asked once, before anyone is offered the choice: a machine that cannot run
  // the denoiser is never shown a switch that would silently do nothing. What
  // this browser remembered is applied on the far side of that answer, so a
  // remembered engine meets the same probe a chosen one does -- and there is
  // one probe rather than two racing each other for the same wasm.
  media.prepareDenoiser().then(restoreChoices).then(paintControls);

  joinWithPass();
}

/**
 * The choices this browser made last time, put back through the same calls the
 * controls make. A remembered suppressor is not treated as truer than a chosen
 * one: it meets the same probe, the same attach and the same fallbacks, so a
 * machine that cannot run ours lands on the browser's own here exactly as it
 * would have if somebody had pressed the row by hand.
 *
 * Nothing here is awaited by the lobby. The form is usable while it runs, and
 * everything it touches is a capture setting that is applied at the moment the
 * microphone opens.
 */
async function restoreChoices() {
  if (!media) return;

  if (prefs.noise) await media.setNoiseEngine(prefs.noise);
  if (prefs.processing) await media.setProcessing(prefs.processing);
  await restoreMicrophone();

  paintDevices();
}

/**
 * A remembered device id is only asked for when the browser will say whether it
 * is still there. Until the microphone has been granted once every entry comes
 * back with a blank id and the list cannot answer at all, so the id is asked
 * for anyway and the capture module catches it if the device is gone: it drops
 * the device from the request, opens the default and says so.
 */
async function restoreMicrophone() {
  const devices = await media.listMicrophones();
  const choice = chooseMicrophone(prefs.microphone, devices.list);

  if (choice.outcome === "gone") {
    arrivalNotice = MICROPHONE_GONE;
    return;
  }
  if (choice.deviceId) await media.selectMicrophone(choice.deviceId);
}

/**
 * Keep the whole blob, and say once per load when this browser will not have
 * it. Silence there would leave a page promising to remember something it is
 * quietly dropping every time.
 */
function remember(patch) {
  prefs = { ...prefs, ...patch };
  const written = writePrefs(store, prefs);
  if (written.saved || storageComplained) return;
  storageComplained = true;
  showNotice(written.reason);
}

function ready() {
  el.join.disabled = false;
  el.name.disabled = false;
  el.joinRoom.disabled = false;
  el.withVideo.disabled = false;
  el.lobbyStatus.textContent = "";
  firstQuestion().focus();
}

/**
 * Which box the cursor belongs in. With signing in switched on there is only
 * one question left in the lobby, and it is the room.
 */
function firstQuestion() {
  return settings.signIn ? el.joinRoom : el.name;
}

/**
 * The lobby, once the server has said whether anybody has to sign in.
 *
 * The name field does not go quiet, it goes away: with a session, the name is
 * the provider's and the server reads it off the session, so a box asking for
 * one would be a box whose contents are thrown out. What replaces it is the
 * name that will actually be used and a way out of it.
 *
 * Nothing here decides anything. Whether this deployment signs people in and
 * who is signed in are both the server's answers, arriving on the request the
 * page was making anyway.
 */
function paintSignIn() {
  if (!settings.signIn) return;

  el.nameField.hidden = true;
  el.name.value = "";

  // A page can only be here with a session -- `/` redirects otherwise -- so the
  // name is missing only when the session ran out between the page loading and
  // this answer. Saying nothing is right: the sentence would have a hole in it,
  // and the press of Join that follows is refused by the server with the reason
  // and the instruction to reload.
  if (!settings.signedInAs) return;

  el.signedInName.textContent = settings.signedInAs;
  el.signedIn.hidden = false;
}

/**
 * Which room the lobby starts on, and what it says about it.
 *
 * A room the URL named is a room this tab is about, so it goes in the title
 * too -- four tabs of four rooms are otherwise four tabs called `mediacore`. A
 * remembered one is a suggestion nobody has accepted yet, and the tab stays
 * unnamed until somebody joins.
 *
 * A name that cannot be a room is put in the field rather than thrown away.
 * The person following that link cannot edit the URL they were sent, but they
 * can edit the field, and they cannot fix what they cannot see.
 */
/**
 * The lobby, when the address carried a pass.
 *
 * There is nothing left to ask. Who this person is was decided by whoever
 * signed the pass, and the room is written into it, so the two fields become a
 * sentence naming both and the button is the only thing left to press. It has
 * to stay a press: `getUserMedia` and `AudioContext` both want a gesture, and
 * a page that joined on load would be refused by the browser and would have
 * nothing to blame it on.
 */
function paintPass() {
  if (!pass) return;

  const problem = passProblem(passClaims, Math.floor(Date.now() / 1000), null);

  if (problem) {
    linkRefused = true;
    fail(el.lobbyError, problem);
    el.join.disabled = true;
    return;
  }

  el.nameField.hidden = true;
  el.roomField.hidden = true;
  el.join.hidden = true;
  el.name.value = "";
  el.joinRoom.value = passClaims.room;
  el.passWho.textContent = whoFrom(passClaims);
  el.passRoom.textContent = passClaims.room;
  el.passNote.hidden = false;
}

/**
 * Joining on arrival, when the address carried a pass.
 *
 * There is nothing to press: both questions are answered and the button would
 * be a step that exists only to be clicked. What the press used to buy was a
 * gesture, and neither thing that wants one is worth stopping for. A
 * microphone refused without one is no longer a refusal of the room -- the
 * person lands as a listener and `Turn on mic` is itself the gesture. Audio
 * playback held back by the browser is what the `Enable audio` banner is for,
 * and it was already there for everybody who reloads into a room.
 */
function joinWithPass() {
  if (!pass || el.join.disabled) return;

  el.form.requestSubmit();
}

function offerRoom() {
  const offer = offeredRoom(arrival.room, prefs.room, settings.room);
  el.joinRoom.value = offer.room;

  if (settings.roomError) {
    linkRefused = true;
    // No preview of a room that cannot exist. The field holds what the link
    // said, because that is what has to be corrected and nobody can correct
    // what they cannot see; the sentence below it says what is wrong with it.
    el.lobbyRoom.textContent = "\u2026";
    fail(
      el.lobbyError,
      `This link does not name a room this server can open: ${settings.roomError}. ` +
        `Type one that can be, then press Join.`,
    );
    return;
  }

  paintLobbyRoom();
}

/** Somebody correcting the room: the preview follows, and the link's refusal goes. */
function onRoomTyped() {
  paintLobbyRoom();
  if (!linkRefused) return;
  linkRefused = false;
  hide(el.lobbyError);
}

/**
 * The room on the server, under the name a person typed. It is a preview and
 * never a decision: the room the page actually joins is the one the token came
 * back naming, and everything shown after that is derived from it.
 */
function paintLobbyRoom() {
  const typed = el.joinRoom.value.trim();
  el.lobbyRoom.textContent =
    typed === ""
      ? "\u2026"
      : composedRoom(settings.room, typed, settings.roomNamespaceSeparator);
}

/**
 * The lobby's own configuration, and -- when this load was addressed to a room
 * -- whether that name can be a room at all. The page keeps no second opinion
 * about what a room name may contain: it asks the one function that decides,
 * on the one request it was making anyway, and prints the answer.
 */
async function loadSettings(room) {
  const asked = room === null ? "" : `?${new URLSearchParams({ room })}`;
  try {
    const response = await fetch(`/api/config${asked}`);
    if (response.ok) return await response.json();
  } catch (error) {
    console.warn("could not read the page configuration", error);
  }
  return settings;
}

async function onJoin(event) {
  event.preventDefault();
  hide(el.lobbyError);
  ensureAudioContext();

  const name = el.name.value.trim();
  if (!pass && !settings.signIn && !name) {
    fail(el.lobbyError, "Type a name so other people know who is talking.");
    el.name.focus();
    return;
  }

  const withVideo = el.withVideo.checked;
  // A name that was never typed is not a preference. With signing in on, the
  // one this browser remembers from before the switch was thrown stays where it
  // is, unread, rather than being overwritten with a name out of a directory.
  remember(settings.signIn ? { camera: withVideo } : { name, camera: withVideo });
  busy(true, "Asking for the microphone...");

  // A browser that will not hand over a microphone is not a room refusing
  // anybody. Everything else this page is -- hearing the room, seeing who is in
  // it, the chat, the bench -- needs no capture at all, and the token minted
  // below grants publishing whether one was opened or not, so the join carries
  // on and the microphone becomes something to turn on rather than a gate to
  // get past. The reason is kept: it is what the button in the bar goes on to
  // say, and what the one line on arrival is made of.
  const opened = await media.openMicrophone();
  micRefusal = opened.opened ? null : opened.reason;

  busy(true, "Connecting...");

  let credentials;
  try {
    credentials = pass ? passCredentials() : await mintToken(name, el.joinRoom.value);
  } catch (error) {
    await media.release();
    busy(false, "");
    fail(el.lobbyError, error.message);
    return;
  }

  try {
    await connect(credentials);
  } catch (error) {
    await discardRoom();
    await media.release();
    busy(false, "");
    fail(el.lobbyError, connectionMessage(error, credentials.url));
    return;
  }

  busy(false, "");
  enterRoom(credentials);

  if (withVideo) await startCamera();
}

/**
 * What a minted token would have come back as, assembled from the pass.
 *
 * The address of the server is the one thing the pass does not carry, and the
 * page already has it: `/api/config` tells every load where the socket is,
 * whether anybody is going to mint anything or not.
 */
function passCredentials() {
  const problem = passProblem(passClaims, Math.floor(Date.now() / 1000), null);

  if (problem) throw new Error(problem);

  return {
    url: settings.wsUrl,
    token: pass,
    room: passClaims.room,
    identity: passClaims.identity,
    name: whoFrom(passClaims),
  };
}

async function mintToken(name, room) {
  // With signing in switched on the browser stops saying who it is. The server
  // reads that off the session and would ignore a `name` here, so none is sent:
  // a field that is read and discarded is a field somebody will one day believe.
  const body = settings.signIn ? { room } : { name, room };

  let response;
  try {
    response = await fetch("/api/token", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });
  } catch {
    throw new Error(
      "This server did not answer. Check that it is still running, then press Join again.",
    );
  }

  const payload = await response.json().catch(() => ({}));
  if (!response.ok) {
    throw new Error(
      payload.error || `This server refused the request (HTTP ${response.status}).`,
    );
  }
  return payload;
}

async function connect(credentials) {
  room = new lk.Room({ adaptiveStream: false, dynacast: true });

  room
    .on(lk.RoomEvent.ParticipantConnected, render)
    .on(lk.RoomEvent.ParticipantDisconnected, onParticipantDisconnected)
    .on(lk.RoomEvent.TrackSubscribed, onTrackSubscribed)
    .on(lk.RoomEvent.TrackUnsubscribed, onTrackUnsubscribed)
    .on(lk.RoomEvent.TrackPublished, render)
    .on(lk.RoomEvent.TrackUnpublished, render)
    .on(lk.RoomEvent.TrackMuted, render)
    .on(lk.RoomEvent.TrackUnmuted, render)
    .on(lk.RoomEvent.TrackStreamStateChanged, render)
    .on(lk.RoomEvent.LocalTrackPublished, render)
    .on(lk.RoomEvent.LocalTrackUnpublished, render)
    .on(lk.RoomEvent.ChatMessage, onChatMessage)
    .on(lk.RoomEvent.ParticipantPermissionsChanged, onPermissions)
    .on(lk.RoomEvent.ConnectionStateChanged, onConnectionState)
    .on(lk.RoomEvent.AudioPlaybackStatusChanged, onPlaybackStatus)
    .on(lk.RoomEvent.Disconnected, onDisconnected);

  await room.connect(credentials.url, credentials.token);
  media.attach(room);

  // Somebody without a microphone has nothing to publish, and a join that threw
  // over that would hand one browser permission the power to keep them out of
  // the room altogether. A publication that fails with a microphone in hand is
  // still fatal: that is the server refusing a source the token names.
  if (media.state().microphone.open) {
    const published = await media.publishMicrophone();
    if (!published.published) throw new Error(published.reason);
    watchLevel(room.localParticipant.identity, media.microphoneTrack);
  }

  startBench();
}

function enterRoom(credentials) {
  // Everything said about the room from here is derived from the name the
  // token came back with, and not from the field: the server composes the room
  // it issued a token for, and a page that titled itself from what was typed
  // would be naming a room it is not necessarily in.
  const address = plainRoom(
    credentials.room,
    settings.room,
    settings.roomNamespaceSeparator,
  );

  el.roomName.textContent = address;
  showAddress(address);
  remember({ room: address });
  el.footUrl.textContent = `${credentials.url} - ${credentials.room} - ${credentials.identity}`;
  hide(el.roomError);
  el.lobby.hidden = true;
  el.room.hidden = false;
  onPlaybackStatus();
  paintControls();
  paintDevices();
  paintChat();
  render();
  // Arriving without a microphone outranks anything worked out before the join.
  // Both sentences are about the same device, and this one is about the room
  // this person is now in rather than about the list they were offered.
  if (micRefusal) showNotice(listeningLine(micRefusal));
  else if (arrivalNotice) showNotice(arrivalNotice);
  arrivalNotice = null;
  el.mute.focus();
}

/**
 * The address bar catches up at the moment there is a room to catch up with.
 * Until then `/` is honest: nothing has been joined, and a URL that named a
 * room somebody was still typing would be a link to a room nobody is in.
 *
 * `replaceState`, not `pushState`. Back would otherwise return to the lobby's
 * URL while the page is still in the room -- nothing here listens for
 * `popstate` -- and leaving the room is what the Leave button is for. The room
 * survives the leave in the URL and in the field, so pressing it and changing
 * your mind costs one press rather than the link you no longer have.
 */
function showAddress(room) {
  const path = roomPath(room, location.search);
  if (path === `${location.pathname}${location.search}`) return;
  history.replaceState(null, "", path);
}

async function leave(reason) {
  await discardRoom();
  await releaseCamera();
  if (media) {
    await media.detach();
    await media.release();
  }
  stopBench();
  dropAllLevels();
  tiles.clear();
  clearCards();
  clearChat();
  // Leaving is a decision, and a reload after one should land in the lobby
  // rather than walking straight back in.
  forgetPass(sessionStorage);
  leaveFullscreen();
  scheduleShareRecheck(null);
  paintShare(null, false);
  paintShareTabs([]);
  el.audio.replaceChildren();
  deafened = false;
  mutedBeforeDeafened = false;
  pinnedShare = null;
  stageFocus = null;
  layoutStage({ mode: "grid", main: null });
  paintFocusControl();
  toggleMenu(false);
  toggleTray(false);
  togglePeople(false);
  clearNotice();
  micRefusal = null;
  el.room.hidden = true;
  el.lobby.hidden = false;
  busy(false, reason || "");
  firstQuestion().focus();
}

async function discardRoom() {
  if (!room) return;
  const leaving = room;
  room = null;
  leaving.removeAllListeners();
  await leaving.disconnect();
}

async function releaseCamera() {
  if (!cameraTrack) return;
  const track = cameraTrack;
  cameraTrack = null;
  await track.stop();
}

function capturePreset() {
  const name = PRESETS[query.get("res")] || "h720";
  return lk.VideoPresets[name];
}

async function startCamera() {
  if (!room || cameraTrack) return;

  pending.camera = true;
  paintControls();

  const preset = capturePreset();
  try {
    cameraTrack = await lk.createLocalVideoTrack({ resolution: preset.resolution });
    await room.localParticipant.publishTrack(cameraTrack, {
      simulcast: wantsSimulcast,
      videoCodec: "vp8",
      backupCodec: false,
      videoEncoding: preset.encoding,
    });
  } catch (error) {
    await releaseCamera();
    fail(el.roomError, cameraMessage(error));
  }

  pending.camera = false;
  paintControls();
  render();
}

async function stopCamera() {
  if (!room || !cameraTrack) return;

  pending.camera = true;
  paintControls();
  try {
    await room.localParticipant.unpublishTrack(cameraTrack, true);
  } catch (error) {
    console.warn("the camera track could not be unpublished cleanly", error);
  }
  await releaseCamera();
  pending.camera = false;
  paintControls();
  render();
}

async function onCamera() {
  hide(el.roomError);
  if (cameraTrack) {
    await stopCamera();
  } else {
    await startCamera();
  }
}

async function onShareScreen() {
  if (!media || !room) return;
  hide(el.roomError);

  pending.share = true;
  paintControls();

  if (media.state().screenShare.active) {
    await media.stopScreenShare();
    showNotice("You stopped sharing your screen.");
  } else {
    const result = await media.startScreenShare();
    if (result.outcome === "started") {
      showNotice(
        result.audioNote ||
          (result.audio
            ? "Sharing your screen, with the sound the browser handed over with it."
            : "Sharing your screen."),
      );
    } else if (result.outcome === "cancelled" || result.outcome === "unsupported") {
      showNotice(result.reason);
    } else {
      fail(el.roomError, result.reason);
    }
  }

  pending.share = false;
  paintControls();
  render();
}

function onScreenShareStopped() {
  paintControls();
  render();
}

async function onMute() {
  if (!media) return;
  // With no microphone open this button is not a toggle at all: it is the way
  // to one, for somebody who joined as a listener and has since told the
  // browser to allow the device.
  if (!media.state().microphone.open) return turnMicrophoneOn();

  pending.mute = true;
  paintControls();
  const result = await media.toggleMuted();
  pending.mute = false;
  if (result.reason) fail(el.roomError, result.reason);
  paintControls();
  render();
}

/**
 * Ask for the microphone again, in the room, without leaving it.
 *
 * The token every join mints carries `microphone`, so the grant was never the
 * thing that was missing -- the browser was. Which leaves an ordinary publish
 * onto a connection that is already up: nobody is unsubscribed, nothing beyond
 * the new track is negotiated, and the room simply sees somebody who was quiet
 * start talking.
 *
 * It has to be a press, and it cannot be anything else. `getUserMedia` only
 * puts its question inside a gesture, so a page that retried on a timer would
 * be asking a question the browser refuses to ask, and one that retried on
 * every render would spend a call being refused in silence. A refusal that
 * comes back is shown as it is, without the invitation to press again that
 * arriving carries: the press has just happened.
 */
async function turnMicrophoneOn() {
  hide(el.roomError);
  pending.mute = true;
  paintControls();

  const opened = await media.openMicrophone();
  micRefusal = opened.opened ? null : opened.reason;

  if (!opened.opened) {
    fail(el.roomError, opened.reason);
  } else {
    const published = await media.publishMicrophone();
    if (published.published) {
      showNotice("Your microphone is on, and the room can hear you.");
    } else {
      fail(el.roomError, published.reason);
    }
  }

  pending.mute = false;
  paintControls();
  render();
}

/**
 * The one line somebody who has no microphone gets on arrival, and the only
 * place the page says it.
 *
 * Two halves, because they are two different claims. The first is the
 * browser's: what it refused and where the padlock that undoes it is, in the
 * words `media.js` already wrote for it. The second is this page's: the room
 * refused nobody, and the way back to a microphone from here is a button in the
 * bar rather than a rejoin.
 */
function listeningLine(reason) {
  return `${reason} You are in the room meanwhile, listening; press Turn on mic to ask again.`;
}

/**
 * Silences every sink that carries somebody else's audio, and keeps silencing
 * the ones that appear later.
 *
 * `Track.attach` writes `muted = false` on every attach, so a participant who
 * arrives after the button was pressed, a resubscription after a reconnect and
 * `room.startAudio()` all bring the room back at full volume unless this runs
 * again. Muting the element rather than turning the volume down is deliberate:
 * iOS Safari ignores writes to `volume`, a recycled element carries a volume of
 * zero on to the next person's track, and screen-share audio is not covered by
 * a participant's volume at all.
 */
function silenceIncoming() {
  for (const element of el.audio.children) element.muted = deafened;
}

async function onDeafen() {
  if (!media) return;

  deafened = !deafened;
  silenceIncoming();

  const state = media.state();
  const muted = Boolean(state.microphone.muted);

  if (deafened) {
    mutedBeforeDeafened = muted;
  }

  const wanted = deafened || mutedBeforeDeafened;

  paintControls();

  if (state.microphone.open && muted !== wanted) {
    pending.mute = true;
    paintControls();
    const result = await media.setMuted(wanted);
    pending.mute = false;
    if (result.reason) fail(el.roomError, result.reason);
  }

  paintControls();
  render();
}

async function applyProcessing(key) {
  if (!media) return;
  hide(el.roomError);

  const wanted = !media.state().processing[key];
  const result = await media.setProcessing({ [key]: wanted });
  const label = PROCESSING_ROWS.find((row) => row.key === key).label;
  remember({ processing: media.state().processing });

  if (result.applied) {
    showNotice(`${label} is now ${wanted ? "on" : "off"}.`);
  } else if (result.deferred) {
    showNotice(
      `${label} will change when you unmute. Restarting the capture while you are muted would ` +
        "re-open the device and light the browser's recording indicator.",
    );
  } else if (result.reason) {
    fail(el.roomError, result.reason);
  }

  paintControls();
}

function offeredEngines() {
  const state = media ? media.state() : null;
  const available = Boolean(state && state.noise.available);
  return NOISE_ROWS.filter((row) => row.engine !== "mediacore" || available);
}

function onCycleNoise() {
  const state = media ? media.state() : null;
  if (!state) return;
  const offered = offeredEngines();
  const index = offered.findIndex((row) => row.engine === state.noise.engine);
  const next = offered[(index + 1) % offered.length];
  return applyNoiseEngine(next.engine);
}

async function applyNoiseEngine(engine) {
  if (!media || pending.noise) return;
  hide(el.roomError);

  // What is remembered is what a person chose, not where a fallback put them.
  // The overload watch pulling our node out is a measurement of this machine at
  // this moment; writing that down as the choice would quietly answer the
  // question on their behalf every time they reloaded.
  remember({ noise: engine });

  pending.noise = true;
  paintControls();
  const result = await media.setNoiseEngine(engine);
  pending.noise = false;

  const row = NOISE_ROWS.find((entry) => entry.engine === engine);
  const label = row ? row.label.toLowerCase() : engine;

  if (result.applied) {
    showNotice(
      engine === "mediacore"
        ? "The mediacore denoiser is in the capture path, and the browser's own suppression is " +
            "off. Nobody was unsubscribed: only the capture behind the publication changed."
        : `Noise suppression is now ${label}.`,
    );
  } else if (result.deferred) {
    showNotice(
      "The suppressor will change when you unmute. Restarting the capture while you are muted " +
        "would re-open the device and light the browser's recording indicator.",
    );
  } else if (result.reason) {
    fail(el.roomError, result.reason);
  }

  paintControls();
}

async function onSelectMicrophone() {
  if (!media) return;
  hide(el.roomError);

  const chosen = el.micSelect.value;
  remember({ microphone: chosen === SYSTEM_DEFAULT ? null : chosen });
  const result = await media.selectMicrophone(chosen === SYSTEM_DEFAULT ? null : chosen);

  // Choosing an input with nothing open is not a failure to report in red. It
  // is what the next open will ask for, which is the whole of what somebody
  // listening means by choosing one before they press Turn on mic.
  if (!media.state().microphone.open) {
    showNotice("That is the input this page will ask for when the microphone goes on.");
  } else if (result.applied) {
    showNotice(
      "Switched microphone. Nobody was unsubscribed: only the capture behind the publication changed.",
    );
  } else if (result.deferred) {
    showNotice("The microphone will switch when you unmute.");
  } else if (result.reason) {
    fail(el.roomError, result.reason);
  }

  paintControls();
}

function onBenchToggle() {
  showBench = !showBench;
  if (showBench && fullscreen !== "off") {
    leaveFullscreen();
    showNotice("The bench holds the video while it is open, so the fullscreen view closed.");
  }
  paintBenchToggle();
  render();
  if (lastReading) paint(lastReading);
}

function paintBenchToggle() {
  el.benchToggle.setAttribute("aria-pressed", String(showBench));
  el.benchToggle.setAttribute(
    "aria-label",
    showBench ? "Hide the bandwidth bench" : "Show the bandwidth bench",
  );
  el.benchToggle.dataset.on = String(showBench);
  el.bench.hidden = !showBench;
}

function toggleMenu(open) {
  const next = open === undefined ? el.audioMenu.hidden : open;
  el.audioMenu.hidden = !next;
  el.audioSettings.setAttribute("aria-expanded", String(next));
  el.audioSettings.dataset.on = String(next);
  // A phone shows this menu as a sheet across the bottom of the window, and
  // the stylesheet draws the sheet a backdrop off this. It is on the bar
  // rather than on the menu because the backdrop has to cover the bar too:
  // a tap that lands on a control while a sheet is open is a tap somebody
  // meant for the sheet.
  el.bar.dataset.menu = next ? "open" : "shut";
  if (!next) return;

  paintDevices();
  el.micSelect.focus();
  if (media) media.listMicrophones().then(paintDevices);
}

/**
 * The controls that do not fit across a phone. Four of the eight stay in the
 * bar -- the microphone, the camera, this button and Leave -- and the rest are
 * rows above it, one press away, with the names they never had room for on a
 * desktop. Nothing here paints a control: they are the same buttons, in the
 * same bar, carrying the same state they were painted with.
 */
function toggleTray(open) {
  const next = open === undefined ? el.bar.dataset.more !== "open" : open;
  el.bar.dataset.more = next ? "open" : "shut";
  el.barMore.setAttribute("aria-expanded", String(next));
  el.barMore.dataset.on = String(next);
  if (!next) keepFocus(el.bar, el.barMore);
}

/**
 * The list of who is here. It is the column it always was on a desktop, where
 * this button is not drawn at all; on a phone the column is one line -- the
 * room, the connection pill and this count -- and the list comes over the
 * stage rather than pushing it down, because the picture must not move under
 * a thumb that is reaching for something else.
 */
function togglePeople(open) {
  const next = open === undefined ? el.roster.dataset.people !== "open" : open;
  el.roster.dataset.people = next ? "open" : "shut";
  el.peopleToggle.setAttribute("aria-expanded", String(next));
  if (!next) keepFocus(el.roster, el.peopleToggle);
  paintPeople();
  if (chatShowing()) {
    unread = 0;
    paintChat();
  }
}

/**
 * A panel closing under the keyboard. Both of these are put away with
 * `display: none`, and `display: none` on the element that has the focus hands
 * the focus to `document.body` -- from where the next Tab starts at the top of
 * the page. Whoever was inside gets the button that holds it instead. Pressing
 * outside the panel is the same story with a pointer, which is why this lives
 * here rather than beside the Escape key.
 */
function keepFocus(within, fallback) {
  if (within.contains(document.activeElement)) fallback.focus();
}

/**
 * On a phone the count is the control that opens the list, so it says both the
 * number and what pressing it does -- a button named "3 people" tells a screen
 * reader nothing about what happens next. The number is read off the list
 * itself, which `render` has just written.
 */
function paintPeople() {
  const count = el.participants.children.length;
  const many = count === 1 ? "Just you" : `${count} people`;
  const open = el.roster.dataset.people === "open";
  const news = unread === 0 ? "" : ` ${unread} unread ${unread === 1 ? "message" : "messages"}.`;
  el.peopleCount.textContent = many;
  el.peopleToggle.setAttribute(
    "aria-label",
    `${many} in the room. Press to ${open ? "hide" : "show"} the list.${news}`,
  );
}

async function onChatSubmit(event) {
  event.preventDefault();

  const verdict = canSend({
    connected: room !== null && room.state === "connected",
    allowed: mayPublishData(),
    text: el.chatText.value,
  });

  if (!verdict.send) {
    sayChat(verdict.reason);
    return;
  }

  el.chatText.value = "";
  el.chatSend.disabled = true;
  sayChat("");

  try {
    await room.localParticipant.sendChatMessage(verdict.text);
  } catch (error) {
    el.chatText.value = verdict.text;
    sayChat(`That message did not leave this browser: ${(error && error.message) || error}.`);
  } finally {
    paintChat();
  }
}

function onChatMessage(message, participant) {
  const labels = labelsFor([room.localParticipant, ...room.remoteParticipants.values()]);
  const line = lineFrom(
    message,
    participant,
    room.localParticipant,
    participant && labels.get(participant.identity),
  );

  if (!line) return;

  appendChatLine(line);
  unread = afterArrival(unread, chatShowing());
  paintChat();
}

function onPermissions(_previous, participant) {
  if (room && participant === room.localParticipant) paintChat();
}

function appendChatLine(line) {
  const log = el.chatLog;
  const atBottom = log.scrollHeight - log.scrollTop - log.clientHeight < 40;

  const row = document.createElement("li");
  row.dataset.mine = String(line.mine);

  const who = document.createElement("span");
  who.className = "who";
  who.textContent = line.label;
  if (line.mine) {
    const you = document.createElement("span");
    you.className = "you";
    you.textContent = "you";
    who.append(" ", you);
  }

  const when = document.createElement("time");
  when.className = "when";
  when.dateTime = new Date(line.at).toISOString();
  when.textContent = clockOf(line.at);

  const said = document.createElement("p");
  said.className = "said";
  said.setAttribute("dir", "auto");
  said.textContent = line.text;

  row.append(who, when, said);
  log.append(row);

  while (log.children.length > KEEP_LINES) log.firstElementChild.remove();
  if (atBottom) log.scrollTop = log.scrollHeight;
}

function showPanel(which) {
  panel = which;
  el.roster.dataset.panel = which;
  el.participants.hidden = which !== "people";
  el.chat.hidden = which !== "chat";
  el.peopleTab.setAttribute("aria-pressed", String(which === "people"));
  el.chatTab.setAttribute("aria-pressed", String(which === "chat"));

  if (chatShowing()) unread = 0;

  paintChat();
  paintPeople();

  if (which === "chat") el.chatText.focus();
}

/**
 * Whether the chat panel is on the screen, measured rather than guessed. On a
 * phone the sheet is put away with `display: none`, so a chosen tab is not the
 * same thing as a visible panel, and unread would reset for somebody who never
 * saw it.
 */
function chatShowing() {
  return el.chat.offsetParent !== null;
}

function paintChat() {
  const allowed = mayPublishData();

  el.chatEmpty.hidden = el.chatLog.children.length > 0;
  el.chatUnread.textContent = String(unread);
  el.chatUnread.hidden = unread === 0;
  el.peopleUnread.textContent = String(unread);
  el.peopleUnread.hidden = unread === 0 || panel === "chat";
  el.chatTab.setAttribute("aria-label", unreadLabel(unread));
  el.chatText.disabled = room === null;
  el.chatSend.disabled = room === null || !allowed;

  if (!allowed) sayChat(NOT_ALLOWED);
  else if (el.chatNote.textContent === NOT_ALLOWED) sayChat("");
}

/**
 * The server drops a data packet from somebody whose token does not carry the
 * right and says nothing about it, so the refusal has to be shown here: an
 * attempt would resolve as if it had been sent.
 */
function mayPublishData() {
  const permissions = room ? room.localParticipant.permissions : null;
  return !permissions || permissions.canPublishData !== false;
}

function sayChat(text) {
  el.chatNote.textContent = text;
  el.chatNote.hidden = text === "";
}

function clearChat() {
  el.chatLog.replaceChildren();
  el.chatText.value = "";
  unread = 0;
  sayChat("");
  showPanel("people");
}

function onKeydown(event) {
  if (event.key !== "Escape") return;

  if (!el.audioMenu.hidden) {
    toggleMenu(false);
    el.audioSettings.focus();
    return;
  }

  // The two panels a narrow window opens over the room, in the order they are
  // stacked and one press each. Neither is ever open on a desktop, where both
  // buttons are laid out away, so this costs a wide window nothing.
  if (el.bar.dataset.more === "open") {
    toggleTray(false);
    el.barMore.focus();
    return;
  }

  if (el.roster.dataset.people === "open") {
    togglePeople(false);
    el.peopleToggle.focus();
    return;
  }

  // A fullscreen the browser granted is the browser's to leave, and it does --
  // the key is usually eaten before the page ever sees it. One the page drew
  // itself is the page's, and nothing else would leave it.
  //
  // Either way the shared screen is the whole of what is on the screen, and the
  // tile somebody put in the middle is behind it, so this key is not about that
  // tile yet. It becomes about it on the next press, once the picture is gone
  // and the strip is back.
  if (fullscreen !== "off") {
    if (fullscreen === "fallback") {
      leaveFullscreen();
      el.shareFullscreen.focus();
    }
    return;
  }

  // Leaving the middle puts the tile back in the strip; `syncCards` carries the
  // keyboard to it, so somebody who got here by pressing it is still on it.
  if (stageFocus) {
    stageFocus = null;
    render();
  }
}

function onPointerDown(event) {
  // The tray survives a press inside the menu it opens: the menu is a sheet
  // over the bar, and closing the tray under it would leave the button the
  // menu hands focus back to laid out away.
  if (
    el.bar.dataset.more === "open" &&
    !el.bar.contains(event.target) &&
    !el.audioMenu.contains(event.target)
  ) {
    toggleTray(false);
  }

  if (el.roster.dataset.people === "open" && !el.roster.contains(event.target)) {
    togglePeople(false);
  }

  if (el.audioMenu.hidden) return;
  if (el.audioMenu.contains(event.target) || el.audioSettings.contains(event.target)) return;
  toggleMenu(false);
}

function onMediaChange() {
  paintControls();
  paintDevices();
  render();
}

function onMicrophoneTrack(entry) {
  if (!room || !entry.track) return;
  watchLevel(room.localParticipant.identity, entry.track);
  render();
}

function startBench() {
  bench = createBench(room);
  lastReading = null;
  if (!benchTimer) benchTimer = setInterval(pump, BENCH_INTERVAL_MS);
}

function stopBench() {
  if (benchTimer) {
    clearInterval(benchTimer);
    benchTimer = null;
  }
  if (bench) bench.reset();
  bench = null;
  lastReading = null;
  paintLink({
    estimate: null,
    arriving: null,
    video: null,
    rtt: null,
    sending: null,
    note: NO_SUBSCRIBER,
  });
}

async function pump() {
  if (!bench || !room) return;

  let reading;
  try {
    reading = await bench.read();
  } catch (error) {
    console.warn("the browser refused to report statistics", error);
    return;
  }
  if (!bench) return;

  lastReading = reading;
  paint(reading);
}

function paint(reading) {
  paintLink(reading.link);
  if (showBench) tiles.paint(reading);
  paintArriving(reading);
  paintShareStats();
}

function paintLink(link) {
  write(
    el.estimate,
    formatBitrate(link.estimate),
    link.note || link.pairNote || link.estimateNote || NOT_REPORTED,
  );
  write(el.arrivingRate, formatBitrate(link.arriving), link.note || FIRST_SAMPLE);
  write(el.videoRate, formatBitrate(link.video), link.note || link.videoNote || FIRST_SAMPLE);
  write(el.rtt, formatMillis(link.rtt), link.note || link.pairNote || NOT_REPORTED);
  write(el.sending, formatBitrate(link.sending), link.sendingNote || NOT_REPORTED);
}

function paintArriving(reading) {
  for (const entry of arriving.values()) {
    const described = describeAudio(reading.tracks.get(entry.sid));
    write(entry.node, described.text, described.reason);
  }
}

function onTrackSubscribed(track, _publication, participant) {
  if (track.kind === lk.Track.Kind.Audio) {
    const element = track.attach();
    element.dataset.identity = participant.identity;
    element.muted = deafened;
    el.audio.appendChild(element);
    if (track.source === lk.Track.Source.Microphone) watchLevel(participant.identity, track);
  }
  render();
}

function onTrackUnsubscribed(track, publication, participant) {
  if (track.kind === lk.Track.Kind.Audio) {
    track.detach().forEach((element) => element.remove());
    if (track.source === lk.Track.Source.Microphone) dropLevel(participant.identity);
  }
  if (bench && publication) bench.forget(publication.trackSid);
  render();
}

function onParticipantDisconnected(participant) {
  dropLevel(participant.identity);
  dropAudioElements(participant.identity);
  render();
}

/**
 * The `<audio>` element somebody's track was attached to, when the person went
 * without the track being unsubscribed first. `onTrackUnsubscribed` detaches on
 * the way out and does not fire for a participant who simply left, so every
 * departure used to leave an element behind in `#audio` holding a dead stream,
 * for the rest of the call.
 */
function dropAudioElements(identity) {
  for (const element of [...el.audio.children]) {
    if (element.dataset.identity !== identity) continue;
    element.srcObject = null;
    element.remove();
  }
}

/**
 * Puts a meter on one participant's microphone.
 *
 * The measurement runs in an AudioWorklet, so this cannot be synchronous: the
 * module has to be in the context before a node can be built from it. Which
 * makes the ordering matter -- a device change replaces the local track and
 * calls this again while the first attach is still waiting on `addModule` --
 * so each attempt takes a token, and one that finds a newer attempt in its
 * place when it wakes up disposes of what it built instead of installing it.
 */
function watchLevel(identity, track) {
  // Two handlers rather than a `catch` after a `then`, so that a throw from the
  // repaint is not reported as the meter having failed. `attachLevel` used to
  // end in `render()` inside the try, and any exception a repaint raised put
  // "this browser will not run the level meter" on the row while a perfectly
  // good meter went on measuring.
  attachLevel(identity, track).then(
    () => render(),
    (error) => {
      console.warn("no level meter could be put on this track", error);
      meterFailures.set(identity, NO_LEVEL_METER);
      attaching.delete(identity);
      render();
    },
  );
}

async function attachLevel(identity, track) {
  // What is *published*, not what was captured. A LiveKit track with a
  // processor on it sends `processor.processedTrack`, and measuring the capture
  // instead would judge your own row on audio nobody else receives: RNNoise
  // takes most of the keyboard and the room tone out before any remote meter
  // sees them, so your row and everybody else's row for you could disagree
  // permanently. Remote tracks have no processor and this is the track itself.
  const processed = track && track.processor ? track.processor.processedTrack : null;
  const measured = processed || (track && track.mediaStreamTrack);
  if (!measured) return;

  dropLevel(identity);
  const context = ensureAudioContext();
  if (!context) return;

  attachCount += 1;
  const token = attachCount;
  attaching.set(identity, token);

  const ready = await meterModuleIn(context);
  if (attaching.get(identity) !== token) return;
  attaching.delete(identity);
  if (!ready) return;

  const node = context.createMediaStreamSource(new MediaStream([measured]));
  const meter = new AudioWorkletNode(context, LEVEL_METER_PROCESSOR, {
    numberOfInputs: 1,
    numberOfOutputs: 1,
    outputChannelCount: [1],
    channelCount: 1,
    channelCountMode: "explicit",
    channelInterpretation: "speakers",
    processorOptions: { reportEveryMs: REPORT_EVERY_MS },
  });

  // A node whose output goes nowhere is a node the graph never has to pull, and
  // this one must never be pulled towards the speakers: everybody in the room
  // is already audible through their own `<audio>` element, and a second path
  // to the output would be each of them heard twice. A
  // MediaStreamAudioDestinationNode is pulled like any other destination and
  // its stream is attached to nothing at all.
  const sink = context.createMediaStreamDestination();
  node.connect(meter).connect(sink);

  const entry = { node, meter, sink, gate: createVoiceGate(), state: "unknown" };
  meter.port.onmessage = (event) => onLevel(identity, entry, event.data);
  meter.onprocessorerror = () => onMeterError(identity);
  meters.set(identity, entry);

  meterFailures.delete(identity);
  if (!meterSweep) meterSweep = setInterval(sweepLevels, METER_SWEEP_MS);
}

/**
 * Installs the meter's worklet module, once per context, and remembers the
 * answer. A browser that will not take it is not quietly demoted to a
 * main-thread meter: that is the meter this page just stopped believing, and
 * one that reads a fraction of the audio chosen by whether the tab has focus is
 * worse than saying nothing. It says nothing, with the reason attached.
 */
function meterModuleIn(context) {
  if (meterModule) return meterModule;

  if (typeof AudioWorkletNode !== "function" || !context.audioWorklet) {
    meterFault = NO_LEVEL_METER;
    meterModule = Promise.resolve(false);
    return meterModule;
  }

  meterModule = context.audioWorklet
    .addModule(LEVEL_METER_URL)
    .then(() => true)
    .catch((error) => {
      console.warn("the level meter would not load, so nobody can be shown as speaking", error);
      meterFault = NO_LEVEL_METER;
      return false;
    });
  return meterModule;
}

/**
 * One report from the audio thread. The gate is what decides; this only asks
 * whether the answer on screen has stopped being the answer.
 */
function onLevel(identity, entry, report) {
  if (meters.get(identity) !== entry) return;
  const reading = entry.gate.observe(report, performance.now());
  if (reading.state === entry.state) return;
  entry.state = reading.state;
  render();
}

// A worklet that throws is dead for the rest of its life and its node reports
// nothing ever again, so the meter is taken out rather than left to age into
// "unknown" without a reason worth reading.
function onMeterError(identity) {
  console.warn("the level meter stopped on the audio thread", identity);
  dropLevel(identity);
  meterFailures.set(identity, METER_STOPPED);
  render();
}

/**
 * Repaints when a meter's answer has changed without a report arriving to say
 * so -- which is what going stale is. Nothing is decided here.
 */
function sweepLevels() {
  const now = performance.now();
  let changed = false;
  for (const entry of meters.values()) {
    const state = entry.gate.describe(now).state;
    if (state === entry.state) continue;
    entry.state = state;
    changed = true;
  }
  if (changed) render();
}

/**
 * A tab coming back to the front. Nothing about who is speaking changed while
 * it was away -- the audio thread went on measuring, which is the whole reason
 * the meter is on it -- but a context the browser suspended has been measuring
 * nothing, and either way what is on screen was painted from readings that have
 * since aged, so it is painted again from the readings there are now.
 */
function onVisibilityChange() {
  if (document.visibilityState !== "visible") return;
  if (audioContext && audioContext.state === "suspended") {
    audioContext.resume().catch(() => {});
  }
  const now = performance.now();
  for (const entry of meters.values()) entry.state = entry.gate.describe(now).state;
  render();
}

function ensureAudioContext() {
  const Ctor = window.AudioContext || window.webkitAudioContext;
  if (!Ctor) return null;

  if (!audioContext) {
    try {
      audioContext = new Ctor();
    } catch (error) {
      console.warn("no audio context, so this page cannot say who is speaking", error);
      return null;
    }
  }
  if (audioContext.state === "suspended") {
    audioContext.resume().catch(() => {});
  }
  return audioContext;
}

function dropLevel(identity) {
  attaching.delete(identity);
  meterFailures.delete(identity);
  const entry = meters.get(identity);
  if (!entry) return;
  meters.delete(identity);

  entry.meter.port.onmessage = null;
  entry.meter.onprocessorerror = null;
  try {
    entry.meter.port.postMessage("stop");
  } catch {
    // The port went with the context; there is nothing left to stop.
  }
  for (const node of [entry.node, entry.meter, entry.sink]) node.disconnect();
  for (const track of entry.sink.stream.getTracks()) track.stop();

  // The sweep exists to notice readings going stale, and there is nothing left
  // to go stale. Clearing it only on the way out of the room left it firing
  // twice a second over an empty map for the rest of the session.
  if (meters.size === 0 && meterSweep) {
    clearInterval(meterSweep);
    meterSweep = null;
  }
}

function dropAllLevels() {
  for (const identity of [...meters.keys()]) dropLevel(identity);
  attaching.clear();
  meterFailures.clear();
  // `meterFault` is deliberately kept: it is a fact about this browser, not
  // about this visit to a room, and `meterModule` has cached the same answer.
  if (meterSweep) {
    clearInterval(meterSweep);
    meterSweep = null;
  }
}

function onConnectionState(state) {
  if (!room) return;

  if (state === lk.ConnectionState.Connected) {
    setState("connected", "live");
    hide(el.roomError);
  } else if (state === lk.ConnectionState.SignalReconnecting) {
    setState("lost", "signalling lost");
    fail(
      el.roomError,
      "The connection to the server dropped. This server cannot resume a session that was " +
        "interrupted, so it will not come back on its own: press Leave and join again.",
    );
  } else if (state === lk.ConnectionState.Reconnecting) {
    setState("lost", "reconnecting");
    fail(el.roomError, "The connection dropped. Trying to get it back.");
  } else if (state === lk.ConnectionState.Connecting) {
    setState("", "connecting");
  }
}

function onPlaybackStatus() {
  el.enableAudio.hidden = !room || room.canPlaybackAudio;
}

async function onEnableAudio() {
  if (!room) return;
  ensureAudioContext();
  try {
    await room.startAudio();
    silenceIncoming();
    onPlaybackStatus();
  } catch (error) {
    fail(el.roomError, `The browser would not start audio playback: ${error.message || error}`);
  }
}

function onDisconnected(reason) {
  setState("lost", "disconnected");
  fail(
    el.roomError,
    `The room connection ended${disconnectDetail(reason)}. Press Leave and join again.`,
  );
  render();
}

function disconnectDetail(reason) {
  const names = lk.DisconnectReason;
  const name = names && typeof names === "object" ? names[reason] : undefined;
  return typeof name === "string" ? ` (${name.toLowerCase().replace(/_/g, " ")})` : "";
}

function render() {
  if (!room) return;

  const people = [room.localParticipant, ...room.remoteParticipants.values()];
  const labels = labelsFor(people);

  // One description per participant, made once and given to everything that
  // paints them. The roster, the tiles and the line across the shared screen
  // used to ask separately, which meant three answers taken at three instants
  // and, because only two of them went through `describeParticipant` at all,
  // three different opinions about a muted person.
  const described = new Map(
    people.map((participant) => [
      participant.identity,
      describeParticipant(participant, participant === room.localParticipant),
    ]),
  );

  arriving.clear();
  el.participants.replaceChildren(
    ...people.map((participant) =>
      renderParticipant(
        participant,
        participant === room.localParticipant,
        labels.get(participant.identity),
        described.get(participant.identity),
      ),
    ),
  );
  el.roomCount.textContent =
    people.length === 1 ? "Just you so far." : `${people.length} people in the room.`;
  paintPeople();

  const shares = screenShares(people, labels);
  const step = nextShare(pinnedShare, shares, { link: linkState(), now: performance.now() });
  pinnedShare = step.pinned;
  scheduleShareRecheck(step.waitFor);
  paintShareTabs(shares);

  // The stage is holding a share while it is waiting for one too: the section
  // is up, saying so, and the room is laid out around it either way.
  const holding = Boolean(step.entry) || step.outcome === "waiting";
  const entries = cardEntries(people, labels, described);
  const plan = stagePlan(stageFocus, holding ? step : null, entries, {
    link: linkState(),
    fullscreen: fullscreen !== "off",
  });
  stageFocus = plan.focus;

  // A track attached in two places at once is a track either renderer can rip
  // out from under the other, so the bench owns the video or the stage does.
  if (showBench) {
    clearCards();
    paintShare(null, false);
    // The bench holds every video, so there is nothing for the stage to lay
    // out. The choice somebody made is kept, and the stage returns to it when
    // they close the bench again.
    layoutStage({ mode: "grid", main: null });
    tiles.sync(videoTiles(people, labels));
  } else {
    tiles.clear();
    paintShare(step.entry, step.outcome === "waiting");
    // A tile behind a fullscreen share is a video this machine keeps decoding
    // into something `display: none`, which buys the share nothing at all.
    if (fullscreen === "off") syncCards(entries, plan);
    else clearCards();
    layoutStage(plan);
  }

  el.bench.hidden = !showBench;
  el.cards.dataset.compact = String(plan.mode === "share");
  paintStageSpeaking(people, labels, described);
  announceShareChange(step);
  paintFocusControl();
  paintControls();

  if (lastReading) paint(lastReading);
}

/**
 * Whether the room is in a position to say anything about a share at all. A
 * reconnecting room has dropped every remote participant, so its empty share
 * list is the absence of news rather than news of an absence.
 */
function linkState() {
  if (!room) return "gone";
  if (room.state === lk.ConnectionState.Connected) return "connected";
  if (room.state === lk.ConnectionState.Disconnected) return "gone";
  return "waiting";
}

function scheduleShareRecheck(waitFor) {
  if (shareTimer) clearTimeout(shareTimer);
  shareTimer = null;
  if (typeof waitFor !== "number") return;
  shareTimer = setTimeout(() => {
    shareTimer = null;
    render();
  }, waitFor);
}

function screenShares(people, labels) {
  const found = [];
  const local = media ? media.screenShareTracks.video : null;

  if (local) {
    found.push({
      key: "local-share",
      identity: room.localParticipant.identity,
      label: labels.get(room.localParticipant.identity),
      owner: "You",
      track: local,
      local: true,
    });
  }

  for (const participant of people) {
    if (participant === room.localParticipant) continue;
    for (const publication of participant.videoTrackPublications.values()) {
      if (publication.source !== lk.Track.Source.ScreenShare) continue;
      if (!publication.track) continue;
      found.push({
        key: publication.trackSid,
        identity: participant.identity,
        label: labels.get(participant.identity),
        owner: labels.get(participant.identity),
        track: publication.track,
        local: false,
      });
    }
  }

  return found;
}

/**
 * A share that ends under a viewer who is watching it fullscreen leaves them
 * looking at a dead rectangle, so the fullscreen view is closed and the reason
 * is said. When somebody else is still sharing there is somewhere to go instead
 * of out, and going there is named too.
 */
function announceShareChange(step) {
  if (showBench) return;

  if (step.outcome === "switched") {
    showNotice(`${step.gone} stopped sharing, so you are now watching ${shareName(step.entry)}.`);
    return;
  }
  if (step.outcome === "none" && step.gone && fullscreen !== "off") {
    leaveFullscreen();
    showNotice(`${step.gone} stopped sharing, so the fullscreen view closed.`);
    return;
  }
  // A room that ended took the picture with it, and `#room-error` is already
  // saying so. Naming the person whose screen it was would be blaming them for
  // the one thing here that was nobody's doing.
  if (step.outcome === "ended" && fullscreen !== "off") leaveFullscreen();
}

/**
 * Fullscreen hides the roster and the tiles, which were the only two places the
 * page ever showed who was talking, so the stage carries its own line.
 */
function paintStageSpeaking(people, labels, described) {
  const talking = people
    .filter((participant) => {
      // The same description the roster is painted from, muted people already
      // taken out of it. Reading the meter straight from here named a person
      // who had muted mid-word for as long as the hold lasted, on the one view
      // that hides the roster saying otherwise.
      const entry = described.get(participant.identity);
      return Boolean(entry && entry.speaking);
    })
    .map((participant) =>
      participant === room.localParticipant
        ? "you"
        : labels.get(participant.identity) || displayName(participant),
    );

  el.shareSpeaking.textContent =
    talking.length === 0 ? "Nobody is speaking." : `Speaking: ${talking.join(", ")}.`;
}

function shareOwner(entry) {
  return entry.local ? "You" : entry.label;
}

function shareName(entry) {
  return entry.local ? "your own screen" : entry.label;
}

function paintShare(entry, waiting) {
  stageShare = entry || null;

  if (!entry) {
    hideShareStats();
    if (sharedTrack && sharedElement) sharedTrack.detach(sharedElement);
    if (sharedElement) sharedElement.remove();
    sharedTrack = null;
    sharedElement = null;
    el.shareFrame.replaceChildren();

    if (waiting) {
      el.share.hidden = false;
      el.shareWho.textContent = "Waiting for the shared screen.";
      return;
    }

    rescueFocus(el.share);
    el.share.hidden = true;
    el.shareWho.textContent = "";
    return;
  }

  el.share.hidden = false;

  if (sharedTrack !== entry.track) {
    if (sharedTrack && sharedElement) sharedTrack.detach(sharedElement);
    if (sharedElement) sharedElement.remove();
    sharedElement = entry.track.attach();
    sharedElement.autoplay = true;
    sharedElement.playsInline = true;
    sharedElement.muted = true;
    sharedTrack = entry.track;
    el.shareFrame.replaceChildren(sharedElement);
  }

  el.shareWho.textContent = entry.local
    ? "You are sharing your screen. Everyone in the room sees this."
    : `${entry.label} is sharing a screen.`;

  paintShareStats();
}

/**
 * `[hidden]` is `display: none`, and the focused element going away hands focus
 * back to the body, so the next Tab restarts at the top of the page -- from
 * exactly the button a keyboard user pressed to get here.
 */
function rescueFocus(container) {
  if (!container.contains(document.activeElement)) return;
  for (const control of [el.mute, el.shareScreen, el.leave]) {
    if (control && !control.disabled) {
      control.focus();
      return;
    }
  }
}

function paintShareTabs(shares) {
  const signature = shares.map((entry) => `${entry.key} ${entry.label}`).join("");
  const same = signature === shareSignature;
  shareSignature = signature;

  el.shareTabs.hidden = shares.length < 2;
  if (shares.length < 2) {
    if (!same) el.shareTabs.replaceChildren();
    return;
  }

  if (!same) {
    const standing =
      document.activeElement && el.shareTabs.contains(document.activeElement)
        ? document.activeElement.dataset.key
        : null;

    el.shareTabs.replaceChildren(
      ...shares.map((entry) => {
        const button = document.createElement("button");
        button.type = "button";
        button.className = "share-tab";
        button.dataset.key = entry.key;
        button.textContent = entry.local ? `${entry.label} (you)` : entry.label;
        button.addEventListener("click", () => {
          pinnedShare = {
            key: entry.key,
            identity: entry.identity || null,
            label: shareOwner(entry),
            since: null,
          };
          render();
        });
        return button;
      }),
    );

    if (standing) {
      const back = [...el.shareTabs.children].find((button) => button.dataset.key === standing);
      if (back) back.focus();
    }
  }

  const held = pinnedShare ? pinnedShare.key : null;
  for (const button of el.shareTabs.children) {
    button.setAttribute("aria-pressed", String(button.dataset.key === held));
  }
}

/**
 * Pressing a tile, from the picture of a person or from the control on the
 * shared screen. Nothing else in the room moves the choice: it is the viewer's,
 * and `stagePlan` lets go of it only when what they were looking at has gone.
 */
function onFocus(target) {
  stageFocus = nextFocus(stageFocus, target);
  render();
}

/**
 * Where the two halves of the stage sit. One node moves and no more -- the tile
 * in the middle, out of the strip and back -- because `#share` carries a playing
 * video, and a video element taken out of the document is a video the browser
 * may pause. Which of `#share` and `#cards` is the strip is left to the
 * stylesheet, which can place a child of the stage in either column without
 * anything being moved to get it there.
 */
function layoutStage(plan) {
  el.stage.dataset.layout = plan.mode;
  el.stage.dataset.main = plan.main ? plan.main.kind : "none";
}

/**
 * The shared screen's own way into the middle, and out again. It is the tile
 * that cannot answer a plain click: a double click on the picture is already
 * fullscreen, and a click bound beside it would fire twice on the way there.
 */
function paintFocusControl() {
  const held = stageFocus !== null && stageFocus.kind === "share";

  el.shareFocusLabel.textContent = held ? "Exit focus" : "Focus";
  el.shareFocus.setAttribute("aria-pressed", String(held));
  el.shareFocus.setAttribute(
    "aria-label",
    held ? "Exit focus and show everybody again" : "Focus the shared screen",
  );
  el.shareFocus.title = held
    ? "Everybody goes back to an even grid under the shared screen."
    : "The shared screen fills the stage and everybody else moves to a strip at the side.";
  icon(el.shareFocus, held ? "#icon-grid" : "#icon-middle");
}

/**
 * Fullscreen is asked for on the whole room, not on the video: everything a
 * person reaches for once the picture fills the screen -- mute, the microphone
 * chooser, the tab of the other person's share, leave -- is inside `#room`, and
 * a video element taken fullscreen on its own takes none of it along.
 */
function fullscreenSupport() {
  return {
    request: typeof el.room.requestFullscreen === "function",
    enabled:
      typeof document.fullscreenEnabled === "boolean" ? document.fullscreenEnabled : undefined,
  };
}

function onToggleFullscreen() {
  // The picture answers a double click, so people double-click the button as
  // well. Bound to `click`, that would be in and straight back out again.
  const now = performance.now();
  if (now - fullscreenToggledAt < TOGGLE_GUARD_MS) return;
  fullscreenToggledAt = now;

  if (fullscreen !== "off") {
    leaveFullscreen();
    el.shareFullscreen.focus();
    return;
  }
  if (!stageShare) return;
  enterFullscreen();
}

async function enterFullscreen() {
  const plan = fullscreenPlan(fullscreenSupport());

  if (plan.mode === "native") {
    let taken;
    try {
      await el.room.requestFullscreen();
      taken = afterRequest(document.fullscreenElement === el.room);
    } catch (error) {
      console.warn("the browser refused fullscreen", error);
      taken = afterRefusal(error);
    }

    if (taken.mode === "native") {
      applyFullscreen("native");
      return;
    }
    fillTheWindow(taken.reason);
    return;
  }

  fillTheWindow(plan.reason);
}

function fillTheWindow(reason) {
  applyFullscreen("fallback");
  showNotice(
    `The shared screen now fills the window, because ${reason}. Press Escape, or the same ` +
      "button, to go back to the room.",
  );
}

function leaveFullscreen() {
  const wasNative = fullscreen === "native";
  applyFullscreen("off");

  if (wasNative && document.fullscreenElement === el.room && document.exitFullscreen) {
    document.exitFullscreen().catch((error) => {
      console.warn("the browser would not leave fullscreen", error);
    });
  }
}

function applyFullscreen(mode) {
  const was = fullscreen;
  fullscreen = mode;
  el.room.dataset.fullscreen = mode;
  paintFullscreenControl();

  // The tiles behind the share are attached and detached by `render`, which is
  // not on this stack: leaving fullscreen is one of the things `render` does.
  if ((was === "off") !== (mode === "off")) queueMicrotask(() => render());
}

// The browser owns Escape while it owns the screen, so this is how the page
// hears about a fullscreen it did not leave itself.
function onFullscreenChange() {
  if (document.fullscreenElement === el.room) {
    applyFullscreen("native");
    return;
  }
  if (fullscreen === "native") applyFullscreen("off");
}

function paintFullscreenControl() {
  const active = fullscreen !== "off";
  // A window with the browser's own chrome still around it is not fullscreen,
  // and a control that says it is would be the last thing on screen still
  // claiming it once the notice has cleared.
  const drawn = fullscreen === "fallback";

  el.shareFullscreenLabel.textContent = active
    ? drawn
      ? "Exit full window"
      : "Exit fullscreen"
    : "Fullscreen";
  el.shareFullscreen.setAttribute("aria-pressed", String(active));
  el.shareFullscreen.setAttribute(
    "aria-label",
    active
      ? drawn
        ? "Leave the full-window view and go back to the room"
        : "Leave fullscreen and go back to the room"
      : "Show the shared screen fullscreen",
  );
  el.shareFullscreen.title = active
    ? fullscreen === "fallback"
      ? "This browser would not hand the page fullscreen, so the shared screen fills the " +
        "window instead. Escape leaves it, and the controls below stay where they are."
      : "Escape leaves fullscreen. The controls below stay where they are."
    : "The shared screen fills the screen, controls and all. Double-clicking it does the same.";
  icon(el.shareFullscreen, active ? "#icon-collapse" : "#icon-expand");
  el.shareLive.hidden = !active;

  // Fullscreen is the shared screen's whole claim on the stage, so the control
  // that asks for the middle of it has nothing left to ask for. The choice
  // itself is kept: this hides the button, not the state behind it.
  if (active && document.activeElement === el.shareFocus) el.shareFullscreen.focus();
  el.shareFocus.hidden = active;
}

/**
 * What actually arrived, in the corner: the frame size and the bitrate this
 * browser reports for the track on the stage, and nothing at all when it
 * reports neither. Nothing here is inferred from what was published.
 */
function paintShareStats() {
  if (!stageShare) {
    hideShareStats();
    return;
  }

  const reading = shareReading(stageShare);
  const badge = shareBadge(reading, formatBitrate);

  const measured = { key: stageShare.key, resolution: badge.resolution, area: frameArea(reading) };
  // Two readings a second apart have to agree before a size is called a change,
  // so an encoder drawing breath is not reported as somebody changing window.
  if (shareSeen && shareSeen.key === measured.key && shareSeen.resolution === measured.resolution) {
    const change = sizeChange(shareSize, measured);
    if (change) showNotice(change.text);
    shareSize = measured;
  }
  shareSeen = measured;

  if (badge.text === null) {
    hideShareStats();
    return;
  }

  const measures = stageShare.local
    ? "What this browser reports it is sending for the shared screen."
    : "What this browser reports arriving for the shared screen.";

  el.shareStats.hidden = false;
  el.shareStats.textContent = badge.text;
  el.shareStats.title = badge.note ? `${measures} ${capitalise(badge.note)}.` : measures;
}

function hideShareStats() {
  shareSize = { key: null, resolution: null, area: null };
  shareSeen = null;
  el.shareStats.hidden = true;
  el.shareStats.textContent = "";
  el.shareStats.removeAttribute("title");
}

function shareReading(entry) {
  if (!lastReading || !entry) return null;
  if (!entry.local) return lastReading.tracks.get(entry.key) || null;

  const source = entry.track ? entry.track.mediaStreamTrack : null;
  if (!source || !lastReading.localTracks) return null;
  return lastReading.localTracks.get(source.id) || null;
}

function capitalise(text) {
  return text.charAt(0).toUpperCase() + text.slice(1);
}

function cardEntries(people, labels, described) {
  return people.map((participant) => {
    const isLocal = participant === room.localParticipant;
    return {
      key: participant.identity,
      label: labels.get(participant.identity) || displayName(participant),
      local: isLocal,
      track: cameraFor(participant, isLocal),
      ...described.get(participant.identity),
    };
  });
}

function cameraFor(participant, isLocal) {
  if (isLocal) return cameraTrack;
  for (const publication of participant.videoTrackPublications.values()) {
    if (publication.source === lk.Track.Source.ScreenShare) continue;
    if (publication.track) return publication.track;
  }
  return null;
}

function videoTiles(people, labels) {
  const wanted = [];
  const localShare = media ? media.screenShareTracks.video : null;

  // One local tile, whatever is going out. bench.js reads the rungs off every
  // outbound video encoding at once, so a second tile would repeat the first
  // one's numbers under a label claiming they belonged to one of the two.
  if (cameraTrack || localShare) {
    const sending =
      cameraTrack && localShare ? "camera and screen" : cameraTrack ? "camera" : "screen";
    wanted.push({
      key: "local",
      label: `${labels.get(room.localParticipant.identity)} - you, ${sending}`,
      track: cameraTrack || localShare,
      local: true,
    });
  }

  for (const participant of people) {
    if (participant === room.localParticipant) continue;
    for (const publication of participant.videoTrackPublications.values()) {
      if (!publication.track) continue;
      const label = labels.get(participant.identity);
      wanted.push({
        key: publication.trackSid,
        label:
          publication.source === lk.Track.Source.ScreenShare ? `${label} - screen` : label,
        track: publication.track,
        local: false,
      });
    }
  }

  return wanted;
}

function labelsFor(people) {
  const seen = new Map();
  for (const participant of people) {
    const name = displayName(participant);
    const counted = seen.get(name);
    seen.set(name, counted === undefined ? 1 : counted + 1);
  }

  const labels = new Map();
  for (const participant of people) {
    const name = displayName(participant);
    labels.set(
      participant.identity,
      seen.get(name) > 1 ? `${name} (${shortIdentity(participant.identity)})` : name,
    );
  }
  return labels;
}

function displayName(participant) {
  return (participant.name || "").trim() || participant.identity;
}

function shortIdentity(identity) {
  const tail = String(identity || "").slice(-4);
  return tail || String(identity);
}

function microphoneOf(participant) {
  const publications = [...participant.audioTrackPublications.values()];
  return (
    publications.find((publication) => publication.source === lk.Track.Source.Microphone) ||
    publications.find(
      (publication) => publication.source !== lk.Track.Source.ScreenShareAudio,
    ) ||
    null
  );
}

/**
 * The one place that decides what a participant's audio is doing. Anything the
 * page cannot measure comes back as "unknown" carrying the reason, because the
 * alternative -- printing "listening" for a track nobody is receiving -- is the
 * page telling a comfortable lie.
 */
function describeParticipant(participant, isLocal) {
  const identity = participant.identity;
  const status = media ? media.state().microphone : null;

  if (isLocal) {
    if (!status || !status.open) {
      return unknownState("no microphone", "this page has not opened a microphone");
    }
    if (status.muted) return mutedState();
    if (!status.published) return unknownState("not published", NOT_PUBLISHED);
    return level(identity);
  }

  const publication = microphoneOf(participant);
  if (!publication) {
    return unknownState("no microphone", "this participant is not publishing a microphone");
  }
  if (publication.isMuted) return mutedState();
  if (!publication.track) return unknownState("not subscribed", NOT_SUBSCRIBED);
  if (publication.track.streamState === "paused") {
    return unknownState(
      "paused",
      "the server stopped forwarding this track, so there is nothing arriving to measure",
    );
  }

  return level(identity);
}

function mutedState() {
  return { state: "muted", text: "muted", reason: null, detail: null, muted: true, speaking: false };
}

/**
 * What the meter says, or why there is nothing to say. A reading the meter has
 * stopped refreshing is withdrawn here rather than repeated: `voice.js` decides
 * when it has gone stale, and a stale reading comes back as unknown carrying
 * the sentence that explains it, the same as a track nobody is subscribed to.
 */
function level(identity) {
  const entry = meters.get(identity);
  if (!entry) return unknownState("level unknown", whyNoMeter(identity));

  const reading = entry.gate.describe(performance.now());
  if (reading.state === "unknown") return unknownState("level unknown", reading.reason);

  return {
    state: reading.speaking ? "speaking" : "listening",
    text: reading.speaking ? "speaking" : "listening",
    reason: null,
    detail: reading.detail,
    muted: false,
    speaking: reading.speaking,
  };
}

/**
 * Why there is no meter on somebody, in the order the reasons actually apply.
 * The order is the whole of it: the page used to answer "this browser gave the
 * page no audio context" for everybody without an entry, which during the
 * second or two the worklet module takes to install was said about a browser
 * that had just handed over a perfectly good context.
 */
function whyNoMeter(identity) {
  if (attaching.has(identity)) return METER_ATTACHING;
  const failure = meterFailures.get(identity);
  if (failure) return failure;
  if (meterFault) return meterFault;
  if (!audioContext) return NO_AUDIO_CONTEXT;
  return METER_NOT_YET;
}

function unknownState(text, reason) {
  return { state: "unknown", text, reason, detail: null, muted: false, speaking: false };
}

function renderParticipant(participant, isLocal, label, described) {
  const row = document.createElement("li");
  row.dataset.speaking = String(described.speaking);
  row.dataset.muted = String(described.muted);
  row.dataset.state = described.state;

  const dot = document.createElement("span");
  dot.className = "level";

  const who = document.createElement("span");
  who.className = "who";
  who.textContent = label || displayName(participant);
  if (isLocal) {
    const you = document.createElement("span");
    you.className = "you";
    you.textContent = "you";
    who.appendChild(you);
  }

  const state = document.createElement("span");
  state.className = "state";
  state.textContent = described.text;
  // Why it is unknown when it is, and the two numbers behind it when it is not:
  // a level on its own cannot be argued with, and neither can a row that only
  // says "listening".
  const note = described.reason || described.detail;
  if (note) state.title = note;

  row.append(dot, who, state);

  const publication = isLocal ? null : microphoneOf(participant);
  if (publication) {
    const heard = document.createElement("span");
    heard.className = "heard";
    heard.textContent = "...";
    row.appendChild(heard);
    arriving.set(participant.identity, { node: heard, sid: publication.trackSid });
  }

  return row;
}

/**
 * The tiles, and which of them is in the middle. Moving a node takes it out of
 * the document for the length of one call, and an element that leaves the
 * document takes the keyboard's focus with it -- so somebody who pressed a tile
 * would find their next Tab starting again at the top of the page. The one that
 * was pressed is put back on it afterwards.
 */
function syncCards(entries, plan) {
  const keys = new Set(entries.map((entry) => entry.key));
  const active = document.activeElement;
  const standing =
    active && (el.cards.contains(active) || el.spotlight.contains(active)) ? active : null;

  for (const [key, card] of [...cards]) {
    if (keys.has(key)) continue;
    rescueFocus(card.node);
    detachCard(card);
    card.node.remove();
    cards.delete(key);
  }

  const middle = plan.main && plan.main.kind === "person" ? plan.main.key : null;

  for (const entry of entries) {
    let card = cards.get(entry.key);
    if (!card) {
      card = buildCard(entry.key);
      cards.set(entry.key, card);
      el.cards.appendChild(card.node);
    }
    paintCard(card, entry, entry.key === middle);
  }

  // The spotlight claims its tile first, so the strip settling below never sees
  // the node it is about to give up as one of its own children -- and it is
  // shown before it is filled, because `[hidden]` is `display: none` and an
  // element moved into one of those is an element the keyboard has just left.
  el.spotlight.hidden = !middle;
  if (middle) el.spotlight.replaceChildren(cards.get(middle).node);
  else el.spotlight.replaceChildren();

  const ordered = entries
    .filter((entry) => entry.key !== middle)
    .map((entry) => cards.get(entry.key).node);
  const settled =
    ordered.length === el.cards.children.length &&
    ordered.every((node, index) => el.cards.children[index] === node);
  if (!settled) el.cards.replaceChildren(...ordered);

  el.cards.hidden = ordered.length === 0;
  if (standing && standing.isConnected && document.activeElement !== standing) standing.focus();
}

/**
 * The picture is the control, and the name and state underneath are not: a
 * button's own text is what a screen reader reads instead of its contents, so
 * wrapping the whole tile would have swapped "Alice, speaking" for the name of
 * the action. Outside the button they are still read, still selectable, and
 * still carry the sentence on hover that says why a state is unknown.
 */
function buildCard(key) {
  const node = document.createElement("li");
  node.className = "card-tile";
  node.dataset.video = "false";

  const press = document.createElement("button");
  press.type = "button";
  press.className = "card-press";
  press.setAttribute("aria-pressed", "false");
  press.addEventListener("click", () => onFocus({ kind: "person", key }));

  const frame = document.createElement("span");
  frame.className = "card-frame";

  const avatar = document.createElement("span");
  avatar.className = "avatar";
  frame.appendChild(avatar);
  press.appendChild(frame);

  const foot = document.createElement("div");
  foot.className = "card-foot";

  const who = document.createElement("span");
  who.className = "who";

  const state = document.createElement("span");
  state.className = "state";

  foot.append(who, state);
  node.append(press, foot);

  return { node, press, frame, avatar, who, state, track: null, video: null };
}

function paintCard(card, entry, middle) {
  card.node.dataset.speaking = String(entry.speaking);
  card.node.dataset.muted = String(entry.muted);
  card.node.dataset.state = entry.state;
  card.node.dataset.middle = String(middle);

  const who = entry.local ? "yourself" : entry.label;
  card.press.setAttribute("aria-pressed", String(middle));
  card.press.setAttribute("aria-label", middle ? `Exit focus on ${who}` : `Focus ${who}`);
  card.press.title = middle
    ? "Everybody goes back to an even grid."
    : "Fills the stage, with everybody else in a strip at the side.";

  card.who.textContent = entry.local ? `${entry.label} - you` : entry.label;
  card.state.textContent = entry.text;
  const note = entry.reason || entry.detail;
  if (note) {
    card.state.title = note;
  } else {
    card.state.removeAttribute("title");
  }

  card.avatar.textContent = initials(entry.label);

  if (card.track === entry.track) return;

  detachCard(card);
  card.track = entry.track || null;
  if (!card.track) return;

  card.video = card.track.attach();
  card.video.autoplay = true;
  card.video.playsInline = true;
  card.video.muted = true;
  card.frame.appendChild(card.video);
  card.node.dataset.video = "true";
}

function detachCard(card) {
  if (card.track && card.video) card.track.detach(card.video);
  if (card.video) card.video.remove();
  card.track = null;
  card.video = null;
  card.node.dataset.video = "false";
}

function clearCards() {
  for (const [key, card] of [...cards]) {
    rescueFocus(card.node);
    detachCard(card);
    card.node.remove();
    cards.delete(key);
  }
  el.spotlight.replaceChildren();
  el.spotlight.hidden = true;
  el.cards.hidden = true;
}

function initials(label) {
  const words = String(label || "")
    .trim()
    .split(/\s+/)
    .filter(Boolean);
  if (words.length === 0) return "?";
  const first = [...words[0]][0] || "";
  const second = words.length > 1 ? [...words[words.length - 1]][0] || "" : "";
  return (first + second).toUpperCase();
}

function paintControls() {
  const state = media ? media.state() : null;
  const open = Boolean(state && state.microphone.open);
  const muted = Boolean(state && state.microphone.muted);

  // Without a microphone the button stops being a toggle and becomes the way to
  // one. Leaving it disabled is what the page used to do, and it left the
  // person who had just allowed the device in the padlock with nothing to press
  // and no way back short of leaving the room and joining it again.
  const listening = !open && canOpenMicrophone(state);

  el.mute.disabled = (!open && !listening) || pending.mute || deafened;
  // `aria-pressed` is about the microphone rather than about the label: a
  // browser that has none is as far from talking as a muted one, and a button
  // reporting itself unpressed there would tell a screen reader the room can
  // hear this person.
  el.mute.setAttribute("aria-pressed", String(muted || !open));
  el.mute.dataset.on = String(open && !muted);
  el.mute.dataset.alert = String(muted);
  el.muteLabel.textContent = !open ? "Turn on mic" : muted ? "Unmute" : "Mute";
  el.mute.setAttribute(
    "aria-label",
    !open
      ? "Turn on your microphone, asking this browser for it again"
      : muted
        ? "Unmute your microphone"
        : "Mute your microphone",
  );
  icon(el.mute, open && !muted ? "#icon-mic" : "#icon-mic-off");
  el.mute.title = deafened
    ? "The microphone follows the speaker: it comes back when you hear the room again."
    : open
      ? "Muting stops the packets leaving; every page in the room shows it at once."
      : listening
        ? `${micRefusal ? `${micRefusal} ` : ""}Pressing this asks the browser again. The token ` +
          "this page joined with already grants a microphone, so nothing has to be rejoined."
        : "This browser will not hand a microphone to this page at all.";

  el.deafen.disabled = !room || pending.mute;
  el.deafen.setAttribute("aria-pressed", String(deafened));
  el.deafen.dataset.on = String(Boolean(room) && !deafened);
  el.deafen.dataset.alert = String(deafened);
  el.deafenLabel.textContent = deafened ? "Unmute speaker" : "Mute speaker";
  el.deafen.setAttribute(
    "aria-label",
    deafened ? "Hear the room again, and speak again" : "Stop hearing the room, and stop speaking",
  );
  icon(el.deafen, deafened ? "#icon-speaker-off" : "#icon-speaker");
  el.deafen.title =
    "This silences your own speakers and mutes you with them. Nothing stops arriving, and the " +
    "room is told only that you are muted.";

  paintNoise(state);

  const sharing = Boolean(state && state.screenShare.active);
  const canShare = Boolean(state && state.support.screenShare);
  el.shareScreen.disabled = !room || !canShare || pending.share;
  el.shareScreen.setAttribute("aria-pressed", String(sharing));
  el.shareScreen.dataset.on = String(sharing);
  el.shareLabel.textContent = sharing ? "Stop sharing" : "Share screen";
  el.shareScreen.setAttribute(
    "aria-label",
    sharing ? "Stop sharing your screen" : "Share your screen",
  );
  icon(el.shareScreen, sharing ? "#icon-screen-off" : "#icon-screen");
  el.shareScreen.title = canShare
    ? "The browser asks which window or screen; this page never sees the choice, only the result."
    : "This browser does not offer screen capture to a web page.";

  el.camera.disabled = !room || pending.camera;
  el.camera.setAttribute("aria-pressed", String(Boolean(cameraTrack)));
  el.camera.dataset.on = String(Boolean(cameraTrack));
  el.cameraLabel.textContent = pending.camera
    ? "Working"
    : cameraTrack
      ? "Stop camera"
      : "Start camera";
  el.camera.setAttribute("aria-label", cameraTrack ? "Stop your camera" : "Start your camera");
  icon(el.camera, cameraTrack ? "#icon-camera-off" : "#icon-camera");

  el.audioSettings.disabled = !media;
  el.micSelect.disabled = !state || !state.support.deviceList || state.devices.list.length === 0;

  paintSwitches(state);
  paintNoiseChoices(state);
  paintDenoise(state ? state.noise : null);
}

/**
 * Whether asking for a microphone could get one. A browser with no capture at
 * all, or a page it will not trust with one, has already been told so by the
 * lobby -- which refuses to load over anything but a secure context -- and a
 * button offering an attempt that can only ever fail would be this page arguing
 * with what it said on the way in.
 */
function canOpenMicrophone(state) {
  return Boolean(room && state && state.support.microphone && state.support.secureContext);
}

/**
 * One button, one engine name. What the browser reports about its own two flags
 * is a separate line in the menu, because it is a different claim: this button
 * says what this page put in the path, not what the operating system did behind
 * it.
 */
function paintNoise(state) {
  const engine = state ? state.noise.engine : DEFAULT_NOISE_ENGINE;
  const row = NOISE_ROWS.find((entry) => entry.engine === engine);
  const held = Boolean(state && state.microphone.pendingWhileMuted);
  const pendingRow = state && state.noise.pending
    ? NOISE_ROWS.find((entry) => entry.engine === state.noise.pending)
    : null;

  el.noise.disabled = !state || !state.microphone.open || pending.noise;
  el.noise.setAttribute("aria-pressed", String(engine !== "off"));
  el.noise.dataset.on = String(engine !== "off");

  if (!state || !state.microphone.open) {
    sub("no microphone", true, "Nothing is being captured yet. Press Turn on mic first.");
    return;
  }
  if (pending.noise) {
    sub("switching", true, "The capture is being restarted onto the chosen suppressor.");
    return;
  }
  if (held && pendingRow) {
    sub(
      `${pendingRow.short} on unmute`,
      true,
      "Restarting the capture while you are muted would re-open the device and light the " +
        "browser's recording indicator, so the change is held until you unmute.",
    );
    return;
  }

  if (engine === "mediacore") {
    const claim = state.effective ? state.effective.noiseSuppression : null;
    sub(
      "mediacore",
      false,
      claim === true
        ? "RNNoise is in the capture path. The browser was asked to stop suppressing and reports " +
            "that it is still doing so, which is two suppressors in series -- see the menu."
        : "RNNoise is in the capture path, running in this page before the audio is encoded. " +
            "The browser's own suppression was turned off with it.",
    );
    return;
  }

  const claimed = state.effective ? state.effective.noiseSuppression : null;
  if (claimed === null) {
    sub(
      row ? row.short : engine,
      true,
      "This browser does not report whether it applied noise suppression, so this is what was " +
        "asked for rather than what happened.",
    );
    return;
  }
  if (claimed !== (engine === "browser")) {
    sub(
      claimed ? "browser: on" : "browser: off",
      true,
      "The browser reports something other than what was asked for, and what it reports is what " +
        "the room hears.",
    );
    return;
  }

  sub(
    row ? row.short : engine,
    false,
    "The browser confirms this is what it applied, before the audio was encoded.",
  );
}

function sub(text, missing, title) {
  el.noiseSub.textContent = text;
  el.noiseSub.dataset.missing = String(missing);
  el.noise.title = title;
  el.noise.setAttribute("aria-label", `Noise suppression: ${text}. Press to change`);
}

function buildNoiseChoices() {
  el.noiseChoices.replaceChildren(
    ...NOISE_ROWS.map((row) => {
      const item = document.createElement("li");

      const button = document.createElement("button");
      button.type = "button";
      button.className = "choice";
      button.dataset.engine = row.engine;
      button.setAttribute("role", "radio");
      button.setAttribute("aria-checked", "false");
      button.textContent = row.label;
      button.addEventListener("click", () => applyNoiseEngine(row.engine));

      const note = document.createElement("p");
      note.className = "switch-note";
      note.textContent = row.note;

      item.append(button, note);
      noiseChoices.set(row.engine, { button, note });
      return item;
    }),
  );
}

function paintNoiseChoices(state) {
  const engine = state ? state.noise.engine : DEFAULT_NOISE_ENGINE;
  const open = Boolean(state && state.microphone.open);
  const available = state ? state.noise.available : null;

  for (const row of NOISE_ROWS) {
    const entry = noiseChoices.get(row.engine);
    const ours = row.engine === "mediacore";
    const blocked = ours && available !== true;

    entry.button.setAttribute("aria-checked", String(engine === row.engine));
    entry.button.disabled = !open || pending.noise || blocked;
    entry.button.dataset.blocked = String(blocked);

    // The reason goes under the row, not into a tooltip: a disabled option
    // whose explanation needs a mouse explains nothing on a touchscreen.
    if (ours && available === null) {
      entry.note.textContent = `${row.note} This page has not finished checking whether it can run here.`;
    } else if (blocked && state && state.noise.reason) {
      entry.note.textContent = `${row.note} Not on offer here: ${state.noise.reason}.`;
    } else {
      entry.note.textContent = row.note;
    }
    entry.button.removeAttribute("title");
  }

  const lines = [];
  if (state && state.noise.fallback.reason) {
    lines.push(
      state.noise.fallback.engaged
        ? `It was taken out of this call because ${state.noise.fallback.reason}.`
        : `Earlier in this call it was taken out because ${state.noise.fallback.reason}.`,
    );
  }
  const note = lines.join(" ");
  el.noiseNote.textContent = note;
  el.noiseNote.hidden = note === "";

  paintNoiseEffective(state);
}

/**
 * What the browser says about its own two flags, kept apart from the engine
 * name above it. A browser can report `noiseSuppression: false` while the
 * operating system carries on suppressing, so this line is labelled as a claim
 * and not as a measurement.
 */
function paintNoiseEffective(state) {
  if (!state || !state.microphone.open) {
    say(el.noiseEffective, "no microphone is open, so the browser reports nothing", true);
    return;
  }
  if (state.microphone.pendingWhileMuted) {
    say(el.noiseEffective, "held until you unmute", true);
    return;
  }

  const reported = state.effective || {};
  const parts = [];
  for (const key of ["noiseSuppression", "voiceIsolation"]) {
    const value = reported[key];
    parts.push(`${key} ${value === null || value === undefined ? "not reported" : value ? "on" : "off"}`);
  }
  const missing = ["noiseSuppression", "voiceIsolation"].every(
    (key) => reported[key] === null || reported[key] === undefined,
  );
  say(el.noiseEffective, `the browser says: ${parts.join(", ")}`, missing);
}

function buildDenoiseReadout() {
  el.denoise.replaceChildren(
    ...DENOISE_ROWS.map((row) => {
      const wrap = document.createElement("div");

      const term = document.createElement("dt");
      term.textContent = row.label;

      const value = document.createElement("dd");
      value.textContent = "—";

      wrap.append(term, value);
      denoiseCells.set(row.key, value);
      return wrap;
    }),
  );
}

/**
 * Everything here is measured on the render thread that runs the node, over the
 * window the reading covers. Nothing is a nominal figure from a data sheet, and
 * a value the browser would not give up stays an em dash carrying the reason.
 */
function paintDenoise(noise) {
  const state = noise || (media ? media.state().noise : null);
  const cell = (key) => denoiseCells.get(key);
  if (!cell("engine")) return;

  const engineRow = state ? NOISE_ROWS.find((row) => row.engine === state.engine) : null;
  write(
    cell("engine"),
    engineRow ? engineRow.label : state ? state.engine : null,
    "no capture module yet",
  );

  const fallback = state ? state.fallback : null;
  write(
    cell("fallback"),
    fallback
      ? fallback.engaged
        ? "engaged"
        : fallback.reason
          ? "engaged earlier in this call"
          : "never"
      : null,
    "no capture module yet",
  );
  if (fallback && fallback.reason) {
    cell("fallback").title = fallback.reason;
  }

  const metrics = state ? state.metrics : null;
  const measurable = Boolean(state && state.attached);
  const idle = measurable ? "waiting for the first measurement from the render thread" : DENOISE_OFF;

  if (!metrics) {
    for (const key of [
      "levelIn",
      "levelOut",
      "change",
      "delay",
      "frame",
      "load",
      "realtime",
      "worst",
    ]) {
      write(cell(key), null, idle);
    }
    return;
  }

  write(cell("levelIn"), formatDecibels(metrics.input.rms), "no audio reached the node in this window");
  write(cell("levelOut"), formatDecibels(metrics.output.rms), "no audio left the node in this window");
  write(cell("change"), formatChange(metrics.reduction), "there was no level either side to compare");

  // The one number that decides whether a call feels responsive, and the one
  // figure here that cannot be read off the graph: the vendored worklet buffers
  // internally and does not say by how much. It is found by lining the node's
  // output up against its own input, so it needs signal on both sides.
  write(
    cell("delay"),
    metrics.delay ? formatMillis(metrics.delay.ms) : null,
    "there was not enough signal either side of the node to line its output up against its input",
  );
  if (metrics.delay) {
    cell("delay").title =
      `${metrics.delay.samples} samples at ${metrics.sampleRate} Hz, found by correlating the ` +
      `node's output against its input (peak ${metrics.delay.correlation.toFixed(2)}). This is ` +
      "the node's own buffering; what the browser adds between the capture track and the " +
      "published one is not in it.";
  }

  const costNote = metrics.costNote || "the render thread reported no cost for this window";
  const cost = metrics.cost;

  const estimated = (text) => (text === null ? null : metrics.coarseClock ? `~${text}` : text);
  write(cell("frame"), estimated(cost ? formatMillis(cost.frameMs) : null), costNote);

  // Against the wall clock, not against the audio clock. The audio clock is the
  // device's and runs at real time whatever the render thread is doing, so a
  // share measured against it cannot rise when the thread starts losing.
  write(
    cell("load"),
    estimated(cost && cost.load !== null ? formatPercent(cost.load) : null),
    cost
      ? "the first reading of a run has no wall-clock interval behind it to measure a share of"
      : costNote,
  );
  if (cost && metrics.coarseClock) {
    cell("frame").title = costNote;
    cell("load").title = costNote;
  }
  write(
    cell("worst"),
    cost ? formatMillis(cost.worstQuantumMs) : null,
    metrics.coarseClock
      ? `this browser's audio render thread has only a whole-millisecond clock, which cannot say ` +
          "what one 2.67 ms block cost"
      : costNote,
  );

  // The cost above is what the node took; this is whether the machine got it
  // done in time. It is measured on this thread, so it works in browsers whose
  // render thread has no clock to lend the two above.
  write(
    cell("realtime"),
    metrics.realtime ? formatPercent(metrics.realtime.behind) : null,
    "the first reading of a run has nothing to measure a rate against",
  );
  if (metrics.realtime) {
    cell("realtime").title =
      `${Math.round(metrics.realtime.wallMs)} ms of wall clock for ` +
      `${Math.round(metrics.realtime.audioMs)} ms of audio the node actually emitted. Above ` +
      "100 % the render thread is falling behind, and past 125 % for three seconds the node is " +
      "taken out.";
  }
}

function buildSwitches() {
  el.processing.replaceChildren(
    ...PROCESSING_ROWS.map((row) => {
      const item = document.createElement("li");

      const button = document.createElement("button");
      button.type = "button";
      button.className = "switch";
      button.dataset.key = row.key;
      button.setAttribute("role", "switch");
      button.setAttribute("aria-checked", "false");

      const track = document.createElement("span");
      track.className = "switch-track";
      track.setAttribute("aria-hidden", "true");

      const name = document.createElement("span");
      name.className = "switch-name";
      name.textContent = row.label;

      button.append(track, name);
      button.addEventListener("click", () => applyProcessing(row.key));

      const effective = document.createElement("p");
      effective.className = "switch-effective";

      const note = document.createElement("p");
      note.className = "switch-note";
      note.textContent = row.note;

      item.append(button, effective, note);
      switches.set(row.key, { button, effective });
      return item;
    }),
  );
}

function paintSwitches(state) {
  for (const row of PROCESSING_ROWS) {
    const entry = switches.get(row.key);
    const wanted = state ? Boolean(state.processing[row.key]) : false;
    entry.button.setAttribute("aria-checked", String(wanted));
    entry.button.disabled = !state || !state.microphone.open;

    const reported = state && state.effective ? state.effective[row.key] : null;

    if (!state || !state.microphone.open) {
      say(entry.effective, "no microphone is open, so the browser reports nothing", true);
    } else if (state.microphone.pendingWhileMuted) {
      say(entry.effective, "held until you unmute", true);
    } else if (reported === null) {
      say(entry.effective, "this browser does not report whether it applied this", true);
    } else {
      say(entry.effective, `the browser reports it is ${reported ? "on" : "off"}`, false);
    }
  }
}

function say(node, text, missing) {
  node.textContent = text;
  node.dataset.missing = String(missing);
}

function paintDevices() {
  if (!media) return;
  const { devices, microphone, support } = media.state();

  const options = [
    { value: SYSTEM_DEFAULT, text: "System default" },
    ...devices.list.map((device) => ({ value: device.deviceId, text: device.label })),
  ];
  const signature = options.map((option) => `${option.value} ${option.text}`).join("");

  if (signature !== deviceSignature) {
    deviceSignature = signature;
    el.micSelect.replaceChildren(
      ...options.map((option) => {
        const node = document.createElement("option");
        node.value = option.value;
        node.textContent = option.text;
        return node;
      }),
    );
  }

  el.micSelect.value = microphone.deviceId || SYSTEM_DEFAULT;

  const lines = [];
  if (devices.note) lines.push(devices.note);
  if (!support.deviceList) {
    lines.push(
      "This browser offers no device list, so the input can only be chosen in the system sound " +
        "settings.",
    );
  } else if (microphone.open) {
    const active = devices.list.find((device) => device.deviceId === microphone.activeDeviceId);
    lines.push(
      active && active.named
        ? `The browser reports it opened "${active.label}".`
        : "The browser does not report which device it opened.",
    );
  }

  const note = lines.join(" ");
  el.micNote.textContent = note;
  el.micNote.hidden = note === "";
}

function icon(button, href) {
  const use = button.querySelector("use");
  if (use) use.setAttribute("href", href);
}

function showNotice(text) {
  if (!text) return;
  el.noticeText.textContent = text;
  el.notice.hidden = false;
  if (noticeTimer) clearTimeout(noticeTimer);
  noticeTimer = setTimeout(clearNotice, NOTICE_MS);
}

function clearNotice() {
  if (noticeTimer) clearTimeout(noticeTimer);
  noticeTimer = null;
  el.notice.hidden = true;
  el.noticeText.textContent = "";
}

function cameraMessage(error) {
  switch (error && error.name) {
    case "NotAllowedError":
    case "PermissionDeniedError":
      return (
        "The camera was blocked. Open the padlock in the address bar, set Camera to Allow " +
        "for this site, then press Start camera again. The room carries on without it."
      );
    case "NotFoundError":
    case "DevicesNotFoundError":
      return "No camera was found. The room carries on without it.";
    case "NotReadableError":
    case "TrackStartError":
      return (
        "The camera is already in use by another application. Close it and press Start " +
        "camera again."
      );
    default:
      return `The camera could not be published: ${(error && error.message) || error}.`;
  }
}

function connectionMessage(error, url) {
  const detail = (error && error.message) || error;
  return (
    `Could not reach the media server at ${url} (${detail}). It may be down, or this ` +
    "network may not be able to reach it. Check the server and press Join again."
  );
}

function setState(kind, label) {
  el.roomState.dataset.state = kind;
  el.roomState.textContent = label;
  el.shareState.dataset.state = kind;
  el.shareState.textContent = label;
}

function busy(active, message) {
  el.join.disabled = active;
  el.name.disabled = active;
  el.joinRoom.disabled = active;
  el.withVideo.disabled = active;
  el.join.textContent = active ? "Joining..." : "Join";
  el.lobbyStatus.textContent = message;
}

function block(message) {
  fail(el.lobbyError, message);
  el.join.disabled = true;
  el.name.disabled = true;
  el.joinRoom.disabled = true;
  el.withVideo.disabled = true;
}

function fail(node, message) {
  node.textContent = message;
  node.hidden = false;
}

function hide(node) {
  node.hidden = true;
  node.textContent = "";
}
