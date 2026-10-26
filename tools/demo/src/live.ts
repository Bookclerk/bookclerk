// The session's live status, which the waiting page and the banner follow.
//
// The visitor's session object keeps one WebSocket per open demo tab and
// pushes the status on every change of state, so the waiting page moves on
// as soon as the copy is ready and every tab learns at once that the session
// ended. The time left
// is sent in seconds counted by the server, so a wrong clock on the
// visitor's computer cannot skew the banner's countdown. The countdown is
// display only: the session object's own hard stop ends the session.
//
// A page sends only the word "sync" (when it comes back into view) and gets
// the status again; the object ignores anything else it receives. The
// handshake and the messages are the visitor's input: `liveRequest` and
// `liveCommand` decide them without I/O, and test/inputs.fuzz.test.ts fuzzes
// both.

import type { Env } from "./env.ts";

/** Where the WebSocket is opened. Never forwarded to a container: the Worker answers every `/_demo/` path itself. */
export const LIVE_PATH = "/_demo/live";

/** The one message a page sends: "send me the status again". */
export const LIVE_SYNC = "sync";

/** Open sockets one session keeps; a new one beyond this closes the oldest. */
export const MAX_LIVE_SOCKETS = 8;

/**
 * Close code for a socket closed to make room for a newer one. A page that
 * gets it stops reconnecting and polls instead, so tabs beyond the limit do
 * not take turns evicting each other.
 */
export const CLOSE_TOO_MANY = 4001;

/** Where a session is. */
export type SessionState = "starting" | "ready" | "ending" | "ended" | "failed";

/** What a page is told about its session. */
export type LiveStatus = {
  /** Where the session is; `none` when there is no session. */
  state: SessionState | "none";
  /** Hard stop, Unix seconds (0 without a session). */
  endsAt: number;
  /** Whole seconds left before the hard stop, by the server's clock. */
  remaining: number;
  /** Image version shown in the banner. */
  version: string;
};

/** The status for a browser without a session. */
export const NO_SESSION: LiveStatus = Object.freeze({ state: "none", endsAt: 0, remaining: 0, version: "" });

/**
 * The live status of a session.
 *
 * @param meta - The session's state, hard stop (Unix seconds) and image version.
 * @param nowMs - Current time in milliseconds.
 * @returns The status, with the seconds left counted from `nowMs`.
 */
export function liveStatus(meta: { state: SessionState; endsAt: number; version: string }, nowMs: number): LiveStatus {
  const remaining = Math.max(0, Math.floor(meta.endsAt - nowMs / 1000));
  return { state: meta.state, endsAt: meta.endsAt, remaining, version: meta.version };
}

/**
 * Whether a session in this state is over, so its pages stop following it.
 *
 * @param state - The session's state.
 * @returns True for a session that ended, failed or does not exist.
 */
export function isOver(state: LiveStatus["state"]): boolean {
  switch (state) {
    case "starting":
    case "ready":
      return false;
    case "ending":
    case "ended":
    case "failed":
    case "none":
      return true;
  }
}

/**
 * The website to link back to.
 *
 * @param env - Worker env.
 * @returns `SITE_URL` when it is an https URL; otherwise empty.
 */
export function siteUrl(env: Pick<Env, "SITE_URL">): string {
  return typeof env.SITE_URL === "string" && /^https:\/\/[^\s"<>]+$/.test(env.SITE_URL) ? env.SITE_URL : "";
}

/** What the Worker does with a request to {@link LIVE_PATH}. */
export type LiveVerdict = "open" | "not_websocket" | "cross_origin" | "no_session";

/**
 * Decides a request to open the live status socket.
 *
 * @param r - The request's method, `Upgrade` and `Origin` headers (null
 *   when absent), the demo's own origin, and whether the visitor has a
 *   session.
 * @returns `open` only for a GET WebSocket upgrade from a page on this
 *   origin with a session; otherwise why not.
 */
export function liveRequest(r: {
  method: string;
  upgrade: string | null;
  origin: string | null;
  ownOrigin: string;
  hasSession: boolean;
}): LiveVerdict {
  // ASCII-only match: toLowerCase would also accept the Kelvin sign for "k".
  if (r.method !== "GET" || r.upgrade === null || !/^websocket$/i.test(r.upgrade)) return "not_websocket";
  // Browsers send Origin on every WebSocket handshake, and the socket
  // carries the visitor's cookie: only pages on this origin may open one.
  if (r.origin !== r.ownOrigin) return "cross_origin";
  return r.hasSession ? "open" : "no_session";
}

/**
 * What a page's message asks for.
 *
 * @param message - The message as received.
 * @returns `sync` for exactly the text "sync"; null for anything else.
 */
export function liveCommand(message: string | ArrayBuffer): typeof LIVE_SYNC | null {
  return message === LIVE_SYNC ? LIVE_SYNC : null;
}

/**
 * How many of the open sockets to close before accepting a new one.
 *
 * @param open - Sockets open now.
 * @returns How many, oldest first, so at most {@link MAX_LIVE_SOCKETS} stay open with the new one.
 */
export function socketsToEvict(open: number): number {
  return Math.max(0, open - MAX_LIVE_SOCKETS + 1);
}

/**
 * The close code to answer a page's close with. Codes a peer may not send
 * (1005, 1006, 1015 and anything outside 1000-4999 that is not a plain
 * close) become 1000.
 *
 * @param code - The code the page closed with.
 * @returns A code that is valid to send.
 */
export function replyCloseCode(code: number): number {
  const sendable =
    Number.isInteger(code) &&
    ((code >= 1000 && code <= 1014 && ![1004, 1005, 1006].includes(code)) || (code >= 3000 && code <= 4999));
  return sendable ? code : 1000;
}
