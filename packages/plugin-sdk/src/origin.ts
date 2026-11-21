/**
 * Author-side `ORIGIN` binding of a relay `ingress` plugin: hands each
 * request the provider delivers to the library and returns its response.
 *
 * The adapter isolate holds the origin-only grant and exposes one
 * {@link OriginTransport} call per request; this module turns it into the
 * {@link OriginBinding} on the author's `env`. On the granted channel a
 * request is one `POST /origin/fetch` whose body is a framed head (a 4-byte
 * big-endian length, then the {@link OriginRequest} as JSON) followed by the
 * request body; the reply is a framed {@link OriginResponseHead} followed by
 * the response body. Both bodies stream: nothing is buffered beyond the
 * head. Transport replies carry failures as values, not throws, so a
 * `PluginError` keeps its code across the Workers RPC hop between isolates.
 */

import { MAX_ORIGIN_HEADER_BYTES, MAX_ORIGIN_TARGET_BYTES } from "./abi.js";
import { PluginError } from "./errors.js";
import type { OriginHeader, OriginRequest } from "./generated.js";

/** Bytes of the length prefix in front of a framed head. */
export const ORIGIN_FRAME_PREFIX_BYTES = 4;

/**
 * Largest framed head the host accepts or sends: the JSON of a head at its
 * limits, with room for escaping. Mirrors `MAX_ORIGIN_FRAME_HEAD_BYTES` in
 * `bookclerk-plugin-abi`.
 */
export const MAX_ORIGIN_FRAME_HEAD_BYTES =
  2 * (MAX_ORIGIN_HEADER_BYTES + MAX_ORIGIN_TARGET_BYTES) + 16 * 1024;

/**
 * Header fields that describe one connection rather than the request; the
 * host refuses them, so the binding drops them before sending.
 */
