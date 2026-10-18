/**
 * The daemon's live socket, `GET /api/live`.
 *
 * One WebSocket per page tells the SPA which topics changed; panels then
 * re-read their ordinary routes. The socket carries topic names only: no
 * rows, ids or secrets. While it is down (connecting, refused, daemon
 * restarting) panels fall back to their old polling timers, so nothing goes
 * stale for longer than it used to.
 */

import { useEffect, useRef, useState } from "react";

/** Subprotocol the daemon speaks on `/api/live`. */
export const LIVE_SUBPROTOCOL = "bookclerk-live.v1";

/** Path of the live socket. */
export const LIVE_PATH = "/api/live";

/** Close code the daemon sends when the session ends or is refused. */
export const CLOSE_UNAUTHORIZED = 4401;

/** First reconnect delay. */
export const BACKOFF_MIN_MS = 1_000;

/** Longest reconnect delay. */
export const BACKOFF_MAX_MS = 30_000;

/** What the daemon reports changes for. */
export type LiveTopic =
  | "library"
  | "jobs"
  | "deliveries"
  | "nodes"
  | "config"
  | "consents"
  | "users";

/** Connection state of a {@link LiveClient}. */
export type LiveState = "idle" | "connecting" | "open" | "closed";

/** The part of `WebSocket` the client uses. */
export interface LiveSocket {
  onmessage: ((event: { data: unknown }) => void) | null;
  onclose: ((event: { code: number }) => void) | null;
  onerror: ((event: unknown) => void) | null;
  close(code?: number): void;
}

/** What a {@link LiveClient} needs from its environment (swapped in tests). */
export interface LiveEnv {
  /** Page location the socket URL is built from. */
  location: { protocol: string; host: string };
  /** Opens a socket. */
  connect(url: string, protocol: string): LiveSocket;
  /** A number in `[0, 1)` for reconnect jitter. */
  random(): number;
}

/**
 * The socket URL for a page location: `wss:` under `https:`, else `ws:`,
 * on the page's own host (the daemon, or the Vite dev proxy).
 *
 * @param location - The page's protocol and host.
 * @returns The `/api/live` URL.
 */
export function liveUrl(location: { protocol: string; host: string }): string {
  const scheme = location.protocol === "https:" ? "wss:" : "ws:";
  return `${scheme}//${location.host}${LIVE_PATH}`;
}

/**
 * Reconnect delay after `attempt` failed connections in a row: doubling
 * from {@link BACKOFF_MIN_MS} up to {@link BACKOFF_MAX_MS}, with the upper
 * half jittered so a restarted daemon is not hit by every tab at once.
 *
 * @param attempt - Failed connections so far (0 for the first retry).
 * @param random - A number in `[0, 1)`.
 * @returns The delay in milliseconds.
 */
export function backoffDelay(attempt: number, random: number): number {
  const ceiling = Math.min(BACKOFF_MAX_MS, BACKOFF_MIN_MS * 2 ** Math.max(0, attempt));
  return Math.round(ceiling / 2 + (ceiling / 2) * random);
}

/**
 * Parses a daemon frame.
 *
 * @param data - The message payload.
 * @returns The frame, or `null` for anything unexpected.
 */
function parseFrame(data: unknown): { type: "hello" | "changed"; topics: LiveTopic[] } | null {
  if (typeof data !== "string") return null;
  try {
    const body = JSON.parse(data) as { v?: unknown; type?: unknown; topics?: unknown };
    if (body.v !== 1 || (body.type !== "hello" && body.type !== "changed")) return null;
    if (!Array.isArray(body.topics)) return null;
    const topics = body.topics.filter((topic): topic is LiveTopic => typeof topic === "string");
    return { type: body.type, topics };
  } catch {
    return null;
  }
}

type Subscriber = { topics: ReadonlySet<LiveTopic>; onChange: (topics: LiveTopic[]) => void };

/**
 * One shared connection to `/api/live`. It connects when the first
 * subscriber arrives, reconnects with backoff while anyone listens, and
 * closes when the last one leaves.
 */
export class LiveClient {
  private readonly env: LiveEnv;
  private readonly subscribers = new Set<Subscriber>();
  private readonly stateListeners = new Set<(state: LiveState) => void>();
  private socket: LiveSocket | null = null;
  private retry: ReturnType<typeof setTimeout> | null = null;
  private failures = 0;
  private everOpened = false;
  private current: LiveState = "idle";

  /**
   * @param env - Location, socket factory, and jitter source.
   */
  constructor(env: LiveEnv) {
    this.env = env;
  }

  /**
   * The connection state.
   *
   * @returns The current state.
   */
  get state(): LiveState {
    return this.current;
  }

  /**
   * Calls `onChange` with the changed topics among `topics` until the
   * returned function is called.
   *
   * @param topics - Topics to hear about.
   * @param onChange - Called with the changed topics this subscriber asked for.
   * @returns Unsubscribe.
   */
  subscribe(topics: readonly LiveTopic[], onChange: (topics: LiveTopic[]) => void): () => void {
    const subscriber: Subscriber = { topics: new Set(topics), onChange };
    this.subscribers.add(subscriber);
    if (!this.socket && !this.retry) this.open();
    return () => {
      this.subscribers.delete(subscriber);
      if (this.subscribers.size === 0) this.shutdown();
    };
  }

  /**
   * Calls `listener` on every state change until the returned function is called.
   *
   * @param listener - Receives the new state.
   * @returns Unsubscribe.
   */
  onState(listener: (state: LiveState) => void): () => void {
    this.stateListeners.add(listener);
    return () => this.stateListeners.delete(listener);
  }

  private setState(next: LiveState) {
    if (next === this.current) return;
    this.current = next;
    for (const listener of [...this.stateListeners]) listener(next);
  }

