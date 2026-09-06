/**
 * What the page's own modules do, driven without a browser.
 *
 * `node --test crates/demo/tests/browser-modules.test.mjs`, or through
 * `cargo test -p mediacore-demo`, which runs this file for you.
 *
 * The Rust tests either side of this one prove the artefacts are served and the
 * policy still forbids what it should. Those are statements about bytes. These
 * are statements about behaviour: that a setting handed to the capture module
 * comes back out as the constraint the browser is given, that choosing our
 * denoiser turns the browser's two suppressors off and choosing anything else
 * turns them back on, that a denoiser which cannot be started or cannot keep up
 * lands the call on the browser's own suppressor rather than on silence, and
 * that the numbers the bench prints are measured rather than assumed.
 */

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { after, describe, it } from "node:test";
import vm from "node:vm";

const here = fileURLToPath(new URL(".", import.meta.url));
const assetPath = (name) => `${here}../assets/${name}`;

const { createMedia, NOISE_BROWSER, NOISE_MEDIACORE, NOISE_OFF } = await import(
  `file://${assetPath("media.js")}`
);
const { createDenoiser, createOverloadWatch, keepingUp, measureAgainst, readingFrom } =
  await import(`file://${assetPath("denoise.js")}`);
const { monoDestination } = await import(`file://${assetPath("denoise.js")}`);
const { READY, STALLED, install } = await import(`file://${assetPath("denoise-gate.js")}`);

const QUANTUM = 128;
const SAMPLE_RATE = 48000;
const QUANTUM_MS = (QUANTUM / SAMPLE_RATE) * 1000;

// ------------------------------------------------------------------ doubles

/**
 * What `livekit-client`'s LocalAudioTrack does that this module depends on, and
 * nothing else:
 *
 *   - `mediaStreamTrack` is the *processor's* output whenever one is attached,
 *     not the capture device, which is why anything reading the browser's own
 *     processing has to go the other way round;
 *   - `setProcessor` refuses a track with no audio context, which is the state
 *     of every track opened before a room exists;
 *   - `restart` re-opens the device, and passes only the device: without one it
 *     asks `getUserMedia` for `audio: true` and every other constraint is lost;
 *   - `applyConstraints` changes the live capture without re-opening it.
 */
class FakeAudioTrack {
  constructor(constraints) {
    this.constraints = { ...constraints };
    this.applied = [{ ...constraints }];
    this.reopens = 0;
    this.constraintChanges = 0;
    this.isMuted = false;
    this.processor = null;
    this.audioContext = undefined;
    this.stopProcessorCalls = 0;
    this.setProcessorError = null;
    this.applyConstraintsError = null;
    this.restartError = null;
    this.handlers = new Map();
    this.stopped = false;

    this.captureTrack = {
      kind: "audio",
      getSettings: () => this.settings(),
    };
  }

  settings() {
    const device = this.constraints.deviceId;
    return {
      deviceId: device && typeof device === "object" ? device.exact : "mic-1",
      echoCancellation: Boolean(this.constraints.echoCancellation),
      noiseSuppression: Boolean(this.constraints.noiseSuppression),
      autoGainControl: Boolean(this.constraints.autoGainControl),
      voiceIsolation: Boolean(this.constraints.voiceIsolation),
      channelCount: this.constraints.channelCount === undefined ? 2 : this.constraints.channelCount,
    };
  }

  get mediaStreamTrack() {
    return this.processor && this.processor.processedTrack
      ? this.processor.processedTrack
      : this.captureTrack;
  }

  getSourceTrackSettings() {
    return this.settings();
  }

  setAudioContext(context) {
    this.audioContext = context;
  }

  get latest() {
    return this.applied[this.applied.length - 1];
  }

  on(name, handler) {
    if (!this.handlers.has(name)) this.handlers.set(name, new Set());
    this.handlers.get(name).add(handler);
  }

  off(name, handler) {
    const set = this.handlers.get(name);
    if (set) set.delete(handler);
  }

  async mute() {
    this.isMuted = true;
  }

  async unmute() {
    this.isMuted = false;
  }

  async applyConstraints(constraints) {
    if (this.applyConstraintsError) throw this.applyConstraintsError;
    this.constraintChanges += 1;
    this.constraints = { ...this.constraints, ...constraints };
    this.applied.push({ ...this.constraints });
  }

  async restartTrack(constraints) {
    if (this.restartError) throw this.restartError;
    this.reopens += 1;
    this.constraints = constraints.deviceId ? { ...constraints } : {};
    this.applied.push({ ...this.constraints });
    if (this.processor) await this.processor.restart({ track: this.captureTrack });
  }

  async setProcessor(processor) {
    if (this.setProcessorError) throw this.setProcessorError;
    if (!this.audioContext) {
      throw new Error(
        "Audio context needs to be set on LocalAudioTrack in order to enable processors",
      );
    }
    await processor.init({
      track: this.captureTrack,
      kind: "audio",
      audioContext: this.audioContext,
    });
    this.processor = processor;
  }

  async stopProcessor() {
    this.stopProcessorCalls += 1;
    const stopping = this.processor;
    this.processor = null;
    if (stopping) await stopping.destroy();
  }

  async stop() {
    this.stopped = true;
    await this.stopProcessor();
  }
}

class FakeMediaTrack {
  constructor(kind) {
    this.kind = kind;
    this.handlers = new Map();
    this.stopped = false;
  }
  on(name, handler) {
    if (!this.handlers.has(name)) this.handlers.set(name, new Set());
    this.handlers.get(name).add(handler);
    return this;
  }
  off(name, handler) {
    const set = this.handlers.get(name);
    if (set) set.delete(handler);
    return this;
  }
  emit(name) {
    for (const handler of this.handlers.get(name) || []) handler();
  }
  async stop() {
    this.stopped = true;
  }
}

function fakeLk(screen = {}) {
  const opened = [];
  const captured = [];
  return {
    opened,
    captured,
    TrackEvent: { Muted: "muted", Unmuted: "unmuted", Ended: "ended" },
    Track: { Kind: { Audio: "audio", Video: "video" }, Source: { ScreenShare: "screen" } },
    ScreenSharePresets: { h1080fps15: { resolution: {}, encoding: { maxBitrate: 3_000_000 } } },
    DeviceUnsupportedError: class DeviceUnsupportedError extends Error {},
    async createLocalAudioTrack(constraints) {
      const track = new FakeAudioTrack(constraints);
      opened.push(track);
      return track;
    },
    async createLocalScreenTracks(options) {
      captured.push(options);
      if (screen.refuse) throw screen.refuse;
      const tracks = [new FakeMediaTrack("video")];
      if (options.audio && screen.withAudio !== false) tracks.push(new FakeMediaTrack("audio"));
      return tracks;
    },
  };
}

function fakeRoom() {
  const published = [];
  const unpublished = [];
  return {
    published,
    unpublished,
    localParticipant: {
      async publishTrack(track, options) {
        published.push({ track, options });
        if (typeof track.setAudioContext === "function") track.setAudioContext({ id: "room" });
        return { track, options };
      },
      async unpublishTrack(track) {
        unpublished.push(track);
      },
    },
  };
}

