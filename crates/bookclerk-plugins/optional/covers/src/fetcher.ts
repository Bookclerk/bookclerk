/**
 * Fetches cover images and streams each body into the host's sink.
 *
 * `fetch` and the sink are passed in so the unit tests can answer them; in
 * the isolate `fetch` is the global one, which the host's egress gate limits
 * to public hostnames, and the sink is the host-served `Destination` the call
 * received. Nothing here trusts a body: the host sniffs, decodes and
 * re-encodes every image it stores. This module only keeps a guest from
 * spending its budget on answers that cannot be covers.
 */

import type { CoverImage } from "@bookclerk/plugin-sdk/workerd";

/** The subset of `fetch` the fetcher calls. */
export type Fetch = (url: string, init: RequestInit) => Promise<Response>;

/** Options of one `put` into the sink. */
export interface PutOptions {
  /** `Content-Type` the server sent. */
  contentType?: string;
  /** Exact body length, sent only when the server declared one. */
  contentLength?: number;
}

/** The subset of the host's `Destination` the fetcher calls. */
export interface CoverSink {
  /**
   * Stores one body under `key`.
   *
   * @param key - The URL's index in the request, as a decimal string.
   * @param body - The body stream.
   * @param options - Content type and declared length.
   * @returns Whatever the sink answers; ignored.
   */
  put(key: string, body: ReadableStream<Uint8Array>, options?: PutOptions): Promise<unknown>;
}

/** How the fetcher reaches the network. */
export interface FetcherOptions {
  /** `fetch` to call. */
  fetch: Fetch;
  /** Plugin version, sent in `User-Agent`. */
  version: string;
}

/** Most URLs one call may ask for (the ABI's `maxCoverUrls`). */
export const MAX_URLS = 32;

/** Largest per-image cap the host may ask for (the ABI's `maxCoverBytes`). */
export const MAX_BYTES = 10 * 1024 * 1024;

/** Longest URL accepted, in bytes. */
export const MAX_URL_BYTES = 4096;

/** Requests in flight at once; keeps one call's buffered chunks small. */
export const CONCURRENCY = 4;

/** Longest one image may take, headers and body, in milliseconds. */
export const REQUEST_TIMEOUT_MS = 15_000;

/** A failure with the `PluginError` code it maps to. */
export class CoverFailure extends Error {
  /** `PluginError` wire code. */
  readonly code: "invalid_params";

  /**
   * Creates a failure.
   *
   * @param message - What was wrong with the request.
   */
  constructor(message: string) {
    super(message);
    this.code = "invalid_params";
  }
}

/** A checked `fetchCovers` request. */
export interface CoverRequest {
  /** URLs to fetch, in request order. */
  urls: string[];
  /** Largest body accepted for one URL, in bytes. */
  maxBytes: number;
}

/**
 * Checks the host's params. The host has checked them already; this keeps a
 * malformed call from reaching the network.
 *
 * @param params - `CoverFetchParams` as received.
 * @returns The request.
 * @throws {CoverFailure} When `urls` is not 1 to `MAX_URLS` strings or
 *   `maxBytes` is not a whole number from 1 to `MAX_BYTES`.
 */
export function checkParams(params: unknown): CoverRequest {
  const p = (params ?? {}) as { urls?: unknown; maxBytes?: unknown };
  const urls = p.urls;
  if (!Array.isArray(urls) || urls.length === 0 || urls.length > MAX_URLS) {
    throw new CoverFailure(`urls must hold 1 to ${MAX_URLS} entries`);
  }
  if (!urls.every((u): u is string => typeof u === "string")) {
    throw new CoverFailure("every url must be a string");
  }
  const maxBytes = p.maxBytes;
  if (
    typeof maxBytes !== "number" ||
    !Number.isInteger(maxBytes) ||
    maxBytes < 1 ||
    maxBytes > MAX_BYTES
  ) {
    throw new CoverFailure(`maxBytes must be a whole number from 1 to ${MAX_BYTES}`);
  }
  return { urls, maxBytes };
}

