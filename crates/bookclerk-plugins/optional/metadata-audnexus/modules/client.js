/**
 * The network calls the Audnexus guest makes.
 *
 * `fetch` is passed in so the unit tests can answer it; in the isolate it is
 * the global `fetch`, which the host's egress gate limits to the hosts
 * `plugin.toml` declares.
 */
import { ProviderFailure, asinRequests, bookUrl, chaptersFromBody, chaptersUrl, clampLimit, notFound, recordFromBook, } from "./audnexus.js";
/** Longest a request may take before it is abandoned, in milliseconds. */
export const REQUEST_TIMEOUT_MS = 15_000;
/** Largest response body read, in bytes; a long layout is far smaller. */
export const MAX_BODY_BYTES = 4 * 1024 * 1024;
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
export async function readCapped(response, max) {
    const reader = response.body?.getReader();
    if (reader === undefined)
        return new Uint8Array();
    const chunks = [];
    let total = 0;
    for (;;) {
        const { done, value } = await reader.read();
        if (done)
            break;
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
/**
 * GETs `url` and parses its JSON body.
 *
 * @param options - How to reach Audnexus.
 * @param url - URL to read.
 * @returns The body, or `undefined` when Audnexus answers 404.
 * @throws {ProviderFailure} `unavailable` when the request fails, times out,
 *   gets an error status, or the body is too large or not JSON.
 */
export async function getJson(options, url) {
    let response;
    try {
        response = await options.fetch(url, {
            method: "GET",
            headers: {
                accept: "application/json",
                "user-agent": `Bookclerk-Audnexus/${options.version} (https://bookclerk.app)`,
            },
            signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
        });
    }
    catch (err) {
        const reason = err instanceof Error ? err.message : String(err);
        throw new ProviderFailure("unavailable", `Audnexus request failed: ${reason}`);
    }
    if (response.status === 404)
        return undefined;
    if (!response.ok) {
        throw new ProviderFailure("unavailable", `Audnexus answered HTTP ${response.status}`);
    }
    const declared = Number(response.headers.get("content-length") ?? "0");
    if (declared > MAX_BODY_BYTES) {
        throw new ProviderFailure("unavailable", "Audnexus sent a response that is too large");
    }
    const bytes = await readCapped(response, MAX_BODY_BYTES);
    if (bytes === undefined) {
        throw new ProviderFailure("unavailable", "Audnexus sent a response that is too large");
    }
    try {
        return JSON.parse(new TextDecoder().decode(bytes));
    }
    catch {
        throw new ProviderFailure("unavailable", "Audnexus sent a response that is not JSON");
    }
}
/**
 * Answers `MetadataProvider.lookup`: one request per ASIN, up to the limit.
 *
 * @param options - How to reach Audnexus.
 * @param params - Lookup params from the host.
 * @returns A record for each ASIN Audnexus knows, in the caller's order.
 * @throws {ProviderFailure} When the params are invalid or a request fails.
 */
export async function lookup(options, params) {
    const records = [];
    for (const request of asinRequests(params.identifiers, params.marketplace, clampLimit(params.limit))) {
        const record = recordFromBook(await getJson(options, bookUrl(request)), request);
        if (record !== undefined)
            records.push(record);
    }
    return records;
}
/**
 * Answers `MetadataProvider.chapters` for the first ASIN given.
 *
 * @param options - How to reach Audnexus.
 * @param params - Chapters params from the host.
 * @returns The layout, or a not-found layout when there is no ASIN or
 *   Audnexus has none.
 * @throws {ProviderFailure} When the params are invalid or the request fails.
 */
export async function chapters(options, params) {
    const [request] = asinRequests(params.identifiers, params.marketplace, 1);
    if (request === undefined)
        return notFound();
    const body = await getJson(options, chaptersUrl(request));
    return body === undefined ? notFound() : chaptersFromBody(body);
}
