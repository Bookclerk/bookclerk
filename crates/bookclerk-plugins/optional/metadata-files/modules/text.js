/**
 * Shared pieces of every dialect: decoding file bytes, cleaning values into
 * the shapes the host accepts, and building a `MetadataFileRecord`.
 *
 * The host refuses a whole record when one value breaks a rule, so a
 * dialect never passes a file's text through unchecked. Every value goes
 * through the cleaners here, which drop control characters, keep labels on
 * one line, cut text at the byte caps on a character boundary and drop what
 * is left blank. A file therefore reads as what it can say, never as an
 * error, unless its whole structure is unreadable.
 */
/** Largest file `parse` reads and `render` returns, in bytes (ABI cap). */
export const MAX_FILE_BYTES = 262144;
/** Longest description, in UTF-8 bytes (ABI cap). */
export const MAX_DESCRIPTION_BYTES = 65536;
/** Longest other text value, in UTF-8 bytes (ABI cap). */
export const MAX_TEXT_BYTES = 1024;
/** Most entries in a list other than chapters (ABI cap). */
export const MAX_LIST_ENTRIES = 256;
/** Most chapters in a record (ABI cap). */
export const MAX_CHAPTERS = 2000;
/** Largest millisecond offset a record carries, so every value stays exact. */
export const MAX_MS = Number.MAX_SAFE_INTEGER;
/**
 * Why a dialect refused a request. The entrypoint turns it into a
 * `PluginError` with the same code; the modules stay free of the SDK so the
 * fuzz tests drive them directly.
 */
export class DialectFailure extends Error {
    /** Wire error code: `invalid_params` for a file or request the dialect cannot use. */
    code;
    /**
     * Builds a failure.
     *
     * @param code - Wire error code.
     * @param message - What was wrong, for logs.
     */
    constructor(code, message) {
        super(message);
        this.code = code;
        this.name = "DialectFailure";
    }
}
/**
 * Builds an `invalid_params` failure.
 *
 * @param message - What was wrong.
 * @returns The failure, for `throw`.
 */
export function invalid(message) {
    return new DialectFailure("invalid_params", message);
}
/** Windows-1252 code points for bytes 0x80-0x9F; 0 marks an unused byte. */
const CP1252_HIGH = [
    0x20ac, 0, 0x201a, 0x0192, 0x201e, 0x2026, 0x2020, 0x2021, 0x02c6, 0x2030, 0x0160, 0x2039, 0x0152,
    0, 0x017d, 0, 0, 0x2018, 0x2019, 0x201c, 0x201d, 0x2022, 0x2013, 0x2014, 0x02dc, 0x2122, 0x0161,
    0x203a, 0x0153, 0, 0x017e, 0x0178,
];
/**
 * Decodes a text file: UTF-8 when the bytes are valid UTF-8 (a byte order
 * mark is dropped), otherwise Windows-1252, the usual encoding of NFO and
 * plain-text sidecars written on Windows.
 *
 * @param bytes - The whole file.
 * @returns The file's text.
 */
export function decodeText(bytes) {
    try {
        return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
    }
    catch {
        let out = "";
        for (const byte of bytes) {
            const high = byte >= 0x80 && byte <= 0x9f ? CP1252_HIGH[byte - 0x80] : undefined;
            out += String.fromCodePoint(high === undefined ? byte : high === 0 ? 0xfffd : high);
        }
        return out;
    }
}
/**
 * Decodes a JSON file, which RFC 8259 requires to be UTF-8; invalid
 * sequences become U+FFFD and a byte order mark is dropped.
 *
 * @param bytes - The whole file.
 * @returns The file's text.
 */
export function decodeUtf8(bytes) {
    return new TextDecoder("utf-8").decode(bytes);
}
/**
 * Encodes rendered text as UTF-8 and checks the ABI cap.
 *
 * @param text - The new file.
 * @returns Its bytes.
 * @throws {DialectFailure} When the file would be larger than the cap.
 */