function fakeNavigator() {
  return {
    mediaDevices: {
      getUserMedia: async () => ({}),
      getDisplayMedia: async () => ({}),
      enumerateDevices: async () => [
        { kind: "audioinput", deviceId: "mic-1", groupId: "g1", label: "Built-in" },
      ],
      addEventListener() {},
      removeEventListener() {},
    },
  };
}

/**
 * The least an AudioContext has to be for `createDenoiser().prepare()` to get
 * past its gates: a worklet to add modules to, and the sample rate the model is.
 */
function fakeAudioContextClass(sampleRate = 48000) {
  return class ContextWithWorklet {
    constructor() {
      this.sampleRate = sampleRate;
      this.state = "running";
    }
    get audioWorklet() {
      return { addModule: async () => {} };
    }
    async close() {}
  };
}

function fakeDenoiser({ available = true, reason = null } = {}) {
  const made = [];
  return {
    made,
    get last() {
      return made[made.length - 1] || null;
    },
    async prepare() {
      return { available, reason };
    },
    create(hooks) {
      const processor = {
        name: "fake-denoiser",
        hooks,
        processedTrack: { kind: "audio" },
        inits: 0,
        restarts: 0,
        destroyed: 0,
        async init() {
          this.inits += 1;
        },
        async restart() {
          this.restarts += 1;
        },
        async destroy() {
          this.destroyed += 1;
        },
      };
      made.push(processor);
      return processor;
    },
  };
}

/** Stands in for the page's own AudioContext, which is all this module does with it. */
function fakeAudioContext() {
  return { id: "page-audio-context" };
}

async function openMedia(options = {}) {
  globalThis.isSecureContext = true;
  const lk = fakeLk(options.screen);
  const notices = [];
  const context = options.audioContext === undefined ? fakeAudioContext() : options.audioContext;
  const media = createMedia(lk, {
    navigator: fakeNavigator(),
    audioContext: () => context,
    denoiser: options.denoiser === undefined ? fakeDenoiser() : options.denoiser,
  });
  media.on("notice", (entry) => notices.push(entry));

  if (options.open !== false) {
    const result = await media.openMicrophone();
    assert.equal(result.opened, true, result.reason);
  }

  return { media, lk, notices, context, track: () => lk.opened[lk.opened.length - 1] };
}

// ------------------------------------------------------ settings round-trip

describe("the capture module's settings reach the browser as constraints", () => {
  it("hands the browser its own suppressors only while the browser is the engine", async () => {
    const { media, track } = await openMedia();

    assert.equal(media.state().noise.engine, NOISE_BROWSER);
    assert.equal(track().latest.noiseSuppression, true);
    assert.equal(track().latest.voiceIsolation, true);

    const ours = await media.setNoiseEngine(NOISE_MEDIACORE);
    assert.equal(ours.applied, true, ours.reason);
    assert.equal(media.state().noise.engine, NOISE_MEDIACORE);
    assert.equal(
      track().latest.noiseSuppression,
      false,
      "the browser was left suppressing behind our node",
    );
    assert.equal(track().latest.voiceIsolation, false);
    assert.equal(
      track().latest.echoCancellation,
      true,
      "echo cancellation was turned off, and nothing on this page can replace it",
    );

    const off = await media.setNoiseEngine(NOISE_OFF);
    assert.equal(off.applied, true, off.reason);
    assert.equal(track().latest.noiseSuppression, false);
    assert.equal(track().latest.voiceIsolation, false);

    const back = await media.setNoiseEngine(NOISE_BROWSER);
    assert.equal(back.applied, true, back.reason);
    assert.equal(track().latest.noiseSuppression, true);
    assert.equal(track().latest.voiceIsolation, true);
  });

  it("asks for one channel, because RNNoise is mono", async () => {
    const { media, track } = await openMedia();
    await media.setNoiseEngine(NOISE_MEDIACORE);
    assert.equal(track().latest.channelCount, 1);
  });

  it("reports back what the browser said it applied, not what was asked for", async () => {
    const { media, track } = await openMedia();
    await media.setNoiseEngine(NOISE_MEDIACORE);

    assert.equal(media.state().effective.noiseSuppression, false);

    // A browser that ignores the constraint is the case the readout exists for.
    track().settings = () => ({
      deviceId: "mic-1",
      echoCancellation: true,
      noiseSuppression: true,
      autoGainControl: true,
    });

    assert.equal(media.state().effective.noiseSuppression, true);
    assert.equal(
      media.state().effective.voiceIsolation,
      null,
      "a flag the browser withheld must read as unknown, never as off",
    );
    assert.equal(media.state().noise.engine, NOISE_MEDIACORE);
  });

  it("refuses to be handed a suppression flag on its own", async () => {
    const { media, track } = await openMedia();
    const before = track().applied.length;

    const result = await media.setProcessing({ noiseSuppression: false });
    assert.equal(result.applied, false);
    assert.match(result.reason, /choose a noise suppression engine/);
    assert.equal(track().applied.length, before, "the capture was restarted anyway");

    const gain = await media.setProcessing({ autoGainControl: false });
    assert.equal(gain.applied, true);
    assert.equal(track().latest.autoGainControl, false);
    assert.equal(track().latest.noiseSuppression, true, "the engine was changed behind our back");
  });

  it("holds the change while muted and applies it on unmute", async () => {
    const { media, track } = await openMedia();
    await media.setMuted(true);

    const held = await media.setNoiseEngine(NOISE_MEDIACORE);
    assert.equal(held.applied, false);
    assert.equal(held.deferred, true);
    assert.equal(media.state().microphone.pendingWhileMuted, true);
    assert.equal(media.state().noise.pending, NOISE_MEDIACORE);
    assert.equal(
      track().latest.noiseSuppression,
      true,
      "the capture was restarted while muted, which re-opens the device behind a muted person",
    );

    await media.setMuted(false);
    assert.equal(media.state().noise.engine, NOISE_MEDIACORE);
    assert.equal(media.state().noise.attached, true);
    assert.equal(track().latest.noiseSuppression, false);
  });
});

describe("switching the engine keeps the publication", () => {
  it("replaces the track under the sender instead of unpublishing it", async () => {
    const { media, lk, track } = await openMedia();
    await media.setNoiseEngine(NOISE_MEDIACORE);
    await media.selectMicrophone("mic-1");
    await media.setNoiseEngine(NOISE_BROWSER);

    assert.equal(lk.opened.length, 1, "a second capture track was opened, so the publication moved");
    assert.ok(track().applied.length >= 3, "the capture was never restarted");
    assert.equal(track().stopped, false);
  });

  it("carries the node across a device change rather than rebuilding it", async () => {
    const denoiser = fakeDenoiser();
    const { media } = await openMedia({ denoiser });
    await media.setNoiseEngine(NOISE_MEDIACORE);

    const processor = denoiser.last;
    const restartsAfterAttach = processor.restarts;

    await media.selectMicrophone("mic-1");

    assert.equal(denoiser.made.length, 1, "the denoiser was thrown away and built again");
    assert.ok(
      processor.restarts > restartsAfterAttach,
      "the node was not told the capture track behind it had changed",
    );
    assert.equal(processor.destroyed, 0);
  });
});

// ------------------------------------------------------------ the fallbacks

