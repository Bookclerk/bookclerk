/**
 * Libation's `<book name>.metadata.json`: the store's catalog record for one
 * Audible product, saved once at download.
 *
 * The file holds an Audible catalog product, at the top level or under
 * `product` or `item`. The reader takes `title`, `subtitle`, `authors` and
 * `narrators` (`{ name }` lists), `publisher_name`, `release_date` (else
 * `issue_date`), `language`, `series` (`{ title, sequence, asin }`),
 * `publisher_summary` (HTML, else `merchandising_summary`),
 * `category_ladders` as genres, `format_type` as the abridgement, `asin`
 * and `isbn`, and the chapter layout and running time from
 * `content_metadata.chapter_info` when the file has them (else
 * `runtime_length_min`).
 *
 * The format is read-only: it is the store's record, not the library's.
 */
import { MAX_CHAPTERS, RecordBuilder, chapterTitle, decodeUtf8, invalid, wholeMs } from "./text.js";
/** Deepest chapter nesting followed; deeper chapters are left out. */
const MAX_DEPTH = 8;
/**
 * Whether a JSON value is an object (not an array or `null`).
 *
 * @param value - Any JSON value.
 * @returns True for an object.
 */
function isObject(value) {
    return typeof value === "object" && value !== null && !Array.isArray(value);
}
/**
 * Parses the file into its top-level object.
 *
 * @param bytes - The whole file.
 * @returns The object.
 * @throws {DialectFailure} When the file is not a JSON object.
 */
function parseObject(bytes) {
    let value;
    try {
        value = JSON.parse(decodeUtf8(bytes));
    }
    catch {
        throw invalid("Libation metadata is not valid JSON");
    }
    if (!isObject(value))
        throw invalid("Libation metadata is not a JSON object");
    return value;
}
/**
 * The entries of a JSON array that are objects.
 *
 * @param value - Any JSON value.
 * @returns The objects; empty for a non-array.
 */
function objects(value) {
    return Array.isArray(value) ? value.filter(isObject) : [];
}
/** Named HTML entities a store summary uses. */
const ENTITIES = {
    amp: "&",
    lt: "<",
    gt: ">",
    quot: '"',
    apos: "'",
    nbsp: " ",
    hellip: "…",
    mdash: "—",
    ndash: "–",
    lsquo: "‘",
    rsquo: "’",
    ldquo: "“",
    rdquo: "”",
};
/** Closing tags that end a block and become a paragraph break. */
const BLOCK_END = /^\/\s*(?:p|div|li|h[1-6])\s*$/i;
/** A line break tag. */
const LINE_BREAK = /^br\s*\/?\s*$/i;
/**
 * Drops a summary's tags in one pass: `<br>` becomes a line break, the end
 * of a paragraph, `div`, list item or heading a paragraph break, and every
 * other tag nothing. A `<` with no `>` after it is dropped on its own, so
 * no `<` survives in the result.
 *
 * @param html - The summary.
 * @returns The text between the tags, entities still encoded.
 */
function stripTags(html) {
    let out = "";
    let at = 0;
    for (;;) {
        const open = html.indexOf("<", at);
        if (open === -1)
            return out + html.slice(at);
        out += html.slice(at, open);
        const close = html.indexOf(">", open + 1);
        if (close === -1) {
            at = open + 1;
            continue;
        }
        const tag = html.slice(open + 1, close).trim();
        if (LINE_BREAK.test(tag))
            out += "\n";
        else if (BLOCK_END.test(tag))
            out += "\n\n";
        at = close + 1;
    }
}
/**
 * Turns a store's HTML summary into plain text: tags go (see `stripTags`),
 * then entities are decoded, so the only `<` in the text is one the summary
 * wrote as an entity.
 *
 * @param html - The summary.
 * @returns Plain text.
 */
