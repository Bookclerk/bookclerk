/**
 * OPF: the package document of an EPUB, which Calibre and other tools also
 * save on its own next to a book as `metadata.opf`.
 *
 * The reader takes the first `<metadata>` of the `<package>` root and reads
 * its children: `dc:title`, `dc:description` (HTML, turned into text),
 * `dc:publisher`, the publication `dc:date`, `dc:language`, `dc:subject` as
 * genres, `dc:identifier` with an ISBN or ASIN scheme (or a `urn:isbn:`
 * value), `dc:creator` and `dc:contributor` by MARC relator role (`aut`,
 * `nrt`, `trl`, `edt`, `ill`; a creator without a role is an author), and
 * one series from Calibre's `calibre:series` and `calibre:series_index`, or
 * else from EPUB 3's `belongs-to-collection` with its `group-position`. A
 * role or position can sit on the element or in a `<meta refines="#id">`.
 *
 * The writer edits the file in place: for each field it is asked to set, it
 * removes the elements that field reads from and appends new ones at the end
 * of `<metadata>`, and every other byte stays as it was. Credits change only
 * for the roles the request states.
 */
import { htmlToText } from "./html.js";
import { RecordBuilder, decodeUtf8, encodeFile, invalid } from "./text.js";
import { applyEdits, attribute, escapeAttribute, escapeText, parseXml, removal, } from "./xml.js";
/** Dublin Core elements namespace. */
const DC = "http://purl.org/dc/elements/1.1/";
/** OPF namespace, which also qualifies `role` and `scheme` attributes. */
const OPF = "http://www.idpf.org/2007/opf";
/** MARC relator codes and the roles they credit. */
const ROLES = new Map([
    ["aut", "author"],
    ["nrt", "narrator"],
    ["trl", "translator"],
    ["edt", "editor"],
    ["ill", "illustrator"],
]);
/** The relator code written for each role. */
const CODES = new Map([...ROLES].map(([code, role]) => [role, code]));
/** The file a render creates when there is none yet. */
const SKELETON = `<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="${OPF}" version="2.0">
  <metadata xmlns:dc="${DC}" xmlns:opf="${OPF}">
  </metadata>
</package>
`;
/**
 * Scans a package document.
 *
 * @param source - The file's text.
 * @returns The document and its metadata element.
 * @throws {DialectFailure} When the text is not XML or its root is not a
 *   `<package>`.
 */
function readPackage(source) {
    const doc = parseXml(source);
    if (doc.root.local !== "package")
        throw invalid("OPF root element is not a package");
    return { doc, metadata: doc.root.children.find((c) => c.local === "metadata") };
}
/**
 * Finds the `<meta>` elements that refine an element.
 *
 * @param kids - The metadata's children.
 * @param element - The refined element.
 * @param property - The refinement's property.
 * @returns Matching metas in source order.
 */
function refinements(kids, element, property) {
    const id = attribute(element, "id");
    if (id === undefined)
        return [];
    return kids.filter((k) => k.local === "meta" &&
        attribute(k, "refines") === `#${id}` &&
        attribute(k, "property") === property);
}
/**
 * The credit role of a creator or contributor.
 *
 * @param kids - The metadata's children.
 * @param element - Any child.
 * @returns The role, or `undefined` for anything else or a role the
 *   dialect does not carry.
 */
function roleOf(kids, element) {
    if (element.local !== "creator" && element.local !== "contributor")
        return undefined;
    const code = attribute(element, "role") ?? refinements(kids, element, "role")[0]?.text;
    if (code === undefined)
        return element.local === "creator" ? "author" : undefined;
    return ROLES.get(code.trim().toLowerCase());
}
/**
 * Whether a child is the publication date.
 *
 * @param element - Any child.
 * @returns True for a `dc:date` with no event or the publication event.
 */
function isPublicationDate(element) {
    if (element.local !== "date")
        return false;
    const event = attribute(element, "event");
    return event === undefined || event.trim().toLowerCase() === "publication";
}
/**
 * Reads an ISBN or ASIN identifier.
 *
 * @param element - Any child.
 * @returns The scheme and value, or `undefined` for anything else.
 */
function identifierOf(element) {
    if (element.local !== "identifier")
        return undefined;
    const text = element.text.trim();
    const scheme = attribute(element, "scheme")?.trim().toLowerCase();
    if (scheme === "isbn" || scheme === "asin") {
        return { scheme, value: text.replace(/^(?:urn:)?(?:isbn|asin):/i, "") };
    }
    const prefixed = /^(?:urn:)?(isbn|asin):(.*)$/is.exec(text);
    if (prefixed === null)
        return undefined;
    return {
        scheme: prefixed[1]?.toLowerCase() === "asin" ? "asin" : "isbn",
        value: prefixed[2] ?? "",
    };
}
/**
 * Whether a child is a Calibre `<meta name="…">`.
 *
 * @param element - Any child.
 * @param name - The meta's name.
 * @returns True for that meta.
 */
