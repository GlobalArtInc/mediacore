/**
 * The denoiser this page runs between the microphone and the encoder: RNNoise,
 * compiled to WebAssembly, driven from an AudioWorklet.
 *
 * It has to be here rather than on the server. The SFU forwards encoded packets
 * and never decodes them, so a denoiser there would make it a transcoder and
 * cost roughly a core per participant. Here, it runs once, on the machine that
 * owns the microphone, before Opus ever sees the signal.
 *
 * It replaces the browser's noise suppression and voice isolation, and only
 * those. Echo cancellation stays the browser's job and stays on: the canceller
 * needs the render-side reference in the capture clock domain, which a worklet
 * in this graph does not have. Chrome's own capture chain puts its noise
 * suppressor after the echo canceller, and this node goes in exactly that slot,
 * because `getUserMedia` has already cancelled echo by the time a track reaches
 * `createMediaStreamSource`.
 *
 * Nothing here reaches a third party. The wasm and all three worklets are
 * served by the front end from its own origin.
 *
 * Usage:
 *
 *     import { createDenoiser } from "/denoise.js";
 *
 *     const denoiser = createDenoiser({
 *       wasmUrl, simdWasmUrl, workletUrl, gateUrl, probeUrl,
 *     });
 *
 *     const { available, reason } = await denoiser.prepare();
 *     const processor = denoiser.create({ onMetrics, onOverload });
 *     await localAudioTrack.setProcessor(processor);   // livekit-client
 *
 * `prepare()` is the honest gate: it runs once, before anything is offered to a
 * person, and answers whether this browser can run the node at all. Everything
 * it can check without touching the microphone it checks -- Web Audio, the
 * worklet, WebAssembly, whether a graph can be run at the one sample rate this
 * model is, whether the binary can even be fetched from us -- so a machine that
 * cannot run it never gets the option rather than getting an option that
 * silently does nothing.
 *
 * `create()` returns a livekit-client `TrackProcessor`: `init`, `restart`,
 * `destroy` and a `processedTrack` the SDK swaps onto the sender in place of
 * the capture track. The publication and every subscription survive it.
 *
 * **`init` does not resolve until the wasm is live.** The vendored processor
 * compiles its binary asynchronously inside its own constructor and writes
 * nothing to its output until that finishes, so a `processedTrack` handed over
 * before then is a track carrying silence -- for a few hundred milliseconds if
 * the compile succeeds, and for ever if it does not. `denoise-gate.js` makes
 * the node say when it has written its first block; nothing is published until
 * it has, and a node that never does is a failed attach, which leaves the call
 * on the browser's own suppressor.
 *
 * `onMetrics(reading)` fires about twice a second with what the render thread
 * measured; `onOverload({ reason })` fires once when it stops keeping up.
 */

// Relative, not rooted: this module is served from the page root, so it lands on
// /denoise-gate.js either way, and the module can also be loaded straight off
// disk by the tests. The gate itself imports nothing, because an AudioWorklet
// module script cannot.
import { READY, STALLED } from "./denoise-gate.js";

const RNNOISE_PROCESSOR = "@sapphi-red/web-noise-suppressor/rnnoise";
const TAP_PROCESSOR = "mediacore/denoise-tap";
const PROBE_PROCESSOR = "mediacore/denoise-probe";

/** RNNoise is a 48 kHz model. At any other rate it is not the thing we tested. */
export const REQUIRED_SAMPLE_RATE = 48000;

/**
 * How long the node gets to compile its wasm and write its first block before
 * the attach is called a failure. It is generous on purpose: the cost of
 * waiting is a few hundred milliseconds on the browser's own suppressor, and
 * the cost of giving up early is a denoiser that works and was refused.
 */
export const READY_TIMEOUT_MS = 8000;

/**
 * How far behind real time the render thread may fall before we take the node
 * out. This is the trigger that works everywhere, because it is measured on the
 * main thread, where there is always a clock: the audio the render thread
 * actually emitted against the wall time it took to emit it. A graph that
 * cannot keep up emits audio slower than real time, and that is what a call
 * breaking up sounds like.
 */
const OVERLOAD_BEHIND = 1.25;
const OVERLOAD_BEHIND_MS = 3000;

/**
 * Sustained share of *real* time the render thread spent inside the node.
 * Against wall time, not against the audio clock: the audio clock is the
 * device's, it advances at real time whatever the thread is doing, and a
 * fraction measured against it cannot rise when the thread starts losing.
 */
const OVERLOAD_LOAD = 0.5;
const OVERLOAD_LOAD_MS = 3000;

