/**
 * Where the "Sample" control of a title's details finds audio.
 *
 * A sample is a short preview, not listening: it plays in its own audio
 * element, outside the {@link Player}, so it reports no position, keeps no
 * bookmark and never reaches recommendations.
 *
 * @module
 */

import {
  editionSampleUrl,
  fetchTitleSample,
  type TitleMetaQuery,
  type TitleSample,
  type TitleSampleQuery,
} from "@/lib/api";
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
 * What the sample of a title depends on: the held edition and the
 * identifiers the store lookup uses. When enrichment changes it, the
 * sample is looked up again.
 *
 * @param editionId - The library edition, when the title is held.
 * @param query - The title, as sent to title-meta.
 * @returns A string equal for equal sources.
 */
export function sampleSourceKey(
  editionId: string | null | undefined,
  query: TitleMetaQuery,
): string {
  const editions = (query.store_editions ?? []).map(
    (edition) => `${edition.source}:${edition.product_id}`,
  );
  return JSON.stringify([editionId ?? "", query.asin ?? "", query.isbn ?? "", editions]);
}

/** A sample the details can play, and where it was cut from. */
export type FoundSample = {
  /** Same-origin URL of the audio. */
  url: string;
  /** Cut from the held edition, or a storefront's public sample. */
  kind: "edition" | "store";
};

/** Asks the daemon which sample a title can play (its title-sample route). */
export type SampleLookup = (query: TitleSampleQuery) => Promise<TitleSample>;

/**
 * Reads a title-sample answer: the held edition's sample when one was
 * offered for that edition, else a same-origin store sample, else none.
 * Paths are rebuilt or checked here, never loaded as given.
 *
 * @param answer - The daemon's answer.
 * @param editionId - The edition that was asked about, when any.
 * @returns The sample to offer, or `null`.
 */
export function readSample(answer: TitleSample, editionId?: string | null): FoundSample | null {
  if (editionId && answer.edition_sample_path?.trim()) {
    return { url: editionSampleUrl(editionId), kind: "edition" };
  }
  const path = answer.sample_path?.trim();
  if (path && isStoreSamplePath(path)) return { url: path, kind: "store" };
  return null;
}

/**
 * Finds the sample a title's details can offer, before any control is
 * shown: one cut from the edition this library holds when this node can,
 * else the public sample a storefront that listed the title offers. The
 * stores are asked only when the held edition gives none, so a held book
 * never calls out to a store.
 *
 * @param editionId - The library edition, when the title is held.
 * @param query - The title, as sent to title-meta.
 * @param lookup - Asks the daemon (the title-sample route).
 * @returns The sample, or `null` when none can be played here, so the
 *   details show no sample control.
 * @throws When the lookup itself fails (offline, a timeout, a server error).
 */
export async function findSample(
  editionId: string | null | undefined,
  query: TitleMetaQuery,
  lookup: SampleLookup = fetchTitleSample,
): Promise<FoundSample | null> {
  if (!query.title.trim() && !query.asin?.trim() && !query.isbn?.trim()) return null;
  return readSample(await lookup({ ...query, edition_id: editionId || null }), editionId);
}

/**
 * The storefront sample to fall back on when the held edition's sample
 * could not be played (its audio failed to load or decode).
 *
 * @param query - The title, as sent to title-meta.
 * @param lookup - Asks the daemon (the title-sample route).
 * @returns A store sample, or `null` when no store offers one.
 * @throws When the lookup itself fails.
 */
export async function storeSampleFallback(
  query: TitleMetaQuery,
  lookup: SampleLookup = fetchTitleSample,
): Promise<FoundSample | null> {
  if (!query.title.trim() && !query.asin?.trim() && !query.isbn?.trim()) return null;
  const found = readSample(await lookup({ ...query, edition_id: null }), null);
  return found?.kind === "store" ? found : null;
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
