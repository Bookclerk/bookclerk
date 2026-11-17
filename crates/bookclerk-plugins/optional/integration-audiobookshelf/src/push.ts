/**
 * Audiobookshelf's pushed events, read from its Socket.io channel.
 *
 * Audiobookshelf pushes changes to signed-in admins over Socket.io
 * (Engine.IO v4 over a WebSocket). The host holds that socket while the
 * plugin sleeps (`[sockets]`): it answers the server's `2` ping with `3`
 * itself, and wakes the plugin for every other frame. This module is the
 * sans-IO side of that conversation: {@link socketUrl} names the endpoint,
 * and {@link onFrame} reads one text frame and says what to send back,
 * whether to close, and whether the frame announced a new user.
 *
 * The handshake is:
 *
 * 1. The server opens Engine.IO with `0{...}`; the plugin joins the default
 *    namespace with `40`.
 * 2. The server confirms with `40{...}`; the plugin authenticates with
 *    `42["auth","<api key>"]`.
 * 3. The server answers `42["init",...]` and from then on pushes events
 *    such as `42["user_added",{...}]`. A rejected key gets
 *    `42["invalid_token"]`.
 */

/** The Socket.io path Audiobookshelf serves, with the Engine.IO query. */
const SOCKET_PATH = "/socket.io/?EIO=4&transport=websocket";

/** The Engine.IO ping the server sends; the host answers it without waking the plugin. */
export const PING = "2";

/** The Engine.IO pong the host sends back for {@link PING}. */
export const PONG = "3";

/** What the plugin does after one pushed frame. */
export interface Reaction {
  /** A text frame to send back, if any. */
  readonly reply: string | null;
  /** Whether to close the socket: the server rejected the key or left the namespace. */
  readonly close: boolean;
  /** Whether the frame announced a user created on the server, so the host should poll now. */
  readonly newUser: boolean;
}

/** Nothing to do. */
const IGNORE: Reaction = { reply: null, close: false, newUser: false };

/** Close the socket. */
const CLOSE: Reaction = { reply: null, close: true, newUser: false };

/**
 * The WebSocket URL of the Socket.io endpoint under `baseUrl`.
 *
 * `http` becomes `ws` and `https` becomes `wss`; a path prefix on the base
 * URL (Audiobookshelf behind a sub-path) is kept.
 *
 * @param baseUrl - The configured server address, such as `https://abs.example`.
 * @returns The URL, or `null` when the address is not `http` or `https`.
 */
export function socketUrl(baseUrl: string): string | null {
  const base = baseUrl.trim().replace(/\/+$/, "");
  let rest: string;
  if (base.startsWith("https://")) rest = `wss://${base.slice("https://".length)}`;
  else if (base.startsWith("http://")) rest = `ws://${base.slice("http://".length)}`;
  else return null;
  if (rest.endsWith("://")) return null;
  return `${rest}${SOCKET_PATH}`;
}

/**
 * The `auth` event carrying `apiKey`.
 *
 * @param apiKey - The API key.
 * @returns The Socket.io frame.
 */
function authFrame(apiKey: string): string {
  return `42${JSON.stringify(["auth", apiKey])}`;
}

/**
 * Reads a Socket.io event's JSON array (after its `2` packet type).
 *
 * @param body - The packet after its type digit.
 * @returns The reaction.
 */
function onEvent(body: string): Reaction {
  // An event may carry an ack id before the array; the default namespace
  // carries no prefix. Skip to the array either way.
  const start = body.indexOf("[");
  if (start < 0) return IGNORE;
  let items: unknown;
  try {
    items = JSON.parse(body.slice(start));
  } catch {
    return IGNORE;
  }
  if (!Array.isArray(items)) return IGNORE;
  const name: unknown = items[0];
  if (name === "user_added") return { reply: null, close: false, newUser: true };
  if (name === "invalid_token") return CLOSE;
  return IGNORE;
}

/**
 * Reads a Socket.io packet (an Engine.IO message without its `4`).
 *
 * @param packet - The packet.
 * @param apiKey - The key to authenticate with.
 * @returns The reaction.
 */
function onSocketIo(packet: string, apiKey: string): Reaction {
  switch (packet[0]) {
    // Namespace joined: authenticate.
    case "0":
      return { reply: authFrame(apiKey), close: false, newUser: false };
    // Namespace left, or the connect was refused.
    case "1":
    case "4":
      return CLOSE;
    case "2":
      return onEvent(packet.slice(1));
    default:
      return IGNORE;
  }
}

/**
 * Reads one text frame from the server and says how to answer it.
 *
 * Unknown or malformed frames are ignored, so a server that adds events or
 * sends noise never breaks the socket.
 *
 * @param frame - The text frame as received.
 * @param apiKey - The key to authenticate with once the namespace is joined.
 * @returns The reply to send, whether to close, and whether a user was added.
 */
export function onFrame(frame: string, apiKey: string): Reaction {
  switch (frame[0]) {
    // Engine.IO open: join the default namespace.
    case "0":
      return { reply: "40", close: false, newUser: false };
    // Engine.IO close.
    case "1":
      return CLOSE;
    case "4":
      return onSocketIo(frame.slice(1), apiKey);
    default:
      return IGNORE;
  }
}
