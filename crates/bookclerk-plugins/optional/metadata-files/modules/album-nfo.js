/**
 * Kodi's `album.nfo`: the XML sidecar media centers such as Kodi and
 * Jellyfin read for a music album, which audiobook folders reuse.
 *
 * The reader takes the `<album>` root's children: `title`, `review` as the
 * description, `label` as the publisher, a non-blank `releasedate` (else
 * `year`) as the date, each `genre`, and as authors the `artist` of each
 * `albumArtistCredits` (what Kodi exports), else each direct `artist`
 * (what older files and other tools write). Text is plain, not HTML.
 *
 * The writer edits the file in place, as the OPF writer does: for each
 * field it sets, it removes the elements that field reads from and appends
 * new ones at the end of `<album>`, and adds nothing when there is nothing
 * to add. Writing the date also writes `year`; writing authors writes each
 * as a direct `artist` and an `albumArtistCredits` entry, and replaces
 * `artistdesc` (the artists as Kodi shows them) with the names joined by
 * ` / `.
 */
import { RecordBuilder, decodeUtf8, encodeFile, invalid, labels } from "./text.js";
import { appendChildren, applyEdits, escapeText, parseXml, removal, } from "./xml.js";
/** The file a render creates when there is none yet. */
const SKELETON = `<?xml version="1.0" encoding="UTF-8" standalone="yes" ?>
<album>
</album>
`;
/**
 * Scans an `album.nfo`.
 *
 * @param source - The file's text.
 * @returns The `<album>` root.
 * @throws {DialectFailure} When the text is not XML or its root is not an
 *   `<album>`.
 */
function readAlbum(source) {
    const { root } = parseXml(source);
    if (root.name !== "album")
        throw invalid("album.nfo root element is not an album");
    return root;
}
/**
 * Whether writing a field replaces a child.
 *
 * @param element - Any child of `<album>`.
 * @param field - A field being written.
 * @param authors - Whether the request states authors.
 * @returns True when the child goes.
 */
function replaces(element, field, authors) {
    switch (field) {
        case "title":
            return element.name === "title";
        case "description":
            return element.name === "review";
        case "publisher":
            return element.name === "label";
        case "publishedAt":
            return element.name === "releasedate" || element.name === "year";
        case "genres":
            return element.name === "genre";
        case "credits":
            return (authors &&
                (element.name === "artist" ||
                    element.name === "albumArtistCredits" ||
                    element.name === "artistdesc"));
        case "subtitle":
        case "language":
        case "tags":
        case "abridgement":
        case "expressionKind":
        case "identifiers":
        case "series":
        case "chapters":
        case "duration":
        case "explicitness":
            return false;
    }
}
/**
 * Parses a Kodi `album.nfo`.
 *
 * @param bytes - The whole file.
 * @returns What the file states.
 * @throws {DialectFailure} When the file is not well-formed XML with an
 *   `<album>` root.
 */
export function parseAlbumNfo(bytes) {
    const kids = readAlbum(decodeUtf8(bytes)).children;
    const first = (name) => kids.find((k) => k.name === name);
    const all = (name) => kids.filter((k) => k.name === name).map((k) => k.text);
    const out = new RecordBuilder();
    const title = first("title");
    if (title !== undefined)
        out.text("title", title.text);
    const review = first("review");
    if (review !== undefined)
        out.description(review.text);
    const label = first("label");
    if (label !== undefined)
        out.text("publisher", label.text);
    const released = first("releasedate");
    // A blank releasedate gives way to year, else still states the date empty.
    const date = released !== undefined && released.text.trim() !== "" ? released : (first("year") ?? released);
    if (date !== undefined)
        out.publishedAt(date.text);
    if (first("genre") !== undefined)
        out.labels("genres", all("genre"));
    // Credits with a usable name win; blank ones give way to direct artists.
    const credits = kids.filter((k) => k.name === "albumArtistCredits");
    const credited = labels(credits.flatMap((k) => k.children.filter((c) => c.name === "artist").map((c) => c.text)));
    if (credited.length > 0)
        out.credits("author", credited);
    else if (first("artist") !== undefined)
        out.credits("author", all("artist"));
    else if (credits.length > 0)
        out.credits("author", []);
    return out.build();
}
/**
 * Writes the fields asked for into a Kodi `album.nfo`.
 *
 * @param existing - The file as it is now; empty to create it.
 * @param record - Values to write.
 * @param fields - Fields to set.
 * @returns The new file.
 * @throws {DialectFailure} When `existing` cannot be read or the result is
 *   over the size cap.
 */
export function renderAlbumNfo(existing, record, fields) {
    const source = existing.length === 0 ? SKELETON : decodeUtf8(existing);
    const album = readAlbum(source);
    const authors = record.creditRoles.includes("author");
    const removed = album.children.filter((kid) => fields.some((field) => replaces(kid, field, authors)));
    const added = [];
    const set = (field) => fields.includes(field);
    const element = (name, text) => {
        added.push(`<${name}>${escapeText(text)}</${name}>`);
    };
    if (set("title") && record.title !== undefined)
        element("title", record.title);
    if (set("description") && record.description !== undefined) {
        element("review", record.description);
    }
    if (set("publisher") && record.publisher !== undefined)
        element("label", record.publisher);
    if (set("publishedAt") && record.publishedAt !== undefined) {
        element("releasedate", record.publishedAt);
        element("year", record.publishedAt.slice(0, 4));
    }
    if (set("genres"))
        for (const genre of record.genres)
            element("genre", genre);
    if (set("credits") && authors) {
        const names = record.credits.filter((c) => c.role === "author").map((c) => c.name);
        if (names.length > 0)
            element("artistdesc", names.join(" / "));
        for (const name of names)
            element("artist", name);
        for (const name of names) {
            added.push(`<albumArtistCredits><artist>${escapeText(name)}</artist></albumArtistCredits>`);
        }
    }
    const edits = removed.map((kid) => removal(source, kid));
    if (added.length > 0)
        edits.push(appendChildren(source, album, added));
    return encodeFile(applyEdits(source, edits));
}