/**
 * Whether `url` is one the fetcher will request: absolute `https`, the
 * default port, no user name or password, a hostname (no empty labels)
 * rather than an address, and no longer than `MAX_URL_BYTES`. The egress
 * gate enforces the address rules too; checking here gives a clear per-URL
 * error instead. The fetcher applies it to every redirect hop as well.
 *
 * @param url - The requested URL.
 * @returns The URL as the parser serialized it, or `undefined` when refused.
 */
export function checkUrl(url: string): string | undefined {
  if (url.length > MAX_URL_BYTES || /[\u0000- \u007f]/.test(url)) return undefined;
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return undefined;
  }
  if (parsed.protocol !== "https:" || parsed.port !== "" || parsed.username || parsed.password) {
    return undefined;
  }
  const host = parsed.hostname;
  if (host.startsWith("[")) return undefined;
  // One trailing dot is a fully qualified name; any other empty label is no
  // real hostname. A numeric last label is how the URL parser spots IPv4.
  const labels = (host.endsWith(".") ? host.slice(0, -1) : host).split(".");
  const last = labels[labels.length - 1] ?? "";
  if (labels.includes("") || /^(?:\d+|0x[0-9a-f]*)$/i.test(last)) return undefined;
  return parsed.href;
}

/**
 * The body length a response declares, when it is exact.
 *
 * A length only describes the decoded body when the response has no
 * `Content-Encoding`, so an encoded response declares none here.
 *
 * @param headers - Response headers.
 * @returns The length in bytes, or `undefined` when absent, encoded or not a
 *   plain decimal number.
 */
export function declaredLength(headers: Headers): number | undefined {
  if (headers.get("content-encoding")) return undefined;
  const raw = headers.get("content-length")?.trim();
  if (raw === undefined || !/^[0-9]{1,15}$/.test(raw)) return undefined;
  return Number(raw);
}

/**
 * Whether a `Content-Type` could be an image. Absent and generic binary types
 * pass, since the host sniffs the bytes anyway; text, HTML and JSON (an error
 * page, usually) do not.
 *
 * @param contentType - The header value, or `null` when absent.
 * @returns `true` unless the type names something other than an image.
 */
export function maybeImage(contentType: string | null): boolean {
  if (contentType === null) return true;
  const essence = (contentType.split(";")[0] ?? "").trim().toLowerCase();
  return (
    essence === "" ||
    essence.startsWith("image/") ||
    essence === "application/octet-stream" ||
    essence === "binary/octet-stream"
  );
}

/** Records whether a capped stream refused its body. */
export interface CapState {
  /** True once the body passed the cap. */
  exceeded: boolean;
}

/**
 * Passes `body` through, failing the stream once it passes `max` bytes.
 *
 * @param body - The response body.
 * @param max - Most bytes accepted.
 * @param state - Set to exceeded when the cap is passed.
 * @returns The capped stream.
 */
export function capped(
  body: ReadableStream<Uint8Array>,
  max: number,
  state: CapState,
): ReadableStream<Uint8Array> {
  let total = 0;
  return body.pipeThrough(
    new TransformStream<Uint8Array, Uint8Array>({
      transform(chunk, controller) {
        total += chunk.byteLength;
        if (total > max) {
          state.exceeded = true;
          controller.error(new Error(`body exceeds ${max} bytes`));
          return;
        }
        controller.enqueue(chunk);
      },
    }),
  );
}

/**
 * Cancels a body the fetcher will not read, ignoring failures.
 *
 * @param response - The response to drop.
 */
async function drop(response: Response): Promise<void> {
  try {
    await response.body?.cancel();
  } catch {
    // The connection is going away either way.
  }
}

/**
 * The message of an unknown thrown value.
 *
 * @param err - What was thrown.
 * @returns Its message.
 */
