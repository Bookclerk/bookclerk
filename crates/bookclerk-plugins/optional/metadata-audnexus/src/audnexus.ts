/**
 * Audnexus request building and response mapping, without I/O.
 *
 * Audnexus (<https://audnex.us>) serves Audible catalog metadata and chapter
 * layouts by ASIN, with no Audible account. It has no title search, so this
 * provider answers `lookup` and `chapters` only.
 *
 * Every function here is pure: the guest entrypoint and the unit tests drive
 * the same code, and the network calls live in `client.ts`. Responses are
 * untrusted JSON, so each field is checked for its type and range before it
 * is copied, and every list is capped.
 */

import type {
  Abridgement,
  Credit,
  Identifier,
  MetadataChapter,
  MetadataChapters,
  MetadataRecord,
  SeriesEntry,
} from "@bookclerk/plugin-sdk/workerd";

/** Origin every request goes to; `plugin.toml` grants only this host. */
export const ORIGIN = "https://api.audnex.us";

/** Records a lookup returns when the caller sets no `limit`. */
export const DEFAULT_LIMIT = 1;

/** Most records, and so requests, one lookup makes. */
export const MAX_LIMIT = 5;

/** Most authors and most narrators copied into a record's credits. */
export const MAX_PEOPLE = 20;

/** Most genre and tag names copied into `categories`. */
export const MAX_GENRES = 24;

/** Most chapters kept from one layout, nested ones included. */
export const MAX_CHAPTERS = 2000;

/** Deepest chapter nesting followed; deeper chapters are dropped. */
export const MAX_DEPTH = 8;

/** Longest text kept from any one response field, in UTF-16 code units. */
export const MAX_TEXT = 16384;

/** Marketplaces Audnexus serves, by the `region` value it expects. */
export const REGIONS = ["us", "ca", "uk", "au", "fr", "de", "jp", "it", "in", "es"] as const;

/** One Audnexus `region` value. */
export type Region = (typeof REGIONS)[number];

/** A failure the entrypoint turns into a `PluginError` with the same code. */
export class ProviderFailure extends Error {
  /** `PluginErrorCode` wire string (`invalid_params`, `unavailable`, ...). */
  readonly code: "invalid_params" | "unavailable";

  /**
   * Creates a failure.
   *
   * @param code - `PluginErrorCode` wire string the host receives.
   * @param message - Operator-facing explanation; never holds a secret.
   */
  constructor(code: "invalid_params" | "unavailable", message: string) {
    super(message);
    this.name = "ProviderFailure";
    this.code = code;
  }
}

/** One ASIN to ask Audnexus about. */
export interface AsinRequest {
  /** The ASIN, upper-cased. */
  asin: string;
  /** Marketplace to answer for. */
  region: Region;
}

/**
 * Normalises a marketplace to an Audnexus region.
 *
 * `gb` is Audible's `uk`; anything Audnexus does not serve falls back to
 * `us`, as the host's old client did.
 *
 * @param marketplace - Marketplace code as the caller printed it.
 * @returns The region.
 */
export function normalizeRegion(marketplace: string | undefined): Region {
  const m = (marketplace ?? "").trim().toLowerCase();
  if (m === "gb") return "uk";
  return (REGIONS as readonly string[]).includes(m) ? (m as Region) : "us";
}

/**
 * Normalises an ASIN: ten letters or digits, upper-cased.
 *
 * @param raw - ASIN as the caller printed it.
 * @returns The ASIN, or `undefined` when it is not one.
 */
export function normalizeAsin(raw: string): string | undefined {
  const asin = raw.trim().toUpperCase();
  return /^[A-Z0-9]{10}$/.test(asin) ? asin : undefined;
}

/**
 * Clamps a caller's `limit` to `1..=MAX_LIMIT`.
 *
 * @param limit - Requested record count; absent or 0 means the default.
 * @returns The record count to fetch.
 */
