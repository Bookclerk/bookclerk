/**
 * Hashes that keep Audiobookshelf ids out of the plugin database.
 *
 * The keys are the first 8 bytes of SHA-256 over a domain tag, a NUL and
 * the input, written as 16 lowercase hex digits. The tags are the ones the
 * plugin has always used, so a database written by an earlier release keeps
 * matching the same users.
 */

/** Domain tag of user keys in the user watch (`abs_watch_users`). */
export const WATCH_KEY_TAG = "bookclerk-abs-watch-key-v1";

/** Domain tag of user keys in the listening memory (`person` columns). */
export const LISTENING_KEY_TAG = "bookclerk-abs-listening-key-v1";

/** Domain tag of progress-row fingerprints. */
export const FINGERPRINT_TAG = "bookclerk-abs-listening-v2";

/**
 * SHA-256 of `tag`, a NUL, then `input`, as UTF-8.
 *
 * @param tag - Domain tag.
 * @param input - Text to hash.
 * @returns The 32-byte digest.
 */
async function digest(tag: string, input: string): Promise<Uint8Array> {
  const bytes = new TextEncoder().encode(`${tag}\u0000${input}`);
  return new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
}

/**
 * A key for `input`: 16 hex digits of its tagged hash.
 *
 * @param tag - Domain tag.
 * @param input - Usually an Audiobookshelf user id.
 * @returns The key.
 */
export async function hexKey(tag: string, input: string): Promise<string> {
  const hash = await digest(tag, input);
  return Array.from(hash.subarray(0, 8), (b) => b.toString(16).padStart(2, "0")).join("");
}

/**
 * A signed 64-bit fingerprint of `input`: its tagged hash's first 8 bytes,
 * big-endian.
 *
 * @param tag - Domain tag.
 * @param input - Canonical text of what is fingerprinted.
 * @returns The fingerprint.
 */
export async function fingerprint64(tag: string, input: string): Promise<bigint> {
  const hash = await digest(tag, input);
  return new DataView(hash.buffer, hash.byteOffset, 8).getBigInt64(0, false);
}
