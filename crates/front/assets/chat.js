/**
 * The decisions behind the chat panel, kept away from the DOM so they can be
 * tested with `node --test`: whether a message may leave this browser, what one
 * arriving turns into, and how many of them are still unread.
 *
 * **This room keeps no history, and that is a decision rather than a gap.** A
 * data channel carries nothing to somebody who was not there: the server
 * forwards what it is handed and stores none of it, so a person who joins at
 * half past six sees an empty panel and not the morning. Keeping a backlog
 * would mean the server holding what was said, in memory that dies with the
 * process or in a record that outlives the room, and neither was asked for.
 * What the panel promises is exactly what it does -- the room while you are in
 * it -- and it says so where a newcomer will read it.
 *
 * The sender is the other half worth knowing. A chat packet leaves the browser
 * without a name on it; the server stamps the identity of the connection it
 * arrived on, because a name a participant writes for themselves is a name
 * anyone can write for anyone. So the participant handed to `lineFrom` is the
 * one the client resolved from that stamp, and a message whose sender the room
 * does not know is dropped rather than shown as coming from nobody.
 */

export const MESSAGE_LIMIT = 2000;
export const KEEP_LINES = 200;
export const NOT_ALLOWED =
  "This room is not letting you send messages: the token it was joined with does not carry that, and the server drops what it does not allow.";
export const NOT_CONNECTED = "The connection to the room is down, so nothing can be sent.";

export function canSend({ connected, allowed, text }) {
  const said = String(text ?? "")
    .trim()
    .slice(0, MESSAGE_LIMIT);

  if (said === "") {
    return { send: false, text: "", reason: "" };
  }
  if (!allowed) {
    return { send: false, text: said, reason: NOT_ALLOWED };
  }
  if (!connected) {
    return { send: false, text: said, reason: NOT_CONNECTED };
  }

  return { send: true, text: said, reason: "" };
}

export function lineFrom(message, participant, self, label) {
  if (!participant) {
    return null;
  }

  const said = typeof message?.message === "string" ? message.message.trim() : "";

  if (said === "") {
    return null;
  }

  const at = Number(message.timestamp);

  return {
    id: typeof message.id === "string" ? message.id : "",
    text: said.slice(0, MESSAGE_LIMIT),
    label: label || participant.identity || "",
    mine: participant === self,
    at: Number.isFinite(at) && at > 0 ? at : Date.now(),
  };
}

export function afterArrival(unread, showing) {
  return showing ? 0 : unread + 1;
}

export function unreadLabel(unread) {
  return unread === 0 ? "Chat" : `Chat, ${unread} unread`;
}

export function clockOf(at, locale) {
  return new Date(at).toLocaleTimeString(locale, { hour: "2-digit", minute: "2-digit" });
}