/** Fraction of quanta whose cost alone exceeded the whole quantum. */
const OVERLOAD_OVER_BUDGET = 0.02;
const OVERLOAD_OVER_BUDGET_MS = 5000;

const SIMD_PROBE = new Uint8Array([
  0, 97, 115, 109, 1, 0, 0, 0, 1, 5, 1, 96, 0, 1, 123, 3, 2, 1, 0, 10, 10, 1, 8, 0, 65, 0, 253,
  15, 253, 98, 11,
]);

export function createDenoiser(options = {}) {
  const { wasmUrl, simdWasmUrl, workletUrl, gateUrl, probeUrl } = options;
  const AudioContextClass =
    options.AudioContext || globalThis.AudioContext || globalThis.webkitAudioContext;
  const readyTimeoutMs =
    typeof options.readyTimeoutMs === "number" ? options.readyTimeoutMs : READY_TIMEOUT_MS;

  let probed = null;

  function unavailable(reason) {
    return { available: false, reason, binary: null, simd: false };
  }

  /** Runs once. Later calls get the same answer without a second fetch. */
  function prepare() {
    if (!probed) probed = probeSupport();
    return probed;
  }

  async function probeSupport() {
    if (!AudioContextClass) {
      return unavailable("this browser gives the page no Web Audio, so no node can be inserted");
    }
    if (typeof AudioWorkletNode !== "function" || !("audioWorklet" in AudioContextClass.prototype)) {
      return unavailable(
        "this browser has no AudioWorklet, so audio cannot be processed off the main thread",
      );
    }
    if (
      typeof WebAssembly !== "object" ||
      typeof WebAssembly.instantiate !== "function" ||
      typeof WebAssembly.validate !== "function"
    ) {
      return unavailable("this browser will not run WebAssembly, which the denoiser is");
    }
    if (typeof fetch !== "function") {
      return unavailable("this browser cannot fetch the denoiser from this server");
    }

    const rate = await probeSampleRate();
    if (rate !== REQUIRED_SAMPLE_RATE) {
      return unavailable(
        `this browser will not run an audio graph at ${REQUIRED_SAMPLE_RATE} Hz (it offered ` +
          `${rate === null ? "no graph at all" : `${rate} Hz`}) and RNNoise is a 48 kHz model, so ` +
          "it would not be the denoiser we tested",
      );
    }

    const simd = WebAssembly.validate(SIMD_PROBE);
    const url = simd ? simdWasmUrl : wasmUrl;

    let binary;
    try {
      const response = await fetch(url);
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      binary = await response.arrayBuffer();
    } catch (error) {
      return unavailable(
        `the denoiser could not be loaded from this server (${message(error)}), so the browser's ` +
          "own suppressor is all there is",
      );
    }

    if (!binary || binary.byteLength === 0) {
      return unavailable("this server served an empty denoiser");
    }
    if (!WebAssembly.validate(new Uint8Array(binary))) {
      return unavailable(
        "this browser rejected the denoiser binary this server served, so it will not be run",
      );
    }

    return { available: true, reason: null, binary, simd };
  }

  /**
   * The rate is part of the gate, not a surprise at attach time: a browser that
   * will not give this page a 48 kHz graph cannot run this model, and finding
   * that out only once somebody has chosen it is how an option that does
   * nothing gets offered. The context is opened and closed here and never
   * carries audio.
   */
  async function probeSampleRate() {
    let context;
    try {
      context = new AudioContextClass({ sampleRate: REQUIRED_SAMPLE_RATE });
    } catch {
      return null;
    }
    const rate = typeof context.sampleRate === "number" ? context.sampleRate : null;
    if (typeof context.close === "function") {
      try {
        await context.close();
      } catch {
        // Nothing was published through it; a context that will not close is
        // the browser's problem and not this answer's.
      }
    }
    return rate;
  }

  /**
   * A livekit-client TrackProcessor. The SDK calls `init` when it is attached,
   * `restart` when the capture track behind it is replaced -- a device change,
   * or a constraint change -- and `destroy` when it is taken off.
   */
  function create(hooks = {}) {
    const watch = createOverloadWatch(hooks.onOverload);

    let context = null;
    let ownsContext = false;
    let source = null;
    let tap = null;
    let denoise = null;
    let probe = null;
    let destination = null;
    let processedTrack = null;
    let latest = null;

    // Anything that fails part way through leaves an audio context holding a
    // microphone open, so a failed build cleans up after itself rather than
    // relying on whoever catches the error to know that.
    async function build(track) {
      try {
        await assemble(track);
      } catch (error) {
        await teardown();
        throw error;
      }
    }

    async function assemble(track) {
      const support = await prepare();
      if (!support.available) throw new DenoiserUnavailable(support.reason);

      context = new AudioContextClass({ sampleRate: REQUIRED_SAMPLE_RATE });
      ownsContext = true;

      if (context.sampleRate !== REQUIRED_SAMPLE_RATE) {
        const chosen = context.sampleRate;
        throw new DenoiserUnavailable(
          `this browser will not run an audio graph at ${REQUIRED_SAMPLE_RATE} Hz (it chose ` +
            `${chosen} Hz) and RNNoise is a 48 kHz model, so it would not be the denoiser we tested`,
        );
      }

      // The gate goes in first: it wraps `registerProcessor`, so it has to be
      // in the scope before the module that calls it.
      await context.audioWorklet.addModule(gateUrl);
      await context.audioWorklet.addModule(workletUrl);
      await context.audioWorklet.addModule(probeUrl);

      // RNNoise is mono. Down-mixing here rather than hoping the capture is
      // mono keeps a stereo device from having one channel denoised and the
      // other passed through untouched.
      const mono = {
        numberOfInputs: 1,
        numberOfOutputs: 1,
        outputChannelCount: [1],
        channelCount: 1,
        channelCountMode: "explicit",
        channelInterpretation: "speakers",
      };

      tap = new AudioWorkletNode(context, TAP_PROCESSOR, mono);
      denoise = new AudioWorkletNode(context, RNNOISE_PROCESSOR, {
        ...mono,
        processorOptions: { maxChannels: 1, wasmBinary: support.binary },
      });
      probe = new AudioWorkletNode(context, PROBE_PROCESSOR, mono);
      destination = context.createMediaStreamDestination();

      // A MediaStreamAudioDestinationNode is two channels by default, and a
      // mono graph connected to it is up-mixed rather than left alone. The page
      // asked getUserMedia for one channel; publishing two of the same thing
      // would undo that on the way out.
      monoDestination(destination);

      const live = firstBlock();
      // It is awaited below. This only keeps a build that fails between here
      // and there from leaving a rejection nobody is holding.
      live.catch(() => {});

      for (const node of [tap, denoise, probe]) {
        node.onprocessorerror = () => onProcessorError();
      }

      tap.connect(denoise).connect(probe).connect(destination);
      connectSource(track);

      if (context.state === "suspended") await context.resume();

      await live;

      probe.port.onmessage = (event) => onReading(event.data);

      processedTrack = destination.stream.getAudioTracks()[0] || null;
      if (!processedTrack) {
        throw new DenoiserUnavailable(
          "this browser produced no track out of the denoiser graph, so nothing would be published",
        );
      }
    }

    /**
     * Resolves when the gated node has written its first block of audio, which
     * is the first moment publishing its output is publishing anything at all.
     */
    function firstBlock() {
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
          settle(
            new DenoiserUnavailable(
              `the denoiser did not start within ${Math.round(readyTimeoutMs / 1000)} seconds in ` +
                "this browser, and a node that has not started publishes silence",
            ),
          );
        }, readyTimeoutMs);

        function settle(error) {
          clearTimeout(timer);
          if (denoise) denoise.port.onmessage = null;
          if (error) reject(error);
          else resolve();
        }

        denoise.port.onmessage = (event) => {
          const type = event.data && event.data.type;
          if (type === READY) settle(null);
          if (type === STALLED) {
            settle(
              new DenoiserUnavailable(
                "the denoiser's WebAssembly never came up in this browser, so the node was " +
                  "passing silence and was taken back out",
              ),
            );
          }
        };
      });
    }

    function connectSource(track) {
      if (source) source.disconnect();
      source = context.createMediaStreamSource(new MediaStream([track]));
      source.connect(tap);
    }

    function onReading(raw) {
      const reading = measureAgainst(latest, readingFrom(raw));
      latest = reading;
      watch.consider(reading);
      if (hooks.onMetrics) hooks.onMetrics(reading);
    }

    // A worklet that throws is dead for the rest of its life and its node
    // passes silence, so this is not a warning, it is the fallback trigger.
    function onProcessorError() {
      watch.trip(
        "the denoiser's audio worklet stopped in this browser, so it was taken out of the path",
      );
    }

    async function closeContext() {
      const closing = context;
      context = null;
      if (!closing || !ownsContext) return;
      ownsContext = false;
      try {
        await closing.close();
      } catch (error) {
        console.warn("the denoiser's audio context would not close", error);
      }
    }

    async function teardown() {
      if (probe) {
        probe.port.onmessage = null;
        try {
          probe.port.postMessage("stop");
        } catch {
          // The port is gone with the context; there is nothing to stop.
        }
      }
      if (denoise) {
        denoise.port.onmessage = null;
        try {
          denoise.port.postMessage("destroy");
        } catch {
          // Same.
        }
      }
      for (const node of [source, tap, denoise, probe]) {
        if (node) node.disconnect();
      }
      source = null;
      tap = null;
      denoise = null;
      probe = null;
      destination = null;
      processedTrack = null;
      await closeContext();
    }

    return {
      name: "mediacore-rnnoise",

      get processedTrack() {
        return processedTrack;
      },

      async init(opts) {
        await build(opts.track);
      },

      // The SDK hands over the newly captured track. Swapping the source keeps
      // the same processed track on the sender, so a device change does not
      // renegotiate and nobody is unsubscribed.
      async restart(opts) {
        if (!context) {
          await build(opts.track);
          return;
        }
        connectSource(opts.track);
        latest = null;
        watch.reset();
      },

      async destroy() {
        await teardown();
      },

      metrics() {
        return latest;
      },
    };
  }

  return { prepare, create };
}

