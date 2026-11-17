/**
 * The plain-text NFO that audiobook release tools write and Audiobookshelf
 * reads: `Key: value` lines, then an optional `Book Description` section
 * that runs to the end of the file.
 *
 * Keys compare without case or surrounding space, and the last line for a
 * key wins, as in Audiobookshelf. A line with an empty value states
 * nothing. The keys read:
 *
 * | Key | Field |
 * | --- | --- |
 * | `Title` | title, and the subtitle after its first `:` |
 * | `Author` | author credits, comma separated |
 * | `Narrator`, `Read By` | narrator credits, comma separated |
 * | `Series Name`, `Position in Series` | one series entry |
 * | `Genre`, `Tags` | genres and tags, comma separated |
 * | `Release Date`, `Date`, `Audible.com Release` | publication date |
 * | `Copyright` and its variants | publication year, when no date line exists |
 * | `Unabridged`, `Abridged` | abridgement (`Yes` / `No`) |
 * | `Publisher`, `ASIN`, `ISBN`, `Language` | as named |
 *
 * Rendering edits lines in place: a changed field rewrites its first line
 * (keeping the key's spelling and spacing) and drops its other lines, a new
 * field is added after the last `Key: value` line, and a removed field
 * loses all its lines. Every other line, and the file's line endings, stay.
 */
import { RecordBuilder, decodeText, encodeFile, firstId, namesIn } from "./text.js";
/** `Title`: title and subtitle. */
const TITLE = { canonical: "Title", keys: ["title"] };
/** `Author`: author credits. */
const AUTHOR = { canonical: "Author", keys: ["author"] };
/** `Narrator`: narrator credits. */
const NARRATOR = { canonical: "Narrator", keys: ["narrator", "read by"] };
/** `Series Name`: the series. */
const SERIES = { canonical: "Series Name", keys: ["series name"] };
/** `Position in Series`: the series position. */
const POSITION = { canonical: "Position in Series", keys: ["position in series"] };
/** `Genre`: genres. */
const GENRE = { canonical: "Genre", keys: ["genre"] };
/** `Tags`: tags. */
const TAGS = { canonical: "Tags", keys: ["tags"] };
/** `Release Date`: the publication date. */
const RELEASE = {
    canonical: "Release Date",
    keys: ["release date", "date", "audible.com release"],
};
/** Copyright lines: the publication year when no date line exists. */
const COPYRIGHT = {
    canonical: "Copyright",
    keys: ["copyright", "audiobook copyright", "book copyright", "recording copyright"],
};
/** `Unabridged`: the abridgement. */
const UNABRIDGED = { canonical: "Unabridged", keys: ["unabridged", "abridged"] };
/** `Publisher`. */
const PUBLISHER = { canonical: "Publisher", keys: ["publisher"] };
/** `ASIN`. */
const ASIN = { canonical: "ASIN", keys: ["asin"] };
/** `ISBN`. */
const ISBN = { canonical: "ISBN", keys: ["isbn", "isbn-10", "isbn-13"] };
/** `Language`. */
const LANGUAGE = { canonical: "Language", keys: ["language", "lang"] };
/** Header line that starts the description section. */
const DESCRIPTION_HEADER = /^\s*book description\s*$/i;
/** Underline of a section header. */
const RULE = /^\s*=+\s*$/;
/** A `Key: value` line. */
const KEY_LINE = /^([^:]*):(\s*)(.*)$/;
/**
 * Splits a file into header lines and the description section.
 *
 * @param text - The whole file.
 * @returns Its parts.
 */
function split(text) {
    const eol = text.includes("\r\n") ? "\r\n" : "\n";
    const lines = text === "" ? [] : text.split(/\r\n|\n|\r/);
    if (lines.at(-1) === "")
        lines.pop();
    const start = lines.findIndex((l) => DESCRIPTION_HEADER.test(l));
    if (start === -1)
        return { head: lines, sectionHeader: [], body: [], eol };
    let bodyStart = start + 1;
    while (bodyStart < lines.length && RULE.test(lines[bodyStart] ?? ""))
        bodyStart += 1;
    return {
        head: lines.slice(0, start),
        sectionHeader: lines.slice(start, bodyStart),
        body: lines.slice(bodyStart),
        eol,
    };
}
/**
 * Reads a header line's key and value.
 *
 * @param text - One line.
 * @returns Lower-case key and trimmed value, or `undefined` for a line that
 *   is not `Key: value`.
 */
