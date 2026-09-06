/**
 * The capture side of the demo page: the microphone, the browser's own audio
 * processing, the choice of input device, and the screen share.
 *
 * Nothing in here draws anything. The interface calls these functions and reads
 * `state()`; this module never touches the DOM, so the page can be redesigned
 * without opening this file.
 *
 * Usage, in the order the page needs it:
 *
 *     import { createMedia } from "/media.js";
 *
 *     const media = createMedia(lk);            // lk = the livekit-client module namespace
 *     media.on("change", (state) => render(state));
 *
 *     await media.openMicrophone();             // asks for the microphone, before connecting
 *     media.attach(room);                       // after room.connect() resolved
 *     await media.publishMicrophone();
 *
 *     await media.setProcessing({ noiseSuppression: false });
 *     await media.selectMicrophone(deviceId);   // null = whatever the system calls default
 *     await media.startScreenShare();
 *
 * Every method that talks to a device is async and resolves to a plain result
 * object rather than throwing, so a refused microphone or a cancelled picker is
 * an outcome the interface can render, not an exception it has to catch.
 *
 * Events (`media.on(name, handler)` returns an unsubscribe function):
 *
 *   "change"           (state)                  anything below changed; re-render
 *   "microphonetrack"  ({ track, reason })      the live MediaStreamTrack was replaced, so a
 *                                               level meter or a waveform must be re-attached
 *   "screensharestopped" ({ reason })           the share ended without our stop button
 *   "notice"           ({ kind, text })         a sentence a person needs to read
 *
 * The whole surface:
 *
 *   support                          what this browser can do at all; frozen, read any time
 *   state()                          the snapshot below; safe to call on every render
 *   on(name, handler)                subscribe; returns an unsubscribe function
 *
 *   openMicrophone()                 -> { opened, track } | { opened: false, reason }
 *   publishMicrophone(options?)      -> { published, publication } | { published: false, reason }
 *   setMuted(muted) / toggleMuted()  -> { muted }
 *   setProcessing(patch)             -> { applied } | { applied: false, deferred } | { reason }
 *   selectMicrophone(deviceId|null)  -> same shape as setProcessing
 *   listMicrophones()                -> state().devices, after a fresh enumeration
 *   refreshDevices()                 -> the raw input list
 *   startScreenShare(options?)       -> { outcome: started|cancelled|unsupported|failed, ... }
 *   stopScreenShare()                -> { stopped }
 *   attach(room) / await detach()    bind and unbind the connected room
 *   await release()                  stop and forget every device this module opened
 *   microphoneTrack, screenShareTracks   the live LocalTracks, for attaching meters and tiles
 *
 * state() shape:
 *
 *   support      { secureContext, microphone, deviceList, screenShare, deviceChangeEvent }
 *   microphone   { open, published, muted, deviceId, activeDeviceId, pendingWhileMuted }
 *   processing   { echoCancellation, noiseSuppression, autoGainControl, voiceIsolation }
 *   effective    the same four keys as the browser reports them, each true, false or null
 *                for "the browser does not say"; null altogether when no track is open
 *   devices      { list: [{ deviceId, groupId, label, named, selected }], named, note }
 *   screenShare  { active, hasAudio, audioNote }
 *
 * What the audio processing flags do is the browser's business, not ours. They
 * are constraints handed to getUserMedia and applied before Opus encoding. The
 * server forwards encoded packets and never decodes them, so nothing on this
 * page can be undone or improved on later.
 */

const PROCESSING_KEYS = [
  "echoCancellation",
  "noiseSuppression",
  "autoGainControl",
  "voiceIsolation",
];

/**
 * The capture defaults. These match what livekit-client would apply on its own,
 * so leaving them alone changes nothing about how the demo has always sounded.
 */
export const DEFAULT_PROCESSING = Object.freeze({
  echoCancellation: true,
  noiseSuppression: true,
  autoGainControl: true,
  voiceIsolation: true,
});

export const SYSTEM_DEFAULT_MICROPHONE = null;

const NAMES_AFTER_PERMISSION =
  "The browser hides device names until the microphone has been allowed once, so these are " +
  "positions in the list rather than names.";

const NO_DEVICE_LIST =
  "This browser does not offer a device list, so the microphone cannot be chosen from the page. " +
  "Choose it in the system sound settings instead.";