  private dispatch(topics: LiveTopic[]) {
    for (const subscriber of [...this.subscribers]) {
      const mine = topics.filter((topic) => subscriber.topics.has(topic));
      if (mine.length > 0) subscriber.onChange(mine);
    }
  }

  private open() {
    this.retry = null;
    this.setState("connecting");
    let socket: LiveSocket;
    try {
      socket = this.env.connect(liveUrl(this.env.location), LIVE_SUBPROTOCOL);
    } catch {
      this.scheduleRetry(false);
      return;
    }
    this.socket = socket;
    socket.onmessage = (event) => {
      if (this.socket !== socket) return;
      const frame = parseFrame(event.data);
      if (!frame) return;
      if (frame.type === "hello") {
        this.failures = 0;
        const reconnected = this.everOpened;
        this.everOpened = true;
        this.setState("open");
        // Changes made while the socket was down were never announced.
        if (reconnected) this.dispatch(frame.topics);
        return;
      }
      this.dispatch(frame.topics);
    };
    socket.onclose = (event) => {
      if (this.socket !== socket) return;
      this.socket = null;
      this.scheduleRetry(event.code === CLOSE_UNAUTHORIZED);
    };
    socket.onerror = () => {
      // A close event follows; it schedules the retry.
    };
  }

  private scheduleRetry(unauthorized: boolean) {
    this.setState("closed");
    if (this.subscribers.size === 0) return;
    // Signed out: the panels' own requests notice and sign the page out;
    // keep trying only slowly in case the session comes back.
    const delay = unauthorized
      ? BACKOFF_MAX_MS
      : backoffDelay(this.failures, this.env.random());
    this.failures += 1;
    this.retry = setTimeout(() => this.open(), delay);
  }

  private shutdown() {
    if (this.retry) clearTimeout(this.retry);
    this.retry = null;
    const socket = this.socket;
    this.socket = null;
    socket?.close(1000);
    this.failures = 0;
    this.everOpened = false;
    this.setState("idle");
  }
}

let shared: LiveClient | null = null;

/**
 * The page's shared {@link LiveClient}.
 *
 * @returns The client, created on first use.
 */
export function liveClient(): LiveClient {
  shared ??= new LiveClient({
    location: window.location,
    connect: (url, protocol) => {
      const socket = new WebSocket(url, protocol);
      // The client assigns its handlers right after; this adapter only
      // narrows the DOM event types to what it reads.
      const adapter: LiveSocket = {
        onmessage: null,
        onclose: null,
        onerror: null,
        close: (code) => socket.close(code),
      };
      socket.onmessage = (event) => adapter.onmessage?.({ data: event.data });
      socket.onclose = (event) => adapter.onclose?.({ code: event.code });
      socket.onerror = (event) => adapter.onerror?.(event);
      return adapter;
    },
    random: Math.random,
  });
  return shared;
}

/** How {@link watchLive} refreshes. */
export interface LiveRefreshOptions {
  /** Poll interval while the socket is not open. */
  pollMs: number;
  /**
   * Poll interval while it is open, for what changes without a row
   * changing (a node going quiet, presence ageing). Omitted: no polling
   * while open.
   */
  openPollMs?: number;
}

/**
 * Calls `refresh` when one of `topics` changes, and polls on a timer
 * while the socket is down (and, with `openPollMs`, while it is up).
 *
 * @param client - The live client.
 * @param topics - Topics that should trigger a refresh.
 * @param refresh - Re-reads the panel's routes.
 * @param options - Poll intervals.
 * @returns Stops watching.
 */
export function watchLive(
  client: LiveClient,
  topics: readonly LiveTopic[],
  refresh: () => void,
  options: LiveRefreshOptions,
): () => void {
  let timer: ReturnType<typeof setInterval> | null = null;
  let every: number | undefined;
  const schedule = (state: LiveState) => {
    const ms = state === "open" ? options.openPollMs : options.pollMs;
    // Connecting, closed and idle share one cadence: retries that flap
    // between them must not keep restarting (and so starving) the timer.
    if (ms === every && (timer || !ms)) return;
    if (timer) clearInterval(timer);
    timer = null;
    every = ms;
    if (ms && ms > 0) timer = setInterval(refresh, ms);
  };
  const stopState = client.onState(schedule);
  const unsubscribe = client.subscribe(topics, () => refresh());
  schedule(client.state);
  return () => {
    stopState();
    unsubscribe();
    if (timer) clearInterval(timer);
  };
}

/**
 * React binding of {@link watchLive} on the shared client. `refresh` may
 * change between renders without reconnecting.
 *
 * @param topics - Topics that should trigger a refresh.
 * @param refresh - Re-reads the panel's routes.
 * @param options - Poll intervals, and `enabled` (default true).
 * @returns The socket state, for a status hint.
 */
export function useLiveRefresh(
  topics: readonly LiveTopic[],
  refresh: () => void | Promise<void>,
  options: LiveRefreshOptions & { enabled?: boolean },
): LiveState {
  const latest = useRef(refresh);
  useEffect(() => {
    latest.current = refresh;
  }, [refresh]);
  const [state, setState] = useState<LiveState>("idle");
  const enabled = options.enabled ?? true;
  const key = [...topics].sort().join(",");
  const { pollMs, openPollMs } = options;
  useEffect(() => {
    if (!enabled) return;
    const client = liveClient();
    const stopState = client.onState(setState);
    setState(client.state);
    const stop = watchLive(
      client,
      key.split(",").filter(Boolean) as LiveTopic[],
      () => void latest.current(),
      { pollMs, openPollMs },
    );
    return () => {
      stopState();
      stop();
    };
  }, [enabled, key, pollMs, openPollMs]);
  return state;
}
