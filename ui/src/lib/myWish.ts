/**
 * Matching the viewer's own wishes to shared request-queue entries.
 *
 * @module
 */
import type { GlobalQueueEntry, TitleRequest } from "@/lib/api";

/**
 * The viewer's own open wish behind a queue entry: the one resolved to the
 * entry's library work, else the one with its key. Wishes for one work share
 * an entry even when their keys differ, so the key alone can miss.
 *
 * @param mine - The viewer's wishes.
 * @param entry - A request-queue entry.
 * @returns The matching wish, or `undefined` when the viewer has none.
 */
export function myWishFor(
  mine: readonly TitleRequest[],
  entry: GlobalQueueEntry,
): TitleRequest | undefined {
  const workId = entry.work_id?.trim();
  return (
    (workId ? mine.find((r) => r.work_id === workId) : undefined) ??
    mine.find((r) => r.work_key && r.work_key === entry.work_key)
  );
}