const NO_SCREEN_SHARE =
  "This browser does not offer screen capture to a web page.";

const SCREEN_AUDIO_ABSENT =
  "The browser handed over the screen without an audio track. Chrome and Edge share tab or " +
  "system sound only when the box in the picker is ticked; Firefox and Safari do not offer it " +
  "at all. The screen is being shared silently.";

export function createMedia(lk, options = {}) {
  const agent = options.navigator || globalThis.navigator;
  const devices = agent && agent.mediaDevices ? agent.mediaDevices : null;

  const support = Object.freeze({
    secureContext: Boolean(globalThis.isSecureContext),
    microphone: Boolean(devices && devices.getUserMedia),
    deviceList: Boolean(devices && devices.enumerateDevices),
    screenShare: Boolean(devices && devices.getDisplayMedia),
    deviceChangeEvent: Boolean(devices && devices.addEventListener),
  });

  const listeners = new Map();

  let room = null;
  let mic = null;
  let micPublication = null;
  let wantedDevice = SYSTEM_DEFAULT_MICROPHONE;
  let processing = { ...DEFAULT_PROCESSING };
  let deferred = false;
  let recovering = false;

  let inputs = [];
  let inputsError = null;

  let screen = emptyScreen();
  let watchingDevices = false;

  function emptyScreen() {
    return { video: null, audio: null, audioNote: null };
  }

  function emit(name, payload) {
    const handlers = listeners.get(name);
    if (!handlers) return;
    for (const handler of [...handlers]) {
      try {
        handler(payload);
      } catch (error) {
        console.error(`a "${name}" handler threw`, error);
      }
    }
  }

  function changed() {
    emit("change", state());
  }

  function notice(kind, text) {
    emit("notice", { kind, text });
  }

  function on(name, handler) {
    if (!listeners.has(name)) listeners.set(name, new Set());
    listeners.get(name).add(handler);
    return () => listeners.get(name).delete(handler);
  }

  /**
   * A snapshot of everything the interface is allowed to display. Values the
   * browser did not report are `null`, never a stand-in number or a `false`
   * that would read as "off".
   */
  function state() {
    const reported = reportedSettings(mic);
    return {
      support,
      microphone: {
        open: Boolean(mic),
        published: Boolean(micPublication),
        muted: Boolean(mic && mic.isMuted),
        deviceId: wantedDevice,
        activeDeviceId: reported ? reported.deviceId : null,
        pendingWhileMuted: deferred,
      },
      processing: { ...processing },
      effective: reported ? reported.processing : null,
      devices: {
        list: inputs.map((input) => ({
          ...input,
          selected: input.deviceId === wantedDevice,
        })),
        named: inputs.length > 0 && inputs.every((input) => input.named),
        note: inputsError || (inputs.some((input) => !input.named) ? NAMES_AFTER_PERMISSION : null),
      },
      screenShare: {
        active: Boolean(screen.video),
        hasAudio: Boolean(screen.audio),
        audioNote: screen.audioNote,
      },
    };
  }

  function reportedSettings(track) {
    const source = track && track.mediaStreamTrack;
    if (!source || typeof source.getSettings !== "function") return null;

    let settings;
    try {
      settings = source.getSettings();
    } catch {
      return null;
    }

    const values = {};
    for (const key of PROCESSING_KEYS) {
      values[key] = key in settings ? Boolean(settings[key]) : null;
    }
    return {
      processing: values,
      deviceId: typeof settings.deviceId === "string" ? settings.deviceId : null,
    };
  }

  function captureOptions() {
    const constraints = { ...processing };
    if (wantedDevice) constraints.deviceId = { exact: wantedDevice };
    return constraints;
  }

  // ---------------------------------------------------------------- devices

  async function refreshDevices() {
    if (!support.deviceList) {
      inputs = [];
      inputsError = NO_DEVICE_LIST;
      return inputs;
    }

    let found;
    try {
      found = await devices.enumerateDevices();
    } catch (error) {
      inputs = [];
      inputsError = `The browser refused to list input devices: ${message(error)}.`;
      return inputs;
    }

    inputsError = null;
    inputs = found
      .filter((device) => device.kind === "audioinput")
      .map((device, index) => ({
        deviceId: device.deviceId,
        groupId: device.groupId || null,
        label: device.label || `Microphone ${index + 1}`,
        named: Boolean(device.label),
      }));

    return inputs;
  }

  function watchDevices() {
    if (watchingDevices || !support.deviceChangeEvent) return;
    watchingDevices = true;
    devices.addEventListener("devicechange", onDeviceChange);
  }

  function unwatchDevices() {
    if (!watchingDevices) return;
    watchingDevices = false;
    devices.removeEventListener("devicechange", onDeviceChange);
  }

  async function onDeviceChange() {
    const chosen = wantedDevice;
    await refreshDevices();

    if (chosen && !inputs.some((input) => input.deviceId === chosen)) {
      wantedDevice = SYSTEM_DEFAULT_MICROPHONE;
      notice(
        "device",
        "The microphone you had chosen is no longer connected. Falling back to whatever the " +
          "system calls the default input.",
      );
      if (mic) await applyCapture("device-lost");
    }

    changed();
  }

  // ------------------------------------------------------------ microphone

  async function openMicrophone() {
    if (mic) return { opened: true, track: mic };
    if (!support.secureContext) {
      return {
        opened: false,
        reason:
          "This page is not a secure context, so the browser will not hand over the microphone. " +
          "Open it over https, or over http://localhost while developing.",
      };
    }
    if (!support.microphone) {
      return { opened: false, reason: "This browser does not offer microphone capture." };
    }

    try {
      mic = await lk.createLocalAudioTrack(captureOptions());
    } catch (error) {
      if (isOverconstrained(error) && wantedDevice) {
        wantedDevice = SYSTEM_DEFAULT_MICROPHONE;
        notice(
          "device",
          "That microphone could not be opened, so the system default was used instead.",
        );
        return openMicrophone();
      }
      changed();
      return { opened: false, reason: microphoneMessage(error), error };
    }

    bindMicrophone(mic);
    watchDevices();
    await refreshDevices();

    emit("microphonetrack", { track: mic, reason: "opened" });
    changed();
    return { opened: true, track: mic };
  }

  function bindMicrophone(track) {
    track.on(lk.TrackEvent.Muted, changed);
    track.on(lk.TrackEvent.Unmuted, changed);
    track.on(lk.TrackEvent.Ended, onMicrophoneEnded);
  }

  function unbindMicrophone(track) {
    track.off(lk.TrackEvent.Muted, changed);
    track.off(lk.TrackEvent.Unmuted, changed);
    track.off(lk.TrackEvent.Ended, onMicrophoneEnded);
  }

  // The browser ends the track when its device disappears: unplugged, taken by
  // another application, or a laptop lid closing on a bluetooth headset.
  async function onMicrophoneEnded() {
    if (!mic || recovering) return;
    recovering = true;
    try {
      await refreshDevices();
      if (wantedDevice && !inputs.some((input) => input.deviceId === wantedDevice)) {
        wantedDevice = SYSTEM_DEFAULT_MICROPHONE;
      }

      const result = await applyCapture("track-ended");
      notice(
        "device",
        result.applied
          ? "The microphone stopped and was re-opened on the device that is still there."
          : "The microphone stopped and could not be re-opened. Choose another input, or leave " +
              "and join again.",
      );
    } finally {
      recovering = false;
      changed();
    }
  }

  async function publishMicrophone(publishOptions = {}) {
    if (!room) return { published: false, reason: "not connected to a room yet" };
    if (!mic) return { published: false, reason: "the microphone is not open" };
    if (micPublication) return { published: true, publication: micPublication };

    try {
      micPublication = await room.localParticipant.publishTrack(mic, {
        dtx: true,
        red: true,
        stopMicTrackOnMute: false,
        ...publishOptions,
      });
    } catch (error) {
      changed();
      return { published: false, reason: publishMessage("microphone", error), error };
    }

    changed();
    return { published: true, publication: micPublication };
  }

  async function setMuted(muted) {
    if (!mic) return { muted: false, reason: "the microphone is not open" };

    if (muted) {
      await mic.mute();
    } else {
      await mic.unmute();
      if (deferred) await applyCapture("unmuted");
    }

    changed();
    return { muted: Boolean(mic.isMuted) };
  }

  async function toggleMuted() {
    return setMuted(!(mic && mic.isMuted));
  }

  /**
   * Change any of echoCancellation / noiseSuppression / autoGainControl /
   * voiceIsolation while the call is running. Only the keys given are touched.
   */
  async function setProcessing(patch) {
    const next = { ...processing };
    for (const key of PROCESSING_KEYS) {
      if (key in patch) next[key] = Boolean(patch[key]);
    }
    processing = next;
    return applyCapture("processing");
  }

  /**
   * Switch the input device. Pass null for whatever the system calls default.
   * The publication survives: only the MediaStreamTrack behind the sender is
   * replaced, so subscribers are never unsubscribed and never re-negotiate.
   */
  async function selectMicrophone(deviceId) {
    wantedDevice = deviceId || SYSTEM_DEFAULT_MICROPHONE;
    return applyCapture("device");
  }

  async function applyCapture(reason) {
    if (!mic) {
      changed();
      return { applied: false, reason: "the microphone is not open" };
    }

    // Restarting a muted track would re-open the device and light the browser's
    // recording indicator while the person believes they are muted.
    if (mic.isMuted) {
      deferred = true;
      changed();
      return { applied: false, deferred: true };
    }

    try {
      await mic.restartTrack(captureOptions());
    } catch (error) {
      if (isOverconstrained(error) && wantedDevice) {
        wantedDevice = SYSTEM_DEFAULT_MICROPHONE;
        notice(
          "device",
          "That microphone could not be opened, so the system default was used instead.",
        );
        return applyCapture(reason);
      }
      changed();
      return { applied: false, reason: microphoneMessage(error), error };
    }

    deferred = false;
    await refreshDevices();
    emit("microphonetrack", { track: mic, reason });
    changed();
    return { applied: true, track: mic };
  }

  // ---------------------------------------------------------- screen share

  /**
   * Ask for a screen and publish it as a screen share, not as a camera: the
   * server keys its stream tracker and its bandwidth policy off the source, and
   * a screen that holds still must not be read as a layer that stopped arriving.
   */
  async function startScreenShare(shareOptions = {}) {
    if (!room) return { outcome: "failed", reason: "not connected to a room yet" };
    if (screen.video) return { outcome: "started", audio: Boolean(screen.audio) };
    if (!support.screenShare) return { outcome: "unsupported", reason: NO_SCREEN_SHARE };

    const wantsAudio = shareOptions.audio !== false;

    let captured;
    try {
      captured = await lk.createLocalScreenTracks({
        audio: wantsAudio,
        resolution: lk.ScreenSharePresets.h1080fps15.resolution,
        // Sharing the tab the call is in produces a hall of mirrors, so it is
        // kept out of the picker; switching surfaces mid-share is allowed.
        selfBrowserSurface: "exclude",
        surfaceSwitching: "include",
        systemAudio: wantsAudio ? "include" : "exclude",
      });
    } catch (error) {
      if (isPickerRefusal(error)) {
        return {
          outcome: "cancelled",
          reason:
            "No screen was shared. The browser reports a closed picker and a blocked permission " +
            "the same way, so this is either a cancelled picker or screen capture being refused " +
            "for this page.",
          error,
        };
      }
      if (error instanceof lk.DeviceUnsupportedError) {
        return { outcome: "unsupported", reason: NO_SCREEN_SHARE, error };
      }
      return { outcome: "failed", reason: `The screen could not be captured: ${message(error)}.`, error };
    }

    screen = emptyScreen();
    for (const track of captured) {
      if (track.kind === lk.Track.Kind.Video) screen.video = track;
      if (track.kind === lk.Track.Kind.Audio) screen.audio = track;
    }

    if (!screen.video) {
      await releaseScreen();
      return { outcome: "failed", reason: "The browser returned a screen capture with no video." };
    }

    screen.audioNote = wantsAudio && !screen.audio ? SCREEN_AUDIO_ABSENT : null;
    screen.video.on(lk.TrackEvent.Ended, onScreenEnded);
    if (screen.audio) screen.audio.on(lk.TrackEvent.Ended, onScreenEnded);

    try {
      await room.localParticipant.publishTrack(screen.video, {
        simulcast: false,
        videoCodec: "vp8",
        backupCodec: false,
        screenShareEncoding: lk.ScreenSharePresets.h1080fps15.encoding,
        degradationPreference: "maintain-resolution",
        ...(shareOptions.publish || {}),
      });

      if (screen.audio) {
        await room.localParticipant.publishTrack(screen.audio, { dtx: false, red: true });
      }
    } catch (error) {
      await releaseScreen();
      changed();
      return { outcome: "failed", reason: publishMessage("screen share", error), error };
    }

    if (screen.audioNote) notice("screen", screen.audioNote);
    changed();
    return { outcome: "started", audio: Boolean(screen.audio), audioNote: screen.audioNote };
  }

  async function stopScreenShare() {
    if (!screen.video) return { stopped: false, reason: "nothing is being shared" };
    await releaseScreen();
    changed();
    return { stopped: true };
  }

  // The browser's own "Stop sharing" bar ends the track behind our back.
  async function onScreenEnded() {
    if (!screen.video) return;
    await releaseScreen();
    emit("screensharestopped", { reason: "stopped-in-browser" });
    notice("screen", "You stopped sharing your screen from the browser's own controls.");
    changed();
  }

  async function releaseScreen() {
    const ending = screen;
    screen = emptyScreen();

    for (const track of [ending.video, ending.audio]) {
      if (track) track.off(lk.TrackEvent.Ended, onScreenEnded);
    }

    if (room) {
      for (const track of [ending.video, ending.audio]) {
        if (!track) continue;
        try {
          await room.localParticipant.unpublishTrack(track, true);
        } catch (error) {
          console.warn("a screen share track could not be unpublished cleanly", error);
        }
      }
    }

    for (const track of [ending.video, ending.audio]) {
      if (track) await track.stop();
    }
  }

  // ------------------------------------------------------------- lifecycle

  function attach(connected) {
    room = connected;
    changed();
  }

  // Leaving the room without releasing the screen would leave the browser
  // capturing a display nothing is sending any more.
  async function detach() {
    await releaseScreen();
    room = null;
    micPublication = null;
    changed();
  }

  /** Stop and forget every device this module opened. */
  async function release() {
    await releaseScreen();

    if (mic) {
      const track = mic;
      mic = null;
      micPublication = null;
      unbindMicrophone(track);
      await track.stop();
    }

    unwatchDevices();
    room = null;
    deferred = false;
    changed();
  }

  return {
    on,
    state,
    support,
    attach,
    detach,
    release,
    refreshDevices,
    listMicrophones: async () => {
      await refreshDevices();
      return state().devices;
    },
    openMicrophone,
    publishMicrophone,
    selectMicrophone,
    setProcessing,
    setMuted,
    toggleMuted,
    startScreenShare,
    stopScreenShare,
    get microphoneTrack() {
      return mic;
    },
    get screenShareTracks() {
      return { video: screen.video, audio: screen.audio };
    },
  };
}

