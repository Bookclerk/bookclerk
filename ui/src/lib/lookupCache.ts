/**
 * Per-title answers to Discover's batched lookups, kept for the tab.
 *
 * Prices and details travel in batch `POST`s, which the browser's HTTP cache
 * never stores, and the daemon marks API JSON `no-store` because it is per
 * viewer. So the browser keeps its own cache here, one entry **per title**
 * rather than per batch: a page of cards looks each title up first and only
 * the titles missing go into the batch, so a reload or a second visit to
 * Discover sends nothing for what it already has, and a batch never repeats
 * a title just because its neighbours changed.
 *
 * Entries live in memory and are copied to `sessionStorage`, so they last as
 * long as the tab (a reload included) and never reach another tab or
 * outlive the browser session. Signing out clears them, since what a viewer
 * pays depends on whose store accounts are linked. Storage that is missing
 * or full only makes the cache memory-only.
 *
 * @module
 */

/** One cached answer and when it stops counting. */
type Entry<T> = { value: T; expires: number };

/** Every cache made here, so {@link clearLookupCaches} can reach them. */
const caches: Array<LookupCache<unknown>> = [];

/** Prefix of every `sessionStorage` key this module writes. */
const STORAGE_PREFIX = "bookclerk.lookup.";

/** A per-title cache with a fixed lifetime and size. */
export interface LookupCache<T> {
  /**
   * The cached answer for `key`, or `undefined` when none is fresh.
   *
   * @param key - The title's lookup key.
   */
  get(key: string): T | undefined;
  /**
   * Keeps `value` for `key` for the cache's lifetime.
   *
   * @param key - The title's lookup key.
   * @param value - The answer.
   */
  set(key: string, value: T): void;
  /** Forgets every entry, in memory and in storage. */
  clear(): void;
}

/**
 * Reads a cache's saved entries, dropping expired ones.
 *
 * @param storageKey - Its `sessionStorage` key.
 * @returns The entries still fresh.
 */
function load<T>(storageKey: string): Map<string, Entry<T>> {
  const out = new Map<string, Entry<T>>();
  try {
    const raw = sessionStorage.getItem(storageKey);
    if (!raw) return out;
    const now = Date.now();
    const rows = JSON.parse(raw) as Array<[string, Entry<T>]>;
    if (!Array.isArray(rows)) return out;
    for (const row of rows) {
      if (!Array.isArray(row) || typeof row[0] !== "string") continue;
      const entry = row[1];
      if (entry && typeof entry.expires === "number" && entry.expires > now) {
        out.set(row[0], entry);
      }
    }
  } catch {
    // Unavailable or unreadable storage starts the cache empty.
  }
  return out;
}

/**
 * Creates a per-title cache.
 *
 * @param name - Short name, part of its storage key.
 * @param ttlMs - How long an answer counts, in milliseconds.
 * @param maxEntries - Most entries kept; the oldest go first.
 * @returns The cache.
 */
export function createLookupCache<T>(
  name: string,
  ttlMs: number,
  maxEntries: number,
): LookupCache<T> {
  const storageKey = `${STORAGE_PREFIX}${name}`;
  const entries = load<T>(storageKey);
  let saveTimer: ReturnType<typeof setTimeout> | null = null;

  const save = () => {
    saveTimer = null;
    try {
      sessionStorage.setItem(storageKey, JSON.stringify([...entries]));
    } catch {
      // Full or unavailable storage leaves the cache in memory only.
    }
  };
  // A page of cards answers together; write once after the burst.
  const scheduleSave = () => {
    if (saveTimer == null) saveTimer = setTimeout(save, 250);
  };

  const cache: LookupCache<T> = {
    get(key) {
      const hit = entries.get(key);
      if (!hit) return undefined;
      if (hit.expires <= Date.now()) {
        entries.delete(key);
        scheduleSave();
        return undefined;
      }
      return hit.value;
    },
    set(key, value) {
      entries.delete(key);
      entries.set(key, { value, expires: Date.now() + ttlMs });
      // Maps iterate in insertion order, so the first keys are the oldest.
      for (const old of entries.keys()) {
        if (entries.size <= maxEntries) break;
        entries.delete(old);
      }
      scheduleSave();
    },
    clear() {
      entries.clear();
      if (saveTimer != null) clearTimeout(saveTimer);
      saveTimer = null;
      try {
        sessionStorage.removeItem(storageKey);
      } catch {
        // Nothing saved to remove.
      }
    },
  };
  caches.push(cache as LookupCache<unknown>);
  return cache;
}

/** Bumped by {@link clearLookupCaches}: which viewer a lookup was asked for. */
let generation = 0;

/**
 * The current viewer generation. A lookup records it when asked and keeps
 * its answer only if it is unchanged when the answer arrives, so a lookup
 * still running at sign-out never fills the next viewer's cache.
 *
 * @returns The generation number.
 */
export function lookupGeneration(): number {
  return generation;
}

/** Forgets every lookup cache and starts a new viewer generation, as signing out does. */
export function clearLookupCaches(): void {
  generation += 1;
  for (const cache of caches) cache.clear();
}