function isCalibreMeta(element, name) {
    return element.local === "meta" && attribute(element, "name") === name;
}
/**
 * Whether a child is an EPUB 3 collection.
 *
 * @param element - Any child.
 * @returns True for `belongs-to-collection`.
 */
function isCollection(element) {
    return element.local === "meta" && attribute(element, "property") === "belongs-to-collection";
}
/**
 * Writes a series index as people read it: Calibre saves `2` as `2.0`.
 *
 * @param value - The index as the file has it.
 * @returns The position label.
 */
function position(value) {
    return value?.trim().replace(/^(\d+)\.0+$/, "$1");
}
/**
 * Parses an OPF package document.
 *
 * @param bytes - The whole file.
 * @returns What the file states.
 * @throws {DialectFailure} When the file is not well-formed XML with a
 *   `<package>` root.
 */
export function parseOpf(bytes) {
    const { metadata } = readPackage(decodeUtf8(bytes));
    const out = new RecordBuilder();
    if (metadata === undefined)
        return out.build();
    const kids = metadata.children;
    const first = (local) => kids.find((k) => k.local === local);
    const title = first("title");
    if (title !== undefined)
        out.text("title", title.text);
    const description = first("description");
    if (description !== undefined)
        out.description(htmlToText(description.text));
    const publisher = first("publisher");
    if (publisher !== undefined)
        out.text("publisher", publisher.text);
    const date = kids.find(isPublicationDate);
    if (date !== undefined)
        out.publishedAt(date.text);
    const language = first("language");
    if (language !== undefined)
        out.text("language", language.text);
    const subjects = kids.filter((k) => k.local === "subject");
    if (subjects.length > 0)
        out.labels("genres", subjects.map((s) => s.text));
    for (const id of kids.map(identifierOf)) {
        if (id !== undefined)
            out.identifier(id.scheme, id.value);
    }
    const credits = new Map();
    for (const kid of kids) {
        const role = roleOf(kids, kid);
        if (role === undefined)
            continue;
        const names = credits.get(role) ?? [];
        names.push(kid.text);
        credits.set(role, names);
    }
    for (const [role, names] of credits)
        out.credits(role, names);
    const calibre = kids.find((k) => isCalibreMeta(k, "calibre:series"));
    const collection = kids.find(isCollection);
    if (calibre !== undefined) {
        const index = kids.find((k) => isCalibreMeta(k, "calibre:series_index"));
        out.series(attribute(calibre, "content"), position(index && attribute(index, "content")));
    }
    else if (collection !== undefined) {
        out.series(collection.text, position(refinements(kids, collection, "group-position")[0]?.text));
    }
    return out.build();
}
/**
 * Whether a field reads from a child, so writing the field replaces it.
 *
 * @param kids - The metadata's children.
 * @param element - Any child.
 * @param field - A field being written.
 * @param roles - Credit roles the request states.
 * @returns True when the child goes.
 */
function replaces(kids, element, field, roles) {
    switch (field) {
        case "title":
        case "description":
        case "publisher":
        case "language":
            return element.local === field;
        case "publishedAt":
            return isPublicationDate(element);
        case "genres":
            return element.local === "subject";
        case "identifiers":
            return identifierOf(element) !== undefined;
        case "credits": {
            const role = roleOf(kids, element);
            return role !== undefined && roles.includes(role);
        }
        case "series":
            return (isCalibreMeta(element, "calibre:series") ||
                isCalibreMeta(element, "calibre:series_index") ||
                isCollection(element));
        case "subtitle":
        case "tags":
        case "abridgement":
        case "expressionKind":
        case "chapters":
        case "duration":
            return false;
    }
}
/**
 * The new elements for the fields being written, in a fixed order.
 *
 * @param record - Values to write.
 * @param fields - Fields to set.
 * @param dc - Prefix bound to Dublin Core.
 * @param opf - Prefix bound to the OPF namespace, for attributes.
 * @returns One element per entry, as source text.
 */
function newElements(record, fields, dc, opf) {
    const out = [];
    const set = (field) => fields.includes(field);
    const element = (local, text, attrs = "") => {
        out.push(`<${dc}:${local}${attrs}>${escapeText(text)}</${dc}:${local}>`);
    };
    if (set("title") && record.title !== undefined)
        element("title", record.title);
    if (set("description") && record.description !== undefined) {
        // The element holds HTML, so the text is escaped once as HTML and once
        // more as XML.
        element("description", escapeText(record.description));
    }
    if (set("publisher") && record.publisher !== undefined)
        element("publisher", record.publisher);
    if (set("publishedAt") && record.publishedAt !== undefined)
        element("date", record.publishedAt);
    if (set("language") && record.language !== undefined)
        element("language", record.language);
    if (set("genres"))
        for (const genre of record.genres)
            element("subject", genre);
    if (set("identifiers")) {
        for (const id of record.identifiers) {
            if (id.scheme !== "isbn" && id.scheme !== "asin")
                continue;
            element("identifier", id.value, ` ${opf}:scheme="${id.scheme.toUpperCase()}"`);
        }
    }
    if (set("credits")) {
        for (const credit of record.credits) {
            const code = CODES.get(credit.role);
            if (code === undefined || !record.creditRoles.includes(credit.role))
                continue;
            element("creator", credit.name, ` ${opf}:role="${code}"`);
        }
    }
    const series = record.seriesEntries[0];
    if (set("series") && series !== undefined) {
        out.push(`<meta name="calibre:series" content="${escapeAttribute(series.name)}"/>`);
        if (series.position !== undefined) {
            out.push(`<meta name="calibre:series_index" content="${escapeAttribute(series.position)}"/>`);
        }
    }
    return out;
}
/**
 * Finds the prefix a document binds to a namespace on the root or the
 * metadata element.
 *
 * @param elements - Elements whose declarations count.
 * @param uri - The namespace.
 * @returns The prefix, or `undefined` when none is bound.
 */
