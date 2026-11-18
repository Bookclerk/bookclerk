/**
 * A small XML scanner for the XML dialects, written for files the host does
 * not control.
 *
 * It reads a document into tokens that keep their source offsets, so a
 * writer can change a few elements and leave every other byte as it was.
 * It never expands entities beyond XML's five and numeric references, and
 * it refuses a document type with an internal subset, so no entity
 * declaration in a file can make it grow. Anything else that is not
 * well-formed (an unclosed tag, a close tag that does not match, a second
 * root or text outside the root) refuses the whole file.
 */
import { invalid } from "./text.js";
/**
 * Gives an attribute's value by its name after the prefix.
 *
 * @param element - The element.
 * @param local - Attribute name without a prefix.
 * @returns The first matching value.
 */
export function attribute(element, local) {
    return element.attributes.find((a) => localName(a.name) === local)?.value;
}
/**
 * Strips a prefix from a qualified name.
 *
 * @param name - Qualified name.
 * @returns The part after the last `:`.
 */
export function localName(name) {
    return name.slice(name.lastIndexOf(":") + 1);
}
/**
 * Decodes the five predefined entities and numeric character references;
 * any other reference stays as written.
 *
 * @param text - Raw text or attribute value.
 * @returns Decoded text.
 */
export function unescapeXml(text) {
    if (!text.includes("&"))
        return text;
    return text.replace(/&(#x[0-9a-fA-F]{1,6}|#[0-9]{1,7}|lt|gt|amp|quot|apos);/g, (_, ref) => {
        switch (ref) {
            case "lt":
                return "<";
            case "gt":
                return ">";
            case "amp":
                return "&";
            case "quot":
                return '"';
            case "apos":
                return "'";
            default: {
                const code = ref[1] === "x" ? Number.parseInt(ref.slice(2), 16) : Number.parseInt(ref.slice(1), 10);
                const valid = code > 0 && code <= 0x10ffff && (code < 0xd800 || code > 0xdfff);
                return valid ? String.fromCodePoint(code) : "�";
            }
        }
    });
}
/**
 * Escapes text for element content.
 *
 * @param text - Plain text.
 * @returns Text safe between tags.
 */
export function escapeText(text) {
    return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}
/**
 * Escapes text for a double-quoted attribute value.
 *
 * @param text - Plain text.
 * @returns Text safe inside `"…"`.
 */
export function escapeAttribute(text) {
    return escapeText(text).replace(/"/g, "&quot;");
}
/** Deepest element nesting read; deeper documents are refused. */
export const MAX_XML_DEPTH = 64;
/** An element name or attribute name. */
const NAME = /[^\s<>/=!?"'&]+/y;
/** Whitespace between attributes. */
const SPACE = /\s*/y;
/** An attribute value in either kind of quotes. */
const QUOTED = /"([^"<]*)"|'([^'<]*)'/y;
/**
 * Matches a sticky pattern at an offset.
 *
 * @param pattern - A sticky regular expression.
 * @param source - The document.
 * @param offset - Offset.
 * @returns The match, or `null`.
 */
function at(pattern, source, offset) {
    pattern.lastIndex = offset;
    return pattern.exec(source);
}
/**
 * Finds the end of a construct.
 *
 * @param source - The document.
 * @param from - Offset to search from.
 * @param close - The closing delimiter.
 * @param what - What is being closed, for the error.
 * @returns Offset just past the delimiter.
 * @throws {DialectFailure} When the delimiter is missing.
 */
function through(source, from, close, what) {
    const found = source.indexOf(close, from);
    if (found === -1)
        throw invalid(`XML ${what} is not closed`);
    return found + close.length;
}
/**
 * Reads a start tag.
 *
 * @param source - The document.
 * @param start - Offset of `<`.
 * @returns The tag.
 * @throws {DialectFailure} When the tag is malformed.
 */
function startTag(source, start) {
    const name = at(NAME, source, start + 1)?.[0];
    if (name === undefined)
        throw invalid("XML has a malformed tag");
    let i = start + 1 + name.length;
    const attributes = [];
    for (;;) {
        const space = at(SPACE, source, i)?.[0] ?? "";
        i += space.length;
        if (source.startsWith("/>", i))
            return { name, attributes, end: i + 2, selfClosing: true };
        if (source[i] === ">")
            return { name, attributes, end: i + 1, selfClosing: false };
        if (space === "")
            throw invalid("XML has a malformed tag");
        const attr = at(NAME, source, i)?.[0];
        if (attr === undefined)
            throw invalid("XML has a malformed attribute");
        i += attr.length;
        i += at(SPACE, source, i)?.[0].length ?? 0;
        if (source[i] !== "=")
            throw invalid("XML has an attribute without a value");
        i += 1;
        i += at(SPACE, source, i)?.[0].length ?? 0;
        const quoted = at(QUOTED, source, i);
        if (quoted === null)
            throw invalid("XML has an unquoted attribute value");
        attributes.push({ name: attr, value: unescapeXml(quoted[1] ?? quoted[2] ?? "") });
        i += quoted[0].length;
    }
}
/**
 * Scans a whole document.
 *
 * @param source - The document's text.
 * @returns The document.
 * @throws {DialectFailure} When the text is not well-formed XML, has no
 *   root element, nests deeper than {@link MAX_XML_DEPTH}, or declares an
 *   internal DTD subset.
 */
