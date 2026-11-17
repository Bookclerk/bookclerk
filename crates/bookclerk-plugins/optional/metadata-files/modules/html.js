/**
 * Turning a store's or a tool's HTML summary into the plain text a record
 * carries. Shared by the dialects whose descriptions are HTML.
 */
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
