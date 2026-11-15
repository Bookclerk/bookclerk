/**
 * Formats runtime minutes as `Xh Ym` (or `—` when unknown).
 *
 * @param minutes - Length in minutes, or `null`.
 * @returns Human-readable duration.
 */
export function formatDuration(minutes: number | null): string {
  if (minutes == null || minutes <= 0) return "—";
  const h = Math.floor(minutes / 60);
  const m = minutes % 60;
  if (h === 0) return `${m}m`;
  if (m === 0) return `${h}h`;
  return `${h}h ${m}m`;
}

/**
 * Formats an ISO date for library tables.
 *
 * @param iso - ISO timestamp, or `null`.
 * @returns Locale short date, raw string on parse failure, or `—`.
 */
export function formatDate(iso: string | null): string {
  if (!iso) return "—";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleDateString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
  });
}