export function encodeFile(text) {
    const bytes = new TextEncoder().encode(text);
    if (bytes.length > MAX_FILE_BYTES) {
        throw invalid(`rendered file is ${bytes.length} bytes, over the ${MAX_FILE_BYTES} byte cap`);
    }
    return bytes;
}
/** A lone UTF-16 surrogate, which UTF-8 encodes as U+FFFD. */
const LONE_SURROGATE = /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/g;
/** Control characters other than tab and line breaks. */
const CONTROL_KEEP_LINES = /[\u0000-\u0008\u000B\u000C\u000E-\u001F\u007F-\u009F]/g;
/** Every control character, line breaks and tabs included. */
const CONTROL = /[\u0000-\u001F\u007F-\u009F]+/g;
/**
 * Cuts text to at most `max` UTF-8 bytes without splitting a character.
 *
 * @param text - Well-formed text (no lone surrogates).
 * @param max - Byte cap.
 * @returns The longest prefix within the cap.
 */
export function cutBytes(text, max) {
    // Three bytes per UTF-16 unit is the worst case, so short text needs no walk.
    if (text.length * 3 <= max)
        return text;
    let bytes = 0;
    let end = 0;
    for (const char of text) {
        const code = char.codePointAt(0) ?? 0;
        const size = code < 0x80 ? 1 : code < 0x800 ? 2 : code < 0x10000 ? 3 : 4;
        if (bytes + size > max)
            break;
        bytes += size;
        end += char.length;
    }
    return text.slice(0, end);
}
/**
 * Cleans a single-line value: control characters (line breaks included)
 * become one space, runs of white space stay as the file wrote them, the
 * ends are trimmed and the value is cut to the text cap.
 *
 * @param value - Anything a file held.
 * @returns The value, or `undefined` when it is not a string or is blank.
 */
export function line(value) {
    if (typeof value !== "string")
        return undefined;
    const clean = cutBytes(value.replace(LONE_SURROGATE, "�").replace(CONTROL, " ").trim(), MAX_TEXT_BYTES).trim();
    return clean === "" ? undefined : clean;
}
/**
 * Cleans a chapter title, which may be empty.
 *
 * @param value - Anything a file held.
 * @returns The cleaned title, `""` when there is none.
 */
export function chapterTitle(value) {
    return line(value) ?? "";
}
/**
 * Cleans a description: line breaks become `\n`, other control characters
 * are dropped, the ends are trimmed and the text is cut to the description
 * cap.
 *
 * @param value - Anything a file held.
 * @returns The description, or `undefined` when it is not a string or is
 *   blank.
 */
export function longText(value) {
    if (typeof value !== "string")
        return undefined;
    const clean = cutBytes(value
        .replace(LONE_SURROGATE, "�")
        .replace(/\r\n?/g, "\n")
        .replace(CONTROL_KEEP_LINES, "")
        .trim(), MAX_DESCRIPTION_BYTES).trim();
    return clean === "" ? undefined : clean;
}
/**
 * Cleans a list of labels: each entry through {@link line}, blanks and
 * repeats dropped, at most the list cap kept.
 *
 * @param values - Anything a file held; a non-array reads as empty.
 * @returns The labels in file order.
 */
export function labels(values) {
    if (!Array.isArray(values))
        return [];
    const out = [];
    const seen = new Set();
    for (const value of values) {
        const clean = line(value);
        if (clean === undefined || seen.has(clean))
            continue;
        seen.add(clean);
        out.push(clean);
        if (out.length === MAX_LIST_ENTRIES)
            break;
    }
    return out;
}
/** Days in each month of a common year. */
const MONTH_DAYS = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
/**
 * Whether `date` is `YYYY`, `YYYY-MM` or `YYYY-MM-DD` and a real date,
 * the rule the host applies to `publishedAt`.
 *
 * @param date - Candidate date.
 * @returns True when the host accepts it.
 */
export function isPartialDate(date) {
    const match = /^(\d{4})(?:-(\d{2})(?:-(\d{2}))?)?$/.exec(date);
    if (match === null)
        return false;
    const [, y, m, d] = match;
    if (m === undefined)
        return true;
    const month = Number(m);
    if (month < 1 || month > 12)
        return false;
    if (d === undefined)
        return true;
    const year = Number(y);
    const leap = (year % 4 === 0 && year % 100 !== 0) || year % 400 === 0;
    const last = month === 2 && leap ? 29 : (MONTH_DAYS[month - 1] ?? 0);
    const day = Number(d);
    return day >= 1 && day <= last;
}
/**
 * Reads a publication date the way people write it into sidecars:
 * `YYYY-MM-DD` (or a prefix of it), an ISO timestamp, US `MM-DD-YYYY` or
 * `MM/DD/YYYY` as Audible prints release dates, and otherwise the last
 * four-digit year in the text.
 *
 * @param value - Anything a file held.
 * @returns A date the host accepts, or `undefined`.
 */
