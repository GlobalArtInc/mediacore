/**
 * Two pass-through AudioWorklet processors that sit either side of the
 * denoiser, so the page can report what it actually did rather than claim it.
 *
 *     source -> mediacore/denoise-tap -> rnnoise -> mediacore/denoise-probe -> destination
 *
 * The tap sees the microphone as the browser handed it over; the probe sees
 * what comes out of the denoiser. Both run in the same AudioWorkletGlobalScope,
 * which is why they can share `shared` below: one module file, one scope, one
 * render thread.
 *
 * Three things are measured here and nothing is assumed.
 *
 * **Cost.** The wall-clock time between the tap returning and the probe being
 * entered inside the same render quantum. The graph between those two points is
 * the denoiser and nothing else, so that time is the denoiser's own plus the
 * render thread's dispatch between two nodes. It is reported as a total and a
 * count of quanta; what fraction of *real* time that is gets worked out on the
 * main thread, which is the only place with a clock that is not the audio
 * clock. See `keepingUp` in `denoise.js` for why that matters.
 *
 * **Delay.** The samples between the signal entering the node and coming back
 * out of it, found by correlating the probe's newest block against the tap's
 * recent history. This is the number that decides whether a call feels
 * responsive, and it is the one figure here that cannot be reasoned out of the
 * graph: the vendored worklet buffers internally and does not say by how much.
 * The search is spread across render quanta -- a few lags each -- so it never
 * costs a whole block, and it reports nothing at all rather than a guess when
 * there is not enough signal either side to correlate.
 *
 * **Level**, either side, which needs no clock and is reported either way.
 *
 * There is no good clock on the render thread. `performance` is not part of the
 * AudioWorklet specification and Chrome does not expose it (measured, Chrome
 * 148 on macOS: `performance` is `undefined` inside `AudioWorkletGlobalScope`,
 * and `AudioContext.renderCapacity` does not exist either). `Date.now` is
 * there, and it ticks in whole milliseconds. So:
 *
 *   - where `performance.now` exists the cost is measured directly, per block;
 *   - where only `Date.now` does, the report says the clock is coarse and
 *     carries the total number of milliseconds the node was found to be inside
 *     a block over the window. Summed over hundreds of blocks that total is an
 *     unbiased estimate of the real one, because the tick boundary falls where
 *     it likes; a single block's cost and a worst-block figure are not
 *     recoverable from it, and are reported as absent rather than as a
 *     rounded-down zero;
 *   - where neither exists there is no cost at all, `clock` is "none", and the
 *     page says so.
 */

const TAP_ID = "mediacore/denoise-tap";
const PROBE_ID = "mediacore/denoise-probe";

const QUANTUM = 128;
const RNNOISE_FRAME = 480;
const REPORT_EVERY_MS = 500;

/**
 * The delay search. `RING` holds both sides' recent history; a pass correlates
 * the newest `DELAY_WINDOW` output samples against the input at every lag up to
 * `DELAY_MAX`, `LAGS_PER_QUANTUM` lags at a time so no single render quantum
 * pays for the whole search. At 48 kHz `DELAY_MAX` is 42 ms, which is far more
 * than any sane node adds, and a full pass takes about 0.7 s.
 */
const RING = 4096;
const DELAY_WINDOW = 512;
const DELAY_MAX = 2048;
const LAGS_PER_QUANTUM = 8;

/** Below this there is not enough signal either side to align anything. */
const DELAY_FLOOR_RMS = 0.0005;

/** Below this the best lag found is not distinguishable from a coincidence. */
const DELAY_CONFIDENCE = 0.5;

const CLOCK = chooseClock();

function chooseClock() {
  if (typeof performance === "object" && performance && typeof performance.now === "function") {
    return { kind: "performance", coarse: false, resolutionMs: 0, now: () => performance.now() };
  }
  if (typeof Date === "function" && typeof Date.now === "function") {
    return { kind: "date", coarse: true, resolutionMs: 1, now: () => Date.now() };
  }
  return { kind: "none", coarse: true, resolutionMs: null, now: null };
}