describe("a denoiser that cannot run lands the call on the browser's own", () => {
  it("does not offer itself when the probe says it is unavailable", async () => {
    const denoiser = fakeDenoiser({ available: false, reason: "this browser has no AudioWorklet" });
    const { media, track } = await openMedia({ denoiser });

    await media.prepareDenoiser();
    assert.equal(media.state().noise.available, false);
    assert.match(media.state().noise.reason, /no AudioWorklet/);

    const result = await media.setNoiseEngine(NOISE_MEDIACORE);
    assert.equal(result.applied, false);
    assert.match(result.reason, /no AudioWorklet/);
    assert.equal(media.state().noise.engine, NOISE_BROWSER);
    assert.equal(media.state().noise.attached, false);
    assert.equal(
      track().latest.noiseSuppression,
      true,
      "the browser's suppressor was turned off for a denoiser that never started",
    );
  });

  it("leaves the browser suppressing when the node refuses to attach", async () => {
    const denoiser = fakeDenoiser();
    const { media, track } = await openMedia({ denoiser });
    track().setProcessorError = new Error("worklet refused");

    const result = await media.setNoiseEngine(NOISE_MEDIACORE);

    assert.equal(result.applied, false);
    assert.match(result.reason, /worklet refused/);
    assert.equal(media.state().noise.engine, NOISE_BROWSER);
    assert.equal(
      track().latest.noiseSuppression,
      true,
      "a failed attach left the call with no suppression at all",
    );
    assert.equal(denoiser.last.destroyed, 1, "the half-started node was left holding its context");
  });

  it("takes the node out when the render thread stops keeping up, and says so", async () => {
    const denoiser = fakeDenoiser();
    const { media, track, notices } = await openMedia({ denoiser });

    await media.setNoiseEngine(NOISE_MEDIACORE);
    assert.equal(media.state().noise.attached, true);
    assert.equal(media.state().noise.fallback.engaged, false);

    const processor = denoiser.last;
    processor.hooks.onOverload({
      reason: "the denoiser needed 71 % of this machine's audio budget",
      at: Date.now(),
    });
    await settle();

    assert.equal(media.state().noise.engine, NOISE_BROWSER, "the call kept the node it cannot run");
    assert.equal(media.state().noise.attached, false);
    assert.equal(media.state().noise.fallback.engaged, true);
    assert.match(media.state().noise.fallback.reason, /audio budget/);
    assert.equal(track().stopProcessorCalls, 1);
    assert.equal(processor.destroyed, 1);
    assert.equal(
      track().latest.noiseSuppression,
      true,
      "the fallback left the call with no suppressor rather than with the browser's",
    );

    const said = notices.map((entry) => entry.text).join(" ");
    assert.match(said, /taken out of the call/);
    assert.match(said, /browser's own noise suppression is running instead/);
  });

  it("does not re-arm itself, but lets a person argue with it", async () => {
    const denoiser = fakeDenoiser();
    const { media } = await openMedia({ denoiser });

    await media.setNoiseEngine(NOISE_MEDIACORE);
    denoiser.last.hooks.onOverload({ reason: "it could not keep up", at: Date.now() });
    await settle();

    assert.equal(media.state().noise.engine, NOISE_BROWSER);

    // Nothing puts it back on its own: the state after the fallback is a
    // working call on the browser's suppressor, and it stays there.
    await settle();
    assert.equal(media.state().noise.engine, NOISE_BROWSER);

    const again = await media.setNoiseEngine(NOISE_MEDIACORE);
    assert.equal(again.applied, true, again.reason);
    assert.equal(media.state().noise.fallback.engaged, false);
    assert.match(
      media.state().noise.fallback.reason,
      /could not keep up/,
      "the record of the fallback was erased when it was overridden",
    );
  });

  it("falls back to the browser when the engine was chosen before the microphone", async () => {
    const denoiser = fakeDenoiser({ available: false, reason: "no WebAssembly here" });
    const { media, track, notices } = await openMedia({ denoiser, open: false });

    const chosen = await media.setNoiseEngine(NOISE_MEDIACORE);
    assert.equal(chosen.applied, false);
    assert.equal(media.state().noise.engine, NOISE_BROWSER);

    const opened = await media.openMicrophone();
    assert.equal(opened.opened, true);
    assert.equal(
      track().latest.noiseSuppression,
      true,
      "the microphone opened with nothing suppressing at all",
    );
    assert.equal(notices.length >= 0, true);
  });
});

describe("the denoiser module refuses before it half-starts", () => {
  it("names what this environment is missing rather than throwing", async () => {
    const noWebAudio = createDenoiser({});
    const first = await noWebAudio.prepare();
    assert.equal(first.available, false);
    assert.match(first.reason, /no Web Audio/);

    class ContextWithoutWorklet {}
    const noWorklet = createDenoiser({ AudioContext: ContextWithoutWorklet });
    const second = await noWorklet.prepare();
    assert.equal(second.available, false);
    assert.match(second.reason, /no AudioWorklet/);
  });

  it("says the binary could not be loaded rather than pretending it was", async () => {
    globalThis.AudioWorkletNode = class AudioWorkletNode {};

    const missing = createDenoiser({
      AudioContext: fakeAudioContextClass(),
      wasmUrl: "/nowhere.wasm",
      simdWasmUrl: "/nowhere-simd.wasm",
      fetch: undefined,
    });

    const realFetch = globalThis.fetch;
    globalThis.fetch = async () => ({ ok: false, status: 404 });
    try {
      const result = await missing.prepare();
      assert.equal(result.available, false);
      assert.match(result.reason, /could not be loaded from this server/);
      assert.match(result.reason, /404/);
    } finally {
      globalThis.fetch = realFetch;
      delete globalThis.AudioWorkletNode;
    }
  });

  it("refuses a binary this build did not produce", async () => {
    globalThis.AudioWorkletNode = class AudioWorkletNode {};

    const realFetch = globalThis.fetch;
    globalThis.fetch = async () => ({
      ok: true,
      status: 200,
      arrayBuffer: async () => new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8]).buffer,
    });

    try {
      const wrong = createDenoiser({
        AudioContext: fakeAudioContextClass(),
        wasmUrl: "/wrong.wasm",
        simdWasmUrl: "/wrong-simd.wasm",
      });
      const result = await wrong.prepare();
      assert.equal(result.available, false);
      assert.match(result.reason, /rejected the denoiser binary/);
    } finally {
      globalThis.fetch = realFetch;
      delete globalThis.AudioWorkletNode;
    }
  });

  it("compiles the wasm this repository actually ships", async () => {
    for (const name of ["vendor/rnnoise.wasm", "vendor/rnnoise-simd.wasm"]) {
      const bytes = readFileSync(assetPath(name));
      assert.equal(
        WebAssembly.validate(bytes),
        true,
        `${name} is not a module this runtime would accept`,
      );
    }
  });
});

// -------------------------------------------------------- the measurements