function reason(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

/** Most redirects followed for one URL. */
export const MAX_REDIRECTS = 5;

/** Statuses that redirect to their `Location`. */
const REDIRECTS: ReadonlySet<number> = new Set([301, 302, 303, 307, 308]);

/**
 * Fetches one URL and puts its body into the sink under `index`.
 *
 * @param options - How to reach the network.
 * @param url - The requested URL.
 * @param index - Its position in the request.
 * @param maxBytes - Largest body accepted.
 * @param sink - The host's destination.
 * @returns What happened; a failure is an answer, never a throw.
 */
export async function fetchOne(
  options: FetcherOptions,
  url: string,
  index: number,
  maxBytes: number,
  sink: CoverSink,
): Promise<CoverImage> {
  const failed = (error: string): CoverImage => ({ url, stored: false, error });
  const target = checkUrl(url);
  if (target === undefined) return failed("not an https URL on a public hostname");
  let response: Response;
  const signal = AbortSignal.timeout(REQUEST_TIMEOUT_MS);
  let current = target;
  for (let hop = 0; ; hop += 1) {
    try {
      response = await options.fetch(current, {
        method: "GET",
        headers: {
          accept: "image/*",
          "user-agent": `Bookclerk-Covers/${options.version} (+https://bookclerk.app)`,
        },
        // Each hop must pass `checkUrl` too, so redirects are followed here.
        redirect: "manual",
        signal,
      });
    } catch (err) {
      return failed(`request failed: ${reason(err)}`);
    }
    if (!REDIRECTS.has(response.status)) break;
    await drop(response);
    if (hop >= MAX_REDIRECTS) return failed("too many redirects");
    const location = response.headers.get("location");
    if (location === null) return failed(`HTTP ${response.status} without a location`);
    let next: string | undefined;
    try {
      next = checkUrl(new URL(location, current).href);
    } catch {
      next = undefined;
    }
    if (next === undefined)
      return failed("redirect to a URL that is not https on a public hostname");
    current = next;
  }
  if (!response.ok) {
    await drop(response);
    return failed(`HTTP ${response.status}`);
  }
  const contentType = response.headers.get("content-type");
  if (!maybeImage(contentType)) {
    await drop(response);
    return failed(`not an image (${(contentType ?? "").slice(0, 100)})`);
  }
  const length = declaredLength(response.headers);
  if (length !== undefined && length > maxBytes) {
    await drop(response);
    return failed(`body exceeds ${maxBytes} bytes`);
  }
  if (response.body === null) return failed("empty body");
  const state: CapState = { exceeded: false };
  const put: PutOptions = {};
  if (contentType !== null) put.contentType = contentType;
  if (length !== undefined) put.contentLength = length;
  try {
    await sink.put(String(index), capped(response.body, maxBytes, state), put);
  } catch (err) {
    return failed(
      state.exceeded ? `body exceeds ${maxBytes} bytes` : `store failed: ${reason(err)}`,
    );
  }
  const image: CoverImage = { url, stored: true };
  if (contentType !== null) image.contentType = contentType;
  return image;
}

/**
 * Answers `CoverFetcher.fetchCovers`: fetches every URL, `CONCURRENCY` at a
 * time, and returns one answer per URL in request order.
 *
 * @param options - How to reach the network.
 * @param params - `CoverFetchParams` from the host.
 * @param sink - The host's destination.
 * @returns One `CoverImage` per URL.
 * @throws {CoverFailure} When the params are malformed.
 */
export async function fetchCovers(
  options: FetcherOptions,
  params: unknown,
  sink: CoverSink,
): Promise<CoverImage[]> {
  const { urls, maxBytes } = checkParams(params);
  const out: CoverImage[] = new Array<CoverImage>(urls.length);
  let next = 0;
  const worker = async (): Promise<void> => {
    for (let i = next++; i < urls.length; i = next++) {
      out[i] = await fetchOne(options, urls[i] as string, i, maxBytes, sink);
    }
  };
  await Promise.all(Array.from({ length: Math.min(CONCURRENCY, urls.length) }, worker));
  return out;
}
