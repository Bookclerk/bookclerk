/**
 * Audiobookshelf's `metadata.json`, as Audiobookshelf 2.x writes and reads
 * it.
 *
 * The file is one JSON object: `title`, `subtitle`, `authors` and
 * `narrators` (name lists), `series` (`"Name #2"` strings), `genres`,
 * `tags`, `publishedYear`, `publishedDate`, `publisher`, `description`,
 * `isbn`, `asin`, `language`, `explicit`, `abridged` and `chapters`
 * (`{ id, start, end, title }` in seconds). Older files nest the book
 * fields under `metadata`; Audiobookshelf lets a nested value win, and so
 * does this reader.
 *
 * A key the file holds is a field it states, even when its value is
 * `null` or empty. `abridged: false` reads as unabridged, as Audiobookshelf
 * treats it. Rendering keeps every key it does not own, in place, and
 * writes the file with two-space indentation as Audiobookshelf does.
 */

import type {
  MetadataChapter,
  MetadataField,
  MetadataFileRecord,
} from "@bookclerk/plugin-sdk/workerd";
import {
  RecordBuilder,
  chapterTitle,
  decodeUtf8,
  decodeUtf8Strict,
  encodeFile,
  firstId,
  invalid,
  namesIn,
  partialDate,
  secondsToMs,
} from "./text.ts";

/** A parsed JSON object. */
type JsonObject = Record<string, unknown>;

/** A series string's trailing ` #position`, the way Audiobookshelf reads it. */
const SERIES_POSITION = / #([^#\s]+)$/;

/**
 * Whether a JSON value is an object (not an array or `null`).
 *
 * @param value - Any JSON value.
 * @returns True for an object.
 */
function isObject(value: unknown): value is JsonObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * Parses the file into its top-level object.
 *
 * @param bytes - The whole file.
 * @param what - File description for errors.
 * @returns The object.
 * @throws {DialectFailure} When the file is not a JSON object.
 */
export function parseObject(bytes: Uint8Array, what: string): JsonObject {
  let value: unknown;
  try {
    value = JSON.parse(decodeUtf8(bytes));
  } catch {
    throw invalid(`${what} is not valid JSON`);
  }
  if (!isObject(value)) throw invalid(`${what} is not a JSON object`);
  return value;
}

/**
 * The book fields of a file, with the legacy nested `metadata` object
 * merged over the top level as Audiobookshelf merges it.
 *
 * @param file - The file's top-level object.
 * @returns Key to value; a key is present when the file holds it.
 */
function bookFields(file: JsonObject): Map<string, unknown> {
  const fields = new Map<string, unknown>(Object.entries(file));
  const nested = file["metadata"];
  if (isObject(nested)) {
    for (const [key, value] of Object.entries(nested)) {
      if (value !== undefined) fields.set(key, value);
    }
  }
  fields.delete("metadata");
  return fields;
}

/**
 * Reads Audiobookshelf's chapter list.
 *
 * @param value - The file's `chapters` value.
 * @returns Chapters at depth 0; entries without a usable start are dropped.
 */
function readChapters(value: unknown): MetadataChapter[] {
  if (!Array.isArray(value)) return [];
  const chapters: MetadataChapter[] = [];
  for (const entry of value) {
    if (!isObject(entry)) continue;
    const startMs = secondsToMs(entry["start"]);
    if (startMs === undefined) continue;
    const endMs = secondsToMs(entry["end"]);
    chapters.push({
      title: chapterTitle(entry["title"]),
      startMs,
      ...(endMs !== undefined && endMs > startMs ? { lengthMs: endMs - startMs } : {}),
      depth: 0,
    });
  }
  return chapters;
}

/**
 * Reads `abridged`, a boolean or the strings `"true"` and `"false"`.
 *
 * @param value - The file's value.
 * @returns The flag, or `undefined` for anything else.
 */
function readAbridged(value: unknown): boolean | undefined {
  if (typeof value === "boolean") return value;
  if (value === "true") return true;
  if (value === "false") return false;
  return undefined;
}

/**
 * Parses an Audiobookshelf `metadata.json`.
 *
 * @param bytes - The whole file.
 * @returns What the file states.
 * @throws {DialectFailure} When the file is not a JSON object.
 */