const shared = {
  frame: -1,
  tapReturnedAt: 0,
  input: level(),
  dry: new Float32Array(RING),
  wet: new Float32Array(RING),
  write: 0,
  aligned: false,
};

function level() {
  return { energy: 0, peak: 0, samples: 0 };
}

function measure(into, channel) {
  let energy = 0;
  let peak = 0;
  for (let index = 0; index < channel.length; index += 1) {
    const value = channel[index];
    energy += value * value;
    const size = value < 0 ? -value : value;
    if (size > peak) peak = size;
  }
  into.energy += energy;
  into.samples += channel.length;
  if (peak > into.peak) into.peak = peak;
}

function summarise(from) {
  if (from.samples === 0) return null;
  return { rms: Math.sqrt(from.energy / from.samples), peak: from.peak, samples: from.samples };
}

/**
 * Copies the input to the output and returns the block it saw, or null when the
 * browser handed over an input with no channels -- which is what it does while
 * the track is disabled. Silence is the honest output then, and it is not a
 * level measurement.
 */
function passThrough(inputs, outputs, into) {
  const from = inputs[0] && inputs[0][0];
  const to = outputs[0] && outputs[0][0];

  if (from && from.length > 0) {
    if (into) measure(into, from);
    if (to) to.set(from);
    return from;
  }

  if (to) to.fill(0);
  return null;
}

function writeRing(ring, at, block) {
  for (let index = 0; index < QUANTUM; index += 1) {
    ring[(at + index) % RING] = block ? block[index] : 0;
  }
}

/**
 * One pass of the delay search, spread over render quanta. The history either
 * side is snapshotted when the pass starts, so the rings can go on being
 * written underneath it without the answer drifting.
 *
 * The snapshots are allocated once, here, and refilled: allocating twelve
 * kilobytes twice a second on the render thread is exactly the kind of thing
 * that makes the figure this node reports about itself untrue.
 */
const pass = {
  wet: new Float32Array(DELAY_WINDOW),
  dry: new Float32Array(DELAY_WINDOW + DELAY_MAX),
  wetEnergy: 0,
  lag: 0,
  bestLag: null,
  best: 0,
  running: false,
};

function startDelayPass(end) {
  for (let index = 0; index < DELAY_WINDOW; index += 1) {
    pass.wet[index] = shared.wet[(end - DELAY_WINDOW + index + RING) % RING];
  }
  for (let index = 0; index < pass.dry.length; index += 1) {
    pass.dry[index] = shared.dry[(end - DELAY_WINDOW - DELAY_MAX + index + RING) % RING];
  }

  let energy = 0;
  for (let index = 0; index < DELAY_WINDOW; index += 1) energy += pass.wet[index] * pass.wet[index];

  if (Math.sqrt(energy / DELAY_WINDOW) < DELAY_FLOOR_RMS) return false;

  pass.wetEnergy = energy;
  pass.lag = 0;
  pass.bestLag = null;
  pass.best = 0;
  pass.running = true;
  return true;
}

function stepDelayPass() {
  const limit = Math.min(pass.lag + LAGS_PER_QUANTUM, DELAY_MAX + 1);

  for (; pass.lag < limit; pass.lag += 1) {
    const offset = DELAY_MAX - pass.lag;
    let dot = 0;
    let energy = 0;
    for (let index = 0; index < DELAY_WINDOW; index += 1) {
      const sample = pass.dry[offset + index];
      dot += sample * pass.wet[index];
      energy += sample * sample;
    }
    if (energy <= 0) continue;
    const correlation = dot / Math.sqrt(energy * pass.wetEnergy);
    if (correlation > pass.best) {
      pass.best = correlation;
      pass.bestLag = pass.lag;
    }
  }

  if (pass.lag <= DELAY_MAX) return null;

  pass.running = false;
  if (pass.bestLag === null || pass.best < DELAY_CONFIDENCE) {
    return { samples: null, correlation: pass.best };
  }
  return { samples: pass.bestLag, correlation: pass.best };
}

