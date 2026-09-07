/**
 * The level meter: one AudioWorklet processor that measures every sample of
 * every block on the audio thread and reports what it found.
 *
 *     participant's track -> mediacore/level-meter -> a stream nobody listens to
 *
 * It exists because the thing it replaces judged a sliver of the audio and
 * guessed at the rest. An `AnalyserNode` with `fftSize` 512 holds 21.3 ms in a
 * 24 kHz context, and the page read it once every 150 ms: between 7 and 14 per
 * cent of the sound was ever looked at, and which 7 per cent was decided by
 * whenever a `setInterval` happened to fire. Chrome throttles that timer to
 * about once a second in a background tab, which takes it to two per cent --
 * measured, on one loud continuous source, the foreground tab said "listening"
 * 0 % of the time and the background tab 67 %. The roster changed its mind
 * about whether somebody was talking depending on which window had focus.
 *
 * Two shapes fix that. The lesser one is a much larger analyser window plus a
 * rolling peak kept between polls, which loses nothing *within* a poll but
 * still runs on a main-thread timer, so a browser that decides to run that
 * timer once a second still decides how much of the audio is judged. The one
 * chosen is this: the render thread is the only place that sees every block by
 * construction, it is never throttled by tab visibility because it is feeding a
 * sound card in real time, and it cannot miss a block without the audio itself
 * breaking up. The measurement is therefore complete by construction rather
 * than by luck, and the main thread's job shrinks to reading messages.
 *
 * This crate already runs worklets -- `denoise.js` drives RNNoise in one and
 * `denoise-probe.js` measures either side of it -- and this is registered, fed
 * and stopped exactly the way those are, down to the `"stop"` message that lets
 * the processor return `false` and be collected.
 *
 * What goes back to the page, about twice every tenth of a second:
 *
 *     {
 *       audioMs,     total audio produced since this meter started
 *       windowMs,    audio in this report
 *       samples,     samples actually measured (0 when nothing arrived)
 *       rms,         null when samples is 0 -- absent, not silent
 *       peak,        null when samples is 0
 *       sampleRate,
 *     }
 *
 * `audioMs` and `windowMs` count the blocks this node was asked for, not the
 * context's frame counter. The frame counter is the audio device's clock and
 * advances whatever the render thread is doing; these two say how much audio
 * this node really saw, which is the time `voice.js` reasons in.
 *
 * `rms` is null rather than zero when the browser handed over a block with no
 * channels, which is what it does while a track is disabled. "Nothing arrived"
 * and "silence arrived" are different facts, and a meter that reported the
 * second when it meant the first would be inventing a measurement. What the
 * gate downstream does with them is its own business -- today it keeps both out
 * of its estimate of the room, because neither is one -- but that is a decision
 * it can only take if it is told which it got.
 *
 * The module imports nothing, because an AudioWorklet module script cannot, and
 * it builds its processor class from a scope passed in rather than from the
 * globals directly -- so the same file can be loaded by `addModule` in the
 * browser, imported by `app.js` on the main thread for the processor's name,
 * and driven a block at a time by the tests with a scope they made up.
 */

export const LEVEL_METER_PROCESSOR = "mediacore/level-meter";

/**
 * How much audio goes into one report. Fifty milliseconds is short against a
 * syllable, so the gate downstream sees the shape of speech rather than an
 * average of a phrase, and long enough that ten people in a room cost twenty
 * messages a second each rather than four hundred.
 */
export const REPORT_EVERY_MS = 50;

/** A render quantum, fixed by the specification. Used only to count blocks that arrived empty. */
const QUANTUM = 128;

/**
 * Builds the processor against a scope, which in the browser is the
 * `AudioWorkletGlobalScope` itself: `AudioWorkletProcessor` to extend and
 * `sampleRate` to read. Taking them from an object instead of the module's own
 * globals is what lets this class be constructed anywhere, including a test
 * that has neither.
 */
export function levelMeter(scope) {
  const Base = scope && scope.AudioWorkletProcessor;
  if (typeof Base !== "function") {
    throw new Error("this scope has no AudioWorkletProcessor, so no level meter can be built");
  }

  return class LevelMeter extends Base {
    constructor(options) {
      super(options);
      const settings = (options && options.processorOptions) || {};
      this.reportEveryMs =
        typeof settings.reportEveryMs === "number" && settings.reportEveryMs > 0
          ? settings.reportEveryMs
          : REPORT_EVERY_MS;
      this.alive = true;
      this.producedFrames = 0;
      this.reset();
      this.port.onmessage = (event) => {
        if (event.data === "stop") this.alive = false;
      };
    }

    reset() {
      this.energy = 0;
      this.peak = 0;
      this.samples = 0;
      this.frames = 0;
    }

    /**
     * Every sample of every block, and no output written. The node's output
     * stays silent on purpose: it is connected to a sink so the graph pulls it,
     * and anything it wrote there would be a second path to a device the
     * participant is already audible on.
     */
    process(inputs) {
      const block = inputs && inputs[0] && inputs[0][0];

      if (block && block.length > 0) {
        for (let index = 0; index < block.length; index += 1) {
          const value = block[index];
          this.energy += value * value;
          const size = value < 0 ? -value : value;
          if (size > this.peak) this.peak = size;
        }
        this.samples += block.length;
        this.frames += block.length;
        this.producedFrames += block.length;
      } else {
        this.frames += QUANTUM;
        this.producedFrames += QUANTUM;
      }

      const rate = scope.sampleRate;
      if ((this.frames / rate) * 1000 >= this.reportEveryMs) this.report(rate);

      return this.alive;
    }

    report(rate) {
      this.port.postMessage({
        audioMs: (this.producedFrames / rate) * 1000,
        windowMs: (this.frames / rate) * 1000,
        samples: this.samples,
        rms: this.samples > 0 ? Math.sqrt(this.energy / this.samples) : null,
        peak: this.samples > 0 ? this.peak : null,
        sampleRate: rate,
      });
      this.reset();
    }
  };
}

export function install(scope) {
  scope.registerProcessor(LEVEL_METER_PROCESSOR, levelMeter(scope));
}

// A module loaded with `addModule` runs here; the same file is imported by
// `app.js` and by the tests, where there is no worklet scope and nothing to do.
if (typeof AudioWorkletProcessor === "function" && typeof registerProcessor === "function") {
  install(globalThis);
}