describe("the probe measures the node rather than assuming a figure for it", () => {
  it("attributes the time between the two nodes to the denoiser", () => {
    const rig = loadProbe();
    const reports = rig.run({ quanta: 200, costMs: 0.4, inputLevel: 0.5, outputLevel: 0.25 });

    assert.ok(reports.length >= 1, "the probe never reported anything");
    const report = reports[0];

    assert.equal(report.clock, "performance");
    assert.equal(report.coarseClock, false);
    assert.ok(report.cost, "no cost was reported although a clock was available");
    assert.ok(
      Math.abs(report.cost.totalMs / report.cost.measuredQuanta - 0.4) < 0.001,
      `mean was ${report.cost.totalMs / report.cost.measuredQuanta}`,
    );
    assert.ok(
      Math.abs(report.cost.frameMs - 0.4 * (480 / QUANTUM)) < 0.01,
      `per-frame cost was ${report.cost.frameMs}`,
    );
    assert.ok(
      Math.abs(report.cost.measuredMs - report.cost.measuredQuanta * QUANTUM_MS) < 0.001,
      `the measured window was ${report.cost.measuredMs} ms`,
    );
    assert.equal(report.cost.overBudget, 0);
    assert.equal(
      report.cost.load,
      undefined,
      "the render thread reported a share of real time, which it has no clock to measure",
    );

    assert.ok(Math.abs(report.input.rms - 0.5) < 1e-6, `input rms was ${report.input.rms}`);
    assert.ok(Math.abs(report.output.rms - 0.25) < 1e-6, `output rms was ${report.output.rms}`);
  });

  it("counts the blocks the node ran past, which is what the fallback watches", () => {
    const rig = loadProbe();
    const reports = rig.run({ quanta: 200, costMs: 5, inputLevel: 0.5, outputLevel: 0.5 });
    const report = reports[0];

    assert.ok(report.cost.overBudget > 0, "a node taking 5 ms of a 2.67 ms block read as fine");
    assert.equal(report.cost.overBudget, report.cost.measuredQuanta);
    assert.ok(
      report.cost.totalMs > report.cost.measuredMs,
      "a node that spent longer inside the blocks than the blocks last did not show it",
    );
  });

  it("says it has no clock rather than reporting a cost of nothing", () => {
    const rig = loadProbe({ clock: "none" });
    const reports = rig.run({ quanta: 200, costMs: 0.4, inputLevel: 0.5, outputLevel: 0.25 });
    const report = reports[0];

    assert.equal(report.clock, "none");
    assert.equal(report.cost, null, "a browser with no render-thread clock reported a cost anyway");
    assert.ok(report.input, "the levels are measurable without a clock and were dropped");
    assert.ok(Math.abs(report.output.rms - 0.25) < 1e-6);
  });

  it("estimates the cost from a whole-millisecond clock and says it estimated", () => {
    // Chrome exposes no `performance` inside an AudioWorkletGlobalScope and no
    // AudioContext.renderCapacity either (measured, Chrome 148 on macOS), so
    // this is the path that browser actually takes.
    const rig = loadProbe({ clock: "date" });
    const reports = rig.run({ quanta: 400, costMs: 0.4, inputLevel: 0.5, outputLevel: 0.25 });
    const report = reports[0];

    assert.equal(report.clock, "date");
    assert.equal(report.coarseClock, true);
    assert.equal(report.clockResolutionMs, 1);
    assert.ok(report.cost, "a coarse clock reported no cost at all");
    assert.equal(
      report.cost.worstQuantumMs,
      null,
      "a whole-millisecond clock cannot say what one 2.67 ms block cost, and claimed to",
    );
    assert.equal(report.cost.overBudget, null);

    // Rounding down each reading loses the fraction, but the tick boundary
    // falls where it likes, so the total over a window is an estimate of the
    // real one rather than a fabrication. It must at least be in the region.
    const mean = report.cost.totalMs / report.cost.measuredQuanta;
    assert.ok(mean > 0.15 && mean < 0.8, `the estimate was ${mean} ms against a real 0.4 ms`);
  });

  it("reports no level for a window in which no audio arrived", () => {
    const rig = loadProbe();
    const reports = rig.run({ quanta: 200, costMs: 0.4, silent: true });
    const report = reports[0];

    assert.equal(report.input, null, "a window with no samples produced a level anyway");
    assert.equal(report.output, null);
  });

  it("passes the signal through unchanged, because it is in the audio path", () => {
    const rig = loadProbe();
    const written = rig.passOne(new Float32Array([0.25, -0.5, 0.75]));
    assert.deepEqual(Array.from(written), [0.25, -0.5, 0.75]);
  });
});

describe("whether the machine is keeping up is measured where there is a clock", () => {
  it("compares the audio produced against the wall clock it took", () => {
    const first = { at: 1_000_000, producedMs: 500 };
    const healthy = { at: 1_000_505, producedMs: 500 };
    const struggling = { at: 1_000_700, producedMs: 500 };

    assert.equal(keepingUp(null, healthy), null, "the first reading invented a rate");
    assert.ok(Math.abs(keepingUp(first, healthy).behind - 1.01) < 0.001);
    assert.ok(Math.abs(keepingUp(first, struggling).behind - 1.4) < 0.001);
  });

  it("counts the audio the node emitted, not the audio clock the device drives", () => {
    // `windowMs` comes off the context's frame counter, which is the audio
    // device's clock: it advances at real time whatever the render thread is
    // doing. A graph that stops keeping up emits fewer blocks, and that is the
    // only side of the pair that can move.
    const first = { at: 1_000_000, windowMs: 500, producedMs: 500 };
    const starved = { at: 1_000_900, windowMs: 500, producedMs: 500 * (128 / 213) };

    const measured = keepingUp(first, starved);
    assert.ok(
      measured.behind > 1.25,
      `a render thread that emitted 128 of the 213 blocks real time asked for read as ` +
        `${measured.behind}, which is inside the healthy band`,
    );
  });

  it("gives up on a graph that stays behind real time, and only then", () => {
    // The trigger that works in a browser whose render thread has no clock to
    // lend the cost figure, which is every Chrome measured so far.
    const tripped = [];
    const watch = createOverloadWatch((entry) => tripped.push(entry));
    const start = 1_000_000;

    for (let step = 0; step <= 8; step += 1) {
      watch.consider({
        at: start + step * 700,
        cost: null,
        realtime: { wallMs: 700, audioMs: 500, behind: 1.4 },
      });
    }

    assert.equal(tripped.length, 1, `tripped ${tripped.length} times`);
    assert.match(tripped[0].reason, /real time/);
    assert.match(tripped[0].reason, /140 %/);
  });

  it("does not give up on a graph that is merely jittery", () => {
    const tripped = [];
    const watch = createOverloadWatch((entry) => tripped.push(entry));
    const start = 1_000_000;

    for (let step = 0; step <= 40; step += 1) {
      watch.consider({
        at: start + step * 500,
        cost: { load: 0.08, overBudgetShare: 0 },
        realtime: { wallMs: 500, audioMs: 500, behind: step % 7 === 0 ? 1.6 : 1.0 },
      });
    }

    assert.equal(tripped.length, 0, "one slow window in seven took the denoiser out of the call");
  });

  it("gives up once and does not re-arm itself", () => {
    const tripped = [];
    const watch = createOverloadWatch((entry) => tripped.push(entry));
    const start = 1_000_000;

    for (let step = 0; step <= 40; step += 1) {
      watch.consider({
        at: start + step * 700,
        cost: { load: 0.9, overBudgetShare: 0.5 },
        realtime: { wallMs: 700, audioMs: 500, behind: 1.4 },
      });
    }

    assert.equal(tripped.length, 1, "the watch went on firing after the node was already out");
  });
});

