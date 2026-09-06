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
  formatMillis,
  write,
} from "/readout.js";
import { createMedia } from "/media.js";

const SDK_URL = "/vendor/livekit-client-2.22.2.esm.js";
const SPEAKING_RMS = 0.02;
const SPEAKING_HOLD_MS = 500;
const METER_INTERVAL_MS = 150;
const BENCH_INTERVAL_MS = 1000;
const NOTICE_MS = 12000;
const PRESETS = { 180: "h180", 360: "h360", 720: "h720" };
const SYSTEM_DEFAULT = "";

const PROCESSING_ROWS = [
  {
    key: "echoCancellation",
    label: "Echo cancellation",
    note: "Subtracts what the speakers are playing from what the microphone hears.",
  },
  {
    key: "noiseSuppression",
    label: "Noise suppression",
    note: "Attenuates steady background noise: fans, traffic, hiss.",
  },
  {
    key: "autoGainControl",
    label: "Automatic gain",
    note: "Rides the level towards a target loudness between phrases.",
  },
  {
    key: "voiceIsolation",
    label: "Voice isolation",
    note:
      "A stronger separation of a voice from everything else. Chrome offers it and the client " +
      "asks for it by default; other browsers do not report it either way.",
  },
];

const NO_AUDIO_CONTEXT =
  "this browser gave the page no audio context, so nothing here can tell whether they are speaking";
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
  participants: document.getElementById("participants"),
  cards: document.getElementById("cards"),
  share: document.getElementById("share"),
  shareFrame: document.getElementById("share-frame"),
  shareWho: document.getElementById("share-who"),
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
  micSelect: document.getElementById("mic-select"),
  micNote: document.getElementById("mic-note"),
  processing: document.getElementById("processing"),
  mute: document.getElementById("mute"),
  muteLabel: document.getElementById("mute-label"),
  noise: document.getElementById("noise"),
  noiseSub: document.getElementById("noise-sub"),
  shareScreen: document.getElementById("share-screen"),
  shareLabel: document.getElementById("share-label"),
  camera: document.getElementById("camera"),
  cameraLabel: document.getElementById("camera-label"),
  benchToggle: document.getElementById("bench-toggle"),
  leave: document.getElementById("leave"),
  audio: document.getElementById("audio"),
  footUrl: document.getElementById("foot-url"),
};

const query = new URLSearchParams(location.search);
const requestedRoom = query.get("room");
const wantsSimulcast = query.get("simulcast") !== "0";

let lk = null;
let media = null;
let settings = {
  room: "demo",
  wsUrl: "",
  maxNameLength: 40,
  roomNamespaceSeparator: ".",
};
let room = null;
let cameraTrack = null;
let speaking = new Set();
const meters = new Map();
const arriving = new Map();
const cards = new Map();
const switches = new Map();
const pending = { mute: false, share: false, camera: false };
let meterTimer = null;
let audioContext = null;
let bench = null;
let benchTimer = null;
let lastReading = null;
let tiles = null;
let showBench = false;
let selectedShare = null;
let shareSignature = "";
let deviceSignature = "";
let sharedElement = null;
let sharedTrack = null;
let noticeTimer = null;

start();

async function start() {
  tiles = createTiles(el.tiles);
  el.withVideo.checked = query.get("video") === "1";
  showBench = query.get("bench") === "1";

  el.form.addEventListener("submit", onJoin);
  el.mute.addEventListener("click", onMute);
  el.noise.addEventListener("click", () => applyProcessing("noiseSuppression"));
  el.audioSettings.addEventListener("click", () => toggleMenu());
  el.micSelect.addEventListener("change", onSelectMicrophone);
  el.shareScreen.addEventListener("click", onShareScreen);
  el.camera.addEventListener("click", onCamera);
  el.benchToggle.addEventListener("click", onBenchToggle);
  el.leave.addEventListener("click", () => leave("You left the room."));
  el.enableAudio.addEventListener("click", onEnableAudio);
  el.noticeDismiss.addEventListener("click", () => clearNotice());
  document.addEventListener("keydown", onKeydown);
  document.addEventListener("pointerdown", onPointerDown);
  window.addEventListener("pagehide", () => {
    if (room) room.disconnect();
  });

  buildSwitches();
  paintBenchToggle();
  paintControls();

  settings = await loadSettings();
  el.lobbyRoom.textContent = roomLabel();
  el.name.maxLength = settings.maxNameLength;
  el.footUrl.textContent = settings.wsUrl;

  if (!window.isSecureContext || !navigator.mediaDevices) {
    block(
      "This page is not a secure context, so the browser will not hand over the microphone. " +
        "Open the demo over https, or over http://localhost while developing.",
    );
    return;
  }

  try {
    lk = await import(SDK_URL);
  } catch (error) {
    console.error("the livekit-client module failed to load", error);
    block(
      "The page could not load the client library it needs. Reload the page; if it keeps " +
        "happening the demo server is serving a broken build.",
    );
    return;
  }

  media = createMedia(lk);
  media.on("change", onMediaChange);
  media.on("microphonetrack", onMicrophoneTrack);
  media.on("screensharestopped", onScreenShareStopped);
  media.on("notice", (entry) => showNotice(entry.text));

  paintControls();
  ready();
}

