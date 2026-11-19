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

import { invalid } from "./text.ts";

/** One attribute of a start tag. */
export interface XmlAttribute {
  /** Qualified name as written, such as `opf:role`. */
  readonly name: string;
  /** Value with references decoded. */
  readonly value: string;
}

/** One element, with the source span it covers. */
export interface XmlElement {
  /** Qualified name as written, such as `dc:title`. */
  readonly name: string;
  /** Name after the prefix, such as `title`. */
  readonly local: string;
  /** Attributes in source order. */
  readonly attributes: readonly XmlAttribute[];
  /** Offset of the `<` that opens the element. */
  readonly start: number;
  /** Offset just past the start tag's `>`. */
  readonly openEnd: number;
  /** Offset of the close tag's `<`; equals `end` for an empty-element tag. */
  readonly closeStart: number;
  /** Offset just past the element's last `>`. */
  readonly end: number;
  /** Whether the element was written as `<name/>`. */
  readonly selfClosing: boolean;
  /** Child elements in source order. */
  readonly children: XmlElement[];
  /** Text and CDATA of the element and its descendants, decoded. */
  text: string;
}

/** A scanned document. */
export interface XmlDocument {
  /** The document's source text. */
  readonly source: string;
  /** The root element. */
  readonly root: XmlElement;
}

/**
 * Gives an attribute's value by its name after the prefix.
 *
 * @param element - The element.
 * @param local - Attribute name without a prefix.
 * @returns The first matching value.
 */
export function attribute(element: XmlElement, local: string): string | undefined {
  return element.attributes.find((a) => localName(a.name) === local)?.value;
}

/**
 * Strips a prefix from a qualified name.
 *
 * @param name - Qualified name.
 * @returns The part after the last `:`.
 */
export function localName(name: string): string {
  return name.slice(name.lastIndexOf(":") + 1);
}

/**
 * Decodes the five predefined entities and numeric character references;
 * any other reference stays as written.
 *
 * @param text - Raw text or attribute value.
 * @returns Decoded text.
 */
export function unescapeXml(text: string): string {
  if (!text.includes("&")) return text;
  return text.replace(
    /&(#x[0-9a-fA-F]{1,6}|#[0-9]{1,7}|lt|gt|amp|quot|apos);/g,
    (_, ref: string) => {
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
          const code =
            ref[1] === "x" ? Number.parseInt(ref.slice(2), 16) : Number.parseInt(ref.slice(1), 10);
          const valid = code > 0 && code <= 0x10ffff && (code < 0xd800 || code > 0xdfff);
          return valid ? String.fromCodePoint(code) : "�";
        }
      }
    },
  );
}

/**
 * Escapes text for element content.
 *
 * @param text - Plain text.
 * @returns Text safe between tags.
 */
