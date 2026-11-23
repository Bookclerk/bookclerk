/**
 * Serves one request the edge sent through the library, and shapes the
 * answer for an HTTP/2 stream.
 *
 * workerd delivers a socket's messages one at a time, and a stream from
 * one invocation can't be read in the next, so the connector can't hold a
 * response body open while it waits for the edge to widen the stream's
 * window. A response is therefore read into memory up to
 * {@link MAX_BUFFERED_RESPONSE} bytes. A larger `GET` answer of known
 * length that allows byte ranges is sent in pieces instead: the head and
 * the first bytes now, then each later piece fetched from the library with
 * a `Range` request when the window opens ({@link Continuation}).
 *
 * A WebSocket request ({@link serveUpgrade}) opens a socket to the library
 * through `ORIGIN.connect`; the session then carries frames between it and
 * the client's stream.
 */

import { acceptKey, validKey } from "./ws.ts";

/** Largest request body the connector buffers before handing it on, in bytes. */
export const MAX_REQUEST_BODY = 32 * 1024 * 1024;

/** Largest response the connector buffers whole, in bytes. */
export const MAX_BUFFERED_RESPONSE = 4 * 1024 * 1024;

/** Bytes each later piece of a large response asks the library for. */
export const CONTINUATION_BYTES = 1024 * 1024;

/** Statuses whose responses carry no body. */
const NULL_BODY = new Set([204, 205, 304]);

/** Request fields that describe the connection, never passed on. */
const REQUEST_HOP_BY_HOP = new Set([
  "connection",
  "keep-alive",
  "proxy-connection",
  "te",
  "trailer",
  "transfer-encoding",
  "upgrade",
  "expect",
  "content-length",
  "host",
]);

/** Response fields HTTP/2 does not carry. */
const RESPONSE_HOP_BY_HOP = new Set([
  "connection",
  "keep-alive",
  "proxy-connection",
  "transfer-encoding",
  "upgrade",
]);

/** WebSocket handshake fields the connector answers itself, never passed on. */
const HANDSHAKE_FIELDS = new Set([
  "sec-websocket-key",
  "sec-websocket-version",
  "sec-websocket-extensions",
]);

/** A request the edge sent, once its header block is read. */
export interface RequestHead {
  /** Method, as sent. */
  method: string;
  /** Origin-form target (`:path`). */
  target: string;
  /** Regular fields in order, names lowercase; repeated names stay separate. */
  headers: Array<[string, string]>;
}

/** A WebSocket to the library, as one invocation reaches it. */
export interface LibrarySocket {
  /**
   * Sends one message to the library.
   *
   * @param message - Text or binary.
   */
  send(message: string | Uint8Array): Promise<void>;
  /**
   * Closes the socket.
   *
   * @param code - Close code.
   * @param reason - Close reason.
   */
  close(code: number, reason: string): Promise<void>;
}

/** What the library answered an upgrade with. */
export type LibraryUpgrade =
  /** It accepted: the socket's id and the subprotocol it chose. */
  | { socket: { id: string; protocol: string } }
  /** It answered without upgrading. */
  | { response: Response };

/** The library, as the session reaches it. */
export interface Library {
  /**
   * Serves one request.
   *
   * @param request - The request.
   * @param clientAddress - The client's IP from the edge.
   * @returns The library's response.
   */
  fetch(request: Request, clientAddress: string | undefined): Promise<Response>;
  /**
   * Opens a WebSocket to the library for one upgrade.
   *
   * @param request - The client's `GET`.
   * @param clientAddress - The client's IP from the edge.
   * @param stream - The HTTP/2 stream that carries the client's side, kept
   *   with the socket so its messages find the stream.
   * @returns The socket, or the library's answer when it did not upgrade.
   */
  connect(
    request: Request,
    clientAddress: string | undefined,
    stream: number,
  ): Promise<LibraryUpgrade>;
  /**
   * A socket the library accepted, by id, for this invocation.
   *
   * @param id - The socket's id.
   * @returns The socket, or `null` once it is gone.
   */
  socket(id: string): LibrarySocket | null;
}

/** What a large response still has to send, and how to fetch it. */
export interface Continuation {
  /** Origin-form target of the request. */
  target: string;
  /** The request's fields, without connection fields or ranges. */
  headers: Array<[string, string]>;
  /** The client's IP. */
  clientAddress: string | undefined;
  /** Next resource offset to fetch. */
  next: number;
  /** Last resource offset the response covers (inclusive). */
  end: number;
  /** The resource's full length, as `Content-Range` names it. */
  total: number | null;
  /** `If-Range` value: a strong ETag or a `Last-Modified` date. */
  validator: string | null;
}