export function clampLimit(limit: number | undefined): number {
  if (limit === undefined || !Number.isFinite(limit) || limit <= 0) return DEFAULT_LIMIT;
  return Math.min(Math.floor(limit), MAX_LIMIT);
}

/**
 * The distinct ASINs among `identifiers`, each with its region.
 *
 * An identifier's own marketplace wins over the call's. Other schemes are
 * ignored, as the ABI asks.
 *
 * @param identifiers - Identifiers from the host.
 * @param marketplace - The call's marketplace.
 * @param max - Most ASINs to return.
 * @returns ASIN requests in the caller's order.
 * @throws {ProviderFailure} `invalid_params` when `identifiers` is empty.
 */
export function asinRequests(
  identifiers: readonly Identifier[],
  marketplace: string | undefined,
  max: number,
): AsinRequest[] {
  if (identifiers.length === 0) {
    throw new ProviderFailure("invalid_params", "at least one identifier is needed");
  }
  const out: AsinRequest[] = [];
  for (const id of identifiers) {
    if (id.scheme !== "asin") continue;
    const asin = normalizeAsin(id.value);
    if (asin === undefined) continue;
    const region = normalizeRegion(id.marketplace ?? marketplace);
    if (out.some((r) => r.asin === asin && r.region === region)) continue;
    out.push({ asin, region });
    if (out.length >= max) break;
  }
  return out;
}

/**
 * URL of a book's metadata.
 *
 * @param request - ASIN and region.
 * @returns The URL.
 */
export function bookUrl(request: AsinRequest): string {
  return `${ORIGIN}/books/${request.asin}?region=${request.region}`;
}

/**
 * URL of a book's chapter layout.
 *
 * @param request - ASIN and region.
 * @returns The URL.
 */
export function chaptersUrl(request: AsinRequest): string {
  return `${ORIGIN}/books/${request.asin}/chapters?region=${request.region}`;
}

/**
 * Returns `value` when it is a JSON object.
 *
 * @param value - Untrusted JSON value.
 * @returns The object, or `undefined`.
 */
function object(value: unknown): Record<string, unknown> | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : undefined;
}

/** A UTF-16 surrogate with no partner: a high one not followed by a low one, or a low one not preceded by a high one. */
const LONE_SURROGATE = /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/g;

/**
 * Returns `value` when it is a non-blank string, trimmed, well-formed and
 * capped.
 *
 * A lone surrogate becomes U+FFFD, and the cap at `MAX_TEXT` UTF-16 code
 * units never splits a surrogate pair, so the text always encodes to UTF-8
 * unchanged.
 *
 * @param value - Untrusted JSON value.
 * @returns The text, or `undefined`.
 */
function text(value: unknown): string | undefined {
  if (typeof value !== "string") return undefined;
  const trimmed = value.replace(LONE_SURROGATE, "\uFFFD").trim();
  if (trimmed === "") return undefined;
  if (trimmed.length <= MAX_TEXT) return trimmed;
  const cut = trimmed.slice(0, MAX_TEXT);
  const last = cut.charCodeAt(MAX_TEXT - 1);
  return last >= 0xd800 && last <= 0xdbff ? cut.slice(0, -1) : cut;
}

/**
 * Returns `value` as a whole number in `0..=max`, accepting a numeric string.
 *
 * @param value - Untrusted JSON value.
 * @param max - Highest accepted value.
 * @returns The number, or `undefined`.
 */
function whole(value: unknown, max: number): number | undefined {
  const n = typeof value === "string" && /^\d+$/.test(value.trim()) ? Number(value) : value;
  return typeof n === "number" && Number.isInteger(n) && n >= 0 && n <= max ? n : undefined;
}

/**
 * Names of the people in an Audnexus people array, deduplicated and capped.
 *
 * @param value - Untrusted `authors` or `narrators` value.
 * @returns Names in billing order.
 */
function people(value: unknown): string[] {
  if (!Array.isArray(value)) return [];
  const out: string[] = [];
  for (const entry of value) {
    const name = text(object(entry)?.name);
    if (name !== undefined && !out.includes(name)) {
      out.push(name);
      if (out.length >= MAX_PEOPLE) break;
    }
  }
  return out;
}

