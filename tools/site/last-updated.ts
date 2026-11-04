// Reads the docs repository's last-updated.json, which scripts/docs-markdown.py
// writes from this repository's git history: the date each page of docs/
// last changed, as an instant in UTC. The file comes from outside the
// Worker, so it is parsed into a validated date here, sans-IO, and fuzzed in
// fuzz.test.ts against an independent oracle (fuzz/last_updated/).

/** The file at the root of each docs version that dates its pages. */
export const LAST_UPDATED_FILE = "last-updated.json";

/** The `version` of last-updated.json this parser reads. */
const LAST_UPDATED_VERSION = 1;

/**
 * Largest last-updated.json the Worker reads, in bytes. It lists every page,
 * the generated API reference included, at about 60 bytes each, so this
 * leaves room for some 30,000 pages.
 */
export const MAX_LAST_UPDATED_BYTES = 2 * 1024 * 1024;

/** A calendar date as `YYYY-MM-DD` (UTC), checked to exist. */
export type IsoDate = string & { readonly __isoDate: unique symbol };

/** `YYYY-MM-DD`, four-digit year. */
const ISO_DATE_RE = /^([0-9]{4})-([0-9]{2})-([0-9]{2})$/;

/**
 * When a page last changed, as last-updated.json gives it: an instant in UTC
 * to the second (`YYYY-MM-DDTHH:MM:SSZ`), or only a calendar date
 * (`YYYY-MM-DD`, UTC) for pages dated before instants were written. Checked
 * to be a real date and time.
 */
export type PageDate = string & { readonly __pageDate: unique symbol };

/** `THH:MM:SSZ` after a page date's calendar date. */
const UTC_TIME_RE = /^T([0-9]{2}):([0-9]{2}):([0-9]{2})Z$/;

/** Length of the calendar date that starts every page date. */
const ISO_DATE_LENGTH = "YYYY-MM-DD".length;

/** Month abbreviations, January first, as the page shows them. */
const MONTHS = [
  "Jan",
  "Feb",
  "Mar",
  "Apr",
  "May",
  "Jun",
  "Jul",
  "Aug",
  "Sep",
  "Oct",
  "Nov",
  "Dec",
] as const;

/**
 * Checks that a string is a real calendar date written as `YYYY-MM-DD`.
 *
 * @param text - The candidate.
 * @returns The date, or null when it is not one (`2026-02-30`, `2026-1-01`).
 */
export function parseIsoDate(text: string): IsoDate | null {
  const match = ISO_DATE_RE.exec(text);
  if (match === null) return null;
  const year = Number(match[1]);
  const month = Number(match[2]);
  const day = Number(match[3]);
  if (year < 1 || month < 1 || month > 12 || day < 1) return null;
  // Date.UTC treats years 0-99 as 1900-1999, so set the year separately.
  const at = new Date(Date.UTC(2000, month - 1, day));
  at.setUTCFullYear(year);
  if (at.getUTCFullYear() !== year || at.getUTCMonth() !== month - 1 || at.getUTCDate() !== day)
    return null;
  return text as IsoDate;
}

/**
 * Checks that a string is a page date: a real calendar date, optionally
 * followed by a real UTC time to the second.
 *
 * @param text - The candidate.
 * @returns The date, or null when it is not one (`2026-10-27T24:00:00Z`,
 *   `2026-10-27T12:00:00+01:00`, `2026-10-27T12:00Z`).
 */
export function parsePageDate(text: string): PageDate | null {
  if (parseIsoDate(text.slice(0, ISO_DATE_LENGTH)) === null) return null;
  const rest = text.slice(ISO_DATE_LENGTH);
  if (rest === "") return text as PageDate;
  const time = UTC_TIME_RE.exec(rest);
  if (time === null) return null;
  if (Number(time[1]) > 23 || Number(time[2]) > 59 || Number(time[3]) > 59) return null;
  return text as PageDate;
}

/**
 * Tells whether a page date carries a time, so the browser can show it in
 * the reader's own time zone.
 *
 * @param date - The date.
 * @returns True for `YYYY-MM-DDTHH:MM:SSZ`, false for a calendar date alone.
 */
export function hasTime(date: PageDate): boolean {
  return date.length > ISO_DATE_LENGTH;
}

/**
 * The calendar date (UTC) of a page date, as the sitemaps' `<lastmod>`
 * gives it.
 *
 * @param date - The date.
 * @returns Its `YYYY-MM-DD` part.
 */
