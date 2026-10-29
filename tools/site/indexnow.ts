// Decides which of the site's URLs to submit to IndexNow
// (https://www.indexnow.org), sans-IO. indexnow-run.ts does the fetching and
// posting from the indexnow workflow; this module only reads what it fetched
// (the live sitemaps and the previous run's snapshot, both bytes this code
// did not produce) and plans the submission. The readers are fuzzed in
// fuzz.test.ts (fuzz/indexnow_sitemap/, fuzz/indexnow_snapshot/).
//
// A URL is submitted when it is new, when its fingerprint (the sitemap's
// `<lastmod>`, else a hash of the page) changed, or when it left the
// sitemaps, and at most once per UTC day: a later change the same day waits
// for the next day's run.

import { parseIsoDate } from "./last-updated.ts";
import type { IsoDate } from "./last-updated.ts";

/** IndexNow's shared endpoint; participating engines share what it receives. */
export const INDEXNOW_ENDPOINT = "https://api.indexnow.org/indexnow";

/**
 * Most entries one sitemap may list, per the protocol (MAX_SITEMAP_URLS in
 * sitemap.ts, which this module does not import: indexnow-run.ts runs
 * without the site's npm packages, which sitemap.ts's imports need).
 */
export const MAX_SITEMAP_ENTRIES = 50_000;

/** Most URLs one IndexNow request may carry, per the protocol. */
export const MAX_URLS_PER_REQUEST = 10_000;

/** Largest sitemap or snapshot the reader accepts, in bytes (UTF-16 code units). */
export const MAX_DOCUMENT_BYTES = 10 * 1024 * 1024;

/** Longest fingerprint a snapshot may hold: `sha256:` and 64 hex digits fit. */
const MAX_FINGERPRINT_LENGTH = 128;

/** The `version` of the snapshot this module writes and reads. */
const SNAPSHOT_VERSION = 1;

/** An IndexNow key: 8 to 128 ASCII letters, digits and `-`, per the protocol. */
const KEY_RE = /^[A-Za-z0-9-]{8,128}$/;

/** One `<loc>`, with its optional `<lastmod>`, in a `<url>` or `<sitemap>` element. */
const ENTRY_RE =
  /<(url|sitemap)>\s*<loc>([^<]*)<\/loc>\s*(?:<lastmod>([^<]*)<\/lastmod>\s*)?<\/\1>/g;

/** The XML entities escapeHtml (render.ts) writes, and the numeric quote. */
const ENTITIES: Readonly<Record<string, string>> = {
  "&amp;": "&",
  "&lt;": "<",
  "&gt;": ">",
  "&quot;": '"',
  "&#39;": "'",
};

/** An IndexNow key, checked against the protocol's alphabet and length. */
export type IndexNowKey = string & { readonly __indexNowKey: unique symbol };

/** A URL on the site's origin, in the form `new URL` prints it. */
export type SiteUrl = string & { readonly __siteUrl: unique symbol };

/** What one sitemap document lists. */
export interface SitemapDocument {
  /** `index` for a `<sitemapindex>`, `urlset` for a sitemap of pages. */
  kind: "index" | "urlset";
  /** Its entries in document order: URLs of pages, or of sitemaps for an index. */
  entries: SitemapItem[];
}

/** One entry of a sitemap or sitemap index. */
export interface SitemapItem {
  /** The page's or sitemap's URL, on the site's origin. */
  url: SiteUrl;
  /** Its `<lastmod>` when present and a real date, else null. */
  lastmod: IsoDate | null;
}

/** What the previous run recorded about one URL. */
export interface SnapshotEntry {
  /** The fingerprint last submitted (`lastmod:<date>` or `sha256:<hex>`). */
  fp: string;
  /** The UTC day it was last submitted. */
  sent: IsoDate;
}

/** URL → what was last submitted for it. */
export type Snapshot = Map<SiteUrl, SnapshotEntry>;

/** A planned submission. */
export interface Plan {
  /** The URLs to submit, sorted. */
  submit: SiteUrl[];
  /** The snapshot to keep once the submission succeeds. */
  next: Snapshot;
}

/**
 * Checks an IndexNow key.
 *
 * @param text - The candidate, usually the `INDEXNOW_KEY` variable.
 * @returns The key, or null when it is empty or not a valid key.
 */
export function parseKey(text: string): IndexNowKey | null {
  return KEY_RE.test(text) ? (text as IndexNowKey) : null;
}

/**
 * Checks that a string is a URL on the site's origin, written the way
 * `new URL` prints it, with no query, fragment or credentials.
 *
 * @param text - The candidate.
 * @param origin - The site's origin, such as `https://bookclerk.app`.
 * @returns The URL, or null.
 */
export function parseSiteUrl(text: string, origin: string): SiteUrl | null {
  if (!URL.canParse(text)) return null;
  const url = new URL(text);
  if (url.origin !== origin || url.href !== text) return null;
  // An empty `?` or `#` leaves search and hash empty but stays in the text.
  if (text.includes("?") || text.includes("#") || url.username !== "" || url.password !== "")
    return null;
  return text as SiteUrl;
}

/**
 * Decodes the XML entities escapeHtml writes.
 *
 * @param text - Element text.
 * @returns The decoded text, or null when it holds another `&` sequence.
 */
function decodeEntities(text: string): string | null {
  let ok = true;
  const out = text.replace(/&[^;]*;?/g, (entity) => {
    const value = ENTITIES[entity];
    if (value === undefined) ok = false;
    return value ?? "";
  });
  return ok ? out : null;
}

