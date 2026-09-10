/**
 * A pass minted somewhere else, read out of the address the page was opened
 * at.
 *
 * The page mints its own token per load, which is right for a page that is its
 * own product: it asks for a name, the server signs it, and nobody outside
 * decides who anybody is. It is the wrong shape for a page a platform sends
 * people to. There the platform already knows who this is -- an account, a
 * contract, a role -- and none of that survives a token this page invented for
 * itself. So a pass is accepted from the address, and when one is there the
 * page does not mint.
 *
 * **After the hash, and nowhere else.** A fragment is never sent to a server:
 * it stays out of access logs, out of the `Referer` header sent to whatever
 * the page loads, and out of a proxy's history. A `?token=` is in all three,
 * which is why one is not read here -- accepting both would mean the safe
 * shape is the one nobody uses.
 *
 * It stays in the address. A link that carried its pass once and lost it on the
 * first reload is a link that works exactly until somebody presses F5, and the
 * address is the only place it can live without this page keeping a copy of a
 * credential somewhere. What that costs is honest and worth naming: the pass is
 * in the address bar, in the history entry, in a bookmark and in a screenshot
 * of any of them, so a short expiry is what keeps an old link from being a way
 * in. A fragment is still never sent to a server, so logs, `Referer` headers
 * and proxies see none of it.
 *
 * Nothing here verifies anything. The signature is the server's business and
 * it will check it on the socket; what these functions do is read the claims
 * so the page can say who somebody is about to be, and refuse early and
 * legibly what the server would refuse late and silently.
 */

export const PASS_FIELD = "token";

export function passFrom(hash) {
  const text = String(hash ?? "").replace(/^#/, "");

  if (text === "") {
    return null;
  }

  const named = new URLSearchParams(text).get(PASS_FIELD);
  const found = named ?? (text.includes("=") ? null : text);

  return found && looksLikeToken(found) ? found : null;
}

export function claimsOf(token) {
  const parts = String(token ?? "").split(".");

  if (parts.length !== 3) {
    return null;
  }

  let payload;
  try {
    payload = JSON.parse(decode(parts[1]));
  } catch {
    return null;
  }

  if (!payload || typeof payload !== "object") {
    return null;
  }

  const video = payload.video && typeof payload.video === "object" ? payload.video : {};

  return {
    identity: typeof payload.sub === "string" ? payload.sub : "",
    name: typeof payload.name === "string" ? payload.name : "",
    room: typeof video.room === "string" ? video.room : "",
    expiresAt: Number.isFinite(payload.exp) ? payload.exp : 0,
    attributes:
      payload.attributes && typeof payload.attributes === "object" ? payload.attributes : {},
  };
}

/**
 * Why this pass cannot be used, in a sentence somebody can act on, or null.
 *
 * The clock is the caller's, because a page that read the time itself could
 * not be tested against an expiry. Both refusals are things the server would
 * refuse too -- the point of finding them here is that a room which closes the
 * socket without a word looks like a broken server, and this looks like an
 * expired link.
 */
export function passProblem(claims, nowSeconds, room) {
  if (!claims) {
    return "The link you opened does not carry a pass this page can read. Ask for a new one.";
  }
  if (!claims.room) {
    return "The pass in this link names no room, so there is nothing to open.";
  }
  if (claims.expiresAt > 0 && claims.expiresAt <= nowSeconds) {
    return "The pass in this link has expired. Ask for a new one.";
  }
  if (room && claims.room !== room) {
    return `The pass in this link is for ${claims.room}, and this address is ${room}.`;
  }

  return null;
}

export function whoFrom(claims) {
  if (!claims) {
    return "";
  }

  return claims.name || claims.identity;
}

function looksLikeToken(value) {
  return /^[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+$/.test(value);
}

function decode(part) {
  const padded = part.replace(/-/g, "+").replace(/_/g, "/");
  const binary = atob(padded + "=".repeat((4 - (padded.length % 4)) % 4));

  // A name is not ascii, and `atob` hands back one byte per character. Reading
  // those bytes as utf-8 is what keeps a Russian display name from arriving as
  // mojibake in the one place a person looks to check they are themselves.
  return new TextDecoder().decode(Uint8Array.from(binary, (character) => character.charCodeAt(0)));
}
