/**
 * A room is an address.
 *
 * `/r/standup` is where a room lives. Opening it goes to that room, reloading
 * it comes back to the same room rather than to the lobby, and it is what
 * somebody pastes to a colleague -- so it carries the room name and nothing
 * else. No token, no ticket, no display name. A token is minted per load and
 * lives in memory for the life of the connection; a URL is written down, sent
 * over chat, and read by whoever the link reaches.
 *
 * What is here is only the reading and writing of that address: which room a
 * page load is addressed to, which room the lobby should offer, what the URL
 * for a room is and what the tab should be called. The question of whether a
 * name *can* be a room is not answered here and deliberately so -- `clean_room`
 * in `src/name.rs` is the only answer to that, and the page asks the server
 * rather than keeping a second opinion that can drift from it.
 *
 * Nothing here imports anything and nothing here touches the DOM, so the
 * behaviour suite loads this file straight off disk where the page's absolute
 * `/module.js` paths do not resolve.
 */

/** Where a room lives. `src/http.rs` serves the page for every name under it. */
export const ROOM_PATH_PREFIX = "/r/";

/** The tab's name when it is about no particular room. */
export const TITLE = "MediaCore";

/**
 * Which room this page load is addressed to, and where that address should
 * have been.
 *
 * `?room=standup` was how a room was named before rooms had URLs, and links
 * with it in them are out in people's chat histories. It still works, and the
 * answer carries the canonical URL for it so the page can quietly correct the
 * address bar -- the other query parameters travel across, because `?video=1`
 * and `?bench=1` are instructions for this visit that a reload should keep.
 *
 * The path wins over the query when a load somehow carries both: one of them
 * is the address of this page and the other is a parameter that was left on
 * it.
 *
 * A percent sequence that does not decode is kept as it was typed. It cannot
 * be a room name either way, and showing somebody the text that is actually in
 * their URL is the only way they can see what is wrong with it.
 */
export function addressedRoom(pathname, search) {
  const fromPath = roomInPath(pathname);
  if (fromPath !== null) return { room: fromPath, canonical: null };

  const fromQuery = queryRoom(search);
  if (fromQuery === null) return { room: null, canonical: null };

  return { room: fromQuery, canonical: roomPath(fromQuery, search) };
}

/**
 * The room named by `/r/<room>`, or null for every other path. A trailing
 * slash is tolerated because a link that picked one up on the way through a
 * chat client is still a link to that room.
 */
export function roomInPath(pathname) {
  const path = typeof pathname === "string" ? pathname : "";
  if (!path.startsWith(ROOM_PATH_PREFIX)) return null;

  const segment = path.slice(ROOM_PATH_PREFIX.length).replace(/\/+$/, "");
  if (segment === "" || segment.includes("/")) return null;

  return decoded(segment);
}

/**
 * The URL of a room, with the rest of this visit's query kept and the legacy
 * `room` parameter dropped -- it is the path's job now, and two of them in one
 * URL is an argument waiting to happen.
 */
export function roomPath(room, search) {
  const rest = new URLSearchParams(typeof search === "string" ? search : "");
  rest.delete("room");

  const query = rest.toString();
  return `${ROOM_PATH_PREFIX}${encodeURIComponent(room)}${query === "" ? "" : `?${query}`}`;
}

/**
 * Which room the lobby offers, and on whose authority.
 *
 * A link outranks everything: following one to a room and being put in another
 * is the failure this whole address exists to prevent. Below it is what this
 * browser last joined, which is worth remembering now that it was not before
 * -- a room name is not a credential, it is already in the URL bar of the tab
 * that joined it, and somebody who joins `standup` every morning should not
 * type it every morning. Below that is the server's own room, which is what
 * `/` means when nothing else has been said.
 *
 * The source travels with the answer because the page treats the three
 * differently: only a room this load was *addressed* to belongs in the tab
 * title, since a remembered name is a suggestion the person has not accepted
 * yet.
 */
export function offeredRoom(addressed, remembered, fallback) {
  if (usable(addressed)) return { room: addressed.trim(), source: "link" };
  if (usable(remembered)) return { room: remembered.trim(), source: "remembered" };
  return { room: usable(fallback) ? fallback.trim() : "", source: "default" };
}

/**
 * The room on the SFU, given the name a person says. The server composes this
 * itself for the token it mints -- `room_for` in `src/name.rs` -- and what is
 * here is a preview for the lobby, never a decision: everything the page shows
 * about a room it has joined is derived from the name the token came back
 * with.
 */
export function composedRoom(namespace, room, separator) {
  const inside = typeof room === "string" ? room.trim() : "";
  if (inside === "" || inside === namespace) return namespace;
  return `${namespace}${separator}${inside}`;
}

/**
 * The name a person says, given the room on the SFU: the inverse of the
 * composition above, and what goes in the URL, the roster header and the tab
 * title. A room that is not inside the namespace at all is handed back whole,
 * because a page that cannot explain a name should show it rather than a
 * truncation of it.
 */
export function plainRoom(composed, namespace, separator) {
  if (typeof composed !== "string" || composed === "") return "";
  if (composed === namespace) return composed;

  const prefix = `${namespace}${separator}`;
  return composed.startsWith(prefix) ? composed.slice(prefix.length) : composed;
}

function queryRoom(search) {
  const asked = new URLSearchParams(typeof search === "string" ? search : "").get("room");
  return usable(asked) ? asked.trim() : null;
}

function decoded(segment) {
  try {
    return decodeURIComponent(segment);
  } catch {
    return segment;
  }
}

function usable(value) {
  return typeof value === "string" && value.trim() !== "";
}