export function utcDate(date: PageDate): IsoDate {
  return date.slice(0, ISO_DATE_LENGTH) as IsoDate;
}

/**
 * Finds a page's date in last-updated.json.
 *
 * The document is
 * `{"version": 1, "pages": {"<key>": "YYYY-MM-DDTHH:MM:SSZ", ...}}`, where
 * a value may also be a date alone (`YYYY-MM-DD`).
 * Anything else (bad JSON, another version, a missing page, a value that is
 * not a real date) means no date, never an error: the page renders without
 * its "Last updated" line.
 *
 * @param json - The file's text.
 * @param key - The page's doc key (`guide/getting-started`).
 * @returns The date, or null.
 */
export function pageDate(json: string, key: string): PageDate | null {
  const pages = readPages(json);
  if (pages === null || !Object.hasOwn(pages, key)) return null;
  const date = pages[key];
  return typeof date === "string" ? parsePageDate(date) : null;
}

/** One page last-updated.json lists, as the sitemap needs it. */
export interface DatedPage {
  /** The page's doc key (`guide/getting-started`), made of plain path segments. */
  key: string;
  /** When the page last changed. */
  date: PageDate;
}

/**
 * A doc key the sitemap may turn into a URL: path segments of letters,
 * digits, `.`, `_` and `-`, none empty or starting with `.`. The Worker
 * further keeps only keys its docs route serves at that URL.
 */
const DOC_KEY_RE = /^(?:[A-Za-z0-9_-][A-Za-z0-9._-]*\/)*[A-Za-z0-9_-][A-Za-z0-9._-]*$/;

/** Longest doc key the sitemap lists, in characters; real keys stay under 150. */
export const MAX_DOC_KEY_LENGTH = 512;

/**
 * Lists every page last-updated.json dates, for the sitemap.
 *
 * Entries whose key is not a plain docs path (see DOC_KEY_RE) or longer
 * than MAX_DOC_KEY_LENGTH, or whose value is not a real date, are left out;
 * the rest come in JSON object order (integer-like keys first, then the
 * file's order).
 *
 * @param json - The file's text.
 * @returns The pages, or null when the document is not a version 1
 *   last-updated.json (bad JSON, another version, no `pages` object).
 */
export function datedPages(json: string): DatedPage[] | null {
  const pages = readPages(json);
  if (pages === null) return null;
  const out: DatedPage[] = [];
  for (const [key, value] of Object.entries(pages)) {
    if (key.length > MAX_DOC_KEY_LENGTH || !DOC_KEY_RE.test(key) || typeof value !== "string")
      continue;
    const date = parsePageDate(value);
    if (date !== null) out.push({ key, date });
  }
  return out;
}

/**
 * Reads the `pages` object of a version 1 last-updated.json.
 *
 * @param json - The file's text.
 * @returns The object, or null when the text is not such a document.
 */
function readPages(json: string): Record<string, unknown> | null {
  let document: unknown;
  try {
    document = JSON.parse(json);
  } catch {
    return null;
  }
  if (!isRecord(document) || document["version"] !== LAST_UPDATED_VERSION) return null;
  const pages = document["pages"];
  return isRecord(pages) ? pages : null;
}

/**
 * Writes a page's date the way the server renders it, in English and
 * without a locale lookup: `Oct 26, 2026`. It is the date in UTC; site.js
 * replaces it with the date in the reader's own time zone when the page
 * date carries a time.
 *
 * @param date - The date.
 * @returns The text, made only of letters, digits, spaces and a comma.
 */
export function formatDate(date: PageDate): string {
  const [year = "", month = "", day = ""] = utcDate(date).split("-");
  return `${MONTHS[Number(month) - 1] ?? ""} ${Number(day)}, ${year}`;
}

/**
 * Writes a page date's full UTC time for the date's tooltip, which readers
 * without JavaScript see: `Oct 26, 2026, 14:05 UTC`.
 *
 * @param date - The date, which must carry a time (see hasTime).
 * @returns The text, made only of letters, digits, spaces, `,` and `:`.
 */
export function formatUtcTime(date: PageDate): string {
  return `${formatDate(date)}, ${date.slice(ISO_DATE_LENGTH + 1, ISO_DATE_LENGTH + 6)} UTC`;
}

/**
 * Tells whether a parsed JSON value is an object (not an array or null).
 *
 * @param value - The value.
 * @returns True for a JSON object.
 */
function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