export function parseAbsJson(bytes: Uint8Array): MetadataFileRecord {
  const fields = bookFields(parseObject(bytes, "metadata.json"));
  const out = new RecordBuilder();
  const has = (key: string): boolean => fields.has(key);
  const get = (key: string): unknown => fields.get(key);
  if (has("title")) out.text("title", get("title"));
  if (has("subtitle")) out.text("subtitle", get("subtitle"));
  if (has("description")) out.description(get("description"));
  if (has("publisher")) out.text("publisher", get("publisher"));
  if (has("language")) out.text("language", get("language"));
  if (has("publishedDate") || has("publishedYear")) {
    // The full date when it reads as one, else the year.
    const year = get("publishedYear");
    out.publishedAt(
      partialDate(get("publishedDate")) ?? (typeof year === "number" ? String(year) : year),
    );
  }
  if (has("genres")) out.labels("genres", get("genres"));
  if (has("tags")) out.labels("tags", get("tags"));
  if (has("abridged")) out.abridged(readAbridged(get("abridged")));
  if (has("isbn")) out.identifier("isbn", get("isbn"));
  if (has("asin")) out.identifier("asin", get("asin"));
  if (has("authors")) out.credits("author", get("authors"));
  if (has("narrators")) out.credits("narrator", get("narrators"));
  if (has("series")) {
    out.state("series");
    const series = get("series");
    for (const entry of Array.isArray(series) ? series : []) {
      if (typeof entry !== "string") continue;
      const text = entry.trim();
      const match = SERIES_POSITION.exec(text);
      if (match === null) out.series(text);
      else out.series(text.slice(0, match.index), match[1]);
    }
  }
  if (has("chapters")) out.chapters(readChapters(get("chapters")));
  return out.build();
}

/**
 * Sets or removes one key at the top level, and removes the same key from
 * a legacy nested `metadata` object so the new value is the one read.
 *
 * @param file - The file being rendered.
 * @param key - The key.
 * @param value - The new value; `undefined` removes the key.
 */
function put(file: JsonObject, key: string, value: unknown): void {
  const nested = file["metadata"];
  if (isObject(nested)) delete nested[key];
  if (value === undefined) delete file[key];
  else file[key] = value;
}

/**
 * Writes one series entry the way Audiobookshelf prints it.
 *
 * @param name - Series name.
 * @param position - Position label, if any.
 * @returns `"Name #position"` or `"Name"`.
 */
function seriesText(name: string, position: string | undefined): string {
  const text = position === undefined ? name : `${name} #${position}`;
  const match = SERIES_POSITION.exec(text);
  const read = match === null ? [text, undefined] : [text.slice(0, match.index), match[1]];
  if (read[0] !== name || read[1] !== position) {
    throw invalid(`series \`${text}\` cannot be written to metadata.json so it reads back`);
  }
  return text;
}

/** A JSON number literal, matched at a given index. */
const JSON_NUMBER = /-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?/y;

/**
 * Spells a JSON number literal's exact value one way: sign, significant
 * digits and a power of ten, so two spellings of one value compare equal
 * (`1.0` and `1`, `1e2` and `100`). Zero has no sign, as JSON compares it.
 *
 * @param token - A JSON number literal.
 * @returns The canonical spelling, such as `-12e3` or `0`.
 */
export function canonicalNumber(token: string): string {
  const match = /^(-?)(\d+)(?:\.(\d+))?(?:[eE]([+-]?\d+))?$/.exec(token);
  if (match === null) return token;
  const [, sign = "", whole = "", fraction = "", power = "0"] = match;
  let digits = (whole + fraction).replace(/^0+/, "");
  if (digits === "") return "0";
  // The exponent may be any length in the file; BigInt keeps it exact.
  let exponent = BigInt(power) - BigInt(fraction.length);
  const trailing = /0+$/.exec(digits)?.[0].length ?? 0;
  digits = digits.slice(0, digits.length - trailing);
  exponent += BigInt(trailing);
  return `${sign}${digits}e${exponent}`;
}