class DenoiseTap extends AudioWorkletProcessor {
  process(inputs, outputs) {
    const block = passThrough(inputs, outputs, shared.input);
    writeRing(shared.dry, shared.write, block);
    shared.frame = currentFrame;
    shared.tapReturnedAt = CLOCK.now ? CLOCK.now() : 0;
    return true;
  }
}

class DenoiseProbe extends AudioWorkletProcessor {
  constructor() {
    super();
    this.alive = true;
    this.delay = null;
    this.reset();
    this.port.onmessage = (event) => {
      if (event.data === "stop") this.alive = false;
    };
  }

  reset() {
    this.startedFrame = currentFrame;
    this.output = level();
    this.quanta = 0;
    this.measuredQuanta = 0;
    this.costMs = 0;
    this.worstMs = 0;
    this.overBudget = 0;
    shared.input = level();
  }

  process(inputs, outputs) {
    const enteredAt = CLOCK.now ? CLOCK.now() : 0;
    const paired = shared.frame === currentFrame;

    if (CLOCK.now && paired) {
      const cost = enteredAt - shared.tapReturnedAt;
      if (cost >= 0) {
        this.measuredQuanta += 1;
        this.costMs += cost;
        if (cost > this.worstMs) this.worstMs = cost;
        if (cost > (QUANTUM / sampleRate) * 1000) this.overBudget += 1;
      }
    }

    const block = passThrough(inputs, outputs, this.output);

    if (paired) {
      writeRing(shared.wet, shared.write, block);
      shared.write = (shared.write + QUANTUM) % RING;
      shared.aligned = true;
      this.stepDelay();
    } else {
      shared.aligned = false;
      pass.running = false;
    }

    this.quanta += 1;

    const elapsedMs = ((currentFrame - this.startedFrame) / sampleRate) * 1000;
    if (elapsedMs >= REPORT_EVERY_MS) this.report(elapsedMs);

    return this.alive;
  }

  stepDelay() {
    if (!pass.running) {
      startDelayPass(shared.write);
      return;
    }
    const found = stepDelayPass();
    if (!found) return;
    this.delay = found.samples === null ? null : found;
  }

  report(elapsedMs) {
    const quantumMs = (QUANTUM / sampleRate) * 1000;
    const frames = (this.measuredQuanta * QUANTUM) / RNNOISE_FRAME;
    const measured = CLOCK.now !== null && this.measuredQuanta > 0;

    this.port.postMessage({
      windowMs: elapsedMs,
      // The audio this node actually emitted, counted in blocks it was asked
      // for rather than read off the context's frame counter. A render thread
      // that stops keeping up emits fewer blocks; the frame counter is the
      // audio clock and goes on advancing with the device either way.
      producedMs: (this.quanta * QUANTUM * 1000) / sampleRate,
      sampleRate,
      quanta: this.quanta,
      clock: CLOCK.kind,
      coarseClock: CLOCK.coarse,
      clockResolutionMs: CLOCK.resolutionMs,
      input: summarise(shared.input),
      output: summarise(this.output),
      delaySamples: this.delay ? this.delay.samples : null,
      delayCorrelation: this.delay ? this.delay.correlation : null,
      cost: measured
        ? {
            measuredQuanta: this.measuredQuanta,
            measuredMs: this.measuredQuanta * quantumMs,
            totalMs: this.costMs,
            frameMs: frames > 0 ? this.costMs / frames : null,
            // A one-millisecond tick cannot say what a single block cost, so
            // neither of these is recoverable from it.
            worstQuantumMs: CLOCK.coarse ? null : this.worstMs,
            overBudget: CLOCK.coarse ? null : this.overBudget,
          }
        : null,
    });

    this.reset();
  }
}

registerProcessor(TAP_ID, DenoiseTap);
registerProcessor(PROBE_ID, DenoiseProbe);
