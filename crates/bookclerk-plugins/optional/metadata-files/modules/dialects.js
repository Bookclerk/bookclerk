/**
 * The dialects this plugin serves, by the ids `plugin.toml` declares.
 *
 * A unit test keeps this table and the manifest's
 * `[[metadata_files.dialects]]` rows in step: the same ids, and `render`
 * exactly for the rows with `writes = true`.
 */
import { parseAlbumNfo, renderAlbumNfo } from "./album-nfo.js";
import { parseCue, renderCue } from "./cue.js";
import { parseLibationJson } from "./libation.js";
import { parseOpf, renderOpf } from "./opf.js";
import { MAX_FILE_BYTES, invalid } from "./text.js";
/** Every dialect, by id. */
export const DIALECTS = new Map([
    ["libation_json", { parse: parseLibationJson }],
    ["opf", { parse: parseOpf, render: renderOpf }],
    ["kodi_album_nfo", { parse: parseAlbumNfo, render: renderAlbumNfo }],
    ["cue", { parse: parseCue, render: renderCue }],
]);
/**
 * Finds a dialect.
 *
 * @param id - Dialect id from the request.
 * @param table - Dialects to look in; tests pass their own.
 * @returns The dialect.
 * @throws {DialectFailure} For an id this plugin does not serve.
 */
export function dialect(id, table = DIALECTS) {
    const found = table.get(id);
    if (found === undefined)
        throw invalid(`unknown dialect \`${id}\``);
    return found;
}
/**
 * Parses one file with the dialect the host chose.
 *
 * @param id - Dialect id.
 * @param bytes - The whole file.
 * @param table - Dialects to look in; tests pass their own.
 * @returns What the file states.
 * @throws {DialectFailure} For an unknown dialect, a file over the cap or a
 *   file the dialect cannot read.
 */
export function parseFile(id, bytes, table = DIALECTS) {
    const found = dialect(id, table);
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
 * @param audioFiles - The book's audio files in playback order; empty when
 *   the host does not know them.
 * @param table - Dialects to look in; tests pass their own.
 * @returns The new file.
 * @throws {DialectFailure} For an unknown or read-only dialect, an existing
 *   file over the cap or unreadable, or a result over the cap.
 */
export function renderFile(id, existing, record, fields, audioFiles = [], table = DIALECTS) {
    const found = dialect(id, table);
    if (found.render === undefined)
        throw invalid(`dialect \`${id}\` is read-only`);
    if (existing.length > MAX_FILE_BYTES) {
        throw invalid(`existing file is over the ${MAX_FILE_BYTES} byte cap`);
    }
    return found.render(existing, record, fields, audioFiles);
}