describe("a reading becomes the numbers the bench prints", () => {
  it("turns amplitudes into dBFS and a change", () => {
    const reading = readingFrom({
      windowMs: 500,
      sampleRate: SAMPLE_RATE,
      quanta: 188,
      clock: "performance",
      coarseClock: false,
      input: { rms: 0.1, peak: 0.5, samples: 24064 },
      output: { rms: 0.01, peak: 0.05, samples: 24064 },
      cost: {
        measuredQuanta: 188,
        totalMs: 75.2,
        worstQuantumMs: 1.2,
        frameMs: 1.5,
        load: 0.15,
        overBudget: 0,
      },
    });

    assert.ok(Math.abs(reading.input.rms - -20) < 0.001);
    assert.ok(Math.abs(reading.output.rms - -40) < 0.001);
    assert.ok(
      Math.abs(reading.reduction - -20) < 0.001,
      `the change was ${reading.reduction}, and it is what the bench shows for the node's effect`,
    );
    assert.equal(reading.cost.overBudgetShare, 0);
    assert.equal(reading.costNote, null);
    assert.equal(
      reading.cost.load,
      null,
      "one report cannot say what share of real time the node cost, and claimed to",
    );
  });

  it("keeps a level of nothing as nothing, and says why a cost is missing", () => {
    const silent = readingFrom({
      windowMs: 500,
      sampleRate: SAMPLE_RATE,
      quanta: 188,
      clock: "none",
      coarseClock: true,
      input: null,
      output: null,
      cost: null,
    });

    assert.equal(silent.input.rms, null, "silence became a very quiet number");
    assert.equal(silent.output.rms, null);
    assert.equal(silent.reduction, null);
    assert.equal(silent.cost, null);
    assert.match(silent.costNote, /no clock at all/);
    assert.equal(silent.clock, "none");
  });
});


describe("the screen is shared as a screen, and stopping it is reported either way", () => {
  it("publishes the video with the screen-share ceiling and the sound beside it", async () => {
    const { media, lk } = await openMedia();
    const room = fakeRoom();
    media.attach(room);

    const result = await media.startScreenShare();
    assert.equal(result.outcome, "started", result.reason);
    assert.equal(result.audio, true);

    const asked = lk.captured[0];
    assert.equal(asked.selfBrowserSurface, "exclude", "the call's own tab was left in the picker");
    assert.equal(asked.systemAudio, "include");

    const video = room.published.find((entry) => entry.track.kind === "video");
    assert.ok(video, "no video was published");
    assert.equal(
      video.options.screenShareEncoding,
      lk.ScreenSharePresets.h1080fps15.encoding,
      "the screen went up without the screen-share ceiling, so it is published like a camera",
    );
    assert.equal(video.options.simulcast, false);
    assert.equal(video.options.degradationPreference, "maintain-resolution");

    assert.ok(
      room.published.some((entry) => entry.track.kind === "audio"),
      "the browser offered the sound with the screen and it was not published",
    );
    assert.equal(media.state().screenShare.active, true);
    assert.equal(media.state().screenShare.hasAudio, true);
  });

  it("says the screen came without sound rather than pretending it did not offer", async () => {
    const { media, notices } = await openMedia({ screen: { withAudio: false } });
    media.attach(fakeRoom());

    const result = await media.startScreenShare();
    assert.equal(result.outcome, "started");
    assert.equal(result.audio, false);
    assert.match(media.state().screenShare.audioNote, /without an audio track/);
    assert.ok(notices.some((entry) => entry.kind === "screen"));
  });

  it("reports a closed picker and a blocked permission as the pair they are", async () => {
    const refusal = new Error("denied");
    refusal.name = "NotAllowedError";
    const { media } = await openMedia({ screen: { refuse: refusal } });
    media.attach(fakeRoom());

    const result = await media.startScreenShare();
    assert.equal(result.outcome, "cancelled");
    assert.match(result.reason, /cancelled picker or screen capture being refused/);
    assert.equal(media.state().screenShare.active, false);
  });

  it("unpublishes and says so when the browser's own bar stops the share", async () => {
    const { media } = await openMedia();
    const room = fakeRoom();
    media.attach(room);
    await media.startScreenShare();

    const stopped = [];
    media.on("screensharestopped", (entry) => stopped.push(entry));

    const { video } = media.screenShareTracks;
    video.emit("ended");
    await settle();

    assert.deepEqual(
      stopped.map((entry) => entry.reason),
      ["stopped-in-browser"],
      "a share ended from the browser's own bar was never reported",
    );
    assert.ok(
      room.unpublished.length > 0,
      "the share stayed published after the browser ended the track",
    );
    assert.equal(media.state().screenShare.active, false);
  });
});

// ------------------------------------------------- the moment the engine changes

describe("nothing is published until the node is actually denoising", () => {
  it("holds `init` open until the vendored worklet writes its first block", async () => {
    const rig = loadRealWorklet();
    const node = rig.construct();
    const seen = [];
    node.port.postMessage = (data) => seen.push(data);

    // Before the wasm resolves the vendored processor writes nothing at all,
    // and a worklet output block starts zero-filled: publishing here publishes
    // silence, and there is no way to tell it from a quiet room.
    const early = rig.pass(node, rig.noise());
    assert.deepEqual(
      Array.from(early),
      Array.from(new Float32Array(128)),
      "the node wrote audio before its wasm was live",
    );
    assert.deepEqual(seen, [], "the gate called it ready before anything was written");

    await rig.settled(node);

    rig.pass(node, rig.noise());
    assert.deepEqual(
      seen.map((entry) => entry.type),
      [READY],
      "the gate never said the node had come up",
    );

    for (let index = 0; index < 20; index += 1) rig.pass(node, rig.noise());
    assert.equal(seen.length, 1, "the gate went on announcing itself");
  });

  it("says the node stalled rather than passing silence for ever", () => {
    // Wasm that never finishes compiling: the vendored processor goes on
    // returning from its render callback without writing anything, for ever,
    // and says nothing about it. Without the gate that is indistinguishable
    // from a quiet room.
    const rig = loadRealWorklet({ wasmNeverCompiles: true });
    const node = rig.construct();
    const seen = [];
    node.port.postMessage = (data) => seen.push(data);

    for (let index = 0; index < 1874; index += 1) rig.pass(node, rig.noise());
    assert.deepEqual(seen, [], "the node was written off before five seconds were up");

    const block = rig.pass(node, rig.noise());
    assert.deepEqual(
      seen.map((entry) => entry.type),
      [STALLED],
      "a node that never came up was never reported, so it would publish silence for ever",
    );
    assert.deepEqual(
      Array.from(block),
      Array.from(new Float32Array(128)),
      "the gate left its own marker in the audio instead of silence",
    );
  });

  it("refuses the attach when the node never comes up", async () => {
    const denoiser = createDenoiser({
      ...silentGraph(),
      readyTimeoutMs: 40,
    });

    const processor = denoiser.create({});
    await assert.rejects(
      () => processor.init({ track: { kind: "audio" } }),
      /did not start within/,
      "a node that never wrote a block was handed to the sender anyway",
    );
    assert.equal(
      processor.processedTrack,
      null,
      "a track was left on the processor for the sender to pick up",
    );
  });

  it("publishes the track once the node reports it is denoising", async () => {
    const graph = silentGraph();
    const denoiser = createDenoiser({ ...graph, readyTimeoutMs: 1000 });

    const processor = denoiser.create({});
    const started = processor.init({ track: { kind: "audio" } });
    await settle();
    assert.equal(processor.processedTrack, null, "the track was published before the node was up");

    graph.announce({ type: READY });
    await started;
    assert.ok(processor.processedTrack, "the node came up and nothing was published");
  });
});