/** Who produced an answer, for the edge's response metadata. */
export type Source = "origin" | "cloudflared";

/** An answer ready for the stream. */
export interface Answer {
  /** HTTP status. */
  status: number;
  /** Response fields (names as the library gave them). */
  fields: Array<[string, string]>;
  /** The body, or its first piece. */
  body: Uint8Array;
  /** The `content-length` to announce, or `null` for none. */
  length: number | null;
  /** The rest of a large body, or `null` when `body` is all of it. */
  more: Continuation | null;
  /** Whether the library or the connector produced it. */
  source: Source;
}

/** What {@link readUpTo} read. */
interface Read {
  /** The bytes, in one buffer. */
  bytes: Uint8Array;
  /** Whether the body ended within the limit. */
  done: boolean;
}

/**
 * Reads a body until it ends or passes `limit` bytes.
 *
 * @param body - The body.
 * @param limit - Bytes past which reading stops.
 * @returns The bytes read and whether the body ended; a body that did not
 *   end is cancelled.
 */
export async function readUpTo(body: ReadableStream<Uint8Array>, limit: number): Promise<Read> {
  const reader = body.getReader();
  const parts: Uint8Array[] = [];
  let total = 0;
  for (;;) {
    const { value, done } = await reader.read();
    if (done) return { bytes: join(parts, total), done: true };
    parts.push(value);
    total += value.length;
    if (total > limit) {
      await reader.cancel().catch(() => {});
      return { bytes: join(parts, total), done: false };
    }
  }
}

/**
 * Joins byte arrays.
 *
 * @param parts - The arrays.
 * @param total - Their summed length.
 * @returns One array.
 */
function join(parts: Uint8Array[], total: number): Uint8Array {
  if (parts.length === 1) return parts[0] as Uint8Array;
  const out = new Uint8Array(total);
  let at = 0;
  for (const part of parts) {
    out.set(part, at);
    at += part.length;
  }
  return out;
}

/**
 * The response fields to send, `set-cookie` kept apart and connection
 * fields dropped.
 *
 * @param headers - The library's fields.
 * @returns Name and value pairs.
 */
export function responseFields(headers: Headers): Array<[string, string]> {
  const out: Array<[string, string]> = [];
  headers.forEach((value, name) => {
    const lower = name.toLowerCase();
    if (lower !== "set-cookie" && !RESPONSE_HOP_BY_HOP.has(lower)) out.push([name, value]);
  });
  const cookies = typeof headers.getSetCookie === "function" ? headers.getSetCookie() : [];
  for (const cookie of cookies) out.push(["set-cookie", cookie]);
  return out;
}

/**
 * The request fields to pass to the library.
 *
 * @param head - The request.
 * @returns Fields without connection fields or the edge's own.
 */
function requestFields(head: RequestHead): Array<[string, string]> {
  return head.headers.filter(
    ([name]) => !REQUEST_HOP_BY_HOP.has(name) && !name.startsWith("cf-cloudflared-"),
  );
}

/**
 * Builds the request the library sees.
 *
 * @param hostname - The public hostname.
 * @param method - Method.
 * @param target - Origin-form target.
 * @param fields - Header fields.
 * @param body - Body bytes, or `null`.
 * @returns The request.
 */
function libraryRequest(
  hostname: string,
  method: string,
  target: string,
  fields: Array<[string, string]>,
  body: Uint8Array<ArrayBuffer> | null,
): Request {
  const headers = new Headers();
  for (const [name, value] of fields) headers.append(name, value);
  return new Request(`https://${hostname}${target}`, {
    method,
    headers,
    ...(body !== null && body.length > 0 && method !== "GET" && method !== "HEAD" ? { body } : {}),
  });
}

/**
 * An error the connector answers itself.
 *
 * @param status - HTTP status.
 * @param message - What happened.
 * @param extra - More response fields.
 * @returns The answer.
 */
export function errorAnswer(
  status: number,
  message: string,
  extra: Array<[string, string]> = [],
): Answer {
  const body = new TextEncoder().encode(
    JSON.stringify({ error: "library_unavailable", message, status }),
  );
  return {
    status,
    fields: [["content-type", "application/json"], ...extra],
    body,
    length: body.length,
    more: null,
    source: "cloudflared",
  };
}

/**
 * Parses `Content-Range: bytes first-last/total`.
 *
 * @param value - The field value.
 * @returns The span, or `null` when it does not parse.
 */
