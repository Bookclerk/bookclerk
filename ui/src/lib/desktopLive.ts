/**
 * Live updates in the desktop window.
 *
 * The window's custom scheme cannot carry the `/api/live` WebSocket, so the
 * window's shell sends the same frames over a Tauri channel instead: the
 * page calls the shell's `live` command once with a channel, and every
 * frame arrives on it as the JSON text the socket would have sent. The
 * window allows that one command and nothing else, and only on the app's
 * own pages.
 */

import type { LiveSocket } from "@/lib/live";

/** The shell command that starts the feed. */
export const LIVE_COMMAND = "live";

/** Prefix Tauri reads a channel argument by. */
const CHANNEL_PREFIX = "__CHANNEL__:";

/** What Tauri delivers on a channel: a message, or the end, in order. */
type ChannelDelivery = { index: number; message: unknown } | { index: number; end: true };

/** The part of the page's Tauri bridge this module uses. */
export interface TauriBridge {
  /** Calls a shell command. */
  invoke(command: string, args?: Record<string, unknown>): Promise<unknown>;
  /** Registers `callback` and answers the id the shell calls it by. */
  transformCallback(callback: (delivery: unknown) => void, once?: boolean): number;
  /** Forgets a callback. */
  unregisterCallback(id: number): void;
}

/**
 * The page's Tauri bridge, when the shell injected one.
 *
 * @param scope - Where to look (defaults to the page's `window`).
 * @returns The bridge, or `null` outside the desktop window.
 */
export function tauriBridge(scope: object = window): TauriBridge | null {
  const internals = (scope as { __TAURI_INTERNALS__?: Partial<TauriBridge> }).__TAURI_INTERNALS__;
  if (
    internals &&
    typeof internals.invoke === "function" &&
    typeof internals.transformCallback === "function" &&
    typeof internals.unregisterCallback === "function"
  ) {
    return internals as TauriBridge;
  }
  return null;
}

/**
 * Reads one channel delivery.
 *
 * @param delivery - What the shell passed to the callback.
 * @returns The delivery, or `null` for anything else.
 */
function parseDelivery(delivery: unknown): ChannelDelivery | null {
  if (typeof delivery !== "object" || delivery === null) return null;
  const body = delivery as { index?: unknown; message?: unknown; end?: unknown };
  if (typeof body.index !== "number" || !Number.isInteger(body.index) || body.index < 0) {
    return null;
  }
  if (body.end === true) return { index: body.index, end: true };
  if ("message" in body) return { index: body.index, message: body.message };
  return null;
}

/**
 * Opens the shell's live feed as a {@link LiveSocket}, so the shared live
 * client treats it as its socket: frames arrive as messages, and the end
 * of the feed (or a failed `live` call) closes it, which the client
 * retries with backoff.
 *
 * Tauri numbers channel deliveries; out-of-order ones are held until the
 * gap fills, as Tauri's own channel class does. Closing from the page
 * stops listening; the shell replaces its feed on the next `live` call.
 *
 * @param bridge - The page's Tauri bridge.
 * @returns The socket.
 */
export function openDesktopLive(bridge: TauriBridge): LiveSocket {
  let closed = false;
  let next = 0;
  let endAt: number | null = null;
  const held = new Map<number, unknown>();

  const socket: LiveSocket = {
    onmessage: null,
    onclose: null,
    onerror: null,
    close: () => finish(null),
  };

  const id = bridge.transformCallback((raw) => {
    if (closed) return;
    const delivery = parseDelivery(raw);
    if (!delivery) return;
    if ("end" in delivery) {
      endAt = delivery.index;
    } else if (delivery.index === next) {
      socket.onmessage?.({ data: delivery.message });
      next += 1;
      while (!closed && held.has(next)) {
        const message = held.get(next);
        held.delete(next);
        socket.onmessage?.({ data: message });
        next += 1;
      }
    } else if (delivery.index > next) {
      held.set(delivery.index, delivery.message);
    }
    if (!closed && endAt !== null && next >= endAt) finish(1000);
  });

  // `code`: the close event to report, or `null` when the page closed it.
  function finish(code: number | null) {
    if (closed) return;
    closed = true;
    held.clear();
    bridge.unregisterCallback(id);
    if (code !== null) socket.onclose?.({ code });
  }

  const channel = { toJSON: () => `${CHANNEL_PREFIX}${id}` };
  bridge.invoke(LIVE_COMMAND, { onFrame: channel }).catch((err: unknown) => {
    socket.onerror?.(err);
    // 1011: the shell could not start the feed; the client retries.
    finish(1011);
  });
  return socket;
}