/**
 * Checks that every number in a JSON text survives a parse and re-write
 * with its exact value. A number a double cannot hold (an integer beyond
 * 2^53, `9007199254740993e0`, `1e-400`, more digits than a double keeps)
 * would be changed in keys this dialect does not own, so the file is
 * refused instead.
 *
 * @param text - The file's text.
 * @throws {DialectFailure} For a number that cannot be kept exactly.
 */
function checkNumbersKept(text: string): void {
  let inString = false;
  for (let i = 0; i < text.length; i += 1) {
    const char = text[i];
    if (inString) {
      if (char === "\\") i += 1;
      else if (char === '"') inString = false;
      continue;
    }
    if (char === '"') {
      inString = true;
      continue;
    }
    if (char !== "-" && (char === undefined || char < "0" || char > "9")) continue;
    JSON_NUMBER.lastIndex = i;
    const token = JSON_NUMBER.exec(text)?.[0] ?? char;
    const value = Number(token);
    if (
      !Number.isFinite(value) ||
      canonicalNumber(JSON.stringify(value)) !== canonicalNumber(token)
    ) {
      throw invalid(
        `metadata.json holds the number ${token.slice(0, 40)}, which cannot be kept exactly`,
      );
    }
    i += token.length - 1;
  }
}

/**
 * Writes chapters as Audiobookshelf stores them; a chapter without a length
 * ends where the next one starts (the last one at its own start).
 *
 * @param chapters - The record's chapters.
 * @returns Audiobookshelf chapter objects.
 */
function writeChapters(chapters: MetadataChapter[]): JsonObject[] {
  return chapters.map((chapter, index) => {
    const end =
      chapter.lengthMs !== undefined
        ? chapter.startMs + chapter.lengthMs
        : (chapters[index + 1]?.startMs ?? chapter.startMs);
    return { id: index, start: chapter.startMs / 1000, end: end / 1000, title: chapter.title };
  });
}

/**
 * Renders an Audiobookshelf `metadata.json`.
 *
 * @param existing - The file as it is now; empty to create it.
 * @param record - Values to write.
 * @param fields - Fields to set; an empty value removes the field's keys.
 * @returns The new file.
 * @throws {DialectFailure} When `existing` is not UTF-8 or not a JSON
 *   object, or the result is over the size cap.
 */
export function renderAbsJson(
  existing: Uint8Array,
  record: MetadataFileRecord,
  fields: readonly MetadataField[],
): Uint8Array {
  const file: JsonObject = existing.length === 0 ? {} : parseObject(existing, "metadata.json");
  if (existing.length > 0) checkNumbersKept(decodeUtf8Strict(existing, "metadata.json"));
  for (const field of fields) {
    switch (field) {
      case "title":
      case "subtitle":
      case "description":
      case "publisher":
      case "language":
        put(file, field, record[field]);
        break;
      case "publishedAt": {
        const date = record.publishedAt;
        put(file, "publishedYear", date?.slice(0, 4));
        put(file, "publishedDate", date !== undefined && date.length > 4 ? date : undefined);
        break;
      }
      case "genres":
      case "tags":
        put(file, field, record[field].length === 0 ? undefined : [...record[field]]);
        break;
      case "abridgement":
        put(
          file,
          "abridged",
          record.abridgement === "unknown" ? undefined : record.abridgement === "abridged",
        );
        break;
      case "identifiers":
        put(file, "isbn", firstId(record, "isbn"));
        put(file, "asin", firstId(record, "asin"));
        break;
      case "credits":
        // A role the record does not state is unknown, so its key stays.
        if (record.creditRoles.includes("author")) put(file, "authors", namesIn(record, "author"));
        if (record.creditRoles.includes("narrator")) {
          put(file, "narrators", namesIn(record, "narrator"));
        }
        break;
      case "series":
        put(
          file,
          "series",
          record.seriesEntries.length === 0
            ? undefined
            : record.seriesEntries.map((s) => seriesText(s.name, s.position)),
        );
        break;
      case "chapters":
        put(
          file,
          "chapters",
          record.chapters.length === 0 ? undefined : writeChapters(record.chapters),
        );
        break;
      case "expressionKind":
      case "duration":
        // Not carried by this format; the manifest keeps the host from asking.
        break;
    }
  }
  return encodeFile(`${JSON.stringify(file, null, 2)}\n`);
}
