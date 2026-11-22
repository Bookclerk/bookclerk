/**
 * The relay's logic, without the Workers runtime around it.
 *
 * The relay Worker (`relay-worker.ts`) hands every request and every plugin
 * socket to one {@link RelayHub} inside its Durable Object. The hub holds
 * each public request until a plugin socket is free, announces it on that
 * socket, hands the request to the plugin's pull and answers the public
 * client with what the plugin pushes back. See `protocol.ts` for the wire
 * format.
 *
 * An upgrade takes the same path; once the plugin pushes `101`, the hub
 * asks {@link LiveClients} for the client's socket and from then on passes
 * the plugin's live frames to it.
 *
 * Nothing here imports `cloudflare:workers`, so the unit tests run the hub
 * under Node with fake sockets.
 */

import {
  FrameReader,
  HOP_BY_HOP,
  MAX_LIVE_FRAME_BYTES,
  RELAY_HEALTH_PATH,
  RELAY_PREFIX,
  RELAY_PULL_PATH,
  RELAY_PUSH_PATH,
  RELAY_SOCKET_HEADER,
  bearerToken,
  constantTimeEqual,
  encodeFrame,
  encodeNotice,
  isIpAddress,
  isRequestId,
  parseLiveFrame,
  parseRequestHead,
  parseResponseHead,
  providerHeader,
  type LiveFrame,
  type RelayHeader,
  type RelayRequestHead,
} from "./protocol.ts";

/** A plugin socket as the hub uses it. */
export interface RelaySocket {
  /**
   * Sends one message: a notice as text, a live frame as bytes.
   *
   * @param message - The message.
   */
  send(message: string | Uint8Array<ArrayBuffer>): void;
}

/** A public client's WebSocket as the hub uses it. */
export interface LiveClient {
  /**
   * Sends one message to the client.
   *
   * @param message - Text or binary.
   */
  send(message: string | Uint8Array<ArrayBuffer>): void;
  /**
   * Closes the client's socket.
   *
   * @param code - A sendable close code.
   * @param reason - The reason.
   */
  close(code: number, reason: string): void;
}

/** Where the hub finds and makes client sockets (the Worker's sockets). */
export interface LiveClients<S extends RelaySocket> {
  /**
   * Accepts a client's upgrade the library accepted.
   *
   * @param id - The request id, the connection's id from now on.
   * @param via - The plugin socket that served the upgrade; the
   *   connection's frames travel on it.
   * @param local - The plugin's socket id for the connection.
   * @param protocol - The subprotocol the library chose, or `""`.
   * @returns The `101` answer for the client and its socket.
   */
  accept(
    id: string,
    via: S,
    local: string,
    protocol: string,
  ): { response: Response; client: LiveClient };
  /**
   * The client socket of a connection.
   *
   * @param id - The connection's id.
   * @returns Its socket, or `null` when it is gone.
   */
  find(id: string): LiveClient | null;
}

/** What became of a live frame the plugin sent. */
export type LiveOutcome = "delivered" | "held" | "dropped" | "malformed";

/** Most frames held for a connection whose client is not accepted yet. */
const MAX_EARLY_FRAMES = 64;

/** Most bytes held for such a connection. */
const MAX_EARLY_BYTES = 4 * MAX_LIVE_FRAME_BYTES;

/** How long the hub waits, in milliseconds. */
export interface RelayTimeouts {
  /** For a free plugin socket before answering 503. */
  queueMs: number;
  /** For the plugin to push a response head after the notice, before 504. */
  headMs: number;
}

/** Default waits: 30 s for a socket, 100 s for a response head. */
export const DEFAULT_TIMEOUTS: RelayTimeouts = { queueMs: 30_000, headMs: 100_000 };

/**
 * One public request the hub holds.
 *
 * @typeParam S - The plugin socket type.
 */
interface Pending<S extends RelaySocket> {
  /** Request head for the plugin's pull. */
  head: RelayRequestHead;
  /** Request body, or `null` for a request without one. */
  body: ReadableStream<Uint8Array> | null;
  /** Answers the public client. */
  answer: (response: Response) => void;
  /** Whether the client has been answered. */
  answered: boolean;
  /** Whether the plugin has pulled the request. */
  pulled: boolean;
  /** The socket serving it, once one was free. */
  socket: S | null;
  /** The running wait (for a socket, then for the head). */
  timer: ReturnType<typeof setTimeout> | null;
  /** Stops the plugin's push while its head is still arriving. */
  headRead: AbortController | null;
  /**
   * Live frames the library sent before the client's socket was accepted,
   * for an upgrade; `null` for any other request.
   */
  early: LiveFrame[] | null;
  /** Bytes of the frames in `early`, against {@link MAX_EARLY_BYTES}. */
  earlyBytes: number;
}

