/**
 * Makes the vendored RNNoise worklet say when it is actually denoising.
 *
 * The vendored processor loads its WebAssembly asynchronously from inside its
 * own constructor, and until that resolves its render callback returns without
 * writing to the output at all. A worklet output block starts every quantum
 * zero-filled, so "not ready yet" and "ready and the room is quiet" look
 * identical from outside, and a binary that never instantiates looks the same
 * as both -- for ever. Publishing that track means publishing silence.
 *
 * This module is loaded into the AudioWorkletGlobalScope *before* the vendored
 * one and wraps `registerProcessor`, so the class the vendor registers is
 * subclassed on its way in. The subclass fills the output with NaN before
 * delegating and looks at it afterwards: a block the vendor wrote is no longer
 * NaN, a block it skipped still is. That is a statement about what the node did
 * with the audio, so it survives the vendor renaming its internals. Until the
 * first written block the output is cleared to silence and nothing is
 * published; after it, this wrapper stops touching the audio.
 *
 * Two messages go to the node's port, each at most once:
 *
 *   { type: "mediacore:denoise-ready" }    the wasm is live and writing audio
 *   { type: "mediacore:denoise-stalled" }  it has been fed for STALL_QUANTA
 *                                          render quanta and has written nothing
 *
 * `denoise.js` waits for the first before it hands a track to livekit-client,
 * and treats the second as this browser being unable to run the denoiser.
 */

const RNNOISE_PROCESSOR = "@sapphi-red/web-noise-suppressor/rnnoise";

export const READY = "mediacore:denoise-ready";
export const STALLED = "mediacore:denoise-stalled";

/** 1875 quanta of 128 samples is five seconds at 48 kHz. */
export const STALL_QUANTA = 1875;

export function gated(Vendored) {
  return class GatedRnnoise extends Vendored {
    constructor(options) {
      super(options);
      this.denoising = false;
      this.starvedQuanta = 0;
      this.saidStalled = false;
    }

    process(inputs, outputs, parameters) {
      if (this.denoising) return super.process(inputs, outputs, parameters);

      const block = outputs && outputs[0] && outputs[0][0];
      if (!block) return super.process(inputs, outputs, parameters);

      const input = inputs && inputs[0] && inputs[0][0];
      const fed = Boolean(input && input.length > 0);

      block.fill(Number.NaN);
      const alive = super.process(inputs, outputs, parameters);

      if (!Number.isNaN(block[0])) {
        this.denoising = true;
        this.port.postMessage({ type: READY });
        return alive;
      }

      block.fill(0);
      if (fed && !this.saidStalled) {
        this.starvedQuanta += 1;
        if (this.starvedQuanta >= STALL_QUANTA) {
          this.saidStalled = true;
          this.port.postMessage({ type: STALLED });
        }
      }
      return alive;
    }
  };
}

export function install(scope) {
  const original = scope.registerProcessor;
  if (typeof original !== "function") {
    throw new Error("this scope has no registerProcessor, so no processor can be gated");
  }

  const wrapped = function registerProcessor(name, processor) {
    return original.call(scope, name, name === RNNOISE_PROCESSOR ? gated(processor) : processor);
  };

  scope.registerProcessor = wrapped;
  if (scope.registerProcessor !== wrapped) {
    throw new Error(
      "this browser will not let the denoiser be gated on its wasm being live, and an ungated " +
        "one publishes silence until it compiles",
    );
  }
  return wrapped;
}

// A module loaded with `addModule` runs here; the same file is imported by the
// tests, where there is no worklet scope to patch and nothing to do.
if (typeof AudioWorkletProcessor === "function" && typeof registerProcessor === "function") {
  install(globalThis);
}
