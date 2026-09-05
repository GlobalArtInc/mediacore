# Vendored browser SDK

`livekit-client.esm.js` is `dist/livekit-client.esm.mjs` from
[`livekit-client`](https://github.com/livekit/client-sdk-js) **2.22.2**, Apache-2.0, with the
trailing `sourceMappingURL` comment removed because the map is not shipped.

It is compiled into `mediacore-demo` and served from the same origin as the page. The demo has no
runtime dependency on a CDN: a deployment with no route to the internet still works, the page's
content security policy can forbid third-party scripts outright, and nobody can change what the
browser executes by changing what a CDN answers.

To move to another version:

```
curl -fsSLO https://cdn.jsdelivr.net/npm/livekit-client@<version>/dist/livekit-client.esm.mjs
sed 's|//# sourceMappingURL=livekit-client.esm.mjs.map||' livekit-client.esm.mjs \
  > crates/demo/assets/vendor/livekit-client.esm.js
```

Then bump `SDK_VERSION` in `crates/demo/src/http.rs` and `SDK_URL` in
`crates/demo/assets/app.js`; the route carries the version so a cached copy of the old file is
never reused.