/**
 * A mono sink. `createMediaStreamDestination` takes no options, so the channel
 * count is set on the node afterwards; a browser that refuses is left alone
 * rather than being argued with, and the reading either side will show it.
 */
export function monoDestination(destination) {
  if (!destination) return destination;
  try {
    destination.channelCount = 1;
    destination.channelCountMode = "explicit";
    destination.channelInterpretation = "speakers";
  } catch (error) {
    console.warn("this browser will not take a one-channel destination", error);
  }
  return destination;
}

/**
 * Turns one report from the render thread into the numbers the page shows.
 * Levels are dBFS; a level with no samples behind it stays null rather than
 * becoming a very quiet number. Exported so it can be tested against a report
 * without a browser to produce one.
 *
 * `cost.load` is deliberately left null here: what share of real time the node
 * cost cannot be worked out from one report, because the only clock that
 * measures real time is on the main thread. `measureAgainst` fills it in.
 */
export function readingFrom(raw) {
  const input = raw && raw.input ? raw.input : null;
  const output = raw && raw.output ? raw.output : null;
  const cost = raw && raw.cost ? raw.cost : null;
  const sampleRate = numberOrNull(raw && raw.sampleRate);
  const delaySamples = numberOrNull(raw && raw.delaySamples);

  return {
    at: Date.now(),
    windowMs: numberOrNull(raw && raw.windowMs),
    producedMs: numberOrNull(raw && raw.producedMs),
    sampleRate,
    quanta: numberOrNull(raw && raw.quanta),
    clock: raw && typeof raw.clock === "string" ? raw.clock : "none",
    coarseClock: Boolean(raw && raw.coarseClock),
    input: {
      rms: decibels(input && input.rms),
      peak: decibels(input && input.peak),
    },
    output: {
      rms: decibels(output && output.rms),
      peak: decibels(output && output.peak),
    },
    reduction:
      input && output && input.rms > 0 && output.rms > 0
        ? 20 * Math.log10(output.rms / input.rms)
        : null,
    delay:
      delaySamples !== null && sampleRate !== null && sampleRate > 0
        ? {
            samples: delaySamples,
            ms: (delaySamples / sampleRate) * 1000,
            correlation: numberOrNull(raw && raw.delayCorrelation),
          }
        : null,
    cost: cost
      ? {
          totalMs: numberOrNull(cost.totalMs),
          frameMs: numberOrNull(cost.frameMs),
          load: null,
          worstQuantumMs: numberOrNull(cost.worstQuantumMs),
          overBudget: numberOrNull(cost.overBudget),
          overBudgetShare:
            typeof cost.overBudget === "number" && cost.measuredQuanta > 0
              ? cost.overBudget / cost.measuredQuanta
              : null,
        }
      : null,
    costNote: costNoteFor(raw, cost),
    realtime: null,
  };
}

