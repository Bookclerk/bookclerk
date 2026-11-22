/**
 * The relay protocol between the Bookclerk relay Worker and this plugin.
 *
 * Both ends load this module: the relay Worker runs it in the operator's
 * Cloudflare account, and the plugin runs it in its workerd isolate. It
 * imports nothing, so the plugin can upload the same file as part of the
 * relay Worker.
 *
 * The relay holds each public request until the plugin asks for it:
 *
 * 1. The plugin keeps up to {@link RELAY_SOCKETS} WebSockets open to
 *    {@link RELAY_CONNECT_PATH}. When a public request arrives, the relay
 *    picks an idle socket and sends one text notice on it,
 *    `{"t":"request","id":"<id>"}` ({@link encodeNotice}).
 * 2. The plugin fetches {@link RELAY_PULL_PATH}`<id>`. The answer is a frame:
 *    a 4-byte big-endian length, the request head as JSON
 *    ({@link RelayRequestHead}), then the request body.
 * 3. The plugin serves the request through the library and posts the answer
 *    to {@link RELAY_PUSH_PATH}`<id>` in the same framing: the response head
 *    ({@link RelayResponseHead}), then the response body.
 *
 * Every relay path needs `Authorization: Bearer <token>`, the token the
 * plugin derived and deployed with the Worker. Each end parses what the
 * other sends with the functions here, sans IO, so the fuzz tests drive the
 * same code both ends run.
 */

/** Path prefix the relay keeps for itself; public requests never reach it. */
export const RELAY_PREFIX = "/__bookclerk/relay/";

/** WebSocket path the plugin connects its sockets to. */
export const RELAY_CONNECT_PATH = `${RELAY_PREFIX}connect`;

/** Path that reports how many plugin sockets the relay holds. */
export const RELAY_HEALTH_PATH = `${RELAY_PREFIX}health`;

/** Path prefix the plugin pulls a request from, followed by its id. */
export const RELAY_PULL_PATH = `${RELAY_PREFIX}pull/`;

/** Path prefix the plugin pushes a response to, followed by its id. */
export const RELAY_PUSH_PATH = `${RELAY_PREFIX}push/`;

/**
 * Sockets the plugin keeps open, and so the requests it serves at once. The
 * host lets an instance hold 8 sockets; two stay free for reconnecting.
 */
export const RELAY_SOCKETS = 6;

/** Bytes of the big-endian length that opens a frame. */
export const FRAME_PREFIX_BYTES = 4;

/** Most header fields a head may carry (the host's `maxOriginHeaders`). */
export const MAX_HEADERS = 128;

/** Most bytes of header names and values together (`maxOriginHeaderBytes`). */
export const MAX_HEADER_BYTES = 65536;

/** Longest request target (`maxOriginTargetBytes`). */
export const MAX_TARGET_BYTES = 8192;

/** Longest method token the host accepts. */
export const MAX_METHOD_BYTES = 32;

/**
 * Largest head JSON a frame may announce: the header and target limits with
 * room for JSON escaping.
 */
export const MAX_HEAD_BYTES = 2 * (MAX_HEADER_BYTES + MAX_TARGET_BYTES) + 16 * 1024;

/** Longest request id the relay mints (a UUID is 36 characters). */
export const MAX_ID_LENGTH = 64;

/** One header field, name lowercase. */
export interface RelayHeader {
  /** Field name, an HTTP token in lowercase. */
  name: string;
  /** Field value: visible ASCII, spaces and tabs. */
  value: string;
}

/** A public request as the relay hands it to the plugin. */
export interface RelayRequestHead {
  /** HTTP method token. */
  method: string;
  /** Path and query, starting with one `/`. */
  target: string;
  /** Header fields in order, without connection or forwarding fields. */
  headers: RelayHeader[];
  /** Client IP address from the provider's connection, when it gave one. */
  client?: string;
}

/** The library's answer as the plugin hands it back to the relay. */
export interface RelayResponseHead {
  /** HTTP status code, 200 to 599 (no informational answers). */
  status: number;
  /** Header fields in order, without connection fields. */
  headers: RelayHeader[];
}

