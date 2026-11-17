/**
 * Audiobookshelf's one-value text files: `desc.txt` holds the description
 * and `reader.txt` the narrators.
 *
 * `desc.txt` is the whole file, trimmed. `reader.txt` is read from its
 * first line, split into names at `,`, `;`, `&` and the word `and`; lines
 * after the first are kept as they are when the file is rewritten. A
 * present file always states its field, so an empty `reader.txt` states
 * that nobody narrates.
 */
import { RecordBuilder, decodeText, encodeFile, invalid, namesIn } from "./text.js";
/** Separators between narrator names on `reader.txt`'s first line. */
const NAME_SEPARATOR = /\s*(?:[,;&]|\band\b)\s*/i;
/**
 * Parses a `desc.txt`.
 *
 * @param bytes - The whole file.
 * @returns The description it states.
 */
export function parseDescTxt(bytes) {
    return new RecordBuilder().description(decodeText(bytes)).build();
}
/**
 * Renders a `desc.txt`.
 *
 * @param record - Values to write.
 * @param fields - Fields to set; only `description` applies.
 * @param existing - The file as it is now.
 * @returns The description and a final line break, or an empty file when
 *   the description is removed.
 */
export function renderDescTxt(existing, record, fields) {
    if (!fields.includes("description"))
        return existing;
    const description = record.description;
    return encodeFile(description === undefined ? "" : `${description}\n`);
}
/**
 * Splits a `reader.txt` line into names.
 *
 * @param text - The first line.
 * @returns The names, blanks dropped.
 */
export function splitNames(text) {
    return text.split(NAME_SEPARATOR).filter((name) => name.trim() !== "");
}
/**
 * Parses a `reader.txt`.
 *
 * @param bytes - The whole file.
 * @returns The narrators it states.
 */
export function parseReaderTxt(bytes) {
    const first = decodeText(bytes).split(/\r?\n|\r/, 1)[0] ?? "";
    return new RecordBuilder().credits("narrator", splitNames(first)).build();
}
/**
 * Renders a `reader.txt`: the narrators, comma separated, replace the first
 * line and the other lines stay.
 *
 * @param existing - The file as it is now.
 * @param record - Values to write.
 * @param fields - Fields to set; only `credits` applies, and only when the
 *   record states the narrator role.
 * @returns The new file; empty when no narrator and nothing else is left.
 */
export function renderReaderTxt(existing, record, fields) {
    if (!fields.includes("credits") || !record.creditRoles.includes("narrator"))
        return existing;
    const text = decodeText(existing);
    const breakAt = text.search(/\r?\n|\r/);
    const rest = breakAt === -1 ? "" : text.slice(breakAt);
    const names = namesIn(record, "narrator");
    const split = names.find((name) => splitNames(name).join("\u0000") !== name);
    if (split !== undefined) {
        throw invalid(`narrator \`${split}\` holds a separator, so reader.txt would read it as several names`);
    }
    const first = names.join(", ");
    if (first === "" && rest.trim() === "")
        return encodeFile("");
    return encodeFile(rest === "" ? `${first}\n` : `${first}${rest}`);
}
