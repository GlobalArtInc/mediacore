/**
 * The capture side of the page: the microphone, the browser's own audio
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
 *     const media = createMedia(lk, { denoiser });   // lk = the livekit-client namespace
 *     media.on("change", (state) => render(state));
 *
 *     await media.openMicrophone();             // asks for the microphone, before connecting
 *     media.attach(room);                       // after room.connect() resolved
 *     await media.publishMicrophone();
 *
 *     await media.setNoiseEngine("mediacore");  // off | browser | mediacore
 *     await media.setProcessing({ autoGainControl: false });
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
 *   "denoise"          (state().noise)          a fresh measurement from our denoiser; this one
 *                                               fires about twice a second and deliberately does
 *                                               not go through "change", so a readout can be
 *                                               repainted without re-rendering the room
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
 *   setNoiseEngine(engine)           -> { applied, engine } | { applied: false, deferred|reason }
 *   prepareDenoiser()                -> { available, reason }, cached after the first call
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
 *   processing   { echoCancellation, autoGainControl } -- the two that are ours to set directly
 *   noise        { engine, pending, attached, available, reason, fallback, metrics }
 *   effective    echoCancellation / noiseSuppression / autoGainControl / voiceIsolation as the
 *                browser reports them, each true, false or null for "the browser does not say";
 *                null altogether when no track is open
 *   devices      { list: [{ deviceId, groupId, label, named, selected }], named, note }
 *   screenShare  { active, hasAudio, audioNote }
 *
 * Noise suppression is one setting with three values, not two switches. Two
 * suppressors in series sound worse than either alone, so `noiseSuppression`
 * and `voiceIsolation` are not offered separately: they are derived from the
 * engine and handed to getUserMedia, and they are both off whenever ours runs.
 * Echo cancellation is never part of that choice — it needs the render-side
 * reference in the capture clock domain, which our node does not have, so it
 * stays the browser's job and stays on.
 *
 * Everything else the browser does to the audio is its business. These are
 * constraints handed to getUserMedia and applied before Opus encoding. The
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
 * The two the interface sets directly. The other two follow the noise engine.
 * Exported because it is also the list of what may be remembered between
 * visits: a page that stored a fifth key would be storing a setting nothing
 * here can apply.
 */
export const DIRECT_PROCESSING_KEYS = Object.freeze(["echoCancellation", "autoGainControl"]);

export const NOISE_OFF = "off";
export const NOISE_BROWSER = "browser";
export const NOISE_MEDIACORE = "mediacore";

export const NOISE_ENGINES = Object.freeze([NOISE_OFF, NOISE_BROWSER, NOISE_MEDIACORE]);

/**
 * The capture defaults. These match what livekit-client would apply on its own,
 * so leaving them alone changes nothing about how the page has always sounded.
 */
export const DEFAULT_PROCESSING = Object.freeze({
  echoCancellation: true,
  autoGainControl: true,
});

/**
 * Ours, from the first load, before anybody has asked for anything. The wasm is
 * fetched on every load whatever this is set to -- the gate validates the binary
 * rather than assuming it -- so making ours the default costs a page nothing it
 * was not already paying, and it puts the better suppressor in front of the
 * person who never opens the menu.
 *
 * It decides which suppressor is *tried* first and nothing else. Every way ours
 * can fail still lands the call on the browser's own: a probe that says this
 * machine cannot have it, a worklet that will not start, and a render thread
 * that cannot keep up with the one that did. Those three are the reason a
 * default this side of the choice is safe to ship, so none of them may be
 * softened to make it look better.
 */
export const DEFAULT_NOISE_ENGINE = NOISE_MEDIACORE;

export const SYSTEM_DEFAULT_MICROPHONE = null;

