// The page and the API are the same origin on purpose. The document names no
// `servers`, so Swagger UI posts to whatever host served this page: behind
// Caddy that is the public origin, and straight against the SFU's own port it
// is that port. Either way Try it out reaches the server you are looking at,
// and nothing here has to be configured per deployment.
const SPEC = "/swagger/openapi.json";

// swagger-ui ships the dark rules under html.dark-mode and leaves switching to
// whoever embeds it.
const scheme = window.matchMedia("(prefers-color-scheme: dark)");
const paint = (dark) => document.documentElement.classList.toggle("dark-mode", dark);
paint(scheme.matches);
scheme.addEventListener("change", (event) => paint(event.matches));

// Every operation names exactly one grant, which is the honest thing for the
// document to say and an unkind thing for the authorize dialog to do: a reader
// who pastes their token into the roomAdmin row and then presses Try it out on
// CreateRoom gets no Authorization header at all, and a 401 that reads as a
// broken server. So if a request is about to leave without one, borrow
// whichever token was actually entered.
function borrowedToken() {
  try {
    const authorized = window.ui.authSelectors.authorized().toJS();
    for (const entry of Object.values(authorized)) {
      if (entry && entry.value) {
        return entry.value;
      }
    }
  } catch (error) {
    // authSelectors is swagger-ui's own shape. If a future version moves it,
    // this fallback goes quiet and the page still works for anyone who filled
    // in the row the operation names.
  }
  return null;
}

function carriesAuthorization(headers) {
  return Object.keys(headers || {}).some((name) => name.toLowerCase() === "authorization");
}

window.ui = SwaggerUIBundle({
  url: SPEC,
  dom_id: "#swagger-ui",
  presets: [SwaggerUIBundle.presets.apis],
  layout: "BaseLayout",
  deepLinking: true,
  tryItOutEnabled: true,
  displayRequestDuration: true,
  docExpansion: "list",
  defaultModelsExpandDepth: 0,
  // Not persisted: this is an API-key-signed server credential, and leaving it
  // in localStorage outlives the reason it was pasted in.
  persistAuthorization: false,
  requestInterceptor(request) {
    if (!carriesAuthorization(request.headers)) {
      const token = borrowedToken();
      if (token) {
        request.headers.Authorization = `Bearer ${token}`;
      }
    }
    return request;
  },
});