/**
 * Reads a sitemap or sitemap index as the site's Worker writes it
 * (sitemap.ts): each entry is `<loc>` and an optional `<lastmod>`.
 *
 * Entries whose URL is not on `origin`, or whose text holds an unknown
 * entity, are left out; a `<lastmod>` that is not a real date reads as
 * null. Only the first MAX_SITEMAP_ENTRIES entries are read.
 *
 * @param xml - The document.
 * @param origin - The site's origin.
 * @returns The entries, or null when the document is too large or is
 *   neither a `<urlset>` nor a `<sitemapindex>` of its own kind's entries.
 */
export function readSitemap(xml: string, origin: string): SitemapDocument | null {
  if (xml.length > MAX_DOCUMENT_BYTES) return null;
  const isIndex = xml.includes("<sitemapindex");
  const isUrlset = xml.includes("<urlset");
  if (isIndex === isUrlset) return null;
  const kind = isIndex ? "index" : "urlset";
  const element = isIndex ? "sitemap" : "url";
  const entries: SitemapItem[] = [];
  for (const match of xml.matchAll(ENTRY_RE)) {
    if (entries.length >= MAX_SITEMAP_ENTRIES) break;
    const [, name = "", loc = "", lastmod] = match;
    if (name !== element) return null;
    const decoded = decodeEntities(loc);
    const url = decoded === null ? null : parseSiteUrl(decoded, origin);
    if (url === null) continue;
    entries.push({ url, lastmod: lastmod === undefined ? null : parseIsoDate(lastmod) });
  }
  return { kind, entries };
}

/**
 * Reads the snapshot the previous run kept.
 *
 * The document is `{"version": 1, "urls": {"<url>": {"fp": "...", "sent":
 * "YYYY-MM-DD"}}}`. Entries that are not on `origin`, or whose fields are
 * not a short string and a real date, are left out.
 *
 * @param json - The file's text.
 * @param origin - The site's origin.
 * @returns The snapshot, or null when the text is not a version 1 snapshot
 *   (the run then treats every URL as new).
 */
export function readSnapshot(json: string, origin: string): Snapshot | null {
  if (json.length > MAX_DOCUMENT_BYTES) return null;
  let document: unknown;
  try {
    document = JSON.parse(json);
  } catch {
    return null;
  }
  if (typeof document !== "object" || document === null || Array.isArray(document)) return null;
  const top: Record<string, unknown> = Object.assign(Object.create(null), document);
  const urls = top["urls"];
  if (top["version"] !== SNAPSHOT_VERSION) return null;
  if (typeof urls !== "object" || urls === null || Array.isArray(urls)) return null;
  const snapshot: Snapshot = new Map();
  for (const [text, value] of Object.entries(urls)) {
    const url = parseSiteUrl(text, origin);
    if (url === null || typeof value !== "object" || value === null || Array.isArray(value))
      continue;
    const entry: Record<string, unknown> = Object.assign(Object.create(null), value);
    const fp = entry["fp"];
    const sent = typeof entry["sent"] === "string" ? parseIsoDate(entry["sent"]) : null;
    if (typeof fp !== "string" || fp === "" || fp.length > MAX_FINGERPRINT_LENGTH) continue;
    if (sent === null) continue;
    snapshot.set(url, { fp, sent });
  }
  return snapshot;
}

/**
 * Writes a snapshot for readSnapshot.
 *
 * @param snapshot - The snapshot.
 * @returns The JSON text, URLs sorted.
 */
export function writeSnapshot(snapshot: Snapshot): string {
  const urls = Object.fromEntries([...snapshot].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)));
  return `${JSON.stringify({ version: SNAPSHOT_VERSION, urls }, null, 1)}\n`;
}

/**
 * Plans a submission from the previous snapshot and the URLs the sitemaps
 * list now.
 *
 * With no previous snapshot every URL is submitted. Otherwise a URL is
 * submitted when it is new, its fingerprint changed, or it is no longer
 * listed (a removed page), unless it was already submitted `today`: then
 * its old entry is kept, so the next day's run still sees the change. A
 * removed URL is dropped from the snapshot once submitted.
 *
 * @param previous - The previous run's snapshot, or null for none.
 * @param current - Fingerprint by URL, for every URL listed now.
 * @param today - The run's UTC day.
 * @returns What to submit and the snapshot to keep after it.
 */
export function planSubmission(
  previous: Snapshot | null,
  current: ReadonlyMap<SiteUrl, string>,
  today: IsoDate,
): Plan {
  const submit: SiteUrl[] = [];
  const next: Snapshot = new Map();
  for (const [url, fp] of current) {
    const before = previous?.get(url);
    if (before !== undefined && (before.fp === fp || before.sent === today)) {
      next.set(url, before);
    } else {
      submit.push(url);
      next.set(url, { fp, sent: today });
    }
  }
  for (const [url, before] of previous ?? []) {
    if (current.has(url)) continue;
    if (before.sent === today) next.set(url, before);
    else submit.push(url);
  }
  submit.sort();
  return { submit, next };
}

/**
 * Builds the JSON bodies of the IndexNow requests for a submission.
 *
 * @param origin - The site's origin; its host is the request's `host`.
 * @param key - The site's key, served at `<origin>/<key>.txt`.
 * @param urls - The URLs to submit.
 * @returns One body per MAX_URLS_PER_REQUEST URLs, none for no URLs.
 */
export function submissionBodies(
  origin: string,
  key: IndexNowKey,
  urls: readonly SiteUrl[],
): string[] {
  const bodies: string[] = [];
  for (let at = 0; at < urls.length; at += MAX_URLS_PER_REQUEST) {
    bodies.push(
      JSON.stringify({
        host: new URL(origin).host,
        key,
        keyLocation: `${origin}/${key}.txt`,
        urlList: urls.slice(at, at + MAX_URLS_PER_REQUEST),
      }),
    );
  }
  return bodies;
}