export function partialDate(value) {
    if (typeof value !== "string")
        return undefined;
    const text = value.trim();
    const iso = /^(\d{4}(?:-\d{2}(?:-\d{2})?)?)(?:T[\d:.]+(?:Z|[+-]\d{2}:?\d{2})?)?$/.exec(text);
    if (iso?.[1] !== undefined && isPartialDate(iso[1]))
        return iso[1];
    const us = /^(\d{1,2})[-/](\d{1,2})[-/](\d{4})$/.exec(text);
    if (us !== null) {
        const date = `${us[3]}-${us[1]?.padStart(2, "0")}-${us[2]?.padStart(2, "0")}`;
        if (isPartialDate(date))
            return date;
    }
    const years = text.match(/\d{4}/g);
    const year = years?.[years.length - 1];
    return year !== undefined && isPartialDate(year) ? year : undefined;
}
/**
 * Converts a duration in seconds to whole milliseconds.
 *
 * @param seconds - Anything a file held.
 * @returns Milliseconds, or `undefined` for a value that is not a finite,
 *   non-negative number within range.
 */
export function secondsToMs(seconds) {
    if (typeof seconds !== "number" || !Number.isFinite(seconds) || seconds < 0)
        return undefined;
    const ms = Math.round(seconds * 1000);
    return ms <= MAX_MS ? ms : undefined;
}
/**
 * Checks a millisecond count from a file.
 *
 * @param ms - Anything a file held.
 * @returns Whole milliseconds, or `undefined` for a value that is not a
 *   finite, non-negative number within range.
 */
export function wholeMs(ms) {
    if (typeof ms !== "number" || !Number.isFinite(ms) || ms < 0)
        return undefined;
    const whole = Math.round(ms);
    return whole <= MAX_MS ? whole : undefined;
}
/**
 * Puts chapters in the order the host requires: by start time (a stable
 * sort keeps the file's order for equal starts), the first at depth 0 and
 * each at most one level deeper than the one before; at most the chapter
 * cap.
 *
 * @param chapters - Chapters as the file lists them.
 * @returns Chapters the host accepts.
 */
export function orderChapters(chapters) {
    const sorted = chapters
        .map((chapter, index) => ({ chapter, index }))
        .sort((a, b) => a.chapter.startMs - b.chapter.startMs || a.index - b.index)
        .slice(0, MAX_CHAPTERS)
        .map(({ chapter }) => chapter);
    let depth = -1;
    return sorted.map((chapter) => {
        depth = Math.min(chapter.depth, depth + 1);
        return { ...chapter, depth };
    });
}
/**
 * A record being filled in by a parser. Setting a value marks its field as
 * present; {@link RecordBuilder.state} marks a field the file states
 * without a usable value.
 */
