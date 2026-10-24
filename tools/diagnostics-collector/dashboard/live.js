/**
 * Live dashboard updates. One Durable Object (`LiveHub`, binding `LIVE`)
 * holds every open dashboard page's WebSocket through the hibernation API,
 * so idle pages cost nothing. When the index or the issue links change
 * (`/submit` indexed a report, the ingest Action named a filed issue, a sync
 * or reindex stored something), the Worker calls `announceChange` and the
 * hub sends each page the text `changed`. The page then fetches its own URL
 * once, with its session cookie, and swaps in the fresh numbers.
 *
 * The push carries no data: every number still comes from an authenticated
 * page request. Pages never send anything the hub reads; a message from a
 * page is ignored without being parsed, so the socket adds no input to
 * decode. A socket is accepted only for a signed-in member (dashboardFetch
 * checks the session and the `Origin` before forwarding), and is closed once
 * the session it was opened under would have expired, or when an owner signs
 * everyone out.
 *
 * Without the `LIVE` binding (tests, or a deployment that removed it) pages
 * simply do not update on their own; nothing else changes.
 */

/** Name of the single hub instance. */
const HUB_NAME = "dashboard";

/** Most sockets the hub keeps open; later pages get no live updates. */
export const MAX_SOCKETS = 200;

/** Message pages receive when they should refresh. */
export const CHANGED = "changed";

/** Close code sent when a session expires or every session ends (RFC 6455 "policy violation"). */
const CLOSE_SIGNED_OUT = 1008;

/**
 * The hub's stub, or null when the binding is missing.
 *
 * @param {Record<string, any>} env Worker bindings.
 * @returns {DurableObjectStub | null} Stub for the one hub.
 */
function hub(env) {
  return env.LIVE ? env.LIVE.get(env.LIVE.idFromName(HUB_NAME)) : null;
}

/**
 * Tells open dashboard pages to refresh. Best effort: a failure is logged
 * and never fails the caller, whose work is already stored.
 *
 * @param {Record<string, any>} env Worker bindings.
 * @returns {Promise<void>}
 */
export async function announceChange(env) {
  await post(env, "/changed");
}

/**
 * Closes every open dashboard socket (owner "sign everyone out").
 *
 * @param {Record<string, any>} env Worker bindings.
 * @returns {Promise<void>}
 */
export async function closeAllLive(env) {
  await post(env, "/close-all");
}

/**
 * Sends one internal command to the hub, logging failures.
 *
 * @param {Record<string, any>} env Worker bindings.
 * @param {"/changed" | "/close-all"} path Command.
 * @returns {Promise<void>}
 */
async function post(env, path) {
  const stub = hub(env);
  if (!stub) return;
  try {
    const res = await stub.fetch(`https://live${path}`, { method: "POST" });
    if (!res.ok) console.error(`live ${path} answered ${res.status}`);
  } catch (err) {
    console.error(`live ${path} failed: ${err instanceof Error ? err.message : String(err)}`);
  }
}

/**
 * Hands a signed-in member's WebSocket upgrade to the hub.
 *
 * @param {Record<string, any>} env Worker bindings.
 * @param {number} expiresAt When the member's session ends, Unix ms; the
 *   hub closes the socket after it.
 * @returns {Promise<Response>} The hub's 101 answer, or 404 without the
 *   binding.
 */
export async function connectLive(env, expiresAt) {
  const stub = hub(env);
  if (!stub) return new Response("live updates are off", { status: 404 });
  return stub.fetch(`https://live/connect?expires=${encodeURIComponent(String(expiresAt))}`, {
    headers: { Upgrade: "websocket" },
  });
}

/**
 * The session expiry a socket was opened under, from its attachment.
 *
 * @param {WebSocket} ws Hibernated socket.
 * @returns {number} Unix ms; 0 when unreadable, so the socket is treated as
 *   expired.
 */
function expiryOf(ws) {
  try {
    const n = Number(ws.deserializeAttachment()?.expiresAt);
    return Number.isSafeInteger(n) ? n : 0;
  } catch {
    return 0;
  }
}

