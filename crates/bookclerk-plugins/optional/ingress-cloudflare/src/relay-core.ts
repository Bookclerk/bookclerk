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
 * Nothing here imports `cloudflare:workers`, so the unit tests run the hub
 * under Node with fake sockets.
 */

import {
  FrameReader,
  HOP_BY_HOP,
  RELAY_HEALTH_PATH,
  RELAY_PREFIX,
  RELAY_PULL_PATH,
  RELAY_PUSH_PATH,
  bearerToken,
  constantTimeEqual,
  encodeFrame,
  encodeNotice,
  isIpAddress,
  isRequestId,
  parseRequestHead,
  parseResponseHead,
  providerHeader,
  type RelayHeader,
  type RelayRequestHead,
} from "./protocol.ts";

/** A plugin socket as the hub uses it. */
export interface RelaySocket {
  /**
   * Sends one text message.
   *
   * @param text - The message.
   */
  send(text: string): void;
}

/** How long the hub waits, in milliseconds. */
export interface RelayTimeouts {
  /** For a free plugin socket before answering 503. */
  queueMs: number;
  /** For the plugin to push a response head after the notice, before 504. */
  headMs: number;
}

/** Default waits: 30 s for a socket, 100 s for a response head. */
export const DEFAULT_TIMEOUTS: RelayTimeouts = { queueMs: 30_000, headMs: 100_000 };

/** One public request the hub holds. */
interface Pending {
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
  socket: RelaySocket | null;
  /** The running wait (for a socket, then for the head). */
  timer: ReturnType<typeof setTimeout> | null;
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
 * @returns The head's JSON and the body, or why the frame is refused.
 */
export async function readFrame(
  stream: ReadableStream<Uint8Array>,
  onBodyDone: () => void = () => {},
): Promise<ReadFrame | string> {
  const reader = stream.getReader();
  const frames = new FrameReader();
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
export function publicHead(request: Request): RelayRequestHead | string {
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
  // The plugin checks the head the same way; refusing here answers the
  // client instead of failing the pull.
  const checked = parseRequestHead(JSON.stringify(head));
  return checked;
}

/**
 * Holds public requests until the plugin serves them.
 */
export class RelayHub {
  /** Token every relay path needs. */
  readonly #token: string;
  /** Waits before giving up. */
  readonly #timeouts: RelayTimeouts;
  /** Plugin sockets, each with the request it serves or `null` when idle. */
  readonly #sockets = new Map<RelaySocket, string | null>();
  /** Requests by id. */
  readonly #pending = new Map<string, Pending>();
  /** Ids waiting for a free socket, oldest first. */
  readonly #queue: string[] = [];

  /**
   * Creates a hub.
   *
   * @param token - The bearer token the plugin deployed with the Worker.
   * @param timeouts - How long to wait for a socket and for a head.
   */
  constructor(token: string, timeouts: RelayTimeouts = DEFAULT_TIMEOUTS) {
    this.#token = token;
    this.#timeouts = timeouts;
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
  attach(socket: RelaySocket): void {
    if (!this.#sockets.has(socket)) this.#sockets.set(socket, null);
    this.#dispatch();
  }

  /**
   * Removes a plugin socket that closed. The request it served, if any, is
   * answered with 502.
   *
   * @param socket - The closed socket.
   */
  detach(socket: RelaySocket): void {
    const id = this.#sockets.get(socket);
    this.#sockets.delete(socket);
    if (id)
      this.#fail(id, problem(502, "relay_lost", "The library's connection to the relay closed."));
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
    if (request.headers.get("upgrade") !== null) {
      return Promise.resolve(
        problem(501, "upgrade_unsupported", "The relay does not carry WebSocket connections yet."),
      );
    }
    const head = publicHead(request);
    if (typeof head === "string") return Promise.resolve(problem(400, "bad_request", head));
    if (this.#sockets.size === 0) {
      return Promise.resolve(
        problem(503, "library_offline", "The library is not connected to the relay."),
      );
    }
    const id = crypto.randomUUID();
    const bodyless = request.method === "GET" || request.method === "HEAD";
    return new Promise<Response>((answer) => {
      const pending: Pending = {
        head,
        body: bodyless ? null : request.body,
        answer,
        answered: false,
        pulled: false,
        socket: null,
        timer: null,
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
    const frame = await readFrame(body, () => resolveDone());
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
    const headers = new Headers();
    for (const field of head.headers) {
      // The relay re-frames the body, so its length is the runtime's to set.
      if (field.name === "content-length") continue;
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
    const socket = pending.socket;
    if (socket && this.#sockets.get(socket) === id) this.#sockets.set(socket, null);
    this.#dispatch();
  }
}