function keyValue(text) {
    const match = KEY_LINE.exec(text);
    if (match === null)
        return undefined;
    return { key: (match[1] ?? "").trim().toLowerCase(), value: (match[3] ?? "").trim() };
}
/**
 * The last non-empty value for a key group in the header.
 *
 * @param head - Header lines.
 * @param group - The keys to look for.
 * @returns The value, or `undefined`.
 */
function lastValue(head, group) {
    let found;
    for (const text of head) {
        const kv = keyValue(text);
        if (kv !== undefined && kv.value !== "" && group.keys.includes(kv.key))
            found = kv.value;
    }
    return found;
}
/**
 * Splits a comma-separated value.
 *
 * @param value - The value.
 * @returns Its entries.
 */
function commaList(value) {
    return value.split(/\s*,\s*/);
}
/**
 * Splits a `Title` value at its first `:` into title and subtitle.
 *
 * @param value - The value.
 * @returns Title and subtitle (`undefined` when there is none).
 */
function splitTitle(value) {
    const colon = value.indexOf(":");
    if (colon === -1)
        return { title: value, subtitle: undefined };
    return { title: value.slice(0, colon).trim(), subtitle: value.slice(colon + 1).trim() };
}
/**
 * Reads the abridgement from the last `Unabridged` or `Abridged` line, as
 * Audiobookshelf does: `Unabridged: Yes` and `Abridged: No` are
 * unabridged, any other answer abridged.
 *
 * @param head - Header lines.
 * @returns True when abridged, false when unabridged, `undefined` when no
 *   line says.
 */
function readAbridged(head) {
    let abridged;
    for (const text of head) {
        const kv = keyValue(text);
        if (kv === undefined || kv.value === "")
            continue;
        const answer = kv.value.toLowerCase();
        if (kv.key === "unabridged")
            abridged = answer !== "yes";
        else if (kv.key === "abridged")
            abridged = answer !== "no";
    }
    return abridged;
}
/**
 * Parses a text NFO.
 *
 * @param bytes - The whole file.
 * @returns What the file states.
 */
export function parseNfo(bytes) {
    const file = split(decodeText(bytes));
    const out = new RecordBuilder();
    const value = (group) => lastValue(file.head, group);
    const title = value(TITLE);
    if (title !== undefined) {
        const parts = splitTitle(title);
        out.text("title", parts.title).text("subtitle", parts.subtitle);
    }
    const author = value(AUTHOR);
    if (author !== undefined)
        out.credits("author", commaList(author));
    const narrator = value(NARRATOR);
    if (narrator !== undefined)
        out.credits("narrator", commaList(narrator));
    const series = value(SERIES);
    if (series !== undefined)
        out.series(series, value(POSITION));
    const genre = value(GENRE);
    if (genre !== undefined)
        out.labels("genres", commaList(genre));
    const tags = value(TAGS);
    if (tags !== undefined)
        out.labels("tags", commaList(tags));
    const date = value(RELEASE) ?? value(COPYRIGHT);
    if (date !== undefined)
        out.publishedAt(date);
    const abridged = readAbridged(file.head);
    if (abridged !== undefined)
        out.abridged(abridged);
    const publisher = value(PUBLISHER);
    if (publisher !== undefined)
        out.text("publisher", publisher);
    const asin = value(ASIN);
    if (asin !== undefined)
        out.identifier("asin", asin);
    const isbn = value(ISBN);
    if (isbn !== undefined)
        out.identifier("isbn", isbn);
    const language = value(LANGUAGE);
    if (language !== undefined)
        out.text("language", language);
    if (file.sectionHeader.length > 0) {
        const body = file.body.filter((l) => !RULE.test(l)).join("\n");
        if (body.trim() !== "")
            out.description(body);
    }
    return out.build();
}
/**
 * Sets or removes one key group's lines in the header.
 *
 * @param head - Header lines, edited in place.
 * @param group - The keys.
 * @param value - New value; `undefined` removes every line of the group.
 * @param keepKey - Whether a rewritten line keeps the file's key; false
 *   writes the canonical key, for a group whose keys give the value
 *   opposite meanings.
 */