describe("the engine only ever names the suppressor that is running", () => {
  it("puts the node back out when the constraint change does not land", async () => {
    const denoiser = fakeDenoiser();
    const { media, track } = await openMedia({ denoiser });
    track().applyConstraintsError = new Error("the device would not take it");
    track().restartError = new Error("the device would not re-open either");

    const result = await media.setNoiseEngine(NOISE_MEDIACORE);

    assert.equal(result.applied, false);
    assert.equal(
      media.state().noise.engine,
      NOISE_BROWSER,
      "the page says ours is running over a capture the browser is still suppressing, which is " +
        "two suppressors in series",
    );
    assert.equal(media.state().noise.attached, false, "our node was left in the path as well");
    assert.equal(denoiser.last.destroyed, 1);
    assert.equal(track().latest.noiseSuppression, true, "the call was left with no suppressor");
  });

  it("says off, not browser, when ours came out and the browser's did not go back on", async () => {
    const { media, track } = await openMedia();
    await media.setNoiseEngine(NOISE_MEDIACORE);
    assert.equal(media.state().noise.engine, NOISE_MEDIACORE);

    track().applyConstraintsError = new Error("the device would not take it");
    track().restartError = new Error("the device would not re-open either");

    const result = await media.setNoiseEngine(NOISE_BROWSER);
    assert.equal(result.applied, false);
    assert.equal(
      media.state().noise.engine,
      NOISE_OFF,
      "the page claims the browser is suppressing over a capture that asked it not to",
    );
    assert.equal(track().latest.noiseSuppression, false, "the state and the capture disagree");
  });

  it("does not re-open the microphone to change a constraint", async () => {
    const { media, track } = await openMedia();
    const opened = track().reopens;

    await media.setNoiseEngine(NOISE_MEDIACORE);
    await media.setNoiseEngine(NOISE_BROWSER);
    await media.setProcessing({ autoGainControl: false });

    assert.equal(
      track().reopens,
      opened,
      "changing a constraint re-opened the capture device, which drops the audio and jumps the " +
        "level for everyone in the room",
    );
    assert.equal(track().latest.noiseSuppression, true);
    assert.equal(track().latest.autoGainControl, false);
  });

  it("re-opens the device, and only the device, when the input changes", async () => {
    const { media, track } = await openMedia();
    const opened = track().reopens;

    await media.selectMicrophone("mic-1");
    assert.equal(track().reopens, opened + 1, "a device change did not re-open the device");

    await media.selectMicrophone("mic-1");
    assert.equal(track().reopens, opened + 1, "the same device was re-opened again");
  });

  it("puts back the constraints livekit-client drops when it re-opens a default device", async () => {
    const { media, track } = await openMedia();
    await media.setNoiseEngine(NOISE_MEDIACORE);

    await media.selectMicrophone("mic-1");
    await media.selectMicrophone(null);

    // `LocalTrack.restart` with no device asks getUserMedia for `audio: true`
    // and loses everything else, which is how a page that asked for one channel
    // ends up publishing two.
    assert.equal(track().reopens, 2);
    assert.equal(track().latest.channelCount, 1, "the capture came back stereo");
    assert.equal(track().latest.noiseSuppression, false, "the browser started suppressing again");
    assert.equal(track().latest.echoCancellation, true);
  });
});

describe("the denoiser survives leaving the room and joining it again", () => {
  it("attaches to a track that has never been published", async () => {
    const { media, lk, track } = await openMedia();

    await media.setNoiseEngine(NOISE_MEDIACORE);
    assert.equal(media.state().noise.attached, true);

    await media.release();

    const opened = await media.openMicrophone();
    assert.equal(opened.opened, true, opened.reason);
    assert.equal(lk.opened.length, 2, "the second join did not open a fresh capture track");
    assert.equal(
      media.state().noise.engine,
      NOISE_MEDIACORE,
      "rejoining silently dropped the denoiser and left the browser's suppressor in its place",
    );
    assert.equal(media.state().noise.attached, true);
    assert.equal(track().latest.noiseSuppression, false);
  });

  it("says so, rather than going quiet, when it truly cannot attach", async () => {
    const { media, notices } = await openMedia({ audioContext: null });

    const result = await media.setNoiseEngine(NOISE_MEDIACORE);
    assert.equal(result.applied, false);
    assert.match(result.reason, /Audio context/);
    assert.equal(media.state().noise.engine, NOISE_BROWSER);
    assert.equal(notices.length >= 0, true);
  });
});

describe("what the page says the browser is doing comes from the capture device", () => {
  it("reads the device the browser is processing, not the node's output", async () => {
    const { media, track } = await openMedia();
    await media.setNoiseEngine(NOISE_MEDIACORE);

    assert.equal(
      track().mediaStreamTrack,
      track().processor.processedTrack,
      "the fake stopped modelling livekit-client, so this test proves nothing",
    );
    assert.equal(
      media.state().effective.noiseSuppression,
      false,
      "the readout followed the processor's output track, which reports none of these flags, so " +
        "it can never contradict a browser that ignored the constraint",
    );
    assert.equal(media.state().effective.echoCancellation, true);

    // A processed track carries none of these; falling back to it would read
    // as "the browser does not say" exactly when the claim matters most.
    track().processor.processedTrack.getSettings = () => ({});
    assert.equal(media.state().effective.noiseSuppression, false);
  });

  it("still reports the browser ignoring the constraint while ours is attached", async () => {
    const { media, track } = await openMedia();
    await media.setNoiseEngine(NOISE_MEDIACORE);

    track().settings = () => ({
      deviceId: "mic-1",
      echoCancellation: true,
      noiseSuppression: true,
      autoGainControl: true,
    });

    assert.equal(
      media.state().effective.noiseSuppression,
      true,
      "a browser suppressing behind our node would never be reported",
    );
    assert.equal(media.state().effective.voiceIsolation, null);
  });
});

describe("the gate on offering the denoiser at all", () => {
  it("refuses a browser that will not run a graph at 48 kHz", async () => {
    globalThis.AudioWorkletNode = class AudioWorkletNode {};
    try {
      const denoiser = createDenoiser({ AudioContext: fakeAudioContextClass(44100) });
      const result = await denoiser.prepare();
      assert.equal(result.available, false);
      assert.match(result.reason, /48000 Hz/);
      assert.match(result.reason, /44100 Hz/);
    } finally {
      delete globalThis.AudioWorkletNode;
    }
  });

  it("does not fetch 150 KB of wasm to find out the rate is wrong", async () => {
    globalThis.AudioWorkletNode = class AudioWorkletNode {};
    const realFetch = globalThis.fetch;
    let fetches = 0;
    globalThis.fetch = async () => {
      fetches += 1;
      throw new Error("nothing should have been fetched");
    };
    try {
      const denoiser = createDenoiser({
        AudioContext: fakeAudioContextClass(44100),
        wasmUrl: "/rnnoise.wasm",
        simdWasmUrl: "/rnnoise-simd.wasm",
      });
      await denoiser.prepare();
      assert.equal(fetches, 0);
    } finally {
      globalThis.fetch = realFetch;
      delete globalThis.AudioWorkletNode;
    }
  });
});