/** Statuses whose responses have no body. */
const NULL_BODY = new Set([204, 205, 304]);

/**
 * A JSON answer for the public client or the plugin.
 *
 * @param status - HTTP status.
 * @param error - Short error code.
 * @param message - What happened, for a person.
 * @returns The response.
 */
function problem(status: number, error: string, message: string): Response {
  return new Response(JSON.stringify({ error, message, status }), {
    status,
    headers: { "content-type": "application/json", "cache-control": "no-store" },
  });
}

/**
 * A stream of `frame` followed by `body`.
 *
 * @param frame - The framed head.
 * @param body - The body that follows, or `null`.
 * @returns The combined stream.
 */
export function framed(
  frame: Uint8Array,
  body: ReadableStream<Uint8Array> | null,
): ReadableStream<Uint8Array> {
  const reader = body?.getReader();
  let sentHead = false;
  return new ReadableStream<Uint8Array>({
    async pull(controller) {
      if (!sentHead) {
        sentHead = true;
        controller.enqueue(frame);
        return;
      }
      if (!reader) {
        controller.close();
        return;
      }
      const { done, value } = await reader.read();
      if (done) controller.close();
      else controller.enqueue(value);
    },
    async cancel(reason) {
      await reader?.cancel(reason);
    },
  });
}

/** A frame's head and the body that follows it. */
export interface ReadFrame {
  /** The head's JSON text. */
  json: string;
  /** The rest of the stream. */
  body: ReadableStream<Uint8Array>;
}

/**
 * Reads a frame's head off a stream.
 *
 * @param stream - The framed stream.
 * @param onBodyDone - Called once when the body ends, fails or is cancelled.
 * @param signal - Cancels the stream while the head is still arriving; the
 *   read then ends as a truncated frame.
 * @returns The head's JSON and the body, or why the frame is refused.
 */
export async function readFrame(
  stream: ReadableStream<Uint8Array>,
  onBodyDone: () => void = () => {},
  signal?: AbortSignal,
): Promise<ReadFrame | string> {
  const reader = stream.getReader();
  const frames = new FrameReader();
  const stop = (): void => {
    reader.cancel().catch(() => {});
  };
  if (signal?.aborted) stop();
  else signal?.addEventListener("abort", stop, { once: true });
  try {
    return await readHead(reader, frames, onBodyDone);
  } finally {
    signal?.removeEventListener("abort", stop);
  }
}

/**
 * The loop behind {@link readFrame}.
 *
 * @param reader - The stream's reader.
 * @param frames - The frame decoder.
 * @param onBodyDone - Called once when the body ends, fails or is cancelled.
 * @returns The head's JSON and the body, or why the frame is refused.
 */
async function readHead(
  reader: ReadableStreamDefaultReader<Uint8Array>,
  frames: FrameReader,
  onBodyDone: () => void,
): Promise<ReadFrame | string> {
  for (;;) {
    let chunk: ReadableStreamReadResult<Uint8Array>;
    try {
      chunk = await reader.read();
    } catch {
      onBodyDone();
      return "stream failed inside the frame head";
    }
    const step = chunk.done ? frames.end() : frames.push(chunk.value);
    if (step.kind === "need") continue;
    if (step.kind === "error") {
      onBodyDone();
      await reader.cancel().catch(() => {});
      return step.reason;
    }
    let finished = false;
    const done = (): void => {
      if (finished) return;
      finished = true;
      onBodyDone();
    };
    const rest = step.rest;
    const body = new ReadableStream<Uint8Array>({
      start(controller) {
        if (rest.length > 0) controller.enqueue(rest);
      },
      async pull(controller) {
        try {
          const { done: ended, value } = await reader.read();
          if (ended) {
            controller.close();
            done();
          } else {
            controller.enqueue(value);
          }
        } catch (err) {
          controller.error(err);
          done();
        }
      },
      async cancel(reason) {
        done();
        await reader.cancel(reason).catch(() => {});
      },
    });
    return { json: step.json, body };
  }
}

/**
 * The head the relay hands the plugin for a public request.
 *
 * @param request - The public request.
 * @returns The head, or why the request is refused.
 */
