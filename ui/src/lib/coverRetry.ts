/**
 * When a cover that failed to load is tried again.
 *
 * A cover can fail for reasons that pass: the server was busy, the download
 * timed out, or the browser cancelled it. Failure answers are never cached
 * (`Cache-Control: no-store`), so asking again later can succeed. The wait
 * doubles after each failed round and the cover gives up after a few.
 */

/** Rounds of retries a cover gets after its first failed load. */
export const MAX_COVER_RETRIES = 4;

/** Wait before the first retry, in milliseconds. */
export const COVER_RETRY_BASE_MS = 5_000;

/** Longest wait between retries, in milliseconds. */
export const COVER_RETRY_MAX_MS = 60_000;

/**
 * The wait before retry number `retry` (0 for the first), or null when the
 * cover has used up its retries.
 *
 * @param retry - Retries already made since the cover last loaded.
 * @returns Milliseconds to wait, or null to give up.
 */
export function coverRetryDelay(retry: number): number | null {
  if (!Number.isInteger(retry) || retry < 0 || retry >= MAX_COVER_RETRIES) return null;
  return Math.min(COVER_RETRY_BASE_MS * 2 ** retry, COVER_RETRY_MAX_MS);
}
