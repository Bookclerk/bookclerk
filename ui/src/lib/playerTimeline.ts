/**
 * Pure arithmetic of the web player: one timeline over several files,
 * clock text and playback speeds.
 *
 * @module
 */

/** A place inside one track. */
export interface TrackPlace {
  /** The track's index. */
  index: number;
  /** Milliseconds from that track's start. */
  offsetMs: number;
}

/**
 * Where each track starts on the edition's timeline.
 *
 * @param durationsMs - Each track's length in play order.
 * @returns One start per track; the first is 0.
 */
export function trackStarts(durationsMs: readonly number[]): number[] {
  const starts: number[] = [];
  let at = 0;
  for (const duration of durationsMs) {
    starts.push(at);
    at += Math.max(0, duration);
  }
  return starts;
}

/**
 * Finds the track a timeline position falls in.
 *
 * @param durationsMs - Each track's length in play order.
 * @param positionMs - A position on the whole timeline.
 * @returns The track and the offset inside it. A position past the end
 *   is the end of the last track; one before the start is the start.
 */
export function locate(durationsMs: readonly number[], positionMs: number): TrackPlace {
  const position = Math.max(0, positionMs);
  const starts = trackStarts(durationsMs);
  for (let index = durationsMs.length - 1; index >= 0; index -= 1) {
    if (position >= starts[index] || index === 0) {
      return {
        index,
        offsetMs: Math.min(position - starts[index], Math.max(0, durationsMs[index])),
      };
    }
  }
  return { index: 0, offsetMs: 0 };
}

/**
 * Text for a length of time: `1:02:03`, or `2:03` under an hour.
 *
 * @param ms - Milliseconds; negative or non-finite reads as 0.
 * @returns The clock text.
 */
export function formatClock(ms: number): string {
  const total = Number.isFinite(ms) && ms > 0 ? Math.floor(ms / 1000) : 0;
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const seconds = String(total % 60).padStart(2, "0");
  return hours > 0
    ? `${hours}:${String(minutes).padStart(2, "0")}:${seconds}`
    : `${minutes}:${seconds}`;
}

/**
 * Text for the listening time left at a speed: `4 h 52 min`, `12 min`.
 *
 * @param remainingMs - Timeline left.
 * @param speed - Playback speed.
 * @returns The text; under a minute reads `less than a minute`.
 */
export function formatRemaining(remainingMs: number, speed: number): string {
  const minutes = Math.floor(Math.max(0, remainingMs) / Math.max(0.1, speed) / 60_000);
  if (minutes < 1) return "less than a minute";
  const hours = Math.floor(minutes / 60);
  return hours > 0 ? `${hours} h ${minutes % 60} min` : `${minutes} min`;
}

/** The speeds the player offers, slowest first. */
export const SPEEDS: readonly number[] = [0.75, 1, 1.1, 1.2, 1.3, 1.5, 1.75, 2, 2.5, 3];

/**
 * The offered speed nearest a stored value.
 *
 * @param value - A speed, from storage or anywhere.
 * @returns One of {@link SPEEDS}; 1 for anything unusable.
 */
export function nearestSpeed(value: unknown): number {
  // Only a number or its text converts; `Number` throws on a symbol.
  const wanted =
    typeof value === "number" ? value : typeof value === "string" ? Number(value) : NaN;
  if (!Number.isFinite(wanted) || wanted <= 0) return 1;
  return SPEEDS.reduce((best, speed) =>
    Math.abs(speed - wanted) < Math.abs(best - wanted) ? speed : best,
  );
}

/**
 * Text for a speed: `1×`, `1.2×`.
 *
 * @param speed - A speed.
 * @returns The label.
 */
export function speedLabel(speed: number): string {
  return `${Number(speed.toFixed(2))}×`;
}

/** A chapter as the web player seeks between them. */
export interface ChapterMark {
  /** Where it starts on the edition's timeline, in milliseconds. */
  start_ms: number;
  /** Where it ends: the next chapter's start, or the end of the timeline. */
  end_ms: number;
}

/**
 * Milliseconds into a chapter within which "previous chapter" goes to the
 * chapter before rather than back to this one's start.
 */
export const RESTART_CHAPTER_MS = 3000;

/**
 * The chapter playing at `positionMs`: the last one starting at or before
 * it, so of chapters sharing a start the innermost (latest in pre-order)
 * wins.
 *
 * @param chapters - The chapters, starts non-decreasing.
 * @param positionMs - The position on the timeline.
 * @returns Its index, or -1 before the first chapter or with none.
 */
export function chapterAt(chapters: readonly ChapterMark[], positionMs: number): number {
  let found = -1;
  for (const [index, chapter] of chapters.entries()) {
    if (chapter.start_ms > positionMs) break;
    found = index;
  }
  return found;
}

/**
 * Where "previous chapter" goes: this chapter's start when more than
 * {@link RESTART_CHAPTER_MS} into it, else the start of the latest earlier
 * chapter that starts before it.
 *
 * @param chapters - The chapters, starts non-decreasing.
 * @param positionMs - The position on the timeline.
 * @returns The position to seek to; 0 with no earlier chapter.
 */
export function previousChapterStart(chapters: readonly ChapterMark[], positionMs: number): number {
  const current = chapterAt(chapters, positionMs);
  if (current < 0) return 0;
  const start = chapters[current].start_ms;
  if (positionMs - start > RESTART_CHAPTER_MS) return start;
  for (let index = current - 1; index >= 0; index -= 1) {
    if (chapters[index].start_ms < start) return chapters[index].start_ms;
  }
  return 0;
}

/**
 * Where "next chapter" goes: the first chapter starting after `positionMs`.
 *
 * @param chapters - The chapters, starts non-decreasing.
 * @param positionMs - The position on the timeline.
 * @returns The position to seek to, or `null` in the last chapter.
 */
export function nextChapterStart(
  chapters: readonly ChapterMark[],
  positionMs: number,
): number | null {
  const next = chapters.find((chapter) => chapter.start_ms > positionMs);
  return next ? next.start_ms : null;
}