export function parseXml(source) {
    const stack = [];
    let root;
    let i = 0;
    /**
     * Adds text to every open element.
     *
     * @param text - Decoded text.
     */
    const addText = (text) => {
        for (const open of stack)
            open.text.push(text);
    };
    while (i < source.length) {
        const lt = source.indexOf("<", i);
        const textEnd = lt === -1 ? source.length : lt;
        if (textEnd > i) {
            const raw = source.slice(i, textEnd);
            if (stack.length === 0) {
                if (raw.trim() !== "" && raw.trim() !== "﻿")
                    throw invalid("XML has text outside the root");
            }
            else {
                addText(unescapeXml(raw));
            }
            i = textEnd;
            continue;
        }
        if (source.startsWith("<!--", i)) {
            i = through(source, i + 4, "-->", "comment");
        }
        else if (source.startsWith("<![CDATA[", i)) {
            if (stack.length === 0)
                throw invalid("XML has text outside the root");
            const end = through(source, i + 9, "]]>", "CDATA section");
            addText(source.slice(i + 9, end - 3));
            i = end;
        }
        else if (source.startsWith("<?", i)) {
            i = through(source, i + 2, "?>", "processing instruction");
        }
        else if (source.startsWith("<!", i)) {
            const end = through(source, i + 2, ">", "declaration");
            if (source.slice(i, end).includes("[")) {
                throw invalid("XML declares an internal DTD subset");
            }
            i = end;
        }
        else if (source.startsWith("</", i)) {
            const name = at(NAME, source, i + 2)?.[0];
            const end = through(source, i + 2, ">", "close tag");
            const open = stack.pop();
            if (name === undefined || open === undefined || open.element.name !== name) {
                throw invalid("XML close tag does not match");
            }
            if (source.slice(i + 2 + name.length, end - 1).trim() !== "") {
                throw invalid("XML has a malformed close tag");
            }
            const done = { ...open.element, closeStart: i, end, text: open.text.join("") };
            finish(done);
            i = end;
        }
        else {
            if (stack.length === 0 && root !== undefined)
                throw invalid("XML has a second root");
            const tag = startTag(source, i);
            const element = {
                name: tag.name,
                local: localName(tag.name),
                attributes: tag.attributes,
                start: i,
                openEnd: tag.end,
                selfClosing: tag.selfClosing,
                children: [],
                text: "",
            };
            if (tag.selfClosing) {
                finish({ ...element, closeStart: tag.end, end: tag.end });
            }
            else {
                if (stack.length === MAX_XML_DEPTH)
                    throw invalid("XML nests too deeply");
                stack.push({ element, text: [] });
            }
            i = tag.end;
        }
    }
    if (stack.length > 0)
        throw invalid("XML element is not closed");
    if (root === undefined)
        throw invalid("XML has no root element");
    return { source, root };
    /**
     * Hangs a finished element on its parent, or makes it the root.
     *
     * @param element - The element.
     */
    function finish(element) {
        const parent = stack[stack.length - 1];
        if (parent === undefined)
            root = element;
        else
            parent.element.children.push(element);
    }
}
/**
 * Applies edits that do not overlap.
 *
 * @param source - The text.
 * @param edits - Changes, in any order; inserts at one offset keep their order.
 * @returns The edited text.
 */
export function applyEdits(source, edits) {
    const ordered = edits
        .map((edit, index) => ({ edit, index }))
        .sort((a, b) => a.edit.start - b.edit.start || a.index - b.index);
    let out = "";
    let done = 0;
    for (const { edit } of ordered) {
        out += source.slice(done, edit.start) + edit.text;
        done = Math.max(done, edit.end);
    }
    return out + source.slice(done);
}
/**
 * The span that removes an element together with the indentation and line
 * break before it, so the lines around it stay tidy.
 *
 * @param source - The document.
 * @param element - The element to remove.
 * @returns The removal.
 */
export function removal(source, element) {
    let start = element.start;
    while (start > 0 && (source[start - 1] === " " || source[start - 1] === "\t"))
        start -= 1;
    if (source[start - 1] === "\n")
        start -= 1;
    if (source[start - 1] === "\r")
        start -= 1;
    return { start, end: element.end, text: "" };
}
