/**
 * Open Library metadata provider (workerd guest).
 *
 * Exports the `metadata` entrypoint `plugin.toml` declares. The work happens
 * in `client.ts` and `openlibrary.ts`; this module only adapts their typed
 * failures to `PluginError`.
 */
import { BookclerkEntrypoint, MetadataEntrypoint, PluginError, } from "@bookclerk/plugin-sdk/workerd";
import { lookup, search } from "./client.js";
import { ProviderFailure } from "./openlibrary.js";
import { VERSION } from "./version.js";
/**
 * Runs `call`, turning a `ProviderFailure` into a `PluginError` with its code.
 *
 * @param call - The provider work.
 * @returns What `call` returns.
 * @throws {PluginError} When `call` fails.
 */
async function guarded(call) {
    try {
        return await call();
    }
    catch (err) {
        if (err instanceof ProviderFailure)
            throw PluginError.fromWire(err.code, err.message);
        throw err;
    }
}
/** `metadata` entrypoint: lookups and title searches against Open Library. */
export class Metadata extends MetadataEntrypoint {
    /**
     * How this isolate reaches Open Library.
     *
     * @returns Client options from the granted settings.
     */
    options() {
        return {
            fetch: (url, init) => fetch(url, init),
            version: VERSION,
            contactEmail: this.env.CONFIG.contact_email,
        };
    }
    /**
     * Reports ready without a request, so a probe never spends Open Library's
     * rate budget.
     *
     * @returns Always ready.
     */
    health() {
        return Promise.resolve({ ok: true, detail: "" });
    }
    /**
     * Records for an Open Library edition or work id, or an ISBN.
     *
     * @param params - Identifiers, marketplace (ignored) and result cap.
     * @returns Matching records.
     */
    lookup(params) {
        return guarded(() => lookup(this.options(), params));
    }
    /**
     * Records whose title overlaps `params.title`.
     *
     * @param params - Title, author, language and result cap.
     * @returns Matching records.
     */
    search(params) {
        return guarded(() => search(this.options(), params));
    }
}
/** Identity the host merges over `plugin.toml`. */
export default class OpenLibraryPlugin extends BookclerkEntrypoint {
    /**
     * Describes the plugin.
     *
     * @returns Display name.
     */
    async describe() {
        return { displayName: "Open Library" };
    }
}