/**
 * Joins names with `; `, keeping only the leading names that fit in
 * `MAX_TEXT` code units together.
 *
 * @param names - Names, each already within `MAX_TEXT`.
 * @returns The joined names, or `undefined` when there are none.
 */
function joinCapped(names: readonly string[]): string | undefined {
  let out = "";
  for (const name of names) {
    const next = out === "" ? name : `${out}; ${name}`;
    if (next.length > MAX_TEXT) break;
    out = next;
  }
  return out === "" ? undefined : out;
}

/**
 * Genre and tag names, genres first, deduplicated and capped.
 *
 * @param value - Untrusted `genres` value.
 * @returns The names joined with `; `, or `undefined` when there are none.
 */
function genres(value: unknown): string | undefined {
  if (!Array.isArray(value)) return undefined;
  const ranked: [number, string][] = [];
  for (const entry of value) {
    const e = object(entry);
    const name = text(e?.name);
    const kind = typeof e?.type === "string" ? e.type.toLowerCase() : "genre";
    if (name === undefined || (kind !== "genre" && kind !== "tag")) continue;
    if (!ranked.some(([, n]) => n === name)) ranked.push([kind === "genre" ? 0 : 1, name]);
  }
  const names = ranked
    .sort((a, b) => a[0] - b[0])
    .slice(0, MAX_GENRES)
    .map(([, n]) => n);
  return joinCapped(names);
}

/**
 * One series entry from a `seriesPrimary` / `seriesSecondary` object.
 *
 * @param value - Untrusted series value.
 * @returns The entry, or `undefined` when it has no name.
 */
function series(value: unknown): SeriesEntry | undefined {
  const s = object(value);
  const name = text(s?.name);
  if (s === undefined || name === undefined) return undefined;
  const entry: SeriesEntry = { name };
  const position =
    text(s.position) ?? (typeof s.position === "number" ? String(s.position) : undefined);
  if (position !== undefined) entry.position = position;
  const asin = typeof s.asin === "string" ? normalizeAsin(s.asin) : undefined;
  if (asin !== undefined) entry.storeSeriesId = asin;
  return entry;
}

/**
 * Maps Audnexus `formatType` to an abridgement.
 *
 * @param value - Untrusted `formatType` value.
 * @returns The abridgement; `unknown` for anything else.
 */
function abridgement(value: unknown): Abridgement {
  const v = typeof value === "string" ? value.trim().toLowerCase() : "";
  if (v === "unabridged") return "unabridged";
  if (v === "abridged") return "abridged";
  return "unknown";
}

/**
 * Maps an Audnexus book to a record.
 *
 * @param body - Parsed `/books/{asin}` body.
 * @param request - The ASIN and region asked for.
 * @returns The record, or `undefined` when the body has no ASIN or title
 *   (Audnexus answers that way for an ASIN it does not know).
 */