/**
 * Fills in the two figures that need two readings and a clock the render thread
 * does not have: whether the graph is keeping up, and what share of real time
 * the node cost. Both are null on the first reading of a run, because there is
 * nothing to measure an interval against yet, and a made-up 1.0 there would
 * read as a healthy machine.
 */
export function measureAgainst(previous, reading) {
  reading.realtime = keepingUp(previous, reading);
  if (reading.cost && reading.realtime && reading.realtime.wallMs > 0) {
    reading.cost.load =
      reading.cost.totalMs === null ? null : reading.cost.totalMs / reading.realtime.wallMs;
  }
  return reading;
}

function costNoteFor(raw, cost) {
  const clock = raw && typeof raw.clock === "string" ? raw.clock : "none";
  if (clock === "none") {
    return (
      "this browser's audio render thread exposes no clock at all, so what the node costs cannot " +
      "be measured from here and the page will not guess at it"
    );
  }
  if (!cost) return "the render thread reported no completed measurement in this window";
  if (raw.coarseClock) {
    return (
      `this browser's audio render thread has only a ${raw.clockResolutionMs} ms clock, so this ` +
      "is the total it was seen inside a block over the whole window, spread across it. It is an " +
      "estimate, and a single block's cost cannot be recovered from it"
    );
  }
  return null;
}