const NO_DENOISER =
  "this page was built without a denoiser of its own, so the browser's is all there is";

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
  const denoiser = options.denoiser || null;
  const audioContextFor =
    typeof options.audioContext === "function"
      ? options.audioContext
      : () => options.audioContext || null;

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
  let openedDevice = SYSTEM_DEFAULT_MICROPHONE;
  let deferred = false;
  let recovering = false;

  let noiseEngine = DEFAULT_NOISE_ENGINE;
  let pendingEngine = null;
  let denoiserSupport = null;
  let denoiserProcessor = null;
  let denoiserMetrics = null;
  let denoiserFallback = { engaged: false, reason: null, at: null };

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
      noise: {
        engine: noiseEngine,
        pending: pendingEngine,
        attached: Boolean(denoiserProcessor),
        available: denoiserSupport ? denoiserSupport.available : null,
        reason: denoiserSupport ? denoiserSupport.reason : denoiser ? null : NO_DENOISER,
        fallback: { ...denoiserFallback },
        metrics: denoiserMetrics,
      },
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

  /**
   * What the browser says it applied to the *capture device*, which is the only
   * track the browser's own processing runs on. `LocalTrack.mediaStreamTrack`
   * is the processor's output whenever ours is attached -- a track out of a
   * `MediaStreamAudioDestinationNode`, whose `getSettings()` carries none of
   * these flags -- so reading that one would answer "not reported" exactly when
   * the claim matters most, and could never contradict us.
   */
  function reportedSettings(track) {
    if (!track) return null;

    let settings = null;
    if (typeof track.getSourceTrackSettings === "function") {
      try {
        settings = track.getSourceTrackSettings();
      } catch {
        settings = null;
      }
    }
    if (!settings) {
      const source = captureTrackOf(track);
      if (!source || typeof source.getSettings !== "function") return null;
      try {
        settings = source.getSettings();
      } catch {
        return null;
      }
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

  function captureTrackOf(track) {
    const processed = track.processor ? track.processor.processedTrack : null;
    const exposed = track.mediaStreamTrack || null;
    if (processed && exposed === processed) return null;
    return exposed;
  }

  /**
   * The browser's own suppressors are derived from the engine, never set on
   * their own. RNNoise is mono and 48 kHz; asking for one channel here keeps a
   * stereo device from being half denoised.
   */
  function processingConstraints() {
    const browserSuppresses = noiseEngine === NOISE_BROWSER;
    return {
      ...processing,
      noiseSuppression: browserSuppresses,
      voiceIsolation: browserSuppresses,
      channelCount: 1,
    };
  }

  function captureOptions() {
    const constraints = processingConstraints();
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
      if (mic) await applyCapture("device-lost", { reopen: true });
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
      if (wantedDevice && isWrongDevice(error)) {
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

    openedDevice = wantedDevice;
    bindMicrophone(mic);
    watchDevices();
    await refreshDevices();

    // The engine can have been chosen before there was a microphone to put it
    // on, and on a first visit it was never chosen at all -- ours is the
    // default, so this is the ordinary path rather than the odd one. Attaching
    // now, rather than at the next restart, is what keeps the capture from
    // running with our suppressor asked for and nothing in place.
    if (noiseEngine === NOISE_MEDIACORE) {
      const attached = await attachDenoiser();
      if (!attached.attached) {
        noiseEngine = NOISE_BROWSER;
        notice("denoise", `${attached.reason} The browser's own suppressor is running instead.`);
        await applyCapture("denoiser-unavailable");
      }
    }

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

      const result = await applyCapture("track-ended", { reopen: true });
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
      if (deferred) await resumeDeferred();
    }

    changed();
    return { muted: Boolean(mic.isMuted) };
  }

  async function resumeDeferred() {
    const next = pendingEngine;
    pendingEngine = null;
    if (next !== null && next !== noiseEngine) return switchNoiseEngine(next, "unmuted");
    return applyCapture("unmuted");
  }

  async function toggleMuted() {
    return setMuted(!(mic && mic.isMuted));
  }

  /**
   * Change echoCancellation or autoGainControl while the call is running. Only
   * the keys given are touched. The two suppression flags are not settable here
   * on purpose: they follow the noise engine, and offering them separately is
   * how a call ends up running two suppressors in series.
   */
  async function setProcessing(patch) {
    const next = { ...processing };
    for (const key of DIRECT_PROCESSING_KEYS) {
      if (key in patch) next[key] = Boolean(patch[key]);
    }

    for (const key of ["noiseSuppression", "voiceIsolation"]) {
      if (key in patch) {
        return {
          applied: false,
          reason: `${key} is not a setting of its own here; choose a noise suppression engine instead`,
        };
      }
    }

    processing = next;
    return applyCapture("processing");
  }

  /** Probe the denoiser once, before it is offered to anyone. */
  async function prepareDenoiser() {
    const support = await ensureDenoiser();
    changed();
    return { available: support.available, reason: support.reason };
  }

  /**
   * Probe ours once, and let the engine go if the answer is that this machine
   * cannot have it at all. Ours being the default is what makes that second
   * half necessary: without it a browser with no AudioWorklet would sit there
   * with `mediacore` named as the engine and the same word greyed out in the
   * menu underneath as not on offer here. Nothing is in the path this early to
   * contradict -- the probe runs before there is a microphone -- so the field
   * is a claim about a suppressor that cannot exist on this machine, and it
   * goes back to the browser's before anybody reads it.
   */
  async function ensureDenoiser() {
    const support = await probeDenoiser();
    if (!support.available && noiseEngine === NOISE_MEDIACORE && !denoiserProcessor) {
      noiseEngine = NOISE_BROWSER;
    }
    return support;
  }

  async function probeDenoiser() {
    if (!denoiser) return { available: false, reason: NO_DENOISER };
    if (!denoiserSupport) {
      try {
        denoiserSupport = await denoiser.prepare();
      } catch (error) {
        denoiserSupport = {
          available: false,
          reason: `the denoiser could not be loaded: ${message(error)}`,
        };
      }
    }
    return denoiserSupport;
  }

  /**
   * Choose which noise suppressor runs: none, the browser's, or ours. One
   * setting, because two of them at once sound worse than either.
   */
  async function setNoiseEngine(engine) {
    if (!NOISE_ENGINES.includes(engine)) {
      return { applied: false, engine: noiseEngine, reason: `${engine} is not a suppressor` };
    }

    if (engine === NOISE_MEDIACORE) {
      const support = await ensureDenoiser();
      if (!support.available) {
        changed();
        return { applied: false, engine: noiseEngine, reason: support.reason };
      }
      // A person asking for it again after a fallback is arguing with the
      // fallback, which is their right; the record of it stays on the state.
      denoiserFallback = { ...denoiserFallback, engaged: false };
    }

    if (!mic) {
      noiseEngine = engine;
      changed();
      return { applied: false, engine, reason: "the microphone is not open" };
    }

    if (mic.isMuted) {
      pendingEngine = engine;
      deferred = true;
      changed();
      return { applied: false, engine: noiseEngine, deferred: true };
    }

    return switchNoiseEngine(engine, "noise-engine");
  }

  /**
   * The one moment that has to be got right: which suppressor is in the path,
   * and what the page believes about it.
   *
   * Three rules, and every branch below is one of them.
   *
   *  1. Nothing is published until it is ready. `attachDenoiser` does not
   *     resolve until the node has written a block of audio, so the engine is
   *     never handed a track that is still compiling.
   *  2. `noiseEngine` is only ever the suppressor that is actually running.
   *     Turning ours on attaches first, while the browser is still suppressing
   *     -- a few hundred milliseconds of both, which is merely worse audio,
   *     against no suppression at all if the attach fails. But the constraint
   *     change is the moment ours *replaces* the browser's, and until it lands
   *     both are running in series. If it does not land, the node comes back
   *     out and the engine goes back to what it was -- except that it cannot go
   *     back to "mediacore", because ours has just been detached. Re-selecting
   *     ours while it is already running and having the constraint change fail
   *     is the one path where "what it was" is a lie, and it reads `off`, which
   *     is what a detached node over an unchanged capture actually is. Leaving
   *     the field saying "mediacore" over a call that is not running it is the
   *     exact failure this control exists to prevent.
   *  3. Turning ours off stops the processor before the constraints change,
   *     never after, so a dead worklet is never left holding the capture. If
   *     that constraint change then fails, ours is out and the browser's has
   *     not come back: the engine reads `off`, which is what the capture is.
   */
  async function switchNoiseEngine(engine, reason) {
    const previous = noiseEngine;

    if (engine === NOISE_MEDIACORE) {
      const attached = await attachDenoiser();
      if (!attached.attached) {
        changed();
        return { applied: false, engine: noiseEngine, reason: attached.reason };
      }

      noiseEngine = NOISE_MEDIACORE;
      const result = await applyCapture(reason);
      if (result.applied) return { ...result, engine: noiseEngine };

      await detachDenoiser();
      noiseEngine = previous === NOISE_MEDIACORE ? NOISE_OFF : previous;
      if (result.deferred) pendingEngine = NOISE_MEDIACORE;
      changed();
      return { ...result, engine: noiseEngine };
    }

    await detachDenoiser();
    noiseEngine = engine;
    const result = await applyCapture(reason);
    if (!result.applied) {
      noiseEngine = previous === NOISE_MEDIACORE ? NOISE_OFF : previous;
      if (result.deferred) pendingEngine = engine;
      changed();
    }
    return { ...result, engine: noiseEngine };
  }

  async function attachDenoiser() {
    if (denoiserProcessor) return { attached: true };

    const support = await ensureDenoiser();
    if (!support.available) return { attached: false, reason: support.reason };

    lendAudioContext(mic);

    const created = denoiser.create({
      onMetrics: onDenoiserMetrics,
      onOverload: onDenoiserOverload,
    });

    try {
      await mic.setProcessor(created);
    } catch (error) {
      try {
        await created.destroy();
      } catch (failure) {
        console.warn("the denoiser would not clean up after refusing to start", failure);
      }
      return {
        attached: false,
        reason: `The denoiser would not start in this browser: ${message(error)}.`,
      };
    }

    denoiserProcessor = created;
    denoiserMetrics = null;
    emit("microphonetrack", { track: mic, reason: "denoiser-attached" });
    return { attached: true };
  }

  /**
   * livekit-client refuses a processor on a local audio track that has no audio
   * context, and it only gives a track one when the track was made through a
   * LocalParticipant or when it is published. Every track this module opens is
   * opened before the room exists, so the first join gets a context from the
   * publication and the second join -- a fresh track, never published yet --
   * gets none. Without this, rejoining a room drops the denoiser on the floor.
   */
  function lendAudioContext(track) {
    if (!track || typeof track.setAudioContext !== "function" || track.audioContext) return;
    let context = null;
    try {
      context = audioContextFor();
    } catch (error) {
      console.warn("this page has no audio context to lend the capture track", error);
      return;
    }
    if (context) track.setAudioContext(context);
  }

  async function detachDenoiser() {
    const stopping = denoiserProcessor;
    denoiserProcessor = null;
    denoiserMetrics = null;
    if (!stopping || !mic) return;

    try {
      await mic.stopProcessor();
    } catch (error) {
      console.warn("the denoiser would not stop cleanly", error);
    }
    emit("microphonetrack", { track: mic, reason: "denoiser-detached" });
  }

  function onDenoiserMetrics(reading) {
    denoiserMetrics = reading;
    emit("denoise", state().noise);
  }

  // Measured, not guessed: the render thread said it could not keep up, so the
  // node comes out and the browser's own suppressor goes back in. It does not
  // re-arm itself; one that switched on and off mid-sentence would be worse
  // than either state.
  function onDenoiserOverload(entry) {
    if (noiseEngine !== NOISE_MEDIACORE) return;
    denoiserFallback = { engaged: true, reason: entry.reason, at: entry.at };
    notice(
      "denoise",
      `The denoiser was taken out of the call: ${entry.reason}. The browser's own noise ` +
        "suppression is running instead. Choose the mediacore denoiser again by hand if you " +
        "want to argue with that.",
    );
    switchNoiseEngine(NOISE_BROWSER, "denoiser-overloaded").catch((error) => {
      console.error("the denoiser could not be taken out of the call", error);
    });
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

  /**
   * Bring the live capture into line with `processing`, the chosen engine and
   * the chosen device.
   *
   * A change that is only about processing is applied to the device already
   * open, with no second `getUserMedia`: the device is not re-opened, the level
   * does not jump and the audio does not drop out. That is what a change of
   * suppressor is -- two constraints and nothing else -- so toggling the engine
   * no longer re-opens the microphone on the way in and again on the way out.
   * Only a change of input device, a track the browser ended, or a browser that
   * refuses the constraint outright costs a restart.
   */
  async function applyCapture(reason, options = {}) {
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

    const mustReopen = options.reopen === true || wantedDevice !== openedDevice;
    if (!mustReopen && (await constrainInPlace())) {
      deferred = false;
      await refreshDevices();
      changed();
      return { applied: true, track: mic, reopened: false };
    }

    try {
      await mic.restartTrack(captureOptions());
    } catch (error) {
      if (wantedDevice && isWrongDevice(error)) {
        wantedDevice = SYSTEM_DEFAULT_MICROPHONE;
        notice(
          "device",
          "That microphone could not be opened, so the system default was used instead.",
        );
        return applyCapture(reason, { reopen: true });
      }
      changed();
      return { applied: false, reason: microphoneMessage(error), error };
    }

    openedDevice = wantedDevice;
    // `LocalTrack.restart` passes the device and, when no device was named,
    // nothing else: `audio: true`. Everything asked for here -- the two
    // suppression flags, the gain, the one channel RNNoise needs -- is dropped
    // on that path, so it goes on again explicitly.
    await constrainInPlace();

    deferred = false;
    await refreshDevices();
    emit("microphonetrack", { track: mic, reason });
    changed();
    return { applied: true, track: mic, reopened: true };
  }

  async function constrainInPlace() {
    if (typeof mic.applyConstraints !== "function") return false;
    try {
      await mic.applyConstraints(processingConstraints());
      return true;
    } catch (error) {
      console.warn("this browser would not change the capture without re-opening it", error);
      return false;
    }
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
      // LocalTrack.stop() destroys an attached processor, which closes the
      // denoiser's audio context with it.
      await track.stop();
    }

    denoiserProcessor = null;
    denoiserMetrics = null;
    pendingEngine = null;
    openedDevice = SYSTEM_DEFAULT_MICROPHONE;
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
    setNoiseEngine,
    prepareDenoiser,
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

/**
 * The browser refusing the device that was asked for, rather than refusing to
 * hand over a microphone at all. `NotFoundError` is in here because a device id
 * outlives the device: one remembered from a previous visit, or one the person
 * chose before unplugging the thing, names something that is not there any
 * more, and Chrome answers an exact id it cannot find with `NotFoundError` --
 * the same name it uses for a machine with no microphone whatsoever. Told
 * apart by whether a device was named at all: with one, the answer is to stop
 * naming it and ask again, and if there really is no microphone the second
 * attempt says so with nothing left to blame.
 */
function isWrongDevice(error) {
  const name = error && error.name;
  return (
    name === "OverconstrainedError" ||
    name === "ConstraintNotSatisfiedError" ||
    name === "NotFoundError" ||
    name === "DevicesNotFoundError"
  );
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
      `The server refused the ${what}: this token does not grant that source. The front end ` +
      "mints its own tokens, so the grant is in the front end, not in the browser."
    );
  }
  return `The ${what} could not be published: ${detail}.`;
}
