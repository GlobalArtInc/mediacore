# Vendored browser assets

Everything in here is compiled into `mediacore-front` and served from the same origin as the page.
The front end has no runtime dependency on a CDN: a deployment with no route to the internet still
works, the page's content security policy can forbid third-party scripts outright, and nobody can
change what the browser executes by changing what a CDN answers. Every route carries the version of
the thing it serves and is sent `immutable`, so an upgrade can never be answered from a browser's
cache of the previous build.

## `livekit-client.esm.js`

`dist/livekit-client.esm.mjs` from [`livekit-client`](https://github.com/livekit/client-sdk-js)
**2.22.2**, Apache-2.0, with the trailing `sourceMappingURL` comment removed because the map is not
shipped.

To move to another version:

```
curl -fsSLO https://cdn.jsdelivr.net/npm/livekit-client@<version>/dist/livekit-client.esm.mjs
sed 's|//# sourceMappingURL=livekit-client.esm.mjs.map||' livekit-client.esm.mjs \
  > crates/front/assets/vendor/livekit-client.esm.js
```

Then bump `SDK_VERSION` in `crates/front/src/http.rs` and `SDK_URL` in
`crates/front/assets/app.js`.

## `rnnoise.wasm`, `rnnoise-simd.wasm`, `rnnoise-worklet.js`

RNNoise, the denoiser the page can run between the microphone and the encoder. Taken from
[`@sapphi-red/web-noise-suppressor`](https://github.com/sapphi-red/web-noise-suppressor) **0.4.0**
(MIT), which is an AudioWorklet wrapper around
[`@shiguredo/rnnoise-wasm`](https://github.com/shiguredo/rnnoise-wasm) **2022.2.0** (Apache-2.0),
which is an emscripten build of [`xiph/rnnoise`](https://github.com/xiph/rnnoise) (BSD-3-Clause).

Three projects, not two, and the middle one is the one whose bytes are actually in the `.wasm`
files and in the emscripten glue bundled into the worklet — its banner is right there in
`rnnoise-worklet.js`. All three, plus `livekit-client`, are reproduced in `NOTICE.txt`, which is
compiled into the binary and served at `/NOTICE.txt`; all of them permit commercial use with no
agreement to negotiate. Two of the four are Apache-2.0, which makes shipping that notice a
condition rather than a courtesy.

| File | From the npm tarball | Bytes | sha256 |
|---|---|---|---|
| `rnnoise.wasm` | `dist/rnnoise.wasm` | 152656 | `8b60a2ab88fdae2d1a9f940249d0eb072f28ba8e796f7304347b4e07839c8853` |
| `rnnoise-simd.wasm` | `dist/rnnoise_simd.wasm` | 157234 | `378fd17c294db15ee4e818ba5ef072242a21c8ad7445b483f7f46d7dc4f2c253` |
| `rnnoise-worklet.js` | `dist/rnnoise/workletProcessor.js` | 64389 | `3a1d461f6bc28fc4d5872ddf6682728fa924f7734b3243ca9298ca47bcce8af7` |

The weights are inside the `.wasm`; there is no model file to serve. The worklet registers
`@sapphi-red/web-noise-suppressor/rnnoise`, which is the name `crates/front/assets/denoise.js`
constructs the node with, and it takes the binary as an `ArrayBuffer` through `processorOptions`
rather than fetching anything itself.

**The file is vendored unmodified, and it is wrapped at load time rather than patched.** Its
processor compiles that binary asynchronously inside its own constructor and writes nothing to its
output until that finishes, saying nothing about either state.
`crates/front/assets/denoise-gate.js` is added to the worklet scope first and wraps
`registerProcessor`, so the class this file registers is subclassed on its way in and made to report
when it has written its first block. Nothing here is edited, so an upgrade is still a copy. Its emscripten glue still contains the dead `fetch` and
`XMLHttpRequest` branches every emscripten build has; they are not reached, because the module is
always loaded with `wasmBinary` supplied.

To move to another version:

```
V=<version>
curl -fsSLO https://registry.npmjs.org/@sapphi-red/web-noise-suppressor/-/web-noise-suppressor-$V.tgz
tar xzf web-noise-suppressor-$V.tgz
cp package/dist/rnnoise.wasm       crates/front/assets/vendor/rnnoise.wasm
cp package/dist/rnnoise_simd.wasm  crates/front/assets/vendor/rnnoise-simd.wasm
sed 's|//# sourceMappingURL=workletProcessor.js.map||' package/dist/rnnoise/workletProcessor.js \
  > crates/front/assets/vendor/rnnoise-worklet.js
shasum -a 256 crates/front/assets/vendor/rnnoise*.wasm crates/front/assets/vendor/rnnoise-worklet.js
```

Then bump `RNNOISE_VERSION` and the three paths in `crates/front/src/http.rs`, the three matching
constants at the top of `crates/front/assets/app.js`, and the table above. `cargo test -p
mediacore-front` fails if the version and the paths disagree, if a route stops being loaded by the
page, if either binary stops being a module this machine's WebAssembly will accept, if the banner
in the worklet stops naming the project the notice was written against, or if the byte counts
quoted in `crates/front/README.md` stop matching the files.

It also fails if the delay the node adds moves. `node --test` drives this worklet and this `.wasm`
through the page's own tap-and-probe chain and asserts 992 samples; a new version that buffers
differently will say so there rather than in somebody's call. Re-measure and update the figure in
`crates/front/README.md` rather than loosening the test.

If we ever want to own the toolchain rather than the artefact, `jitsi/rnnoise-wasm`'s `build.sh` is
a working emscripten recipe against the `xiph/rnnoise` submodule and produces the same thing.