export function parseContentRange(
  value: string | null,
): { first: number; last: number; total: number | null } | null {
  const match =
    value === null ? null : /^bytes (\d{1,15})-(\d{1,15})\/(\d{1,15}|\*)$/.exec(value.trim());
  if (!match) return null;
  const first = Number(match[1]);
  const last = Number(match[2]);
  const total = match[3] === "*" ? null : Number(match[3]);
  if (last < first || (total !== null && last >= total)) return null;
  return { first, last, total };
}

/**
 * The span a large response covers, when the rest may be fetched by range.
 *
 * @param method - The request method.
 * @param response - The library's response.
 * @returns First and last offsets, total and validator, or `null` when the
 *   response can't be continued.
 */
function continuable(
  method: string,
  response: Response,
):
  (Omit<Continuation, "target" | "headers" | "clientAddress" | "next"> & { first: number }) | null {
  if (method !== "GET" || response.headers.has("content-encoding")) return null;
  const lengthText = response.headers.get("content-length");
  if (lengthText === null || !/^\d{1,15}$/.test(lengthText)) return null;
  const length = Number(lengthText);
  const etag = response.headers.get("etag");
  const validator =
    etag !== null && !etag.startsWith("W/") ? etag : response.headers.get("last-modified");
  if (response.status === 200) {
    if (response.headers.get("accept-ranges")?.toLowerCase() !== "bytes" || length === 0) {
      return null;
    }
    return { first: 0, end: length - 1, total: length, validator };
  }
  if (response.status === 206) {
    const span = parseContentRange(response.headers.get("content-range"));
    if (span === null || span.last - span.first + 1 !== length) return null;
    return { first: span.first, end: span.last, total: span.total, validator };
  }
  return null;
}

/**
 * Serves one request through the library.
 *
 * @param library - The library.
 * @param hostname - The public hostname the request arrived on.
 * @param head - The request.
 * @param body - The request body, or `null`.
 * @param clientAddress - The client's IP.
 * @returns The answer to write.
 */
export async function serveRequest(
  library: Library,
  hostname: string,
  head: RequestHead,
  body: Uint8Array<ArrayBuffer> | null,
  clientAddress: string | undefined,
): Promise<Answer> {
  const fields = requestFields(head);
  let response: Response;
  try {
    response = await library.fetch(
      libraryRequest(hostname, head.method, head.target, fields, body),
      clientAddress,
    );
  } catch {
    return errorAnswer(502, "The library could not serve the request.");
  }
  return shapeResponse(head, fields, clientAddress, response);
}

/**
 * Shapes the library's response for the stream: buffered whole, or its
 * first piece and how to fetch the rest.
 *
 * @param head - The request.
 * @param fields - The request fields the library saw.
 * @param clientAddress - The client's IP.
 * @param response - The library's response.
 * @returns The answer to write.
 */
async function shapeResponse(
  head: RequestHead,
  fields: Array<[string, string]>,
  clientAddress: string | undefined,
  response: Response,
): Promise<Answer> {
  const status = response.status >= 200 && response.status <= 599 ? response.status : 502;
  const out = responseFields(response.headers).filter(
    ([name]) => name.toLowerCase() !== "content-length",
  );
  const answer = (body: Uint8Array, length: number | null, more: Continuation | null): Answer => ({
    status,
    fields: out,
    body,
    length,
    more,
    source: "origin",
  });
  if (head.method === "HEAD" || NULL_BODY.has(status) || !response.body) {
    await response.body?.cancel().catch(() => {});
    const length = response.headers.get("content-length");
    const announced =
      head.method === "HEAD" && length !== null && /^\d{1,15}$/.test(length)
        ? Number(length)
        : null;
    return answer(new Uint8Array(), NULL_BODY.has(status) ? null : (announced ?? 0), null);
  }
  const read = await readUpTo(response.body, MAX_BUFFERED_RESPONSE);
  if (read.done) return answer(read.bytes, read.bytes.length, null);
  const span = continuable(head.method, response);
  if (span === null) {
    return errorAnswer(502, "The response is too large to pass through the Cloudflare Tunnel.");
  }
  const length = span.end - span.first + 1;
  const first = read.bytes.subarray(0, Math.min(read.bytes.length, length));
  const next = span.first + first.length;
  return answer(
    first,
    length,
    next > span.end
      ? null
      : {
          target: head.target,
          headers: fields.filter(([name]) => name !== "range" && name !== "if-range"),
          clientAddress,
          next,
          end: span.end,
          total: span.total,
          validator: span.validator,
        },
  );
}

