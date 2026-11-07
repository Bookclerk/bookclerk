/**
 * The collector's cache classes: every `Cache-Control` the Worker sends, and
 * every one public/_headers sets, is one of these, the way bookclerkd's
 * cache_policy.rs names the daemon's. worker.test.ts sends a request down
 * each route and checks the answer's header against this table.
 */
export const CACHE_CONTROL = {
  /**
   * Files whose name changes with their contents: the dashboard's styles and
   * script under /static/assets/ (see asset-names.ts) and the versioned fonts
   * under /static/fonts/. Browsers keep them for a year.
   */
  immutable: "public, max-age=31536000, immutable",
  /** robots.txt: crawlers may keep it for an hour. */
  hour: "public, max-age=3600",
  /** JSON, dashboard pages, redirects and errors: per-user or changing, never kept. */
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
