/**
 * The dialects this plugin serves, by the ids `plugin.toml` declares.
 *
 * A unit test keeps this table and the manifest's
 * `[[metadata_files.dialects]]` rows in step: the same ids, and `render`
 * exactly for the rows with `writes = true`.
 */
import { parseAbsJson, renderAbsJson } from "./abs-json.js";
import { parseLibationJson } from "./libation.js";
import { parseNfo, renderNfo } from "./nfo.js";
import { parseDescTxt, parseReaderTxt, renderDescTxt, renderReaderTxt } from "./plain-text.js";
import { MAX_FILE_BYTES, invalid } from "./text.js";
/** Every dialect, by id. */
export const DIALECTS = new Map([
    ["abs_metadata_json", { parse: parseAbsJson, render: renderAbsJson }],
    ["libation_json", { parse: parseLibationJson }],
    ["abs_nfo", { parse: parseNfo, render: renderNfo }],
    ["desc_txt", { parse: parseDescTxt, render: renderDescTxt }],
    ["reader_txt", { parse: parseReaderTxt, render: renderReaderTxt }],
]);
/**
 * Finds a dialect.
 *
 * @param id - Dialect id from the request.
 * @returns The dialect.
 * @throws {DialectFailure} For an id this plugin does not serve.
 */
export function dialect(id) {
    const found = DIALECTS.get(id);
    if (found === undefined)
        throw invalid(`unknown dialect \`${id}\``);
    return found;
}
/**
 * Parses one file with the dialect the host chose.
 *
 * @param id - Dialect id.
 * @param bytes - The whole file.
 * @returns What the file states.
 * @throws {DialectFailure} For an unknown dialect, a file over the cap or a
 *   file the dialect cannot read.
 */
export function parseFile(id, bytes) {
    const found = dialect(id);
    if (bytes.length > MAX_FILE_BYTES)
        throw invalid(`file is over the ${MAX_FILE_BYTES} byte cap`);
    return found.parse(bytes);
}
/**
 * Renders one file with the dialect the host chose.
 *
 * @param id - Dialect id.
 * @param existing - The file as it is now; empty to create it.
 * @param record - Values to write.
 * @param fields - Fields to set.
 * @returns The new file.
 * @throws {DialectFailure} For an unknown or read-only dialect, an existing
 *   file over the cap or unreadable, or a result over the cap.
 */
export function renderFile(id, existing, record, fields) {
    const found = dialect(id);
    if (found.render === undefined)
        throw invalid(`dialect \`${id}\` is read-only`);
    if (existing.length > MAX_FILE_BYTES) {
        throw invalid(`existing file is over the ${MAX_FILE_BYTES} byte cap`);
    }
    return found.render(existing, record, fields);
}
