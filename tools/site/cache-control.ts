// The site's cache classes: every `Cache-Control` the Worker sends, and every
// one src/_headers sets, is one of these, the way bookclerkd's
// cache_policy.rs names the daemon's. worker.test.ts sends a request down
// each route and checks the answer's header against this table.

/** `Cache-Control` values that never vary, by class. */
export const CACHE_CONTROL = {
  /**
   * Files whose name changes with their contents (the hashed /assets/ and
   * versioned /fonts/ in src/_headers): browsers keep them for a year.
   */
  immutable: "public, max-age=31536000, immutable",
  /**
   * The homepage, whose catalog sample differs per response: kept, but
   * fetched again in full before each use.
   */
  revalidate: "no-cache",
  /** Redirects that may change and answers that must never be kept: errors, a 503 while the docs repository is down. */
  noStore: "no-store",
  /**
   * What Workers static assets send for a file without a `_headers` rule;
   * the Worker passes it on unchanged with the pages it serves from dist/.
   */
  assetDefault: "public, max-age=0, must-revalidate",
} as const;

/** A class whose value never varies. */
export type FixedCacheClass = keyof typeof CACHE_CONTROL;

/**
 * Every class: the fixed ones and `shared`, a docs answer browsers keep
 * briefly and the edge keeps for its version's lifetime.
 */
export type CacheClass = FixedCacheClass | "shared";

/** The `shared` class's value, with its two lifetimes in seconds (no leading zeros). */
const SHARED_RE = /^public, max-age=(0|[1-9]\d*), s-maxage=(0|[1-9]\d*)$/;

/**
 * `Cache-Control` of the `shared` class: browsers keep the answer for
 * `browser` seconds and the edge cache for `edge` seconds.
 *
 * @param browser - Browser lifetime in seconds, a whole number.
 * @param edge - Edge lifetime in seconds, a whole number; 0 keeps it out of
 *   the edge cache.
 * @returns The header value.
 * @throws {RangeError} When a lifetime is negative or not a whole number.
 */
export function shared(browser: number, edge: number): string {
  for (const seconds of [browser, edge]) {
    if (!Number.isSafeInteger(seconds) || seconds < 0) {
      throw new RangeError(`not a lifetime in seconds: ${seconds}`);
    }
  }
  return `public, max-age=${browser}, s-maxage=${edge}`;
}

/**
 * Names the class of a `Cache-Control` value.
 *
 * @param value - The header value, or null when the response has none.
 * @returns The class, or null when the value is in none of them.
 */
export function cacheClassOf(value: string | null): CacheClass | null {
  if (value === null) return null;
  for (const [name, fixed] of Object.entries(CACHE_CONTROL)) {
    if (value === fixed) return name as FixedCacheClass;
  }
  const shared = SHARED_RE.exec(value);
  if (shared === null) return null;
  // The same lifetimes shared() accepts.
  const lifetimes = [shared[1], shared[2]].map(Number);
  return lifetimes.every(Number.isSafeInteger) ? "shared" : null;
}