/** What a frame reader needs next, or what it read. */
export type FrameStep =
  | {
      /** More bytes are needed before the head is complete. */
      kind: "need";
    }
  | {
      /** The head is complete. */
      kind: "head";
      /** The head's JSON text. */
      json: string;
      /** Bytes after the head: the start of the body. */
      rest: Uint8Array;
    }
  | {
      /** The frame is malformed; the reader stays failed. */
      kind: "error";
      /** Why, for a log line (never the frame's bytes). */
      reason: string;
    };

/**
 * Reads a frame's head off a byte stream fed in chunks of any size.
 *
 * Feed each chunk to {@link FrameReader.push} until it returns `head` or
 * `error`; the reader keeps nothing after that.
 */
export class FrameReader {
  /** Bytes read so far, before the head is complete. */
  #buffered: Uint8Array = new Uint8Array(0);
  /** Set once a step returned `head` or `error`. */
  #finished = false;

  /**
   * Adds bytes and reports whether the head is complete.
   *
   * @param chunk - The next bytes of the stream.
   * @returns `need`, `head` with the JSON and the bytes after it, or `error`.
   */
  push(chunk: Uint8Array): FrameStep {
    if (this.#finished) return { kind: "error", reason: "frame reader already finished" };
    const joined = new Uint8Array(this.#buffered.length + chunk.length);
    joined.set(this.#buffered);
    joined.set(chunk, this.#buffered.length);
    this.#buffered = joined;
    if (joined.length < FRAME_PREFIX_BYTES) return { kind: "need" };
    const length = new DataView(joined.buffer, joined.byteOffset, joined.length).getUint32(
      0,
      false,
    );
    if (length > MAX_HEAD_BYTES) return this.#fail("frame head is too large");
    if (joined.length < FRAME_PREFIX_BYTES + length) return { kind: "need" };
    const end = FRAME_PREFIX_BYTES + length;
    let json: string;
    try {
      // `ignoreBOM` keeps a leading byte-order mark, which JSON then refuses.
      json = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(
        joined.subarray(FRAME_PREFIX_BYTES, end),
      );
    } catch {
      return this.#fail("frame head is not UTF-8");
    }
    const rest = joined.slice(end);
    this.#finished = true;
    this.#buffered = new Uint8Array(0);
    return { kind: "head", json, rest };
  }

  /**
   * Reports the end of the stream.
   *
   * @returns `error` when the stream ended inside the head; `need` never.
   */
  end(): FrameStep {
    if (this.#finished) return { kind: "error", reason: "frame reader already finished" };
    return this.#fail("stream ended inside the frame head");
  }

  /**
   * Fails the reader.
   *
   * @param reason - Why.
   * @returns The error step.
   */
  #fail(reason: string): FrameStep {
    this.#finished = true;
    this.#buffered = new Uint8Array(0);
    return { kind: "error", reason };
  }
}

/**
 * Frames a head: its JSON's length as 4 big-endian bytes, then the JSON.
 *
 * @param head - The head to send.
 * @returns The frame's opening bytes.
 * @throws {RangeError} When the JSON is longer than {@link MAX_HEAD_BYTES}.
 */
export function encodeFrame(head: RelayRequestHead | RelayResponseHead): Uint8Array<ArrayBuffer> {
  const json = new TextEncoder().encode(JSON.stringify(head));
  if (json.length > MAX_HEAD_BYTES) throw new RangeError("relay frame head is too large");
  const out = new Uint8Array(FRAME_PREFIX_BYTES + json.length);
  new DataView(out.buffer).setUint32(0, json.length, false);
  out.set(json, FRAME_PREFIX_BYTES);
  return out;
}

/** Characters RFC 9110 allows in a token (`tchar`). */
const TOKEN = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;

/** A field value: visible ASCII, spaces and tabs (no obs-text). */
const FIELD_VALUE = /^[\t\x20-\x7e]*$/;

/** A request target: one `/`, then visible ASCII without `#`. */
const TARGET = /^\/[\x21-\x22\x24-\x7e]*$/;

/**
 * Header fields that describe one connection, never passed on.
 */
export const HOP_BY_HOP: ReadonlySet<string> = new Set([
  "connection",
  "keep-alive",
  "proxy-authenticate",
  "proxy-authorization",
  "proxy-connection",
  "te",
  "trailer",
  "transfer-encoding",
  "upgrade",
]);

/**
 * Whether a header name is one the relay drops from a public request: the
 * provider's own fields (`cf-*`, `cdn-loop`), forwarding fields, and the
 * authority, which the library sets itself.
 *
 * @param name - Lowercase field name.
 * @returns `true` when the relay must not pass it to the library.
 */
export function providerHeader(name: string): boolean {
  return (
    name.startsWith("cf-") ||
    name.startsWith("x-forwarded-") ||
    name === "cdn-loop" ||
    name === "forwarded" ||
    name === "host" ||
    name === "x-real-ip"
  );
}

/**
 * Checks header fields against the shared limits.
 *
 * @param value - The parsed `headers` member.
 * @returns The fields, or why they are refused.
 */
function parseHeaders(value: unknown): RelayHeader[] | string {
  if (!Array.isArray(value)) return "headers is not a list";
  if (value.length > MAX_HEADERS) return "too many header fields";
  const out: RelayHeader[] = [];
  let bytes = 0;
  for (const field of value) {
    if (typeof field !== "object" || field === null) return "malformed header field";
    const { name, value: text } = field as { name?: unknown; value?: unknown };
    if (typeof name !== "string" || typeof text !== "string") return "malformed header field";
    if (!TOKEN.test(name) || name !== name.toLowerCase())
      return "header name is not a lowercase token";
    if (!FIELD_VALUE.test(text)) return "header value holds a control or non-ASCII character";
    if (HOP_BY_HOP.has(name)) return "header field describes the connection";
    bytes += name.length + text.length;
    if (bytes > MAX_HEADER_BYTES) return "header fields are too large";
    out.push({ name, value: text });
  }
  return out;
}

/**
 * Parses the JSON object of a head.
 *
 * @param json - The head's JSON text.
 * @returns The object, or why it is refused.
 */
function parseObject(json: string): Record<string, unknown> | string {
  let value: unknown;
  try {
    value = JSON.parse(json);
  } catch {
    return "head is not JSON";
  }
  if (typeof value !== "object" || value === null || Array.isArray(value))
    return "head is not an object";
  return value as Record<string, unknown>;
}

/**
 * Whether `text` is an IPv4 address in dotted-decimal form.
 *
 * @param text - Candidate address.
 * @returns `true` for four decimal octets, each 0 to 255 without leading zeros.
 */
function isIpv4(text: string): boolean {
  const parts = text.split(".");
  return (
    parts.length === 4 &&
    parts.every((part) => /^(0|[1-9][0-9]{0,2})$/.test(part) && Number(part) <= 255)
  );
}

/**
 * Whether `text` is an IPv6 address (RFC 4291 text forms, no zone).
 *
 * @param text - Candidate address.
 * @returns `true` for a valid IPv6 address.
 */
function isIpv6(text: string): boolean {
  if (text.length > 45 || !/^[0-9A-Fa-f:.]+$/.test(text)) return false;
  let body = text;
  const lastColon = body.lastIndexOf(":");
  if (body.includes(".")) {
    if (!isIpv4(body.slice(lastColon + 1))) return false;
    body = `${body.slice(0, lastColon + 1)}0:0`;
  }
  const halves = body.split("::");
  if (halves.length > 2) return false;
  const groups = (half: string): string[] | null => {
    if (half === "") return [];
    const list = half.split(":");
    return list.every((g) => /^[0-9A-Fa-f]{1,4}$/.test(g)) ? list : null;
  };
  const head = groups(halves[0] ?? "");
  if (head === null) return false;
  if (halves.length === 1) return head.length === 8;
  const tail = groups(halves[1] ?? "");
  if (tail === null) return false;
  return head.length + tail.length < 8;
}

/**
 * Whether `text` is an IP address the relay may report as the client.
 *
 * @param text - Candidate address.
 * @returns `true` for an IPv4 or IPv6 address.
 */
export function isIpAddress(text: string): boolean {
  return isIpv4(text) || isIpv6(text);
}

/**
 * Parses a request head the relay sent.
 *
 * @param json - The head's JSON text, as {@link FrameReader} returned it.
 * @returns The head, or why it is refused.
 */
export function parseRequestHead(json: string): RelayRequestHead | string {
  const record = parseObject(json);
  if (typeof record === "string") return record;
  const { method, target, headers, client } = record;
  if (typeof method !== "string" || method.length > MAX_METHOD_BYTES || !TOKEN.test(method)) {
    return "method is not a token";
  }
  if (
    typeof target !== "string" ||
    target.length > MAX_TARGET_BYTES ||
    !TARGET.test(target) ||
    target.startsWith("//")
  ) {
    return "target is not a path and query";
  }
  const fields = parseHeaders(headers);
  if (typeof fields === "string") return fields;
  for (const field of fields) {
    if (providerHeader(field.name)) return "head carries a provider or forwarding header";
  }
  const head: RelayRequestHead = { method, target, headers: fields };
  if (client !== undefined) {
    if (typeof client !== "string" || !isIpAddress(client)) return "client is not an IP address";
    head.client = client;
  }
  return head;
}

/**
 * Parses a response head the plugin sent.
 *
 * @param json - The head's JSON text, as {@link FrameReader} returned it.
 * @returns The head, or why it is refused.
 */
export function parseResponseHead(json: string): RelayResponseHead | string {
  const record = parseObject(json);
  if (typeof record === "string") return record;
  const { status, headers } = record;
  if (typeof status !== "number" || !Number.isInteger(status) || status < 200 || status > 599) {
    return "status is not 200 to 599";
  }
  const fields = parseHeaders(headers);
  if (typeof fields === "string") return fields;
  return { status, headers: fields };
}

/** A notice the relay sends on a plugin socket. */
export interface RelayNotice {
  /** What the notice announces: a request waiting to be pulled. */
  t: "request";
  /** The request's id, for the pull and push paths. */
  id: string;
}

/** An id: letters, digits and dashes. */
const ID = /^[0-9A-Za-z-]+$/;

/**
 * Whether `id` is a request id the paths can carry.
 *
 * @param id - Candidate id.
 * @returns `true` for 1 to {@link MAX_ID_LENGTH} letters, digits and dashes.
 */
export function isRequestId(id: string): boolean {
  return id.length > 0 && id.length <= MAX_ID_LENGTH && ID.test(id);
}

/**
 * Encodes a notice.
 *
 * @param notice - The notice.
 * @returns Its text frame.
 */
export function encodeNotice(notice: RelayNotice): string {
  return JSON.stringify(notice);
}

/**
 * Parses a notice the relay sent.
 *
 * @param text - The socket message's text.
 * @returns The notice, or why it is refused.
 */
export function parseNotice(text: string): RelayNotice | string {
  if (text.length > 256) return "notice is too long";
  const record = parseObject(text);
  if (typeof record === "string") return record;
  if (record["t"] !== "request") return "notice is not a request notice";
  const id = record["id"];
  if (typeof id !== "string" || !isRequestId(id)) return "notice has no valid id";
  return { t: "request", id };
}

/**
 * Compares two strings in time that depends only on their lengths.
 *
 * @param a - First string.
 * @param b - Second string.
 * @returns `true` when they are equal.
 */
export function constantTimeEqual(a: string, b: string): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return diff === 0;
}

/**
 * Reads the bearer token of an `Authorization` value.
 *
 * @param header - The header value, or `null`.
 * @returns The token, or `null` when the value is not `Bearer <token>`.
 */
export function bearerToken(header: string | null): string | null {
  if (header === null) return null;
  const match = /^Bearer ([A-Za-z0-9_-]{16,128})$/.exec(header);
  return match?.[1] ?? null;
}
