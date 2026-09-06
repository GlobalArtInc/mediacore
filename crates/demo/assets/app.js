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

const SDK_URL = "/vendor/livekit-client-2.22.2.esm.js";
const SPEAKING_RMS = 0.02;
const SPEAKING_HOLD_MS = 500;
const METER_INTERVAL_MS = 150;
const BENCH_INTERVAL_MS = 1000;
const PRESETS = { 180: "h180", 360: "h360", 720: "h720" };

const el = {
  main: document.querySelector("main"),
  lobby: document.getElementById("lobby"),
  lobbyRoom: document.getElementById("lobby-room"),
  lobbyStatus: document.getElementById("lobby-status"),
  lobbyError: document.getElementById("lobby-error"),
  form: document.getElementById("join-form"),
  name: document.getElementById("name"),
  join: document.getElementById("join"),
  video: document.getElementById("with-video"),
  room: document.getElementById("room"),
  roomName: document.getElementById("room-name"),
  roomCount: document.getElementById("room-count"),
  roomState: document.getElementById("room-state"),
  roomError: document.getElementById("room-error"),
  participants: document.getElementById("participants"),
  tiles: document.getElementById("tiles"),
  estimate: document.getElementById("link-estimate"),
  arriving: document.getElementById("link-arriving"),
  video: document.getElementById("link-video"),
  rtt: document.getElementById("link-rtt"),
  sending: document.getElementById("link-sending"),
  enableAudio: document.getElementById("enable-audio"),
  camera: document.getElementById("camera"),
  mute: document.getElementById("mute"),
  leave: document.getElementById("leave"),
  audio: document.getElementById("audio"),
  footUrl: document.getElementById("foot-url"),
};

const query = new URLSearchParams(location.search);
const requestedRoom = query.get("room");
const wantsSimulcast = query.get("simulcast") !== "0";

let lk = null;
let settings = {
  room: "demo",
  wsUrl: "",
  maxNameLength: 40,
  roomNamespaceSeparator: ".",
};
let room = null;
let micTrack = null;
let cameraTrack = null;
let speaking = new Set();
const meters = new Map();
const arriving = new Map();
let meterTimer = null;
let audioContext = null;
let bench = null;
let benchTimer = null;
let lastReading = null;
let tiles = null;

start();

async function start() {
  tiles = createTiles(el.tiles);
  el.video.checked = query.get("video") === "1";

  el.form.addEventListener("submit", onJoin);
  el.camera.addEventListener("click", onCamera);
  el.mute.addEventListener("click", onMute);
  el.leave.addEventListener("click", () => leave("You left the room."));
  el.enableAudio.addEventListener("click", onEnableAudio);
  window.addEventListener("pagehide", () => {
    if (room) room.disconnect();
  });

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

  ready();
}