const HOP_BY_HOP = new Set([
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
 * Header fields the host ignores (the authority is the library's own and
 * the client address travels in `clientAddress`); dropped before sending.
 */
const IGNORED = new Set([
  "host",
  "forwarded",
  "x-forwarded-for",
  "x-forwarded-host",
  "x-forwarded-proto",
  "x-real-ip",
]);

/** The library's response head as the granted channel frames it. */
export interface OriginResponseHead {
  /** HTTP status code, 100 to 599. */
  status: number;
  /** Response header fields in order; repeated names stay separate. */
  headers: OriginHeader[];
}

/** One library response as the transport hands it to the author isolate. */
export interface OriginTransportResponse extends OriginResponseHead {
  /** Response body; `null` when the response has none. */
  body: ReadableStream<Uint8Array> | null;
}

/** One transport answer: the library's response, or a typed failure. */
export type OriginTransportReply =
  { ok: OriginTransportResponse } | { err: { code: string; message: string } };

/**
 * The adapter's origin grant, as the author isolate reaches it. Never
 * throws: every failure comes back as `err`.
 */
export interface OriginTransport {
  /**
   * Hand one request to the library.
   *
   * @param head - Method, origin-form target, header fields and client address.
   * @param body - Request body; `null` when the request has none.
   * @returns The library's response or a typed failure.
   */
  fetch(
    head: OriginRequest,
    body: ReadableStream<Uint8Array> | null,
  ): Promise<OriginTransportReply>;
}

/** Options for one {@link OriginBinding.fetch}. */
export interface OriginFetchOptions {
  /**
   * IP address of the client from the provider's own connection metadata
   * (for example `cf-connecting-ip` on a Cloudflare relay). Omit it when the
   * provider gives none; never take it from a header the client controls.
   */
  clientAddress?: string;
}

/**
 * `env.ORIGIN`: the library as a relay `ingress` plugin reaches it. The host
 * serves each request on public `https` (never as loopback) with the client
 * address the plugin reports.
 */
export interface OriginBinding {
  /**
   * Serves one request through the library.
   *
   * Only the path and query of `request.url` are sent; the library answers
   * on its own public origin. Connection headers (`connection`, `upgrade`,
   * ...) and forwarding headers (`host`, `x-forwarded-*`, `forwarded`,
   * `x-real-ip`) are dropped.
   *
   * @param request - The request the provider delivered.
   * @param options - The client address, when the provider reports one.
   * @returns The library's response; its body streams.
   * @throws {PluginError} `invalid_params` for a request the host refuses
   *   (bad method, target or headers past `maxOriginHeaders` /
   *   `maxOriginHeaderBytes`); `unavailable` when the library cannot serve.
   */
  fetch(request: Request, options?: OriginFetchOptions): Promise<Response>;
}

/**
 * The origin-form target (path and query) of a URL.
 *
 * @param url - Absolute request URL.
 * @returns `pathname` plus `search`.
 */
function originTarget(url: string): string {
  const parsed = new URL(url);
  return `${parsed.pathname}${parsed.search}`;
}

/**
 * The header fields of a request the host accepts, in order.
 *
 * @param headers - Request headers.
 * @returns Fields without connection or forwarding headers.
 */
function requestHeaders(headers: Headers): OriginHeader[] {
  const out: OriginHeader[] = [];
  headers.forEach((value, name) => {
    const lower = name.toLowerCase();
    if (HOP_BY_HOP.has(lower) || IGNORED.has(lower)) return;
    out.push({ name: lower, value });
  });
  return out;
}

/**
 * The author's `ORIGIN` binding over the adapter's grant.
 *
 * @param transport - The adapter's origin grant.
 * @returns The binding installed on `env.ORIGIN`.
 */
export function originBinding(transport: OriginTransport): OriginBinding {
  return Object.freeze({
    async fetch(request: Request, options?: OriginFetchOptions): Promise<Response> {
      const head: OriginRequest = {
        method: request.method,
        target: originTarget(request.url),
        headers: requestHeaders(request.headers),
        ...(options?.clientAddress ? { clientAddress: options.clientAddress } : {}),
      };
      const reply = await transport.fetch(head, request.body);
      if ("err" in reply) throw PluginError.fromWire(reply.err.code, reply.err.message);
      const headers = new Headers();
      for (const field of reply.ok.headers) headers.append(field.name, field.value);
      // A null-body status takes no body even when the stream is empty.
      const nullBody = [101, 204, 205, 304].includes(reply.ok.status);
      return new Response(nullBody ? null : reply.ok.body, { status: reply.ok.status, headers });
    },
  });
}

/**
 * Frames `head` for the granted channel: a 4-byte big-endian length, then
 * the head as JSON.
 *
 * @param head - Request or response head.
 * @returns The frame.
 * @throws {PluginError} `invalid_params` when the head is larger than
 *   {@link MAX_ORIGIN_FRAME_HEAD_BYTES}.
 */
export function encodeOriginFrame(head: OriginRequest | OriginResponseHead): Uint8Array {
  const json = new TextEncoder().encode(JSON.stringify(head));
  if (json.length > MAX_ORIGIN_FRAME_HEAD_BYTES) {
    throw PluginError.fromWire("invalid_params", "origin request head is too large");
  }
  const out = new Uint8Array(ORIGIN_FRAME_PREFIX_BYTES + json.length);
  new DataView(out.buffer).setUint32(0, json.length, false);
  out.set(json, ORIGIN_FRAME_PREFIX_BYTES);
  return out;
}

/**
 * A stream of `frame` followed by `body`.
 *
 * @param frame - Framed head.
 * @param body - Request body, or `null`.
 * @returns The granted request body.
 */
function framedBody(
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

/**
 * Reads a framed response head off `body` and returns it with the rest of
 * the stream as the response body.
 *
 * @param body - The granted reply body.
 * @returns The head and the body that follows it.
 * @throws {PluginError} `internal` when the stream ends inside the head or
 *   the head is oversized or not a response head.
 */
export async function readOriginResponse(
  body: ReadableStream<Uint8Array>,
): Promise<OriginTransportResponse> {
  const reader = body.getReader();
  let buffered = new Uint8Array(0);
  let need = -1;
  for (;;) {
    if (need < 0 && buffered.length >= ORIGIN_FRAME_PREFIX_BYTES) {
      need = new DataView(buffered.buffer, buffered.byteOffset, buffered.length).getUint32(
        0,
        false,
      );
      if (need > MAX_ORIGIN_FRAME_HEAD_BYTES) {
        await reader.cancel();
        throw PluginError.fromWire("internal", "origin response head is too large");
      }
    }
    if (need >= 0 && buffered.length >= ORIGIN_FRAME_PREFIX_BYTES + need) break;
    const { done, value } = await reader.read();
    if (done) throw PluginError.fromWire("internal", "origin response ended inside its head");
    const next = new Uint8Array(buffered.length + value.length);
    next.set(buffered);
    next.set(value, buffered.length);
    buffered = next;
  }
  const end = ORIGIN_FRAME_PREFIX_BYTES + need;
  const head = parseResponseHead(new TextDecoder().decode(buffered.subarray(4, end)));
  const rest = buffered.slice(end);
  const out = new ReadableStream<Uint8Array>({
    start(controller) {
      if (rest.length > 0) controller.enqueue(rest);
    },
    async pull(controller) {
      const { done, value } = await reader.read();
      if (done) controller.close();
      else controller.enqueue(value);
    },
    async cancel(reason) {
      await reader.cancel(reason);
    },
  });
  return { ...head, body: out };
}

/**
 * Parses a response head's JSON.
 *
 * @param text - The head's JSON.
 * @returns The status and header fields.
 * @throws {PluginError} `internal` when it is not a response head.
 */
function parseResponseHead(text: string): OriginResponseHead {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    throw PluginError.fromWire("internal", "origin response head is not JSON");
  }
  const record = (value ?? {}) as { status?: unknown; headers?: unknown };
  const status = record.status;
  if (typeof status !== "number" || !Number.isInteger(status) || status < 100 || status > 599) {
    throw PluginError.fromWire("internal", "origin response head has no valid status");
  }
  const headers: OriginHeader[] = [];
  for (const field of Array.isArray(record.headers) ? record.headers : []) {
    const f = (field ?? {}) as { name?: unknown; value?: unknown };
    if (typeof f.name !== "string" || typeof f.value !== "string") {
      throw PluginError.fromWire("internal", "origin response head has a malformed header");
    }
    headers.push({ name: f.name, value: f.value });
  }
  return { status, headers };
}

/** The granted channel as the adapter reaches it. */
export interface OriginGrantedFetcher {
  /**
   * Call the host grant broker.
   *
   * @param input - Request URL.
   * @param init - Request options.
   * @returns The broker's response.
   */
  fetch(input: string, init?: RequestInit): Promise<Response>;
}

/**
 * Makes one `POST /origin/fetch` call on the granted channel. Never throws.
 *
 * @param granted - Granted reverse channel.
 * @param auth - Bearer header carrying the origin-only grant token.
 * @param head - The request head.
 * @param body - The request body, or `null`.
 * @returns The library's response, or `err` with the host's code
 *   (`unavailable` when the channel cannot be reached, `internal` for a
 *   reply that is not a framed head or a plugin error).
 */
export async function grantedOriginFetch(
  granted: OriginGrantedFetcher,
  auth: Record<string, string>,
  head: OriginRequest,
  body: ReadableStream<Uint8Array> | null,
): Promise<OriginTransportReply> {
  let frame: Uint8Array;
  try {
    frame = encodeOriginFrame(head);
  } catch (err) {
    return { err: errorOf(err) };
  }
  let resp: Response;
  try {
    resp = await granted.fetch("http://granted/origin/fetch", {
      method: "POST",
      headers: { ...auth, "content-type": "application/octet-stream" },
      body: framedBody(frame, body),
      // Node's fetch needs this for a streamed body; workerd ignores it.
      duplex: "half",
    } as RequestInit);
  } catch (err) {
    return { err: { code: "unavailable", message: `origin fetch: ${messageOf(err)}` } };
  }
  if (resp.status !== 200 || !resp.body) {
    const value = (await resp.json().catch(() => null)) as {
      error?: { code?: string; message?: string };
    } | null;
    return {
      err: {
        code: value?.error?.code || "internal",
        message: value?.error?.message || `origin fetch HTTP ${resp.status}`,
      },
    };
  }
  try {
    return { ok: await readOriginResponse(resp.body) };
  } catch (err) {
    return { err: errorOf(err) };
  }
}

/**
 * The wire code and message of a thrown value.
 *
 * @param err - Thrown value.
 * @returns Its `PluginError` code, or `internal`.
 */
function errorOf(err: unknown): { code: string; message: string } {
  if (err instanceof PluginError) return { code: err.code, message: err.message };
  return { code: "internal", message: messageOf(err) };
}

/**
 * The message of a thrown value.
 *
 * @param err - Thrown value.
 * @returns Its message, or its string form.
 */
function messageOf(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}