/**
 * Whether the render thread is producing audio as fast as the world consumes
 * it. The numerator is the audio the node actually emitted, counted in the
 * blocks it was asked for; the denominator is the wall clock between two
 * readings, measured here, on a thread that has one.
 *
 * It is deliberately not `windowMs`. That is the context's frame counter, which
 * is the audio device's clock: it advances at real time no matter how far
 * behind the render thread is, so a ratio built on it is pinned at 1.0 and can
 * never report a machine that is losing.
 *
 * The first reading of a run has nothing to compare against and reports null
 * rather than a made-up 1.0.
 */
export function keepingUp(previous, reading) {
  if (!previous || previous.producedMs === null || reading.producedMs === null) return null;

  const wallMs = reading.at - previous.at;
  const audioMs = reading.producedMs;
  if (!(wallMs > 0) || !(audioMs > 0)) return null;

  return { wallMs, audioMs, behind: wallMs / audioMs };
}

/**
 * Watches the readings and gives up once, loudly, rather than letting a machine
 * that cannot keep up stutter through a call. It never re-arms itself: a
 * suppressor that switches itself on and off mid-sentence is worse than either
 * state, so coming back is a decision a person makes.
 *
 * Exported so the thresholds can be driven against made-up readings without a
 * machine slow enough to produce real ones.
 */
export function createOverloadWatch(onOverload) {
  let behindSince = null;
  let loadedSince = null;
  let overBudgetSince = null;
  let tripped = false;

  function reset() {
    behindSince = null;
    loadedSince = null;
    overBudgetSince = null;
  }

  function trip(reason) {
    if (tripped) return;
    tripped = true;
    if (onOverload) onOverload({ reason, at: Date.now() });
  }

  function consider(reading) {
    if (tripped || !reading) return;

    const now = reading.at;

    const realtime = reading.realtime;
    if (realtime && realtime.behind >= OVERLOAD_BEHIND) {
      if (behindSince === null) behindSince = now;
      if (now - behindSince >= OVERLOAD_BEHIND_MS) {
        trip(
          `this machine's audio thread took ${Math.round(realtime.behind * 100)} % of real time to ` +
            "produce the audio for three seconds together, which is a call starting to break up",
        );
        return;
      }
    } else if (realtime) {
      behindSince = null;
    }

    if (!reading.cost) return;

    if (reading.cost.load !== null && reading.cost.load >= OVERLOAD_LOAD) {
      if (loadedSince === null) loadedSince = now;
      if (now - loadedSince >= OVERLOAD_LOAD_MS) {
        trip(
          `the denoiser needed ${Math.round(reading.cost.load * 100)} % of this machine's audio ` +
            "budget for three seconds together, which is more than it can keep up with",
        );
        return;
      }
    } else {
      loadedSince = null;
    }

    const share = reading.cost.overBudgetShare;
    if (share !== null && share >= OVERLOAD_OVER_BUDGET) {
      if (overBudgetSince === null) overBudgetSince = now;
      if (now - overBudgetSince >= OVERLOAD_OVER_BUDGET_MS) {
        trip(
          `the denoiser ran past a whole audio block on ${Math.round(share * 100)} % of them for ` +
            "five seconds together, which is the shape of audio starting to break up",
        );
      }
    } else {
      overBudgetSince = null;
    }
  }

  return { consider, reset, trip };
}

export class DenoiserUnavailable extends Error {
  constructor(reason) {
    super(reason);
    this.name = "DenoiserUnavailable";
  }
}

function decibels(amplitude) {
  if (typeof amplitude !== "number" || !Number.isFinite(amplitude) || amplitude <= 0) return null;
  return 20 * Math.log10(amplitude);
}

function numberOrNull(value) {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function message(error) {
  return (error && (error.message || error.name)) || String(error);
}