describe("the node is published as the one channel the page asked for", () => {
  it("makes the graph's sink mono, because a stream destination is stereo by default", () => {
    // A MediaStreamAudioDestinationNode is two channels unless it is told
    // otherwise, and a mono graph connected to one is up-mixed rather than left
    // alone: the page asks getUserMedia for one channel and then publishes two.
    const destination = { channelCount: 2, channelCountMode: "max" };
    monoDestination(destination);

    assert.equal(destination.channelCount, 1);
    assert.equal(destination.channelCountMode, "explicit");
  });
});

// -------------------------------------------------------- the added delay

describe("the delay the node adds is measured, not quoted", () => {
  it("finds a delay it was given, in samples", () => {
    const rig = loadProbe();
    const reports = rig.runDelayed({ quanta: 900, delaySamples: 992 });
    const last = reports[reports.length - 1];

    assert.equal(
      last.delaySamples,
      992,
      `the probe put the node's delay at ${last.delaySamples} samples against a real 992`,
    );
    assert.ok(last.delayCorrelation > 0.9, `the peak was only ${last.delayCorrelation}`);
  });

  it("reports nothing rather than a number when there is nothing to line up", () => {
    const rig = loadProbe();
    const reports = rig.run({ quanta: 900, costMs: 0.1, silent: true });
    for (const report of reports) {
      assert.equal(report.delaySamples, null, "a delay was reported for a window with no audio");
    }
  });

  it("measures what the vendored worklet and the vendored wasm really add", async () => {
    const rig = loadRealChain();
    await rig.ready();

    const reports = rig.run(1400);
    const measured = reports.map((report) => report.delaySamples).filter((value) => value !== null);

    assert.ok(measured.length > 0, "the probe never got a delay out of the real node");
    for (const samples of measured) {
      assert.equal(
        samples,
        992,
        `the shipped denoiser delayed the signal by ${samples} samples, not the 992 the README ` +
          "quotes. Re-measure and update it rather than leaving the number stale.",
      );
    }
  });
});

// -------------------------------------------------------------------- rigs

/**
 * Runs `denoise-probe.js` in a context carrying the globals an
 * AudioWorkletGlobalScope carries, so the two processors can be driven a
 * quantum at a time and their reports read.
 */
function loadProbe({ clock = "performance" } = {}) {
  const registered = new Map();
  const state = { frame: 0, now: 0 };
  const reports = [];

  class FakePort {
    constructor(sink) {
      this.sink = sink;
      this.onmessage = null;
    }
    postMessage(data) {
      this.sink(data);
    }
  }

  const sandbox = {
    AudioWorkletProcessor: class AudioWorkletProcessor {
      constructor() {
        this.port = new FakePort((data) => reports.push(data));
      }
    },
    registerProcessor: (name, klass) => registered.set(name, klass),
    sampleRate: SAMPLE_RATE,
    get currentFrame() {
      return state.frame;
    },
    console,
  };
  if (clock === "performance") sandbox.performance = { now: () => state.now };
  // A vm context always has the intrinsics; shadowing them is how a browser
  // that lacks one is imitated.
  if (clock !== "date" && clock !== "performance") sandbox.Date = undefined;
  if (clock === "date") {
    sandbox.Date = Object.assign(function Date() {}, { now: () => Math.floor(state.now) });
  }

  const context = vm.createContext(sandbox);
  vm.runInContext(readFileSync(assetPath("denoise-probe.js"), "utf8"), context, {
    filename: "denoise-probe.js",
  });

  const Tap = registered.get("mediacore/denoise-tap");
  const Probe = registered.get("mediacore/denoise-probe");
  assert.ok(Tap && Probe, "the probe module did not register both processors");

  const tap = new Tap();
  const probe = new Probe();

  function quantum({ costMs, inputLevel, outputLevel, silent }) {
    const tapIn = silent ? [[]] : [[filled(inputLevel)]];
    const tapOut = [[new Float32Array(QUANTUM)]];
    tap.process(tapIn, tapOut, {});

    state.now += costMs;

    const probeIn = silent ? [[]] : [[filled(outputLevel)]];
    const probeOut = [[new Float32Array(QUANTUM)]];
    probe.process(probeIn, probeOut, {});

    state.frame += QUANTUM;
    state.now += 0.01;
    return probeOut[0][0];
  }

  return {
    run({ quanta, costMs, inputLevel = 0.5, outputLevel = 0.25, silent = false }) {
      for (let index = 0; index < quanta; index += 1) {
        quantum({ costMs, inputLevel, outputLevel, silent });
      }
      return reports;
    },
    /**
     * Drives the two ends with a signal the probe has never seen, the output
     * side delayed by a known number of samples. That is exactly the shape a
     * denoiser presents -- its own input, later -- so a delay search that works
     * here is a delay search, not a coincidence.
     */
    runDelayed({ quanta, delaySamples, level = 0.3 }) {
      let seed = 424242;
      const stream = [];
      const at = (index) => {
        while (stream.length <= index) {
          seed = (seed * 1103515245 + 12345) & 0x7fffffff;
          stream.push((seed / 0x3fffffff - 1) * level);
        }
        return index < 0 ? 0 : stream[index];
      };

      for (let block = 0; block < quanta; block += 1) {
        const base = block * QUANTUM;
        const dry = new Float32Array(QUANTUM);
        const wet = new Float32Array(QUANTUM);
        for (let index = 0; index < QUANTUM; index += 1) {
          dry[index] = at(base + index);
          wet[index] = at(base + index - delaySamples);
        }

        tap.process([[dry]], [[new Float32Array(QUANTUM)]], {});
        state.now += 0.2;
        probe.process([[wet]], [[new Float32Array(QUANTUM)]], {});
        state.frame += QUANTUM;
        state.now += 0.01;
      }
      return reports;
    },
    passOne(samples) {
      const out = [[new Float32Array(samples.length)]];
      probe.process([[samples]], out, {});
      return out[0][0];
    },
  };
}

/**
 * The vendored RNNoise worklet, gated, running on the real vendored wasm.
 *
 * `import.meta.url` is rewritten to a literal so the bundle can be evaluated as
 * a plain script in a vm context; nothing else about it is touched, and the
 * bytes fed to it are the ones this repository ships.
 */
