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
 * checks the session and the `Origin` before forwarding). The hub closes it
 * at the first connect or push after the session it was opened under would
 * have expired, and closes every socket when an owner signs everyone out. A
 * page whose session ended earlier (sign-out in another tab) learns it at the
 * next push, when its refresh is redirected to sign-in, and closes itself.
 *
 * Without the `LIVE` binding (tests, or a deployment that removed it) pages
 * simply do not update on their own; nothing else changes.
 */

import type { Env } from "../env.ts";

/** Name of the single hub instance. */
const HUB_NAME = "dashboard";

/** Most sockets the hub keeps open; later pages get no live updates. */
export const MAX_SOCKETS = 200;

/** Message pages receive when they should refresh. */
export const CHANGED = "changed";

/** Storage key for when the hub last announced a change, Unix ms. */
const LAST_CHANGED_KEY = "last_changed";

/**
 * How recent a change must be for a newly connected page to refresh once:
 * longer than a page normally takes between its database reads and opening
 * its socket.
 */
export const CATCH_UP_MS = 15_000;

/** `WebSocket.readyState` of an open socket. */
const OPEN = 1;

/** Close code sent when a session expires or every session ends (RFC 6455 "policy violation"). */
const CLOSE_SIGNED_OUT = 1008;

/**
 * The hub's stub, or null when the binding is missing.
 *
 * @param env - Worker bindings.
 * @returns Stub for the one hub.
 */
function hub(env: Env): DurableObjectStub | null {
  return env.LIVE ? env.LIVE.get(env.LIVE.idFromName(HUB_NAME)) : null;
}

/**
 * Tells open dashboard pages to refresh. Best effort: a failure is logged
 * and never fails the caller, whose work is already stored.
 *
 * @param env - Worker bindings.
 */
export async function announceChange(env: Env): Promise<void> {
  await post(env, "/changed");
}

/**
 * Closes every open dashboard socket (owner "sign everyone out").
 *
 * @param env - Worker bindings.
 */
export async function closeAllLive(env: Env): Promise<void> {
  await post(env, "/close-all");
}

/**
 * Sends one internal command to the hub, logging failures.
 *
 * @param env - Worker bindings.
 * @param path - Command.
 */
async function post(env: Env, path: "/changed" | "/close-all"): Promise<void> {
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
 * @param env - Worker bindings.
 * @param expiresAt - When the member's session ends, Unix ms; the
 *   hub closes the socket after it.
 * @returns The hub's 101 answer, or 404 without the
 *   binding.
 */
export async function connectLive(env: Env, expiresAt: number): Promise<Response> {
  const stub = hub(env);
  if (!stub) return new Response("live updates are off", { status: 404 });
  return stub.fetch(`https://live/connect?expires=${encodeURIComponent(String(expiresAt))}`, {
    headers: { Upgrade: "websocket" },
  });
}

/**
 * The session expiry a socket was opened under, from its attachment.
 *
 * @param ws - Hibernated socket.
 * @returns Unix ms; 0 when unreadable, so the socket is treated as
 *   expired.
 */
function expiryOf(ws: WebSocket): number {
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
 * @param ws - Socket.
 * @param code - Close code.
 * @param reason - Close reason.
 */
function closeQuietly(ws: WebSocket, code: number, reason: string): void {
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
  /** Durable Object state; owns the hibernated sockets. */
  readonly state: DurableObjectState;
  /** Worker bindings. */
  readonly env: Env;

  /**
   * @param state - Durable Object state (WebSocket
   *   hibernation API).
   * @param env - Worker bindings (unused).
   */
  constructor(state: DurableObjectState, env: Env) {
    this.state = state;
    this.env = env;
  }

  /**
   * Internal commands: `/connect` (upgrade), `/changed`, `/close-all`.
   *
   * @param request - Command from the Worker.
   * @param now - Unix ms (tests).
   * @returns The answer.
   */
  async fetch(request: Request, now: number = Date.now()): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === "/connect") return this.connect(request, url, now);
    if (request.method === "POST" && url.pathname === "/changed") {
      await this.state.storage.put(LAST_CHANGED_KEY, now);
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
   * @param request - Upgrade request.
   * @param url - Its URL, carrying `expires`.
   * @param now - Unix ms.
   * @returns 101 with the client end, 426 without an
   *   upgrade, 400 for a bad expiry, or 503 at `MAX_SOCKETS`.
   */
  async connect(request: Request, url: URL, now: number): Promise<Response> {
    if ((request.headers.get("upgrade") || "").toLowerCase() !== "websocket") {
      return new Response("expected a WebSocket upgrade", { status: 426 });
    }
    const expiresAt = Number(url.searchParams.get("expires"));
    if (!Number.isSafeInteger(expiresAt) || expiresAt <= now) {
      return new Response("bad session expiry", { status: 400 });
    }
    this.dropExpired(now);
    // Sockets just closed above stay listed until their close completes.
    if (this.state.getWebSockets().filter((ws) => ws.readyState === OPEN).length >= MAX_SOCKETS) {
      return new Response("too many live pages", { status: 503 });
    }
    const { 0: client, 1: server } = new WebSocketPair();
    this.state.acceptWebSocket(server);
    server.serializeAttachment({ expiresAt });
    // A change between the page's own reads and this connect reached no
    // socket of that page; a recent one gets this page a catch-up refresh.
    const last = Number(await this.state.storage.get(LAST_CHANGED_KEY));
    if (Number.isFinite(last) && last > now - CATCH_UP_MS) server.send(CHANGED);
    return new Response(null, { status: 101, webSocket: client });
  }

  /**
   * Sends `changed` to every socket whose session is still current.
   *
   * @param now - Unix ms.
   */
  broadcast(now: number): void {
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
   * @param now - Unix ms.
   */
  dropExpired(now: number): void {
    for (const ws of this.state.getWebSockets()) {
      if (expiryOf(ws) <= now) closeQuietly(ws, CLOSE_SIGNED_OUT, "session expired");
    }
  }

  /**
   * Pages send nothing the hub uses; whatever arrives is dropped unread.
   *
   * @param _ws - Socket the message came on.
   * @param _message - The message, unread.
   */
  webSocketMessage(_ws: WebSocket, _message: string | ArrayBuffer): void {}

  /**
   * Completes the close handshake a page started.
   *
   * @param ws - Socket.
   * @param code - Close code the page sent.
   */
  webSocketClose(ws: WebSocket, code: number): void {
    // 1005 ("no status") and 1006 may not be sent back; answer with 1000.
    closeQuietly(ws, code === 1005 || code === 1006 ? 1000 : code, "");
  }
}