export function publicHead(request: Request, upgrade = false): RelayRequestHead | string {
  const url = new URL(request.url);
  const headers: RelayHeader[] = [];
  request.headers.forEach((value, name) => {
    const lower = name.toLowerCase();
    if (HOP_BY_HOP.has(lower) || providerHeader(lower)) return;
    headers.push({ name: lower, value });
  });
  const head: RelayRequestHead = {
    method: request.method,
    target: `${url.pathname}${url.search}`,
    headers,
  };
  const client = request.headers.get("cf-connecting-ip");
  if (client !== null && isIpAddress(client)) head.client = client;
  if (upgrade) head.upgrade = true;
  // The plugin checks the head the same way; refusing here answers the
  // client instead of failing the pull.
  const checked = parseRequestHead(JSON.stringify(head));
  return checked;
}

/**
 * Delivers one live frame to a client socket.
 *
 * @param client - The socket.
 * @param frame - The frame.
 */
function deliver(client: LiveClient, frame: LiveFrame): void {
  try {
    switch (frame.op) {
      case "text":
        client.send(frame.text);
        break;
      case "binary":
        client.send(new Uint8Array(frame.data));
        break;
      case "close":
        client.close(frame.code, frame.reason);
        break;
    }
  } catch {
    // The client is already gone; its close tells the plugin.
  }
}

/**
 * Holds public requests until the plugin serves them.
 *
 * @typeParam S - The plugin socket type, which {@link LiveClients} gets back.
 */
export class RelayHub<S extends RelaySocket = RelaySocket> {
  /** Token every relay path needs. */
  readonly #token: string;
  /** Waits before giving up. */
  readonly #timeouts: RelayTimeouts;
  /** Client sockets for live connections, or `undefined` to refuse upgrades. */
  readonly #live: LiveClients<S> | undefined;
  /** Plugin sockets, each with the request it serves or `null` when idle. */
  readonly #sockets = new Map<S, string | null>();
  /** Requests by id. */
  readonly #pending = new Map<string, Pending<S>>();
  /** Ids waiting for a free socket, oldest first. */
  readonly #queue: string[] = [];

  /**
   * Creates a hub.
   *
   * @param token - The bearer token the plugin deployed with the Worker.
   * @param timeouts - How long to wait for a socket and for a head.
   * @param live - Client sockets for live connections; without it the hub
   *   answers upgrades 501.
   */
  constructor(token: string, timeouts: RelayTimeouts = DEFAULT_TIMEOUTS, live?: LiveClients<S>) {
    this.#token = token;
    this.#timeouts = timeouts;
    this.#live = live;
  }

  /**
   * Whether a request carries the relay's token.
   *
   * @param request - Any request to a relay path.
   * @returns `true` when its bearer token matches.
   */
  authorized(request: Request): boolean {
    const token = bearerToken(request.headers.get("authorization"));
    return token !== null && this.#token.length > 0 && constantTimeEqual(token, this.#token);
  }