function ready() {
  el.join.disabled = false;
  el.name.disabled = false;
  el.video.disabled = false;
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

  const withVideo = el.video.checked;
  busy(true, "Asking for the microphone…");

  try {
    micTrack = await lk.createLocalAudioTrack({
      echoCancellation: true,
      noiseSuppression: true,
      autoGainControl: true,
    });
  } catch (error) {
    busy(false, "");
    fail(el.lobbyError, microphoneMessage(error));
    return;
  }

  busy(true, "Connecting…");

  let credentials;
  try {
    credentials = await mintToken(name);
  } catch (error) {
    await releaseMicrophone();
    busy(false, "");
    fail(el.lobbyError, error.message);
    return;
  }

  try {
    await connect(credentials);
  } catch (error) {
    await releaseMicrophone();
    await discardRoom();
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
    .on(lk.RoomEvent.TrackMuted, render)
    .on(lk.RoomEvent.TrackUnmuted, render)
    .on(lk.RoomEvent.TrackStreamStateChanged, render)
    .on(lk.RoomEvent.LocalTrackPublished, render)
    .on(lk.RoomEvent.LocalTrackUnpublished, render)
    .on(lk.RoomEvent.ConnectionStateChanged, onConnectionState)
    .on(lk.RoomEvent.AudioPlaybackStatusChanged, onPlaybackStatus)
    .on(lk.RoomEvent.Disconnected, onDisconnected);

  await room.connect(credentials.url, credentials.token);
  await room.localParticipant.publishTrack(micTrack);
  watchLevel(room.localParticipant.identity, micTrack);
  startBench();
}

function enterRoom(credentials) {
  el.roomName.textContent = credentials.room;
  el.footUrl.textContent = `${credentials.url} · ${credentials.identity}`;
  hide(el.roomError);
  el.lobby.hidden = true;
  el.room.hidden = false;
  updateMuteButton();
  updateCameraButton();
  onPlaybackStatus();
  render();
  el.mute.focus();
}

async function leave(reason) {
  await discardRoom();
  await releaseCamera();
  await releaseMicrophone();
  stopBench();
  dropAllLevels();
  tiles.clear();
  el.main.dataset.wide = "false";
  el.audio.replaceChildren();
  speaking = new Set();
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

async function releaseMicrophone() {
  if (!micTrack) return;
  const track = micTrack;
  micTrack = null;
  await track.stop();
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

  el.camera.disabled = true;
  el.camera.textContent = "Starting camera…";

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

  el.camera.disabled = false;
  updateCameraButton();
  render();
}

async function stopCamera() {
  if (!room || !cameraTrack) return;

  el.camera.disabled = true;
  try {
    await room.localParticipant.unpublishTrack(cameraTrack, true);
  } catch (error) {
    console.warn("the camera track could not be unpublished cleanly", error);
  }
  await releaseCamera();
  el.camera.disabled = false;
  updateCameraButton();
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

function updateCameraButton() {
  el.camera.textContent = cameraTrack ? "Stop camera" : "Start camera";
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
  tiles.paint(reading);
  paintArriving(reading);
}

function paintLink(link) {
  write(
    el.estimate,
    formatBitrate(link.estimate),
    link.note || link.pairNote || link.estimateNote || NOT_REPORTED,
  );
  write(el.arriving, formatBitrate(link.arriving), link.note || FIRST_SAMPLE);
  write(el.video, formatBitrate(link.video), link.note || link.videoNote || FIRST_SAMPLE);
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
    watchLevel(participant.identity, track);
  }
  render();
}

function onTrackUnsubscribed(track, publication, participant) {
  if (track.kind === lk.Track.Kind.Audio) {
    track.detach().forEach((element) => element.remove());
    dropLevel(participant.identity);
  }
  if (bench && publication) bench.forget(publication.trackSid);
  render();
}

function onParticipantDisconnected(participant) {
  dropLevel(participant.identity);
  render();
}

function watchLevel(identity, track) {
  const source = track.mediaStreamTrack;
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
      console.warn("no audio context, the speaking indicator stays dark", error);
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
    fail(el.roomError, "The connection dropped. Trying to get it back…");
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
    fail(
      el.roomError,
      `The browser would not start audio playback: ${error.message || error}`,
    );
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

async function onMute() {
  if (!micTrack) return;
  el.mute.disabled = true;
  try {
    if (micTrack.isMuted) {
      await micTrack.unmute();
    } else {
      await micTrack.mute();
    }
  } finally {
    el.mute.disabled = false;
  }
  updateMuteButton();
  render();
}

function updateMuteButton() {
  const muted = Boolean(micTrack && micTrack.isMuted);
  el.mute.textContent = muted ? "Unmute" : "Mute";
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

  const wanted = videoTiles(people, labels);
  tiles.sync(wanted);
  el.main.dataset.wide = String(wanted.length > 0);

  if (lastReading) paint(lastReading);
}

function videoTiles(people, labels) {
  const wanted = [];

  if (cameraTrack) {
    wanted.push({
      key: "local",
      label: `${labels.get(room.localParticipant.identity)} · you`,
      track: cameraTrack,
      local: true,
    });
  }

  for (const participant of people) {
    if (participant === room.localParticipant) continue;
    for (const publication of participant.videoTrackPublications.values()) {
      if (!publication.track) continue;
      wanted.push({
        key: publication.trackSid,
        label: labels.get(participant.identity),
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
    seen.set(name, (seen.get(name) || 0) + 1);
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

function renderParticipant(participant, isLocal, label) {
  const publications = [...participant.audioTrackPublications.values()];
  const publication = publications[0];
  const muted = isLocal
    ? Boolean(micTrack && micTrack.isMuted)
    : Boolean(publication && publication.isMuted);
  const talking = speaking.has(participant.identity) && !muted;

  const row = document.createElement("li");
  row.dataset.speaking = String(talking);
  row.dataset.muted = String(muted);

  const level = document.createElement("span");
  level.className = "level";

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
  if (!publication) {
    state.textContent = "no microphone";
  } else if (muted) {
    state.textContent = "muted";
  } else if (talking) {
    state.textContent = "speaking";
  } else {
    state.textContent = "listening";
  }

  row.append(level, who, state);

  if (!isLocal && publication) {
    const heard = document.createElement("span");
    heard.className = "heard";
    heard.textContent = "…";
    row.appendChild(heard);
    arriving.set(participant.identity, { node: heard, sid: publication.trackSid });
  }

  return row;
}

function microphoneMessage(error) {
  switch (error && error.name) {
    case "NotAllowedError":
    case "PermissionDeniedError":
      return (
        "The microphone was blocked. Open the padlock in the address bar, set Microphone " +
        "to Allow for this site, then press Join again."
      );
    case "NotFoundError":
    case "DevicesNotFoundError":
      return (
        "No microphone was found. Connect one, or choose an input device in the system " +
        "sound settings, then press Join again."
      );
    case "NotReadableError":
    case "TrackStartError":
      return (
        "The microphone is already in use by another application. Close it and press " +
        "Join again."
      );
    default:
      return `The microphone could not be opened: ${(error && error.message) || error}.`;
  }
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
  el.video.disabled = active;
  el.join.textContent = active ? "Joining…" : "Join";
  el.lobbyStatus.textContent = message;
}

function block(message) {
  fail(el.lobbyError, message);
  el.join.disabled = true;
  el.name.disabled = true;
  el.video.disabled = true;
}

function fail(node, message) {
  node.textContent = message;
  node.hidden = false;
}

function hide(node) {
  node.hidden = true;
  node.textContent = "";
}