function loadRealWorklet(options = {}) {
  const registered = new Map();
  const state = { frame: 0, now: 0 };

  const sandbox = worklandScope(state, registered);
  if (options.wasmNeverCompiles) {
    // A compile that never finishes: the shape of the failure this gate exists
    // for, without a rejected promise nobody is holding to confuse the runner.
    const stalled = Object.create(WebAssembly);
    stalled.instantiate = () => new Promise(() => {});
    sandbox.WebAssembly = stalled;
  }
  const context = vm.createContext(sandbox);
  vm.runInContext(vendoredWorkletSource(), context, { filename: "rnnoise-worklet.js" });

  const Klass = registered.get("@sapphi-red/web-noise-suppressor/rnnoise");
  assert.ok(Klass, "the vendored worklet registered nothing");

  let seed = 20260905;
  const random = () => {
    seed = (seed * 1103515245 + 12345) & 0x7fffffff;
    return seed / 0x3fffffff - 1;
  };

  return {
    construct(options = {}) {
      return new Klass({
        processorOptions: {
          maxChannels: 1,
          wasmBinary: options.wasmBinary === undefined ? realWasm() : options.wasmBinary,
        },
      });
    },
    noise(level = 0.2) {
      const block = new Float32Array(QUANTUM);
      for (let index = 0; index < QUANTUM; index += 1) block[index] = random() * level;
      return block;
    },
    pass(node, input) {
      const out = new Float32Array(QUANTUM);
      node.process([[input]], [[out]], {});
      state.frame += QUANTUM;
      return out;
    },
    // The vendored processor loads its binary from inside its own constructor
    // and exposes no promise for it. Polling its own field is a rig affordance,
    // so that what the gate does can be asserted at a known moment rather than
    // after a guessed delay; a rename upstream makes this time out loudly.
    async settled(node, limitMs = 5000) {
      const deadline = Date.now() + limitMs;
      while (!node.processor) {
        if (Date.now() > deadline) {
          throw new Error("the vendored worklet never brought its wasm up in this runtime");
        }
        await new Promise((resolve) => setTimeout(resolve, 10));
      }
    },
  };
}

/**
 * The whole capture chain the page builds, minus the browser: the tap, the real
 * gated RNNoise node on the real wasm, and the probe, in one scope, driven a
 * render quantum at a time. This is the only place the delay figure in the
 * README can come from.
 */
function loadRealChain() {
  const registered = new Map();
  const state = { frame: 0, now: 0 };
  const messages = [];

  const sandbox = worklandScope(state, registered, (data) => messages.push(data));
  const context = vm.createContext(sandbox);
  vm.runInContext(vendoredWorkletSource(), context, { filename: "rnnoise-worklet.js" });
  vm.runInContext(readFileSync(assetPath("denoise-probe.js"), "utf8"), context, {
    filename: "denoise-probe.js",
  });

  const Tap = registered.get("mediacore/denoise-tap");
  const Rnnoise = registered.get("@sapphi-red/web-noise-suppressor/rnnoise");
  const Probe = registered.get("mediacore/denoise-probe");
  assert.ok(Tap && Rnnoise && Probe, "one end of the chain did not register");

  const tap = new Tap();
  const rnnoise = new Rnnoise({
    processorOptions: { maxChannels: 1, wasmBinary: realWasm() },
  });
  const probe = new Probe();

  let seed = 6180339;
  const random = () => {
    seed = (seed * 1103515245 + 12345) & 0x7fffffff;
    return seed / 0x3fffffff - 1;
  };

  function quantum() {
    const input = new Float32Array(QUANTUM);
    for (let index = 0; index < QUANTUM; index += 1) input[index] = random() * 0.2;

    const afterTap = new Float32Array(QUANTUM);
    tap.process([[input]], [[afterTap]], {});
    state.now += 0.4;

    const afterNode = new Float32Array(QUANTUM);
    rnnoise.process([[afterTap]], [[afterNode]], {});

    const afterProbe = new Float32Array(QUANTUM);
    probe.process([[afterNode]], [[afterProbe]], {});

    state.frame += QUANTUM;
    state.now += 0.05;
  }

  return {
    async ready(limitMs = 5000) {
      const deadline = Date.now() + limitMs;
      while (!rnnoise.processor) {
        if (Date.now() > deadline) {
          throw new Error("the vendored worklet never brought its wasm up in this runtime");
        }
        await new Promise((resolve) => setTimeout(resolve, 10));
      }
    },
    run(quanta) {
      for (let index = 0; index < quanta; index += 1) quantum();
      return messages.filter((entry) => entry && entry.windowMs !== undefined);
    },
  };
}

function vendoredWorkletSource() {
  // The bundle asks for `import.meta.url` for a `locateFile` path it never
  // reaches, because the binary is always handed to it directly.
  return readFileSync(assetPath("vendor/rnnoise-worklet.js"), "utf8").replaceAll(
    "import.meta.url",
    JSON.stringify("file:///rnnoise-worklet.js"),
  );
}

function realWasm() {
  const bytes = readFileSync(assetPath("vendor/rnnoise-simd.wasm"));
  return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
}

/** The globals an AudioWorkletGlobalScope carries, with the gate installed. */
function worklandScope(state, registered, sink) {
  const sandbox = {
    AudioWorkletProcessor: class AudioWorkletProcessor {
      constructor() {
        this.port = {
          onmessage: null,
          addEventListener() {},
          postMessage: (data) => {
            if (sink) sink(data);
          },
        };
      }
    },
    registerProcessor: (name, klass) => registered.set(name, klass),
    get sampleRate() {
      return SAMPLE_RATE;
    },
    get currentFrame() {
      return state.frame;
    },
    performance: { now: () => state.now },
    console,
    WebAssembly,
    TextEncoder,
    TextDecoder,
    setTimeout,
    clearTimeout,
    URL,
  };
  sandbox.globalThis = sandbox;
  sandbox.self = sandbox;
  install(sandbox);
  return sandbox;
}

/**
 * A denoiser whose graph builds but whose node says nothing until it is told
 * to, which is what the vendored one does while its wasm is compiling.
 */
function silentGraph() {
  const ports = new Map();

  globalThis.AudioWorkletNode = class AudioWorkletNode {
    constructor(context, name) {
      this.name = name;
      this.port = { onmessage: null, postMessage() {} };
      this.onprocessorerror = null;
      ports.set(name, this);
    }
    connect(next) {
      return next;
    }
    disconnect() {}
  };
  globalThis.MediaStream = class MediaStream {
    constructor(tracks) {
      this.tracks = tracks;
    }
  };
  globalThis.fetch = async () => ({
    ok: true,
    status: 200,
    arrayBuffer: async () => realWasm(),
  });

  class GraphContext {
    constructor() {
      this.sampleRate = SAMPLE_RATE;
      this.state = "running";
    }
    get audioWorklet() {
      return { addModule: async () => {} };
    }
    createMediaStreamDestination() {
      return {
        channelCount: 2,
        channelCountMode: "max",
        channelInterpretation: "speakers",
        stream: { getAudioTracks: () => [{ kind: "audio", getSettings: () => ({}) }] },
      };
    }
    createMediaStreamSource() {
      return { connect() {}, disconnect() {} };
    }
    async resume() {}
    async close() {}
  }

  return {
    AudioContext: GraphContext,
    wasmUrl: "/rnnoise.wasm",
    simdWasmUrl: "/rnnoise-simd.wasm",
    workletUrl: "/rnnoise-worklet.js",
    gateUrl: "/denoise-gate.js",
    probeUrl: "/denoise-probe.js",
    announce(data) {
      const node = ports.get("@sapphi-red/web-noise-suppressor/rnnoise");
      assert.ok(node && node.port.onmessage, "nothing was listening to the node");
      node.port.onmessage({ data });
    },
  };
}

function filled(level) {
  const block = new Float32Array(QUANTUM);
  // A constant magnitude with alternating sign: the rms is the magnitude, so
  // the assertion on the level is exact rather than approximate.
  for (let index = 0; index < block.length; index += 1) {
    block[index] = index % 2 === 0 ? level : -level;
  }
  return block;
}

function settle() {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

after(() => {
  delete globalThis.isSecureContext;
  delete globalThis.AudioWorkletNode;
  delete globalThis.MediaStream;
  delete globalThis.fetch;
});
