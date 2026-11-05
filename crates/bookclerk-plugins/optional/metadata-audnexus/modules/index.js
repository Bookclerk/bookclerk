/**
 * Audnexus metadata and chapter provider (workerd guest).
 *
 * Exports the `metadata` entrypoint `plugin.toml` declares. The work happens
 * in `client.ts` and `audnexus.ts`; this module only adapts their typed
 * failures to `PluginError`. Audnexus has no title search, so `search` keeps
 * the SDK's `unsupported` answer.
 */
import { BookclerkEntrypoint, MetadataEntrypoint, PluginError, } from "@bookclerk/plugin-sdk/workerd";
import { ProviderFailure } from "./audnexus.js";
import { chapters, lookup } from "./client.js";
import { VERSION } from "./version.js";
/**
 * Runs `call`, turning a `ProviderFailure` into a `PluginError` with its code.
 *
 * @param call - The provider work.
 * @returns What `call` returns.
 * @throws {PluginError} When `call` fails.
 */
export async function guarded(call) {
    try {
        return await call();
    }
    catch (err) {
        if (err instanceof ProviderFailure)
            throw PluginError.fromWire(err.code, err.message);
        throw err;
    }
}
/** How the isolate reaches Audnexus. */
const OPTIONS = { fetch: (url, init) => fetch(url, init), version: VERSION };
/** `metadata` entrypoint: book records and chapter layouts by ASIN. */
export class Metadata extends MetadataEntrypoint {
    /**
     * Reports ready without a request, so a probe never spends Audnexus's
     * capacity.
     *
     * @returns Always ready.
     */
    health() {
        return Promise.resolve({ ok: true, detail: "" });
    }
    /**
     * Records for the ASINs among `params.identifiers`.
     *
     * @param params - Identifiers, marketplace and result cap.
     * @returns One record per known ASIN.
     */
    lookup(params) {
        return guarded(() => lookup(OPTIONS, params));
    }
    /**
     * Chapter layout of the first ASIN among `params.identifiers`.
     *
     * @param params - Identifiers and marketplace.
     * @returns The layout, or `{ found: false }`.
     */
    chapters(params) {
        return guarded(() => chapters(OPTIONS, params));
    }
}
/** Identity the host merges over `plugin.toml`. */
export default class AudnexusPlugin extends BookclerkEntrypoint {
    /**
     * Describes the plugin.
     *
     * @returns Display name.
     */
    async describe() {
        return { displayName: "Audnexus" };
    }
}
