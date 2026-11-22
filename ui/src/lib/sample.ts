/**
 * Where the "Sample" control of a title's details finds audio.
 *
 * A sample is a short preview, not listening: it plays in its own audio
 * element, outside the {@link Player}, so it reports no position, keeps no
 * bookmark and never reaches recommendations.
 *
 * @module
 */

import { editionSampleUrl, fetchTitleSample, type TitleMetaQuery } from "@/lib/api";
import type { Player } from "@/lib/player";

/** Path prefix of the store samples this daemon serves for storefronts. */
export const STORE_SAMPLE_PREFIX = "/api/samples/";

/**
 * Whether a path from the title-sample answer is one this page may load:
 * a same-origin store-sample path, never an absolute or protocol-relative
 * URL.
 *
 * @param path - `sample_path` as the daemon answered it.
 * @returns True for `/api/samples/<token>` paths only.
 */
export function isStoreSamplePath(path: string): boolean {
  return (
    path.startsWith(STORE_SAMPLE_PREFIX) &&
    path.length > STORE_SAMPLE_PREFIX.length &&
    !/[\s\\?#]/.test(path) &&
    !path.slice(STORE_SAMPLE_PREFIX.length).includes("/")
  );
}

/**
 * The sample sources to try for a title, best first: one cut from the
 * edition this library holds, then the public sample a storefront that
 * listed the title offers. The store is asked only when the first source
 * failed to play, so a held book never calls out to a store.
 *
 * @param editionId - The library edition, when the title is held.
 * @param query - The title, as sent to title-meta.
 * @param lookup - Asks the stores (the daemon's title-sample route).
 * @yields Same-origin URLs, one at a time.
 * @throws When the store lookup itself fails (offline, a timeout, a
 *   server error), so the control can offer to try again; a lookup that
 *   answers "no sample" ends the sources instead.
 */
export async function* sampleSources(
  editionId: string | null | undefined,
  query: TitleMetaQuery,
  lookup: (query: TitleMetaQuery) => Promise<string | null> = fetchTitleSample,
): AsyncGenerator<string, void, undefined> {
  if (editionId) yield editionSampleUrl(editionId);
  if (!query.title.trim()) return;
  const path = await lookup(query);
  if (path && isStoreSamplePath(path)) yield path;
}

/**
 * Pauses the book player before a sample starts, so two recordings never
 * play over each other. Pausing reports the book's own position as usual
 * and records nothing about the sample.
 *
 * @param player - The page's book player.
 */
export function pauseBookFor(player: Pick<Player, "pause" | "state">): void {
  const status = player.state.status;
  if (status === "playing" || status === "loading") player.pause();
}