  /**
   * Adds a plugin socket; it starts idle.
   *
   * @param socket - The accepted socket.
   */
  attach(socket: S): void {
    if (!this.#sockets.has(socket)) this.#sockets.set(socket, null);
    this.#dispatch();
  }

  /**
   * Removes a plugin socket that closed. The request it served, if any, is
   * answered with 502.
   *
   * @param socket - The closed socket.
   */
  detach(socket: S): void {
    const id = this.#sockets.get(socket);
    this.#sockets.delete(socket);
    if (id)
      this.#fail(id, problem(502, "relay_lost", "The library's connection to the relay closed."));
  }

  /**
   * Passes a live frame the plugin sent to its client.
   *
   * A frame for an upgrade whose `101` push is still on its way is held
   * and delivered once the client is accepted.
   *
   * @param bytes - The binary message from a plugin socket.
   * @returns What became of it.
   */
  pluginFrame(bytes: Uint8Array): LiveOutcome {
    const frame = parseLiveFrame(bytes);
    if (typeof frame === "string") return "malformed";
    const client = this.#live?.find(frame.id) ?? null;
    if (client) {
      deliver(client, frame);
      return "delivered";
    }
    const pending = this.#pending.get(frame.id);
    if (!pending?.early || pending.answered) return "dropped";
    if (
      pending.early.length >= MAX_EARLY_FRAMES ||
      pending.earlyBytes + bytes.length > MAX_EARLY_BYTES
    ) {
      return "dropped";
    }
    pending.early.push(frame);
    pending.earlyBytes += bytes.length;
    return "held";
  }

  /**
   * Number of plugin sockets held.
   *
   * @returns Idle and busy sockets together.
   */
  get sockets(): number {
    return this.#sockets.size;
  }

  /**
   * Answers one request to the relay: a relay path or a public request.
   *
   * @param request - The request (WebSocket upgrades to the connect path are
   *   handled by the Worker before this).
   * @returns The answer.
   */
  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname.startsWith(RELAY_PREFIX)) {
      if (!this.authorized(request))
        return problem(401, "unauthorized", "This path is for the library only.");
      if (url.pathname === RELAY_HEALTH_PATH && request.method === "GET") {
        return Response.json({ sockets: this.#sockets.size, waiting: this.#queue.length });
      }
      if (url.pathname.startsWith(RELAY_PULL_PATH) && request.method === "GET") {
        return this.#pull(url.pathname.slice(RELAY_PULL_PATH.length));
      }
      if (url.pathname.startsWith(RELAY_PUSH_PATH) && request.method === "POST") {
        return this.#push(url.pathname.slice(RELAY_PUSH_PATH.length), request.body);
      }
      return problem(404, "not_found", "No such relay path.");
    }
    return this.#public(request);
  }

  /**
   * Holds a public request until the plugin answers it.
   *
   * @param request - The public request.
   * @returns The library's answer, or a relay error.
   */
  #public(request: Request): Promise<Response> {
    const upgrade = request.headers.get("upgrade");
    if (upgrade !== null) {
      if (!this.#live) {
        return Promise.resolve(
          problem(501, "upgrade_unsupported", "The relay does not carry WebSocket connections."),
        );
      }
      if (upgrade.toLowerCase() !== "websocket" || request.method !== "GET") {
        return Promise.resolve(
          problem(400, "bad_request", "Only WebSocket upgrades on GET are carried."),
        );
      }
    }
    const head = publicHead(request, upgrade !== null);
    if (typeof head === "string") return Promise.resolve(problem(400, "bad_request", head));
    if (this.#sockets.size === 0) {
      return Promise.resolve(
        problem(503, "library_offline", "The library is not connected to the relay."),
      );
    }
    const id = crypto.randomUUID();
    const bodyless = request.method === "GET" || request.method === "HEAD";
    return new Promise<Response>((answer) => {
      const pending: Pending<S> = {
        head,
        body: bodyless ? null : request.body,
        answer,
        answered: false,
        pulled: false,
        socket: null,
        timer: null,
        headRead: null,
        early: head.upgrade ? [] : null,
        earlyBytes: 0,
      };
      this.#pending.set(id, pending);
      this.#queue.push(id);
      pending.timer = setTimeout(
        () =>
          this.#fail(
            id,
            problem(503, "library_busy", "The library did not take the request in time."),
          ),
        this.#timeouts.queueMs,
      );
      this.#dispatch();
    });
  }

  /**
   * Announces waiting requests on idle sockets.
   */
  #dispatch(): void {
    for (const [socket, busy] of this.#sockets) {
      if (busy !== null) continue;
      let id: string | undefined;
      while ((id = this.#queue.shift()) !== undefined && !this.#pending.has(id)) {
        // Skip requests that already timed out.
      }
      if (id === undefined) return;
      const pending = this.#pending.get(id);
      if (!pending) return;
      this.#sockets.set(socket, id);
      pending.socket = socket;
      if (pending.timer) clearTimeout(pending.timer);
      pending.timer = setTimeout(
        () =>
          this.#fail(id, problem(504, "library_timeout", "The library did not answer in time.")),
        this.#timeouts.headMs,
      );
      try {
        socket.send(encodeNotice({ t: "request", id }));
      } catch {
        this.detach(socket);
      }
    }
  }

  /**
   * Hands a request to the plugin.
   *
   * @param id - The request's id.
   * @returns The framed request, or 404 when it is unknown or already pulled.
   */
  #pull(id: string): Response {
    const pending = isRequestId(id) ? this.#pending.get(id) : undefined;
    if (!pending || pending.pulled || pending.answered) {
      return problem(404, "not_found", "No such request is waiting.");
    }
    pending.pulled = true;
    return new Response(framed(encodeFrame(pending.head), pending.body), {
      headers: { "content-type": "application/octet-stream" },
    });
  }

  /**
   * Answers the public client with what the plugin pushed.
   *
   * @param id - The request's id.
   * @param body - The framed response.
   * @returns 204 once the body has been passed on, or an error.
   */
  async #push(id: string, body: ReadableStream<Uint8Array> | null): Promise<Response> {
    const pending = isRequestId(id) ? this.#pending.get(id) : undefined;
    if (!pending || !pending.pulled || pending.answered) {
      return problem(404, "not_found", "No such request is waiting for an answer.");
    }
    if (!body) {
      this.#fail(id, problem(502, "bad_gateway", "The library sent an empty answer."));
      return problem(400, "bad_frame", "The answer has no body.");
    }
    let resolveDone: () => void = () => {};
    const done = new Promise<void>((resolve) => {
      resolveDone = resolve;
    });
    const headRead = new AbortController();
    pending.headRead = headRead;
    const frame = await readFrame(body, () => resolveDone(), headRead.signal);
    pending.headRead = null;
    if (typeof frame === "string") {
      this.#fail(id, problem(502, "bad_gateway", "The library sent a malformed answer."));
      return problem(400, "bad_frame", frame);
    }
    const head = parseResponseHead(frame.json);
    if (typeof head === "string") {
      await frame.body.cancel().catch(() => {});
      this.#fail(id, problem(502, "bad_gateway", "The library sent a malformed answer."));
      return problem(400, "bad_frame", head);
    }
    if (pending.answered || this.#pending.get(id) !== pending) {
      // The wait for the head ran out while it was arriving; the client
      // already has its 504 and nobody would read this body.
      await frame.body.cancel().catch(() => {});
      return problem(410, "gone", "The request was answered before its head arrived.");
    }
    if (head.status === 101) {
      await frame.body.cancel().catch(() => {});
      return this.#accept(id, pending, head.headers);
    }
    const headers = new Headers();
    for (const field of head.headers) {
      // The relay re-frames the body, so its length is the runtime's to set.
      if (field.name === "content-length" || field.name === RELAY_SOCKET_HEADER) continue;
      headers.append(field.name, field.value);
    }
    const noBody = NULL_BODY.has(head.status) || pending.head.method === "HEAD";
    if (noBody) await frame.body.cancel().catch(() => {});
    this.#answer(id, new Response(noBody ? null : frame.body, { status: head.status, headers }));
    if (!noBody) await done;
    this.#release(id);
    return new Response(null, { status: 204 });
  }

  /**
   * Accepts the client's upgrade once the library accepted it.
   *
   * @param id - The request's id.
   * @param pending - The request.
   * @param fields - The `101` head's fields.
   * @returns 204 for the plugin, or 400 when the `101` does not fit.
   */
  #accept(id: string, pending: Pending<S>, fields: RelayHeader[]): Response {
    const local = fields.find((field) => field.name === RELAY_SOCKET_HEADER)?.value ?? "";
    const via = pending.socket;
    if (!pending.head.upgrade || !this.#live || !via || !isRequestId(local)) {
      this.#fail(id, problem(502, "bad_gateway", "The library sent a malformed answer."));
      return problem(400, "bad_frame", "101 answers only an upgrade, naming the plugin's socket");
    }
    const protocol = fields.find((field) => field.name === "sec-websocket-protocol")?.value ?? "";
    const { response, client } = this.#live.accept(id, via, local, protocol);
    for (const frame of pending.early ?? []) deliver(client, frame);
    pending.early = null;
    this.#answer(id, response);
    this.#release(id);
    return new Response(null, { status: 204 });
  }

  /**
   * Answers the public client once.
   *
   * @param id - The request's id.
   * @param response - The answer.
   */
  #answer(id: string, response: Response): void {
    const pending = this.#pending.get(id);
    if (!pending || pending.answered) return;
    pending.answered = true;
    if (pending.timer) clearTimeout(pending.timer);
    pending.timer = null;
    pending.answer(response);
  }

  /**
   * Answers the client with an error and frees the request's socket.
   *
   * @param id - The request's id.
   * @param response - The error answer.
   */
  #fail(id: string, response: Response): void {
    const pending = this.#pending.get(id);
    if (!pending) return;
    if (!pending.pulled) pending.body?.cancel().catch(() => {});
    // A push still sending its head would otherwise run on unread.
    pending.headRead?.abort();
    this.#answer(id, response);
    this.#release(id);
  }

  /**
   * Forgets a request and frees its socket for the next one.
   *
   * @param id - The request's id.
   */
  #release(id: string): void {
    const pending = this.#pending.get(id);
    if (!pending) return;
    if (pending.timer) clearTimeout(pending.timer);
    this.#pending.delete(id);
    // A request that never reached a socket leaves the queue too, so waits
    // that ran out while every socket was busy do not pile up.
    const queued = this.#queue.indexOf(id);
    if (queued >= 0) this.#queue.splice(queued, 1);
    const socket = pending.socket;
    if (socket && this.#sockets.get(socket) === id) this.#sockets.set(socket, null);
    this.#dispatch();
  }
}