export function htmlToText(html) {
    return stripTags(html)
        .replace(/&(#x[0-9a-f]{1,6}|#[0-9]{1,7}|[a-z]{2,8});/gi, (whole, name) => {
        if (name.startsWith("#")) {
            const code = name[1] === "x" || name[1] === "X"
                ? Number.parseInt(name.slice(2), 16)
                : Number.parseInt(name.slice(1), 10);
            const valid = code > 0 && code <= 0x10ffff && (code < 0xd800 || code > 0xdfff);
            return valid ? String.fromCodePoint(code) : "�";
        }
        return ENTITIES[name.toLowerCase()] ?? whole;
    })
        .replace(/[ \t]+\n/g, "\n")
        .replace(/\n{3,}/g, "\n\n");
}
/**
 * Reads Audible's chapter tree into pre-order chapters.
 *
 * @param list - A `chapters` array.
 * @param depth - Nesting depth of `list`.
 * @param out - Chapters so far.
 */
function walkChapters(list, depth, out) {
    for (const entry of objects(list)) {
        if (out.length === MAX_CHAPTERS)
            return;
        const startMs = wholeMs(entry["start_offset_ms"]);
        if (startMs === undefined)
            continue;
        const lengthMs = wholeMs(entry["length_ms"]);
        out.push({
            title: chapterTitle(entry["title"]),
            startMs,
            ...(lengthMs === undefined || lengthMs === 0 ? {} : { lengthMs }),
            depth,
        });
        if (depth < MAX_DEPTH)
            walkChapters(entry["chapters"], depth + 1, out);
    }
}
/**
 * Parses a Libation `.metadata.json`.
 *
 * @param bytes - The whole file.
 * @returns What the file states.
 * @throws {DialectFailure} When the file is not a JSON object.
 */
export function parseLibationJson(bytes) {
    const file = parseObject(bytes);
    const wrapped = isObject(file["product"]) ? file["product"] : file["item"];
    const item = isObject(wrapped) ? wrapped : file;
    const has = (key) => Object.hasOwn(item, key);
    const out = new RecordBuilder();
    if (has("title"))
        out.text("title", item["title"]);
    if (has("subtitle"))
        out.text("subtitle", item["subtitle"]);
    const summary = has("publisher_summary")
        ? item["publisher_summary"]
        : item["merchandising_summary"];
    if (has("publisher_summary") || has("merchandising_summary")) {
        out.description(typeof summary === "string" ? htmlToText(summary) : undefined);
    }
    if (has("publisher_name"))
        out.text("publisher", item["publisher_name"]);
    if (has("language"))
        out.text("language", item["language"]);
    if (has("release_date") || has("issue_date")) {
        out.publishedAt(has("release_date") ? item["release_date"] : item["issue_date"]);
    }
    if (has("category_ladders")) {
        const names = objects(item["category_ladders"]).flatMap((ladder) => objects(ladder["ladder"]).map((rung) => rung["name"]));
        out.labels("genres", names);
    }
    if (has("format_type")) {
        const format = item["format_type"];
        out.abridged(format === "abridged" ? true : format === "unabridged" ? false : undefined);
    }
    if (has("asin"))
        out.identifier("asin", item["asin"]);
    if (has("isbn"))
        out.identifier("isbn", item["isbn"]);
    if (has("authors"))
        out.credits("author", objects(item["authors"]).map((a) => a["name"]));
    if (has("narrators")) {
        out.credits("narrator", objects(item["narrators"]).map((n) => n["name"]));
    }
    if (has("series")) {
        out.state("series");
        for (const series of objects(item["series"])) {
            out.series(series["title"], series["sequence"], series["asin"]);
        }
    }
    const content = item["content_metadata"];
    const info = isObject(content) ? content["chapter_info"] : undefined;
    if (isObject(info)) {
        if (Object.hasOwn(info, "chapters")) {
            const chapters = [];
            walkChapters(info["chapters"], 0, chapters);
            out.chapters(chapters);
        }
        if (Object.hasOwn(info, "runtime_length_ms"))
            out.duration(wholeMs(info["runtime_length_ms"]));
    }
    if (out.build().durationMs === undefined && has("runtime_length_min")) {
        const minutes = item["runtime_length_min"];
        out.duration(typeof minutes === "number" ? wholeMs(minutes * 60_000) : undefined);
    }
    return out.build();
}