/** What an upgrade came to. */
export type UpgradeAnswer =
  /** An HTTP answer that ends the stream (a refusal or an error). */
  | { kind: "answer"; answer: Answer }
  /**
   * The library accepted: send `fields` with status 200, then the stream
   * carries WebSocket frames to and from socket `id`.
   */
  | { kind: "socket"; id: string; fields: Array<[string, string]> };

/**
 * An answer that ends the stream, as an {@link UpgradeAnswer}.
 *
 * @param status - HTTP status.
 * @param message - What happened.
 * @param extra - More response fields.
 * @returns The answer.
 */
function refuse(
  status: number,
  message: string,
  extra: Array<[string, string]> = [],
): UpgradeAnswer {
  return { kind: "answer", answer: errorAnswer(status, message, extra) };
}

/** A subprotocol token (RFC 9110 `token`). */
const TOKEN = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;

/**
 * Serves one WebSocket request: checks the client's handshake, opens a
 * socket to the library and builds the accepting fields, or passes on the
 * library's refusal.
 *
 * The edge marks the request with its upgrade field and keeps the client's
 * handshake fields; the stream answers with status 200 and the fields a
 * `101` would carry, which the edge turns back into the client's `101`.
 *
 * @param library - The library.
 * @param hostname - The public hostname the request arrived on.
 * @param head - The request.
 * @param clientAddress - The client's IP.
 * @param stream - The stream that carries the client's side.
 * @returns What to write.
 */
export async function serveUpgrade(
  library: Library,
  hostname: string,
  head: RequestHead,
  clientAddress: string | undefined,
  stream: number,
): Promise<UpgradeAnswer> {
  const field = (name: string): string[] =>
    head.headers.filter(([n]) => n === name).map(([, value]) => value);
  if (head.method !== "GET") return refuse(400, "A WebSocket upgrade is a GET.");
  const versions = field("sec-websocket-version");
  if (versions.length !== 1 || versions[0] !== "13") {
    return refuse(426, "Only WebSocket version 13 is supported.", [
      ["sec-websocket-version", "13"],
    ]);
  }
  const keys = field("sec-websocket-key");
  if (keys.length !== 1 || !validKey(keys[0] as string)) {
    return refuse(400, "Malformed Sec-WebSocket-Key.");
  }
  // Worked out first, so nothing waits between the library's accept and the answer.
  const accept = await acceptKey(keys[0] as string);
  const fields = requestFields(head).filter(([name]) => !HANDSHAKE_FIELDS.has(name));
  let result: LibraryUpgrade;
  try {
    result = await library.connect(
      libraryRequest(hostname, "GET", head.target, fields, null),
      clientAddress,
      stream,
    );
  } catch {
    return refuse(502, "The library could not open the connection.");
  }
  if ("response" in result) {
    return {
      kind: "answer",
      answer: await shapeResponse(head, fields, clientAddress, result.response),
    };
  }
  const out: Array<[string, string]> = [
    ["upgrade", "websocket"],
    ["connection", "Upgrade"],
    ["sec-websocket-accept", accept],
  ];
  if (TOKEN.test(result.socket.protocol)) {
    out.push(["sec-websocket-protocol", result.socket.protocol]);
  }
  return { kind: "socket", id: result.socket.id, fields: out };
}

/**
 * Fetches the next piece of a large response.
 *
 * @param library - The library.
 * @param hostname - The public hostname.
 * @param more - What is left.
 * @returns The piece and what is left after it, or `null` when the library
 *   no longer answers with the same resource (the stream must be reset).
 */
export async function continueBody(
  library: Library,
  hostname: string,
  more: Continuation,
): Promise<{ bytes: Uint8Array; more: Continuation | null } | null> {
  const last = Math.min(more.next + CONTINUATION_BYTES - 1, more.end);
  const fields: Array<[string, string]> = [
    ...more.headers,
    ["range", `bytes=${more.next}-${last}`],
  ];
  if (more.validator !== null) fields.push(["if-range", more.validator]);
  let response: Response;
  try {
    response = await library.fetch(
      libraryRequest(hostname, "GET", more.target, fields, null),
      more.clientAddress,
    );
  } catch {
    return null;
  }
  const span = parseContentRange(response.headers.get("content-range"));
  if (
    response.status !== 206 ||
    !response.body ||
    span === null ||
    span.first !== more.next ||
    span.last > last ||
    (more.total !== null && span.total !== more.total)
  ) {
    await response.body?.cancel().catch(() => {});
    return null;
  }
  const want = span.last - span.first + 1;
  const read = await readUpTo(response.body, want);
  if (read.bytes.length < want) return null;
  const bytes = read.bytes.subarray(0, want);
  const next = more.next + want;
  return { bytes, more: next > more.end ? null : { ...more, next } };
}