function prefixFor(elements, uri) {
    for (const element of elements) {
        for (const attr of element.attributes) {
            if (attr.name.startsWith("xmlns:") && attr.value === uri)
                return attr.name.slice(6);
        }
    }
    return undefined;
}
/**
 * The indentation at the start of an element's line.
 *
 * @param source - The document.
 * @param element - The element.
 * @returns Its leading spaces and tabs, or `undefined` when other text
 *   shares the line.
 */
function indentOf(source, element) {
    const lineStart = source.lastIndexOf("\n", element.start - 1) + 1;
    const lead = source.slice(lineStart, element.start);
    return /^[ \t]*$/.test(lead) ? lead : undefined;
}
/**
 * Writes the fields asked for into an OPF package document.
 *
 * @param existing - The file as it is now; empty to create it.
 * @param record - Values to write.
 * @param fields - Fields to set.
 * @returns The new file.
 * @throws {DialectFailure} When `existing` cannot be read or the result is
 *   over the size cap.
 */
export function renderOpf(existing, record, fields) {
    const source = existing.length === 0 ? SKELETON : decodeUtf8(existing);
    const { doc, metadata } = readPackage(source);
    const root = doc.root;
    const edits = [];
    const declared = metadata === undefined ? [root] : [root, metadata];
    let dc = prefixFor(declared, DC);
    let opf = prefixFor(declared, OPF);
    let declare = "";
    if (dc === undefined) {
        dc = "dc";
        if (!declared.some((e) => attribute(e, "dc") !== undefined))
            declare += ` xmlns:dc="${DC}"`;
    }
    if (opf === undefined) {
        opf = "opf";
        if (!declared.some((e) => attribute(e, "opf") !== undefined))
            declare += ` xmlns:opf="${OPF}"`;
    }
    const kids = metadata?.children ?? [];
    const removed = kids.filter((kid) => fields.some((field) => replaces(kids, kid, field, record.creditRoles)));
    // A refinement of a removed element goes with it, unless another element
    // still carries the same id.
    const gone = new Set(removed.map((k) => attribute(k, "id")).filter((id) => id !== undefined));
    for (const kid of kids) {
        if (removed.includes(kid))
            continue;
        const id = attribute(kid, "id");
        if (id !== undefined)
            gone.delete(id);
    }
    for (const kid of kids) {
        const target = attribute(kid, "refines");
        if (kid.local === "meta" && target?.startsWith("#") && gone.has(target.slice(1))) {
            if (!removed.includes(kid))
                removed.push(kid);
        }
    }
    for (const kid of removed)
        edits.push(removal(source, kid));
    const added = newElements(record, fields, dc, opf);
    const rootIndent = indentOf(source, root) ?? "";
    if (metadata === undefined) {
        const indent = `${rootIndent}  `;
        const block = `\n${indent}<metadata xmlns:dc="${DC}" xmlns:opf="${OPF}">` +
            added.map((e) => `\n${indent}  ${e}`).join("") +
            `\n${indent}</metadata>`;
        if (root.selfClosing) {
            edits.push({
                start: root.end - 2,
                end: root.end,
                text: `>${block}\n${rootIndent}</${root.name}>`,
            });
        }
        else {
            edits.push({ start: root.openEnd, end: root.openEnd, text: block });
        }
        return encodeFile(applyEdits(source, edits));
    }
    if (declare !== "") {
        const at = metadata.start + 1 + metadata.name.length;
        edits.push({ start: at, end: at, text: declare });
    }
    const ownIndent = indentOf(source, metadata) ?? rootIndent;
    const firstKid = kids[0];
    const indent = (firstKid && indentOf(source, firstKid)) || `${ownIndent}  `;
    const lines = added.map((e) => `\n${indent}${e}`).join("");
    if (metadata.selfClosing) {
        edits.push({
            start: metadata.end - 2,
            end: metadata.end,
            text: `>${lines}\n${ownIndent}</${metadata.name}>`,
        });
    }
    else {
        let insert = metadata.closeStart;
        while (insert > metadata.openEnd && /\s/.test(source[insert - 1] ?? ""))
            insert -= 1;
        edits.push({ start: insert, end: insert, text: lines });
    }
    return encodeFile(applyEdits(source, edits));
}