/**
 * Closes a socket, ignoring one that is already closing.
 *
 * @param {WebSocket} ws Socket.
 * @param {number} code Close code.
 * @param {string} reason Close reason.
 * @returns {void}
 */
function closeQuietly(ws, code, reason) {
  try {
    ws.close(code, reason);
  } catch {
    // Already closed.
  }
}

/**
 * The hub Durable Object. Reached only through the `LIVE` binding, never
 * from the internet, so its routes are commands from this Worker.
 */
export class LiveHub {
  /**
   * @param {DurableObjectState} state Durable Object state (WebSocket
   *   hibernation API).
   * @param {Record<string, any>} env Worker bindings (unused).
   */
  constructor(state, env) {
    /** Durable Object state; owns the hibernated sockets. */
    this.state = state;
    /** Worker bindings. */
    this.env = env;
  }

  /**
   * Internal commands: `/connect` (upgrade), `/changed`, `/close-all`.
   *
   * @param {Request} request Command from the Worker.
   * @param {number} [now] Unix ms (tests).
   * @returns {Promise<Response>} The answer.
   */
  async fetch(request, now = Date.now()) {
    const url = new URL(request.url);
    if (url.pathname === "/connect") return this.connect(request, url, now);
    if (request.method === "POST" && url.pathname === "/changed") {
      this.broadcast(now);
      return new Response(null, { status: 204 });
    }
    if (request.method === "POST" && url.pathname === "/close-all") {
      for (const ws of this.state.getWebSockets()) closeQuietly(ws, CLOSE_SIGNED_OUT, "signed out");
      return new Response(null, { status: 204 });
    }
    return new Response("not found", { status: 404 });
  }

  /**
   * Accepts one page's socket.
   *
   * @param {Request} request Upgrade request.
   * @param {URL} url Its URL, carrying `expires`.
   * @param {number} now Unix ms.
   * @returns {Response} 101 with the client end, 426 without an upgrade,
   *   400 for a bad expiry, or 503 at `MAX_SOCKETS`.
   */
  connect(request, url, now) {
    if ((request.headers.get("upgrade") || "").toLowerCase() !== "websocket") {
      return new Response("expected a WebSocket upgrade", { status: 426 });
    }
    const expiresAt = Number(url.searchParams.get("expires"));
    if (!Number.isSafeInteger(expiresAt) || expiresAt <= now) {
      return new Response("bad session expiry", { status: 400 });
    }
    this.dropExpired(now);
    if (this.state.getWebSockets().length >= MAX_SOCKETS) {
      return new Response("too many live pages", { status: 503 });
    }
    const [client, server] = Object.values(new WebSocketPair());
    this.state.acceptWebSocket(server);
    server.serializeAttachment({ expiresAt });
    return new Response(null, { status: 101, webSocket: client });
  }

  /**
   * Sends `changed` to every socket whose session is still current.
   *
   * @param {number} now Unix ms.
   * @returns {void}
   */
  broadcast(now) {
    this.dropExpired(now);
    for (const ws of this.state.getWebSockets()) {
      try {
        ws.send(CHANGED);
      } catch {
        // Closing; the runtime drops it.
      }
    }
  }

  /**
   * Closes sockets opened under sessions that have since expired.
   *
   * @param {number} now Unix ms.
   * @returns {void}
   */
  dropExpired(now) {
    for (const ws of this.state.getWebSockets()) {
      if (expiryOf(ws) <= now) closeQuietly(ws, CLOSE_SIGNED_OUT, "session expired");
    }
  }

  /**
   * Pages send nothing the hub uses; whatever arrives is dropped unread.
   *
   * @returns {void}
   */
  webSocketMessage() {}

  /**
   * Completes the close handshake a page started.
   *
   * @param {WebSocket} ws Socket.
   * @param {number} code Close code the page sent.
   * @returns {void}
   */
  webSocketClose(ws, code) {
    // 1005 ("no status") and 1006 may not be sent back; answer with 1000.
    closeQuietly(ws, code === 1005 || code === 1006 ? 1000 : code, "");
  }
}
