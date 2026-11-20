/**
 * Display helpers for a store account's download pace on the Accounts page.
 *
 * @module
 */
import type { PortalPace } from "@/lib/api";

/**
 * A time as the reader's local clock shows it ("14:05"), or with the date
 * when it is not today.
 *
 * @param iso - An RFC 3339 time.
 * @param now - The current time (today's date decides the format).
 * @returns The time for display.
 */
export function shortTime(iso: string, now: Date = new Date()): string {
  const at = new Date(iso);
  const sameDay = at.toDateString() === now.toDateString();
  return sameDay
    ? at.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
    : at.toLocaleString([], { dateStyle: "medium", timeStyle: "short" });
}

/**
 * One line saying how fast automatic downloads run on an account.
 *
 * @param pace - The account's pace.
 * @param now - The current time.
 * @returns The line.
 */
export function paceSummary(pace: PortalPace, now: Date = new Date()): string {
  const parts: string[] = [];
  if (pace.downloads_per_day === null) {
    parts.push(`${pace.downloads_last_day} in the last 24 hours, no daily limit`);
  } else {
    parts.push(`${pace.downloads_last_day} of ${pace.downloads_per_day} in the last 24 hours`);
  }
  if (pace.min_gap_seconds > 0) {
    parts.push(`${pace.min_gap_seconds} s apart`);
  }
  if (pace.cooldown_until) {
    parts.push(`paused by the store until ${shortTime(pace.cooldown_until, now)}`);
  } else if (pace.next_automatic_at) {
    parts.push(`next at ${shortTime(pace.next_automatic_at, now)}`);
  }
  return `Automatic downloads: ${parts.join(" · ")}`;
}