export function escapeText(text: string): string {
  return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

/**
 * Escapes text for a double-quoted attribute value.
 *
 * @param text - Plain text.
 * @returns Text safe inside `"…"`.
 */
export function escapeAttribute(text: string): string {
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
function at(pattern: RegExp, source: string, offset: number): RegExpExecArray | null {
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
function through(source: string, from: number, close: string, what: string): number {
  const found = source.indexOf(close, from);
  if (found === -1) throw invalid(`XML ${what} is not closed`);
  return found + close.length;
}

/** A start tag read from the source. */
interface StartTag {
  /** Qualified name. */
  name: string;
  /** Attributes in source order. */
  attributes: XmlAttribute[];
  /** Offset just past `>`. */
  end: number;
  /** Whether it ends in `/>`. */
  selfClosing: boolean;
}

/**
 * Reads a start tag.
 *
 * @param source - The document.
 * @param start - Offset of `<`.
 * @returns The tag.
 * @throws {DialectFailure} When the tag is malformed.
 */
function startTag(source: string, start: number): StartTag {
  const name = at(NAME, source, start + 1)?.[0];
  if (name === undefined) throw invalid("XML has a malformed tag");
  let i = start + 1 + name.length;
  const attributes: XmlAttribute[] = [];
  for (;;) {
    const space = at(SPACE, source, i)?.[0] ?? "";
    i += space.length;
    if (source.startsWith("/>", i)) return { name, attributes, end: i + 2, selfClosing: true };
    if (source[i] === ">") return { name, attributes, end: i + 1, selfClosing: false };
    if (space === "") throw invalid("XML has a malformed tag");
    const attr = at(NAME, source, i)?.[0];
    if (attr === undefined) throw invalid("XML has a malformed attribute");
    i += attr.length;
    i += at(SPACE, source, i)?.[0].length ?? 0;
    if (source[i] !== "=") throw invalid("XML has an attribute without a value");
    i += 1;
    i += at(SPACE, source, i)?.[0].length ?? 0;
    const quoted = at(QUOTED, source, i);
    if (quoted === null) throw invalid("XML has an unquoted attribute value");
    attributes.push({ name: attr, value: unescapeXml(quoted[1] ?? quoted[2] ?? "") });
    i += quoted[0].length;
  }
}

/** An element still open while scanning. */
interface Open {
  /** Fields known at the start tag. */
  element: Omit<XmlElement, "closeStart" | "end">;
  /** Text gathered so far. */
  text: string[];
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
export function parseXml(source: string): XmlDocument {
  const stack: Open[] = [];
  let root: XmlElement | undefined;
  let i = 0;
  /**
   * Adds text to every open element.
   *
   * @param text - Decoded text.
   */
  const addText = (text: string): void => {
    for (const open of stack) open.text.push(text);
  };
  while (i < source.length) {
    const lt = source.indexOf("<", i);
    const textEnd = lt === -1 ? source.length : lt;
    if (textEnd > i) {
      const raw = source.slice(i, textEnd);
      if (stack.length === 0) {
        if (raw.trim() !== "" && raw.trim() !== "﻿") throw invalid("XML has text outside the root");
      } else {
        addText(unescapeXml(raw));
      }
      i = textEnd;
      continue;
    }
    if (source.startsWith("<!--", i)) {
      i = through(source, i + 4, "-->", "comment");
    } else if (source.startsWith("<![CDATA[", i)) {
      if (stack.length === 0) throw invalid("XML has text outside the root");
      const end = through(source, i + 9, "]]>", "CDATA section");
      addText(source.slice(i + 9, end - 3));
      i = end;
    } else if (source.startsWith("<?", i)) {
      i = through(source, i + 2, "?>", "processing instruction");
    } else if (source.startsWith("<!", i)) {
      const end = through(source, i + 2, ">", "declaration");
      if (source.slice(i, end).includes("[")) {
        throw invalid("XML declares an internal DTD subset");
      }
      i = end;
    } else if (source.startsWith("</", i)) {
      const name = at(NAME, source, i + 2)?.[0];
      const end = through(source, i + 2, ">", "close tag");
      const open = stack.pop();
      if (name === undefined || open === undefined || open.element.name !== name) {
        throw invalid("XML close tag does not match");
      }
      if (source.slice(i + 2 + name.length, end - 1).trim() !== "") {
        throw invalid("XML has a malformed close tag");
      }
      const done: XmlElement = { ...open.element, closeStart: i, end, text: open.text.join("") };
      finish(done);
      i = end;
    } else {
      if (stack.length === 0 && root !== undefined) throw invalid("XML has a second root");
      const tag = startTag(source, i);
      const element = {
        name: tag.name,
        local: localName(tag.name),
        attributes: tag.attributes,
        start: i,
        openEnd: tag.end,
        selfClosing: tag.selfClosing,
        children: [] as XmlElement[],
        text: "",
      };
      if (tag.selfClosing) {
        finish({ ...element, closeStart: tag.end, end: tag.end });
      } else {
        if (stack.length === MAX_XML_DEPTH) throw invalid("XML nests too deeply");
        stack.push({ element, text: [] });
      }
      i = tag.end;
    }
  }
  if (stack.length > 0) throw invalid("XML element is not closed");
  if (root === undefined) throw invalid("XML has no root element");
  return { source, root };

  /**
   * Hangs a finished element on its parent, or makes it the root.
   *
   * @param element - The element.
   */
  function finish(element: XmlElement): void {
    const parent = stack[stack.length - 1];
    if (parent === undefined) root = element;
    else parent.element.children.push(element);
  }
}

/** One change to a source text. */
export interface Edit {
  /** Offset where the replaced span starts. */
  readonly start: number;
  /** Offset just past the replaced span. */
  readonly end: number;
  /** Text put in its place. */
  readonly text: string;
}

/**
 * Applies edits that do not overlap.
 *
 * @param source - The text.
 * @param edits - Changes, in any order; inserts at one offset keep their order.
 * @returns The edited text.
 */
export function applyEdits(source: string, edits: readonly Edit[]): string {
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
export function removal(source: string, element: XmlElement): Edit {
  let start = element.start;
  while (start > 0 && (source[start - 1] === " " || source[start - 1] === "\t")) start -= 1;
  if (source[start - 1] === "\n") start -= 1;
  if (source[start - 1] === "\r") start -= 1;
  return { start, end: element.end, text: "" };
}

/**
 * The line break a document uses, so new lines match the old ones.
 *
 * @param source - The document.
 * @returns `\r\n` when the file has any, else `\n`.
 */
export function lineBreak(source: string): string {
  return source.includes("\r\n") ? "\r\n" : "\n";
}

/**
 * The indentation at the start of an element's line.
 *
 * @param source - The document.
 * @param element - The element.
 * @returns Its leading spaces and tabs, or `undefined` when other text
 *   shares the line.
 */
export function indentOf(source: string, element: XmlElement): string | undefined {
  const lineStart = source.lastIndexOf("\n", element.start - 1) + 1;
  const lead = source.slice(lineStart, element.start);
  return /^[ \t]*$/.test(lead) ? lead : undefined;
}

/**
 * The edit that appends new child elements, one per line, after an
 * element's last content. Each new line takes the indentation of the
 * first child (else the element's own plus two spaces) and the file's
 * line breaks; an empty-element tag is opened up to hold them.
 *
 * @param source - The document.
 * @param parent - The element that gets the children.
 * @param added - The new children, as source text.
 * @returns The insertion.
 */
export function appendChildren(source: string, parent: XmlElement, added: readonly string[]): Edit {
  const nl = lineBreak(source);
  const own = indentOf(source, parent) ?? "";
  const first = parent.children[0];
  const indent = (first && indentOf(source, first)) || `${own}  `;
  const lines = added.map((e) => `${nl}${indent}${e}`).join("");
  if (parent.selfClosing) {
    return {
      start: parent.end - 2,
      end: parent.end,
      text: `>${lines}${nl}${own}</${parent.name}>`,
    };
  }
  let insert = parent.closeStart;
  while (insert > parent.openEnd && /\s/.test(source[insert - 1] ?? "")) insert -= 1;
  return { start: insert, end: insert, text: lines };
}