export function recordFromBook(body: unknown, request: AsinRequest): MetadataRecord | undefined {
  const b = object(body);
  const asin = typeof b?.asin === "string" ? normalizeAsin(b.asin) : undefined;
  const title = text(b?.title);
  if (b === undefined || asin === undefined || title === undefined) return undefined;

  const identifiers: Identifier[] = [{ scheme: "asin", value: asin, marketplace: request.region }];
  const isbn = text(b.isbn) ?? (typeof b.isbn === "number" ? String(b.isbn) : undefined);
  if (isbn !== undefined && /^(\d{9}[\dXx]|\d{13})$/.test(isbn)) {
    identifiers.push({ scheme: "isbn", value: isbn.toUpperCase() });
  }
  const credits: Credit[] = [
    ...people(b.authors).map((name): Credit => ({ name, role: "author", level: "roleDefault" })),
    ...people(b.narrators).map((name): Credit => ({
      name,
      role: "narrator",
      level: "roleDefault",
    })),
  ];
  const seriesEntries = [series(b.seriesPrimary), series(b.seriesSecondary)].filter(
    (s): s is SeriesEntry => s !== undefined,
  );

  const record: MetadataRecord = {
    title,
    abridgement: abridgement(b.formatType),
    workKind: "book",
    expressionKind: "narration",
    mediaKind: "audiobook",
    identifiers,
    credits,
    seriesEntries,
  };
  const subtitle = text(b.subtitle);
  if (subtitle !== undefined && subtitle !== title) record.subtitle = subtitle;
  const description = text(b.description) ?? text(b.summary);
  if (description !== undefined) record.description = description;
  const image = text(b.image);
  if (image !== undefined && image.startsWith("https://")) record.coverUrl = image;
  const publisher = text(b.publisherName);
  if (publisher !== undefined) record.publisher = publisher;
  const minutes = whole(b.runtimeLengthMin, 0xffff_ffff);
  if (minutes !== undefined && minutes > 0) record.lengthMinutes = minutes;
  const released = text(b.releaseDate);
  if (released !== undefined && /^\d{4}-\d{2}-\d{2}/.test(released))
    record.publishedAt = released.slice(0, 10);
  const categories = genres(b.genres);
  if (categories !== undefined) record.categories = categories;
  const language = text(b.language);
  if (language !== undefined) record.language = language.toLowerCase();
  const rating =
    typeof b.rating === "string" || typeof b.rating === "number" ? Number(b.rating) : Number.NaN;
  if (Number.isFinite(rating) && rating > 0 && rating <= 5) record.ratingOverall = rating;
  return record;
}

/**
 * Flattens Audnexus chapters, and any nested `chapters`, into pre-order.
 *
 * @param value - Untrusted `chapters` value.
 * @param depth - Nesting depth of `value`'s entries.
 * @param out - Chapters collected so far; appended to.
 */
function flatten(value: unknown, depth: number, out: MetadataChapter[]): void {
  if (!Array.isArray(value) || depth > MAX_DEPTH) return;
  for (const entry of value) {
    if (out.length >= MAX_CHAPTERS) return;
    const c = object(entry);
    const start = whole(c?.startOffsetMs, Number.MAX_SAFE_INTEGER);
    if (c === undefined || start === undefined) continue;
    const chapter: MetadataChapter = { title: text(c.title) ?? "", startMs: start, depth };
    const length = whole(c.lengthMs, Number.MAX_SAFE_INTEGER);
    if (length !== undefined && length > 0) chapter.lengthMs = length;
    out.push(chapter);
    flatten(c.chapters, depth + 1, out);
  }
}

/**
 * Maps an Audnexus chapter layout.
 *
 * @param body - Parsed `/books/{asin}/chapters` body.
 * @returns The layout; `found` is false when the body has no chapters.
 */
export function chaptersFromBody(body: unknown): MetadataChapters {
  const b = object(body);
  const chapters: MetadataChapter[] = [];
  flatten(b?.chapters, 0, chapters);
  if (b === undefined || chapters.length === 0) return notFound();
  const layout: MetadataChapters = { found: true, chapters, accurate: b.isAccurate === true };
  const intro = whole(b.brandIntroDurationMs, Number.MAX_SAFE_INTEGER);
  if (intro !== undefined && intro > 0) layout.brandIntroMs = intro;
  const outro = whole(b.brandOutroDurationMs, Number.MAX_SAFE_INTEGER);
  if (outro !== undefined && outro > 0) layout.brandOutroMs = outro;
  const runtime = whole(b.runtimeLengthMs, Number.MAX_SAFE_INTEGER);
  if (runtime !== undefined && runtime > 0) layout.runtimeMs = runtime;
  return layout;
}

/**
 * The answer for an edition Audnexus has no layout for.
 *
 * @returns An empty, not-found layout.
 */
export function notFound(): MetadataChapters {
  return { found: false, chapters: [], accurate: false };
}
