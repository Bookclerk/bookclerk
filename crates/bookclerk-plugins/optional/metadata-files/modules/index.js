/**
 * Metadata file dialects (workerd guest).
 *
 * Exports the `metadataFiles` entrypoint `plugin.toml` declares. The
 * formats live in their own modules behind `dialects.ts`; this module only
 * turns a dialect's refusal into a `PluginError`. The plugin has no network
 * or file grants: the host hands every file in and takes every file back.
 */
import { BookclerkEntrypoint, MetadataFilesEntrypoint, PluginError, } from "@bookclerk/plugin-sdk/workerd";
import { parseFile, renderFile } from "./dialects.js";
import { DialectFailure } from "./text.js";
/**
 * Runs a dialect call, turning its refusal into a `PluginError`.
 *
 * @param work - The call.
 * @returns What the call returned.
 * @throws {PluginError} With the refusal's code.
 */
function guarded(work) {
    try {
        return Promise.resolve(work());
    }
    catch (err) {
        if (err instanceof DialectFailure) {
            return Promise.reject(PluginError.fromWire(err.code, err.message));
        }
        return Promise.reject(err);
    }
}
/** `metadataFiles` entrypoint: reads and writes sidecar metadata files. */
export class MetadataFiles extends MetadataFilesEntrypoint {
    /**
     * Reports ready; the dialects need nothing outside the request.
     *
     * @returns Always ready.
     */
    health() {
        return Promise.resolve({ ok: true, detail: "" });
    }
    /**
     * Reads one file.
     *
     * @param params - Dialect id, file name and the whole file.
     * @returns What the file states.
     * @throws {PluginError} `invalid_params` for an unknown dialect or a file
     *   the dialect cannot read.
     */
    parse(params) {
        return guarded(() => parseFile(params.dialect, params.bytes));
    }
    /**
     * Writes one file.
     *
     * @param params - Dialect id, values, fields to set and the current file.
     * @returns The new file's bytes.
     * @throws {PluginError} `invalid_params` for an unknown or read-only
     *   dialect, an existing file the dialect cannot read, or a result over
     *   the size cap.
     */
    render(params) {
        return guarded(() => renderFile(params.dialect, params.existing, params.record, params.fields, params.audioFiles));
    }
}
/** Identity the host merges over `plugin.toml`. */
export default class MetadataFilesPlugin extends BookclerkEntrypoint {
    /**
     * Describes the plugin.
     *
     * @returns Display name.
     */
    async describe() {
        return { displayName: "Metadata files" };
    }
}
