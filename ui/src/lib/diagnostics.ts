/**
 * Helpers for the diagnostics settings: where to look up a report and how
 * its time reads.
 */

/** The repository whose issues the diagnostics dashboard files reports in. */
export const DIAGNOSTICS_ISSUES_REPOSITORY = "Bookclerk/bookclerk";

/** A report ID as the collector assigns it: a lowercase version-4 UUID. */
const REPORT_ID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

/**
 * The GitHub issue search for one report ID.
 *
 * @param reportId - The report's ID.
 * @returns The search URL, or the repository's issue list when the ID is
 *   not a report ID (nothing else is put in the URL).
 */
export function diagnosticsIssueSearchUrl(reportId: string): string {
  const base = `https://github.com/${DIAGNOSTICS_ISSUES_REPOSITORY}/issues`;
  if (!REPORT_ID_RE.test(reportId)) return base;
  return `${base}?q=${encodeURIComponent(`"${reportId}"`)}`;
}

/**
 * A report's send time in the viewer's locale and time zone.
 *
 * @param unixMs - Unix milliseconds.
 * @returns Date and time, or "unknown time" for a value that is not a time.
 */
export function formatReportTime(unixMs: number): string {
  if (!Number.isFinite(unixMs) || unixMs <= 0) return "unknown time";
  const date = new Date(unixMs);
  if (Number.isNaN(date.getTime())) return "unknown time";
  return date.toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });
}

/**
 * Whether a link from a report lookup may be shown as a link.
 *
 * @param url - The link the collector sent.
 * @returns True only for an `https://` URL (the daemon already refuses
 *   anything else; this keeps a stray `javascript:` out of an `href`).
 */
export function isHttpsLink(url: string | null | undefined): url is string {
  return typeof url === "string" && url.startsWith("https://");
}
