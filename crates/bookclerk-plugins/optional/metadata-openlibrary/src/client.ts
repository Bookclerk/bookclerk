/**
 * The one network call the Open Library guest makes.
 *
 * `fetch` is passed in so the unit tests can answer it; in the isolate it is
 * the global `fetch`, which the host's egress gate limits to the hosts
 * `plugin.toml` declares.
 */

import type {
  MetadataLookupParams,
  MetadataRecord,
  MetadataSearchParams,
} from "@bookclerk/plugin-sdk/workerd";
import {
  ProviderFailure,
  planLookup,
  planSearch,
  recordsFromSearch,
  userAgent,
  type SearchRequest,
} from "./openlibrary.ts";

/** The subset of `fetch` the client calls. */
export type Fetch = (url: string, init: RequestInit) => Promise<Response>;

/** Longest a request may take before it is abandoned, in milliseconds. */
export const REQUEST_TIMEOUT_MS = 15_000;

/** Largest response body read, in bytes; a search with 20 hits is far smaller. */
export const MAX_BODY_BYTES = 2 * 1024 * 1024;

/**
 * Reads a response body, stopping as soon as it passes `max` bytes.
 *
 * The body is consumed chunk by chunk and the stream is cancelled once the
 * cap is passed, so a body without `Content-Length` (a chunked response)
 * never buffers more than `max` bytes plus one chunk.
 *
 * @param response - The response to read.
 * @param max - Most bytes accepted.
 * @returns The body, or `undefined` when it is larger than `max`.
 */
export async function readCapped(response: Response, max: number): Promise<Uint8Array | undefined> {
  const reader = response.body?.getReader();
  if (reader === undefined) return new Uint8Array();
  const chunks: Uint8Array[] = [];
  let total = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    if (total > max) {
      await reader.cancel();
      return undefined;
    }
    chunks.push(value);
  }
  const out = new Uint8Array(total);
  let at = 0;
  for (const chunk of chunks) {
    out.set(chunk, at);
    at += chunk.byteLength;
  }
  return out;
}

/** How the client reaches Open Library. */
export interface ClientOptions {
  /** `fetch` to call. */
  fetch: Fetch;
  /** Plugin version, sent in `User-Agent`. */
  version: string;
  /** The `contact_email` setting, sent in `User-Agent` when plausible. */
  contactEmail: unknown;
}

/**
 * Reads a response body as JSON, refusing one larger than `MAX_BODY_BYTES`.
 *
 * @param response - The response to read.
 * @returns The parsed body.
 * @throws {ProviderFailure} `unavailable` when the body is too large or not JSON.
 */
async function readJson(response: Response): Promise<unknown> {
  const declared = Number(response.headers.get("content-length") ?? "0");
  if (declared > MAX_BODY_BYTES) {
    throw new ProviderFailure("unavailable", "Open Library sent a response that is too large");
  }
  let bytes: Uint8Array | undefined;
  try {
    bytes = await readCapped(response, MAX_BODY_BYTES);
  } catch (err) {
    // The body streams after the headers, so a dropped connection or the
    // request timeout can still fail it here.
    const reason = err instanceof Error ? err.message : String(err);
    throw new ProviderFailure("unavailable", `Open Library response failed: ${reason}`);
  }
  if (bytes === undefined) {
    throw new ProviderFailure("unavailable", "Open Library sent a response that is too large");
  }
  try {
    return JSON.parse(new TextDecoder().decode(bytes)) as unknown;
  } catch {
    throw new ProviderFailure("unavailable", "Open Library sent a response that is not JSON");
  }
}

/**
 * Sends one planned search and maps its answer.
 *
 * @param options - How to reach Open Library.
 * @param request - The planned request.
 * @returns The records; empty when Open Library answers 404.
 * @throws {ProviderFailure} `unavailable` when the request fails, times out
 *   or gets an error status.
 */
export async function runSearch(
  options: ClientOptions,
  request: SearchRequest,
): Promise<MetadataRecord[]> {
  let response: Response;
  try {
    response = await options.fetch(request.url, {
      method: "GET",
      headers: {
        accept: "application/json",
        "user-agent": userAgent(options.version, options.contactEmail),
      },
      signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    });
  } catch (err) {
    const reason = err instanceof Error ? err.message : String(err);
    throw new ProviderFailure("unavailable", `Open Library request failed: ${reason}`);
  }
  if (response.status === 404) return [];
  if (!response.ok) {
    throw new ProviderFailure("unavailable", `Open Library answered HTTP ${response.status}`);
  }
  return recordsFromSearch(await readJson(response), request);
}

/**
 * Answers `MetadataProvider.lookup`.
 *
 * @param options - How to reach Open Library.
 * @param params - Lookup params from the host.
 * @returns Records for the identified edition or work; empty when no
 *   identifier is one Open Library serves.
 * @throws {ProviderFailure} When the params are invalid or the request fails.
 */
export async function lookup(
  options: ClientOptions,
  params: MetadataLookupParams,
): Promise<MetadataRecord[]> {
  const request = planLookup(params);
  return request === undefined ? [] : runSearch(options, request);
}

/**
 * Answers `MetadataProvider.search`.
 *
 * @param options - How to reach Open Library.
 * @param params - Search params from the host.
 * @returns Records whose title overlaps the asked title.
 * @throws {ProviderFailure} When the params are invalid or the request fails.
 */
export async function search(
  options: ClientOptions,
  params: MetadataSearchParams,
): Promise<MetadataRecord[]> {
  return runSearch(options, planSearch(params));
}
