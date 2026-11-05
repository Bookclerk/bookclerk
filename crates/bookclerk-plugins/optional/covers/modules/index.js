/**
 * Cover image fetcher (workerd guest).
 *
 * Exports the `covers` entrypoint `plugin.toml` declares. The work happens in
 * `fetcher.ts`; this module only adapts a malformed request to `PluginError`.
 */
import { BookclerkEntrypoint, CoverEntrypoint, PluginError, } from "@bookclerk/plugin-sdk/workerd";
import { CoverFailure, fetchCovers } from "./fetcher.js";
import { VERSION } from "./version.js";
/** `covers` entrypoint: downloads cover images into the host's sink. */
export class Covers extends CoverEntrypoint {
    /**
     * Reports ready without a request.
     *
     * @returns Always ready.
     */
    health() {
        return Promise.resolve({ ok: true, detail: "" });
    }
    /**
     * Fetches each URL and streams its body into `sink` under its index.
     *
     * @param params - URLs and the per-image byte cap.
     * @param sink - The host's destination for the bodies.
     * @returns One answer per URL, in request order.
     * @throws {PluginError} `invalid_params` when the request is malformed.
     */
    async fetchCovers(params, sink) {
        try {
            return await fetchCovers({ fetch: (url, init) => fetch(url, init), version: VERSION }, params, sink);
        }
        catch (err) {
            if (err instanceof CoverFailure)
                throw PluginError.fromWire(err.code, err.message);
            throw err;
        }
    }
}
/** Identity the host merges over `plugin.toml`. */
export default class CoversPlugin extends BookclerkEntrypoint {
    /**
     * Describes the plugin.
     *
     * @returns Display name.
     */
    async describe() {
        return { displayName: "Cover images" };
    }
}
