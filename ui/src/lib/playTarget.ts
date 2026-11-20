/**
 * Choosing what the web player plays: which of a work's editions, and
 * which works belong on the Continue listening shelf.
 *
 * @module
 */
import type { Edition, WorkDetail, WorkSummary } from "@/lib/api";
import type { PlaybackProgress, ProgressPage } from "@/lib/playback";
import type { PlayTarget } from "@/lib/player";

/**
 * Whether a library row offers a play button.
 *
 * A row knows only the work's summed state, so it offers play for a work
 * with audio whose editions are acquired; the detail dialog decides per
 * edition.
 *
 * @param work - A works-page row.
 * @returns True when the row should show play.
 */
export function rowOffersPlay(work: WorkSummary): boolean {
  return work.acquire_status === "acquired" && work.media_kinds.includes("audio");
}

/**
 * The edition of a work the player starts: the acquired audio edition the
 * listener was last in, else the first acquired audio edition.
 *
 * @param work - A work's detail.
 * @param recent - The listener's progress rows, most recent first.
 * @returns The edition, or `null` when no audio edition is acquired.
 */
export function playableEdition(
  work: WorkDetail,
  recent: readonly PlaybackProgress[] = [],
): Edition | null {
  const audio = work.editions.filter(
    (edition) => edition.media_kind === "audio" && edition.acquire_status === "acquired",
  );
  for (const row of recent) {
    const match = audio.find((edition) => edition.id === row.edition_id);
    if (match) return match;
  }
  return audio[0] ?? null;
}

/**
 * What the player shows for an edition of a work.
 *
 * @param work - The work's detail.
 * @param edition - The edition to play.
 * @returns The play target.
 */
export function targetFor(work: WorkDetail, edition: Edition): PlayTarget {
  return {
    editionId: edition.id,
    workId: work.id,
    title: work.title || edition.title,
    authors: work.authors.length > 0 ? work.authors.join(", ") : null,
    coverPath: edition.cover_path,
  };
}

/** One book on the Continue listening shelf. */
export interface ShelfItem {
  /** What pressing it plays. */
  target: PlayTarget;
  /** The listener's progress in it. */
  progress: PlaybackProgress;
}

/** Most books the Continue listening shelf shows. */
export const SHELF_SIZE = 6;

/**
 * Reads the Continue listening shelf: the unfinished editions the listener
 * was most recently in, with the titles and covers of their works.
 *
 * A row whose work cannot be read (removed, or out of the listener's
 * scope since) is left off instead of failing the shelf.
 *
 * @param fetchProgress - Reads a progress page (`fetchPlaybackProgress`).
 * @param fetchWork - Reads a work's detail (`fetchWork`).
 * @returns The shelf, most recent first.
 */
export async function readShelf(
  fetchProgress: (options: { limit: number }) => Promise<ProgressPage>,
  fetchWork: (id: string) => Promise<WorkDetail>,
): Promise<ShelfItem[]> {
  const page = await fetchProgress({ limit: SHELF_SIZE });
  const items = await Promise.all(
    page.items.slice(0, SHELF_SIZE).map(async (progress): Promise<ShelfItem | null> => {
      if (!progress.work_id || progress.is_finished) return null;
      try {
        const work = await fetchWork(progress.work_id);
        const edition = work.editions.find((candidate) => candidate.id === progress.edition_id);
        return edition ? { target: targetFor(work, edition), progress } : null;
      } catch {
        return null;
      }
    }),
  );
  return items.filter((item): item is ShelfItem => item !== null);
}

/**
 * How far through an edition a progress row is.
 *
 * @param progress - The row.
 * @returns A whole percentage from 0 to 100; 0 while the runtime is unknown.
 */
export function percentDone(progress: PlaybackProgress): number {
  if (progress.is_finished) return 100;
  if (!progress.duration_ms || progress.duration_ms <= 0) return 0;
  return Math.min(
    100,
    Math.max(0, Math.round((progress.position_ms / progress.duration_ms) * 100)),
  );
}