export class RecordBuilder {
    /** The record so far. */
    record = {
        present: [],
        genres: [],
        tags: [],
        abridgement: "unknown",
        expressionKind: "unspecified",
        identifiers: [],
        credits: [],
        creditRoles: [],
        seriesEntries: [],
        chapters: [],
    };
    /**
     * Marks a field the file states, with or without a value.
     *
     * @param field - The field.
     * @returns This builder.
     */
    state(field) {
        if (!this.record.present.includes(field))
            this.record.present.push(field);
        return this;
    }
    /**
     * Sets a single-line text field from a file value; a blank value still
     * marks the field as stated.
     *
     * @param field - `title`, `subtitle`, `publisher` or `language`.
     * @param value - Anything the file held.
     * @returns This builder.
     */
    text(field, value) {
        this.state(field);
        const clean = line(value);
        if (clean === undefined)
            delete this.record[field];
        else
            this.record[field] = clean;
        return this;
    }
    /**
     * Sets the description from a file value.
     *
     * @param value - Anything the file held.
     * @returns This builder.
     */
    description(value) {
        this.state("description");
        const clean = longText(value);
        if (clean === undefined)
            delete this.record.description;
        else
            this.record.description = clean;
        return this;
    }
    /**
     * Sets the publication date from a file value.
     *
     * @param value - Anything the file held.
     * @returns This builder.
     */
    publishedAt(value) {
        this.state("publishedAt");
        const date = partialDate(value);
        if (date === undefined)
            delete this.record.publishedAt;
        else
            this.record.publishedAt = date;
        return this;
    }
    /**
     * Sets genres or tags.
     *
     * @param field - `genres` or `tags`.
     * @param values - Labels in file order, cleaned here.
     * @returns This builder.
     */
    labels(field, values) {
        this.state(field);
        this.record[field] = labels(values);
        return this;
    }
    /**
     * Sets the abridgement.
     *
     * @param abridged - True for abridged, false for unabridged, `undefined`
     *   when the file states the field without a usable value.
     * @returns This builder.
     */
    abridged(abridged) {
        this.state("abridgement");
        this.record.abridgement =
            abridged === undefined ? "unknown" : abridged ? "abridged" : "unabridged";
        return this;
    }
    /**
     * Adds one identifier; a blank value only marks the field as stated.
     *
     * @param scheme - `isbn` or `asin`.
     * @param value - Anything the file held.
     * @returns This builder.
     */
    identifier(scheme, value) {
        this.state("identifiers");
        const clean = line(value);
        const ids = this.record.identifiers;
        if (clean !== undefined &&
            ids.length < MAX_LIST_ENTRIES &&
            !ids.some((id) => id.scheme === scheme && id.value === clean)) {
            ids.push({ scheme, value: clean });
        }
        return this;
    }
    /**
     * States one credit role and adds its contributors. A role whose names
     * all fall past the list cap stays unstated (unknown).
     *
     * @param role - The role the file states.
     * @param names - Contributor names in billing order, cleaned here.
     * @returns This builder.
     */
    credits(role, names) {
        const credits = this.record.credits;
        const wanted = labels(names);
        // A role with names is stated only when at least one of them fits under
        // the list cap; otherwise it would read as "nobody in this role".
        let kept = wanted.length === 0;
        for (const name of wanted) {
            if (credits.some((c) => c.role === role && c.name === name)) {
                kept = true;
                continue;
            }
            if (credits.length === MAX_LIST_ENTRIES)
                break;
            const credit = { name, role, level: "roleDefault" };
            credits.push(credit);
            kept = true;
        }
        if (!kept)
            return this;
        this.state("credits");
        if (!this.record.creditRoles.includes(role))
            this.record.creditRoles.push(role);
        return this;
    }
    /**
     * Adds one series entry; a blank name only marks the field as stated.
     *
     * @param name - Series name as the file prints it.
     * @param position - Position label, if any.
     * @param storeSeriesId - Store series id, if any.
     * @returns This builder.
     */
    series(name, position, storeSeriesId) {
        this.state("series");
        const clean = line(name);
        const entries = this.record.seriesEntries;
        if (clean === undefined || entries.length === MAX_LIST_ENTRIES)
            return this;
        if (entries.some((entry) => entry.name === clean))
            return this;
        const pos = line(position);
        const id = line(storeSeriesId);
        entries.push({
            name: clean,
            ...(pos === undefined ? {} : { position: pos }),
            ...(id === undefined ? {} : { storeSeriesId: id }),
        });
        return this;
    }
    /**
     * Sets the chapter list, put in the order the host requires.
     *
     * @param chapters - Chapters as the file lists them.
     * @returns This builder.
     */
    chapters(chapters) {
        this.state("chapters");
        this.record.chapters = orderChapters(chapters);
        return this;
    }
    /**
     * Sets the running time.
     *
     * @param ms - Milliseconds, or `undefined` when the file states the field
     *   without a usable value.
     * @returns This builder.
     */
    duration(ms) {
        this.state("duration");
        if (ms === undefined)
            delete this.record.durationMs;
        else
            this.record.durationMs = ms;
        return this;
    }
    /**
     * The finished record.
     *
     * @returns The record; the builder should not be used afterwards.
     */
    build() {
        return this.record;
    }
}
