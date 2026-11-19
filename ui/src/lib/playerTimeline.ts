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
  const wanted = typeof value === "number" ? value : Number(value);
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