function ready() {
  el.join.disabled = false;
  el.name.disabled = false;
  el.withVideo.disabled = false;
  el.lobbyStatus.textContent = "";
  el.name.focus();
}

function roomLabel() {
  if (!requestedRoom) return settings.room;
  const separator = settings.roomNamespaceSeparator || ".";
  return `${settings.room}${separator}${requestedRoom}`;
}

async function loadSettings() {
  try {
    const response = await fetch("/api/config");
    if (response.ok) return await response.json();
  } catch (error) {
    console.warn("could not read the demo configuration", error);
  }
  return settings;
}

async function onJoin(event) {
  event.preventDefault();
  hide(el.lobbyError);
  ensureAudioContext();

  const name = el.name.value.trim();
  if (!name) {
    fail(el.lobbyError, "Type a name so other people know who is talking.");
    el.name.focus();
    return;
  }

  const withVideo = el.withVideo.checked;
  busy(true, "Asking for the microphone...");

  const opened = await media.openMicrophone();
  if (!opened.opened) {
    busy(false, "");
    fail(el.lobbyError, opened.reason);
    return;
  }

  busy(true, "Connecting...");

  let credentials;
  try {
    credentials = await mintToken(name);
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

async function mintToken(name) {
  const body = { name };
  if (requestedRoom) body.room = requestedRoom;

  let response;
  try {
    response = await fetch("/api/token", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });
  } catch {
    throw new Error(
      "The demo server did not answer. Check that it is still running, then press Join again.",
    );
  }

  const payload = await response.json().catch(() => ({}));
  if (!response.ok) {
    throw new Error(
      payload.error || `The demo server refused the request (HTTP ${response.status}).`,
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
    .on(lk.RoomEvent.ConnectionStateChanged, onConnectionState)
    .on(lk.RoomEvent.AudioPlaybackStatusChanged, onPlaybackStatus)
    .on(lk.RoomEvent.Disconnected, onDisconnected);

  await room.connect(credentials.url, credentials.token);
  media.attach(room);

  const published = await media.publishMicrophone();
  if (!published.published) throw new Error(published.reason);

  watchLevel(room.localParticipant.identity, media.microphoneTrack);
  startBench();
}

function enterRoom(credentials) {
  el.roomName.textContent = credentials.room;
  el.footUrl.textContent = `${credentials.url} - ${credentials.identity}`;
  hide(el.roomError);
  el.lobby.hidden = true;
  el.room.hidden = false;
  onPlaybackStatus();
  paintControls();
  paintDevices();
  render();
  el.mute.focus();
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
  paintShare(null);
  paintShareTabs([]);
  el.audio.replaceChildren();
  speaking = new Set();
  selectedShare = null;
  toggleMenu(false);
  clearNotice();
  el.room.hidden = true;
  el.lobby.hidden = false;
  busy(false, reason || "");
  el.name.focus();
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
  pending.mute = true;
  paintControls();
  const result = await media.toggleMuted();
  pending.mute = false;
  if (result.reason) fail(el.roomError, result.reason);
  paintControls();
  render();
}

async function applyProcessing(key) {
  if (!media) return;
  hide(el.roomError);

  const wanted = !media.state().processing[key];
  const result = await media.setProcessing({ [key]: wanted });
  const label = PROCESSING_ROWS.find((row) => row.key === key).label;

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

async function onSelectMicrophone() {
  if (!media) return;
  hide(el.roomError);

  const chosen = el.micSelect.value;
  const result = await media.selectMicrophone(chosen === SYSTEM_DEFAULT ? null : chosen);

  if (result.applied) {
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
  if (!next) return;

  paintDevices();
  el.micSelect.focus();
  if (media) media.listMicrophones().then(paintDevices);
}

function onKeydown(event) {
  if (event.key !== "Escape" || el.audioMenu.hidden) return;
  toggleMenu(false);
  el.audioSettings.focus();
}

function onPointerDown(event) {
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
  render();
}

function watchLevel(identity, track) {
  const source = track && track.mediaStreamTrack;
  if (!source) return;

  dropLevel(identity);
  const context = ensureAudioContext();
  if (!context) return;

  const node = context.createMediaStreamSource(new MediaStream([source]));
  const analyser = context.createAnalyser();
  analyser.fftSize = 512;
  analyser.smoothingTimeConstant = 0.3;
  node.connect(analyser);

  meters.set(identity, {
    node,
    analyser,
    samples: new Uint8Array(analyser.fftSize),
    loudUntil: 0,
  });

  if (!meterTimer) meterTimer = setInterval(sample, METER_INTERVAL_MS);
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
  const level = meters.get(identity);
  if (!level) return;
  level.node.disconnect();
  meters.delete(identity);
  speaking.delete(identity);
}

function dropAllLevels() {
  for (const identity of [...meters.keys()]) dropLevel(identity);
  if (meterTimer) {
    clearInterval(meterTimer);
    meterTimer = null;
  }
}

function sample() {
  const now = performance.now();
  let changed = false;

  for (const [identity, level] of meters) {
    level.analyser.getByteTimeDomainData(level.samples);
    let energy = 0;
    for (const value of level.samples) {
      const centred = (value - 128) / 128;
      energy += centred * centred;
    }
    if (Math.sqrt(energy / level.samples.length) > SPEAKING_RMS) {
      level.loudUntil = now + SPEAKING_HOLD_MS;
    }

    const talking = now < level.loudUntil;
    if (talking === speaking.has(identity)) continue;
    changed = true;
    if (talking) {
      speaking.add(identity);
    } else {
      speaking.delete(identity);
    }
  }

  if (changed) render();
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

  arriving.clear();
  el.participants.replaceChildren(
    ...people.map((participant) =>
      renderParticipant(
        participant,
        participant === room.localParticipant,
        labels.get(participant.identity),
      ),
    ),
  );
  el.roomCount.textContent =
    people.length === 1 ? "Just you so far." : `${people.length} people in the room.`;

  const shares = screenShares(people, labels);
  const chosen = chooseShare(shares);
  paintShareTabs(shares);

  // A track attached in two places at once is a track either renderer can rip
  // out from under the other, so the bench owns the video or the stage does.
  if (showBench) {
    clearCards();
    paintShare(null);
    tiles.sync(videoTiles(people, labels));
  } else {
    tiles.clear();
    paintShare(chosen);
    syncCards(cardEntries(people, labels));
  }

  el.bench.hidden = !showBench;
  el.cards.dataset.compact = String(Boolean(chosen));
  paintControls();

  if (lastReading) paint(lastReading);
}

function screenShares(people, labels) {
  const found = [];
  const local = media ? media.screenShareTracks.video : null;

  if (local) {
    found.push({
      key: "local-share",
      label: labels.get(room.localParticipant.identity),
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
        label: labels.get(participant.identity),
        track: publication.track,
        local: false,
      });
    }
  }

  return found;
}

function chooseShare(shares) {
  if (shares.length === 0) {
    selectedShare = null;
    return null;
  }
  const kept = shares.find((entry) => entry.key === selectedShare);
  const chosen = kept || shares[0];
  selectedShare = chosen.key;
  return chosen;
}

function paintShare(entry) {
  if (!entry) {
    if (sharedTrack && sharedElement) sharedTrack.detach(sharedElement);
    if (sharedElement) sharedElement.remove();
    sharedTrack = null;
    sharedElement = null;
    el.share.hidden = true;
    el.shareFrame.replaceChildren();
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
    el.shareTabs.replaceChildren(
      ...shares.map((entry) => {
        const button = document.createElement("button");
        button.type = "button";
        button.className = "share-tab";
        button.dataset.key = entry.key;
        button.textContent = entry.local ? `${entry.label} (you)` : entry.label;
        button.addEventListener("click", () => {
          selectedShare = entry.key;
          render();
        });
        return button;
      }),
    );
  }

  for (const button of el.shareTabs.children) {
    button.setAttribute("aria-pressed", String(button.dataset.key === selectedShare));
  }
}

function cardEntries(people, labels) {
  return people.map((participant) => {
    const isLocal = participant === room.localParticipant;
    const described = describeParticipant(participant, isLocal);
    return {
      key: participant.identity,
      label: labels.get(participant.identity) || displayName(participant),
      local: isLocal,
      track: cameraFor(participant, isLocal),
      ...described,
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
  return { state: "muted", text: "muted", reason: null, muted: true, speaking: false };
}

function level(identity) {
  if (!meters.has(identity)) return unknownState("level unknown", NO_AUDIO_CONTEXT);
  const talking = speaking.has(identity);
  return {
    state: talking ? "speaking" : "listening",
    text: talking ? "speaking" : "listening",
    reason: null,
    muted: false,
    speaking: talking,
  };
}

function unknownState(text, reason) {
  return { state: "unknown", text, reason, muted: false, speaking: false };
}

function renderParticipant(participant, isLocal, label) {
  const described = describeParticipant(participant, isLocal);

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
  if (described.reason) state.title = described.reason;

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

function syncCards(entries) {
  const keys = new Set(entries.map((entry) => entry.key));

  for (const [key, card] of [...cards]) {
    if (keys.has(key)) continue;
    detachCard(card);
    card.node.remove();
    cards.delete(key);
  }

  for (const entry of entries) {
    let card = cards.get(entry.key);
    if (!card) {
      card = buildCard();
      cards.set(entry.key, card);
      el.cards.appendChild(card.node);
    }
    paintCard(card, entry);
  }

  const ordered = entries.map((entry) => cards.get(entry.key).node);
  const settled =
    ordered.length === el.cards.children.length &&
    ordered.every((node, index) => el.cards.children[index] === node);
  if (!settled) el.cards.replaceChildren(...ordered);

  el.cards.hidden = entries.length === 0;
}

function buildCard() {
  const node = document.createElement("li");
  node.className = "card-tile";
  node.dataset.video = "false";

  const frame = document.createElement("div");
  frame.className = "card-frame";

  const avatar = document.createElement("span");
  avatar.className = "avatar";
  frame.appendChild(avatar);

  const foot = document.createElement("div");
  foot.className = "card-foot";

  const who = document.createElement("span");
  who.className = "who";

  const state = document.createElement("span");
  state.className = "state";

  foot.append(who, state);
  node.append(frame, foot);

  return { node, frame, avatar, who, state, track: null, video: null };
}

function paintCard(card, entry) {
  card.node.dataset.speaking = String(entry.speaking);
  card.node.dataset.muted = String(entry.muted);
  card.node.dataset.state = entry.state;

  card.who.textContent = entry.local ? `${entry.label} - you` : entry.label;
  card.state.textContent = entry.text;
  if (entry.reason) {
    card.state.title = entry.reason;
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
    detachCard(card);
    card.node.remove();
    cards.delete(key);
  }
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

  el.mute.disabled = !open || pending.mute;
  el.mute.setAttribute("aria-pressed", String(muted));
  el.mute.dataset.on = String(open && !muted);
  el.mute.dataset.alert = String(muted);
  el.muteLabel.textContent = muted ? "Unmute" : "Mute";
  el.mute.setAttribute("aria-label", muted ? "Unmute your microphone" : "Mute your microphone");
  icon(el.mute, muted ? "#icon-mic-off" : "#icon-mic");
  el.mute.title = open
    ? "Muting stops the packets leaving; every page in the room shows it at once."
    : "No microphone is open.";

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
}

function paintNoise(state) {
  const wanted = state ? Boolean(state.processing.noiseSuppression) : false;
  const effective = state && state.effective ? state.effective.noiseSuppression : null;
  const held = Boolean(state && state.microphone.pendingWhileMuted);

  el.noise.disabled = !state || !state.microphone.open;
  el.noise.setAttribute("aria-pressed", String(wanted));
  el.noise.dataset.on = String(wanted);

  if (!state || !state.microphone.open) {
    sub("no microphone", true, "Open a microphone first.");
    return;
  }
  if (held) {
    sub(
      "applies on unmute",
      true,
      "Restarting the capture while you are muted would re-open the device and light the " +
        "browser's recording indicator, so the change is held.",
    );
    return;
  }
  if (effective === null) {
    sub(
      wanted ? "asked for on" : "asked for off",
      true,
      "This browser does not report whether it applied noise suppression.",
    );
    return;
  }
  if (effective !== wanted) {
    sub(
      effective ? "browser: on" : "browser: off",
      true,
      "The browser reports something other than what was asked for, and what it reports is what " +
        "the room hears.",
    );
    return;
  }

  sub(
    effective ? "on" : "off",
    false,
    "The browser confirms this is what it applied, before the audio was encoded.",
  );
}

function sub(text, missing, title) {
  el.noiseSub.textContent = text;
  el.noiseSub.dataset.missing = String(missing);
  el.noise.title = title;
  el.noise.setAttribute("aria-label", `Noise suppression, ${text}`);
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
}

function busy(active, message) {
  el.join.disabled = active;
  el.name.disabled = active;
  el.withVideo.disabled = active;
  el.join.textContent = active ? "Joining..." : "Join";
  el.lobbyStatus.textContent = message;
}

function block(message) {
  fail(el.lobbyError, message);
  el.join.disabled = true;
  el.name.disabled = true;
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