function message(error) {
  return (error && (error.message || error.name)) || String(error);
}

function isOverconstrained(error) {
  return Boolean(error) && (error.name === "OverconstrainedError" || error.name === "ConstraintNotSatisfiedError");
}

function isPickerRefusal(error) {
  const name = error && error.name;
  return name === "NotAllowedError" || name === "AbortError" || name === "NotFoundError";
}

export function microphoneMessage(error) {
  switch (error && error.name) {
    case "NotAllowedError":
    case "PermissionDeniedError":
      return (
        "The microphone was blocked. Open the padlock in the address bar, set Microphone to " +
        "Allow for this site, then try again."
      );
    case "NotFoundError":
    case "DevicesNotFoundError":
      return "No microphone was found. Connect one and try again.";
    case "NotReadableError":
    case "TrackStartError":
      return "The microphone is already in use by another application. Close it and try again.";
    case "OverconstrainedError":
    case "ConstraintNotSatisfiedError":
      return "That microphone could not be opened with these settings.";
    default:
      return `The microphone could not be opened: ${message(error)}.`;
  }
}

function publishMessage(what, error) {
  const detail = message(error);
  if (/not.?allowed|permission/i.test(detail)) {
    return (
      `The server refused the ${what}: this token does not grant that source. The demo mints ` +
      "its own tokens, so the grant is in the demo server, not in the browser."
    );
  }
  return `The ${what} could not be published: ${detail}.`;
}