function setGroup(head, group, value, keepKey = true) {
    let kept = false;
    for (let i = 0; i < head.length; i += 1) {
        const text = head[i] ?? "";
        const kv = keyValue(text);
        if (kv === undefined || !group.keys.includes(kv.key))
            continue;
        if (value === undefined || kept) {
            head.splice(i, 1);
            i -= 1;
            continue;
        }
        const match = KEY_LINE.exec(text);
        const space = match?.[2] === undefined || match[2] === "" ? " " : match[2];
        const key = match?.[1] ?? group.canonical;
        const written = keepKey ? key : `${/^\s*/.exec(key)?.[0] ?? ""}${group.canonical}`;
        head[i] = `${written}:${space}${value}`;
        kept = true;
    }
    if (value === undefined || kept)
        return;
    let at = head.length;
    while (at > 0 && keyValue(head[at - 1] ?? "") === undefined)
        at -= 1;
    if (at === 0) {
        at = head.length;
        while (at > 0 && (head[at - 1] ?? "").trim() === "")
            at -= 1;
    }
    head.splice(at, 0, `${group.canonical}: ${value}`);
}
/**
 * A comma-separated value, or `undefined` for an empty list.
 *
 * @param values - Entries.
 * @returns The joined value.
 */
function joinList(values) {
    return values.length === 0 ? undefined : values.join(", ");
}
/**
 * Renders a text NFO.
 *
 * @param existing - The file as it is now; empty to create it.
 * @param record - Values to write.
 * @param fields - Fields to set; an empty value removes the field's lines.
 * @returns The new file; empty when nothing is left in it.
 * @throws {DialectFailure} When the result is over the size cap.
 */
export function renderNfo(existing, record, fields) {
    const file = split(decodeText(existing));
    const head = file.head;
    const wants = (field) => fields.includes(field);
    if (wants("title") || wants("subtitle")) {
        const current = lastValue(head, TITLE);
        const parts = current === undefined ? undefined : splitTitle(current);
        const title = wants("title") ? record.title : parts?.title;
        const subtitle = wants("subtitle") ? record.subtitle : parts?.subtitle;
        const value = title === undefined || title === ""
            ? undefined
            : subtitle === undefined || subtitle === ""
                ? title
                : `${title}: ${subtitle}`;
        setGroup(head, TITLE, value);
    }
    if (wants("credits")) {
        if (record.creditRoles.includes("author")) {
            setGroup(head, AUTHOR, joinList(namesIn(record, "author")));
        }
        if (record.creditRoles.includes("narrator")) {
            setGroup(head, NARRATOR, joinList(namesIn(record, "narrator")));
        }
    }
    if (wants("series")) {
        const series = record.seriesEntries[0];
        setGroup(head, SERIES, series?.name);
        setGroup(head, POSITION, series?.position);
    }
    if (wants("genres"))
        setGroup(head, GENRE, joinList(record.genres));
    if (wants("tags"))
        setGroup(head, TAGS, joinList(record.tags));
    if (wants("publishedAt")) {
        setGroup(head, RELEASE, record.publishedAt);
        // Copyright years are read when no date line exists, so a removed date
        // takes them too; a set date outranks them and they stay.
        if (record.publishedAt === undefined)
            setGroup(head, COPYRIGHT, undefined);
    }
    if (wants("abridgement")) {
        const value = record.abridgement === "unknown"
            ? undefined
            : record.abridgement === "abridged"
                ? "No"
                : "Yes";
        // `Abridged: Yes` and `Unabridged: Yes` mean opposite things, so the
        // line is always rewritten under the `Unabridged` key.
        setGroup(head, UNABRIDGED, value, false);
    }
    if (wants("publisher"))
        setGroup(head, PUBLISHER, record.publisher);
    if (wants("language"))
        setGroup(head, LANGUAGE, record.language);
    if (wants("identifiers")) {
        setGroup(head, ASIN, firstId(record, "asin"));
        setGroup(head, ISBN, firstId(record, "isbn"));
    }
    let section = [...file.sectionHeader, ...file.body];
    if (wants("description")) {
        const description = record.description;
        if (description === undefined) {
            section = [];
        }
        else {
            const header = file.sectionHeader.length > 0
                ? file.sectionHeader
                : ["Book Description", "================"];
            section = [...header, ...description.split(/\r\n|\n|\r/)];
        }
    }
    const gap = section.length > 0 && head.length > 0 && (head.at(-1) ?? "").trim() !== "";
    const lines = gap ? [...head, "", ...section] : [...head, ...section];
    if (lines.every((l) => l.trim() === ""))
        return encodeFile("");
    return encodeFile(`${lines.join(file.eol)}${file.eol}`);
}
