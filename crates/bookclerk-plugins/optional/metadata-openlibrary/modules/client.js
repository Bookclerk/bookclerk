/**
 * The one network call the Open Library guest makes.
 *
 * `fetch` is passed in so the unit tests can answer it; in the isolate it is
 * the global `fetch`, which the host's egress gate limits to the hosts
 * `plugin.toml` declares.
 */
import { ProviderFailure, planLookup, planSearch, recordsFromSearch, userAgent, } from "./openlibrary.js";
/** Longest a request may take before it is abandoned, in milliseconds. */
export const REQUEST_TIMEOUT_MS = 15_000;
/** Largest response body read, in bytes; a search with 20 hits is far smaller. */
export const MAX_BODY_BYTES = 2 * 1024 * 1024;
/**
 * Reads a response body as JSON, refusing one larger than `MAX_BODY_BYTES`.
 *
 * @param response - The response to read.
 * @returns The parsed body.
 * @throws {ProviderFailure} `unavailable` when the body is too large or not JSON.
 */
async function readJson(response) {
    const declared = Number(response.headers.get("content-length") ?? "0");
    if (declared > MAX_BODY_BYTES) {
        throw new ProviderFailure("unavailable", "Open Library sent a response that is too large");
    }
    const bytes = new Uint8Array(await response.arrayBuffer());
    if (bytes.byteLength > MAX_BODY_BYTES) {
        throw new ProviderFailure("unavailable", "Open Library sent a response that is too large");
    }
    try {
        return JSON.parse(new TextDecoder().decode(bytes));
    }
    catch {
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
export async function runSearch(options, request) {
    let response;
    try {
        response = await options.fetch(request.url, {
            method: "GET",
            headers: {
                accept: "application/json",
                "user-agent": userAgent(options.version, options.contactEmail),
            },
            signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
        });
    }
    catch (err) {
        const reason = err instanceof Error ? err.message : String(err);
        throw new ProviderFailure("unavailable", `Open Library request failed: ${reason}`);
    }
    if (response.status === 404)
        return [];
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
export async function lookup(options, params) {
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
export async function search(options, params) {
    return runSearch(options, planSearch(params));
}
