/**
 * Serves one proxied request through the library and shapes the answer for
 * a muxado stream.
 *
 * workerd delivers a socket's messages one at a time, and a stream from
 * one invocation can't be read in the next, so the agent can't hold a
 * response body open while it waits for ngrok to widen the stream's
 * window. A response is therefore read into memory up to
 * {@link MAX_BUFFERED_RESPONSE} bytes. A larger `GET` answer of known
 * length that allows byte ranges is sent in pieces instead: the head and
 * the first bytes now, then each later piece fetched from the library with
 * a `Range` request when the window opens ({@link Continuation}).
 */

import { encodeResponseHead, errorResponse, type RequestHead } from "./http1.ts";

/** Largest request body the agent buffers before handing it on, in bytes. */
export const MAX_REQUEST_BODY = 32 * 1024 * 1024;

/** Largest response the agent buffers whole, in bytes. */
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
]);

/** The library, as the agent reaches it. */
export interface Library {
  /**
   * Serves one request.
   *
   * @param request - The request.
   * @param clientAddress - The client's IP from ngrok's proxy header.
   * @returns The library's response.
   */
  fetch(request: Request, clientAddress: string | undefined): Promise<Response>;
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

/** An answer ready for the stream. */
export interface Answer {
  /** Bytes to send now: the head and the body or its first piece. */
  bytes: Uint8Array;
  /** The rest of a large body, or `null` when `bytes` is all of it. */
  more: Continuation | null;
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
 * The response fields to send, `set-cookie` kept apart.
 *
 * @param headers - The library's fields.
 * @returns Name and value pairs.
 */
export function responseFields(headers: Headers): Array<[string, string]> {
  const out: Array<[string, string]> = [];
  headers.forEach((value, name) => {
    if (name.toLowerCase() !== "set-cookie") out.push([name, value]);
  });
  const cookies = typeof headers.getSetCookie === "function" ? headers.getSetCookie() : [];
  for (const cookie of cookies) out.push(["set-cookie", cookie]);
  return out;
}

/**
 * The request fields to pass to the library.
 *
 * @param head - The parsed request head.
 * @returns Fields without connection fields.
 */
function requestFields(head: RequestHead): Array<[string, string]> {
  return head.headers.filter(([name]) => !REQUEST_HOP_BY_HOP.has(name));
}

/**
 * Builds the request the library sees.
 *
 * @param domain - The public domain.
 * @param method - Method.
 * @param target - Origin-form target.
 * @param fields - Header fields.
 * @param body - Body bytes, or `null`.
 * @returns The request.
 */
function libraryRequest(
  domain: string,
  method: string,
  target: string,
  fields: Array<[string, string]>,
  body: Uint8Array<ArrayBuffer> | null,
): Request {
  const headers = new Headers();
  for (const [name, value] of fields) headers.append(name, value);
  return new Request(`https://${domain}${target}`, {
    method,
    headers,
    ...(body !== null && body.length > 0 && method !== "GET" && method !== "HEAD" ? { body } : {}),
  });
}

/**
 * Concatenates a head and body.
 *
 * @param head - Head bytes.
 * @param body - Body bytes.
 * @returns Both, in one array.
 */
function withBody(head: Uint8Array, body: Uint8Array): Uint8Array {
  const out = new Uint8Array(head.length + body.length);
  out.set(head);
  out.set(body, head.length);
  return out;
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
 * @param domain - The public domain the request arrived on.
 * @param head - The parsed request head.
 * @param body - The request body, or `null`.
 * @param clientAddress - The client's IP.
 * @returns The answer to write.
 */
export async function serveRequest(
  library: Library,
  domain: string,
  head: RequestHead,
  body: Uint8Array<ArrayBuffer> | null,
  clientAddress: string | undefined,
): Promise<Answer> {
  if (head.upgrade) {
    return {
      bytes: errorResponse(501, "WebSocket connections are not supported through ngrok yet."),
      more: null,
    };
  }
  const fields = requestFields(head);
  let response: Response;
  try {
    response = await library.fetch(
      libraryRequest(domain, head.method, head.target, fields, body),
      clientAddress,
    );
  } catch {
    return { bytes: errorResponse(502, "The library could not serve the request."), more: null };
  }
  const status = response.status >= 200 && response.status <= 599 ? response.status : 502;
  const out = responseFields(response.headers);
  if (head.method === "HEAD" || NULL_BODY.has(status) || !response.body) {
    await response.body?.cancel().catch(() => {});
    const length = response.headers.get("content-length");
    const announced =
      head.method === "HEAD" && length !== null && /^\d{1,15}$/.test(length)
        ? Number(length)
        : null;
    const contentLength = NULL_BODY.has(status) ? null : (announced ?? 0);
    return { bytes: encodeResponseHead(status, out, contentLength), more: null };
  }
  const read = await readUpTo(response.body, MAX_BUFFERED_RESPONSE);
  if (read.done) {
    return {
      bytes: withBody(encodeResponseHead(status, out, read.bytes.length), read.bytes),
      more: null,
    };
  }
  const span = continuable(head.method, response);
  if (span === null) {
    return {
      bytes: errorResponse(502, "The response is too large to pass through the ngrok tunnel."),
      more: null,
    };
  }
  const length = span.end - span.first + 1;
  const first = read.bytes.subarray(0, Math.min(read.bytes.length, length));
  const next = span.first + first.length;
  return {
    bytes: withBody(encodeResponseHead(status, out, length), first),
    more:
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
  };
}

/**
 * Fetches the next piece of a large response.
 *
 * @param library - The library.
 * @param domain - The public domain.
 * @param more - What is left.
 * @returns The piece and what is left after it, or `null` when the library
 *   no longer answers with the same resource (the stream must be reset).
 */
export async function continueBody(
  library: Library,
  domain: string,
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
      libraryRequest(domain, "GET", more.target, fields, null),
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
