/**
 * The demo Worker's cache classes: every `Cache-Control` on an answer the
 * Worker makes itself is one of these, the way bookclerkd's
 * cache_policy.rs names the daemon's. Answers proxied from a visitor's
 * bookclerkd keep the daemon's header (HTML gains `no-transform`, see
 * app.ts `withNoTransform`). test/cache-control.test.ts sends a request down
 * each of the Worker's own routes and checks the answer against this table.
 */
export const CACHE_CONTROL = {
  /** robots.txt: crawlers may keep it for an hour. */
  hour: "public, max-age=3600",
  /** The Worker's pages, JSON and redirects: per-visitor, never kept. */
  noStore: "no-store",
} as const;

/** A cache class. */
export type CacheClass = keyof typeof CACHE_CONTROL;

/**
 * Names the class of a `Cache-Control` value.
 *
 * @param value - The header value, or null when the response has none.
 * @returns The class, or null when the value is in none of them.
 */
export function cacheClassOf(value: string | null): CacheClass | null {
  const classes = Object.keys(CACHE_CONTROL) as CacheClass[];
  return classes.find((name) => CACHE_CONTROL[name] === value) ?? null;
}
