/**
 * The issue summary comment, rendered in the Worker for the owner's
 * "Update GitHub now" button. This is the same renderer as the ingest
 * Action's `scripts/summary_comment.py`, function for function: the Python
 * module is the oracle of `summary.fuzz.test.ts`, which runs both on the
 * same inputs and requires identical output, so an edit to one without the
 * other fails the tests. Keep the two in step, including `defang`, which is
 * `validate_plan.defang`.
 *
 * Every string in a summary comes from reports anyone can submit (versions,
 * platforms), so it is validated or rewritten before it reaches a public
 * comment, and report IDs link only to their own dashboard page.
 */

import { isObject, type JsonObject } from "../json.ts";

/** First line of every summary comment; the writer edits the comment that starts with it. */
export const MARKER = "<!-- bookclerk-diagnostics: summary -->";
/** Most summaries one call renders. */
export const MAX_SUMMARIES = 20;
/** Most versions or platforms listed. */
const MAX_BREAKDOWN = 9;
/** Most newest reports listed. */
const MAX_NEWEST = 10;
/** Longest comment, in characters (code points), well inside GitHub's 65,536. */
const MAX_BODY_CHARS = 8000;
/** A report ID as the collector names them. */
const ID_RE = /^[A-Za-z0-9._:-]{1,128}$/;
/** A version as reports carry it. */
const VERSION_RE = /^v?[0-9]{1,6}\.[0-9]{1,6}(?:\.[0-9]{1,6})?(?:[-+._~][0-9A-Za-z.+_~-]{1,40})?$/;
/** A platform (`os arch`) as reports carry it. */
const PLATFORM_RE = /^[A-Za-z0-9._-]{1,32}(?: [A-Za-z0-9._-]{1,32})?$/;
/** A collector fingerprint: lowercase hex SHA-256. */
const FINGERPRINT_RE = /^[0-9a-f]{64}$/;
/** A lowercase UUID, the report IDs that have a dashboard page. */
const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
/** Largest issue number or count taken. */
const MAX_NUMBER = 999_999_999;
/** Latest time rendered as a date (year 9999). */
const MAX_TIME_MS = 253_402_300_799_000;
/** Whitespace trimmed from versions and collapsed in platforms (Python's `ASCII_WS`). */
const ASCII_WS = /[ \t\n\r\x0b\x0c]+/g;
/** `ASCII_WS` at the ends of a string. */
const ASCII_WS_ENDS = /^[ \t\n\r\x0b\x0c]+|[ \t\n\r\x0b\x0c]+$/g;

/**
 * Strips links and breaks mentions and references in public text
 * (`validate_plan.defang`, ASCII matching).
 *
 * @param text - Report-derived text.
 * @returns The text with Markdown link syntax broken, every
 *   `scheme://` or `www.` URL removed, raw HTML escaped, and mentions and
 *   issue references broken by a zero-width space.
 */
export function defang(text: string): string {
  text = text.replace(/\](?=[(:])/g, "]​");
  text = text.replace(/[a-z0-9+.-]*:\/\/[^ \t\n\r\x0b\x0c]*|\bwww\.[^ \t\n\r\x0b\x0c]*/gi, "[link removed]");
  text = text.replace(/&(?=#|[A-Za-z][A-Za-z0-9]*;)/g, "&amp;");
  text = text.replace(/<(?=[A-Za-z/!?])/g, "&lt;");
  text = text.replace(/@(?=[A-Za-z0-9])/g, "@​");
  text = text.replace(/\bGH-(?=[0-9])/gi, "GH-​");
  return text.replace(/#(?=[0-9])/g, "#​");
}

/** A bare `https://` origin: lowercase host, optional port (`validate_plan.DASHBOARD_RE`). */
const DASHBOARD_RE = /^https:\/\/[a-z0-9](?:[a-z0-9.-]{0,251}[a-z0-9])?(?::[0-9]{1,5})?$/;
/** Longest origin accepted. */
const MAX_DASHBOARD_ORIGIN_CHARS = 255;

/**
 * Validates the collector origin dashboard links point at
 * (`validate_plan.dashboard_origin`).
 *
 * @param raw - The origin, possibly with a trailing slash.
 * @returns The lowercase origin without a trailing slash, or
 *   null when it is missing or not a bare `https://` origin.
 */
export function dashboardOrigin(raw: unknown): string | null {
  if (typeof raw !== "string" || raw === "") return null;
  const origin = raw.replace(ASCII_WS_ENDS, "").replace(/\/+$/, "").toLowerCase();
  if ([...origin].length > MAX_DASHBOARD_ORIGIN_CHARS || !DASHBOARD_RE.test(origin)) return null;
  return origin;
}

/**
 * A field of a plain object, by own property only.
 *
 * @param o - Object.
 * @param k - Key.
 * @returns The value, or undefined.
 */
const get = (o: JsonObject, k: string): unknown => (Object.hasOwn(o, k) ? o[k] : undefined);

/**
 * A count or issue number (`_count`).
 *
 * @param value - Field value.
 * @returns The integer from 0 to `MAX_NUMBER`, else null.
 */
function count(value: unknown): number | null {
  if (typeof value !== "number" || !Number.isInteger(value) || value < 0 || value > MAX_NUMBER) return null;
  return value;
}

/**
 * A UTC date for a time in milliseconds (`_day`).
 *
 * @param value - Field value.
 * @returns `YYYY-MM-DD`, or null.
 */
function day(value: unknown): string | null {
  if (typeof value !== "number" || !Number.isInteger(value) || value < 0 || value > MAX_TIME_MS) return null;
  return new Date(value).toISOString().slice(0, 10);
}

/**
 * A version for the comment (`_version`).
 *
 * @param value - Field value.
 * @returns Validated and rewritten, else `unknown`.
 */
function version(value: unknown): string {
  if (typeof value === "string") {
    const text = value.replace(ASCII_WS_ENDS, "");
    if (text === "others" || VERSION_RE.test(text)) return defang(text);
  }
  return "unknown";
}

/**
 * A platform for the comment (`_platform`).
 *
 * @param value - Field value.
 * @returns Validated and rewritten, else `unknown`.
 */
function platform(value: unknown): string {
  if (typeof value === "string") {
    const text = value.split(ASCII_WS).filter((part) => part !== "").join(" ");
    if (text === "others" || PLATFORM_RE.test(text)) return defang(text);
  }
  return "unknown";
}

/**
 * The `(label, count)` pairs of a versions or platforms list (`_breakdown`).
 *
 * @param value - Field value.
 * @param name - Which list.
 * @returns Pairs.
 */
function breakdown(value: unknown, name: "version" | "platform"): Array<[string, number]> {
  if (!Array.isArray(value)) return [];
  const out: Array<[string, number]> = [];
  for (const entry of value.slice(0, MAX_BREAKDOWN)) {
    if (!isObject(entry)) continue;
    const n = count(get(entry, "n"));
    if (n === null) continue;
    const label = name === "version" ? version(get(entry, "version")) : platform(get(entry, "platform"));
    out.push([label, n]);
  }
  return out;
}

/**
 * `n word(s)`.
 *
 * @param n - Count.
 * @param word - Singular noun.
 * @returns The phrase.
 */
const plural = (n: number, word: string): string => `${n} ${word}${n === 1 ? "" : "s"}`;

/** One summary comment to write. */
export type Rendered = {
  /** The issue's number. */
  issue: number;
  /** The collector's fingerprint of the figures rendered. */
  fingerprint: string;
  /** The comment, starting with `MARKER`. */
  body: string;
};

/**
 * Renders one issue's summary (`render_summary`).
 *
 * @param entry - One element of the collector's `issues` list.
 * @param dashboard - Validated collector origin for report
 *   links, or null.
 * @returns The comment, or null when the entry lacks a
 *   valid issue number, fingerprint or report count, or renders too long.
 */
export function renderSummary(entry: unknown, dashboard: string | null): Rendered | null {
  if (!isObject(entry)) return null;
  const issue = count(get(entry, "issue"));
  const fingerprint = get(entry, "fingerprint");
  const reports = count(get(entry, "reports"));
  if (!issue || reports === null || typeof fingerprint !== "string" || !FINGERPRINT_RE.test(fingerprint)) return null;
  const installs = count(get(entry, "installs")) || 0;
  const first = day(get(entry, "first_seen"));
  const last = day(get(entry, "last_seen"));
  const sourcesField = get(entry, "sources");
  const sources = isObject(sourcesField) ? sourcesField : {};
  const counts = {
    filed: count(get(sources, "filed")) || 0,
    signature: count(get(sources, "signature")) || 0,
    triage: count(get(sources, "triage")) || 0,
    manual: count(get(sources, "manual")) || 0,
  };
  const lines = [MARKER];
  if (reports === 0) {
    lines.push("**Diagnostics summary** · no report is linked to this issue at present");
  } else {
    let head = `**Diagnostics summary** · ${plural(reports, "report")} from ${plural(installs, "installation")}`;
    if (first) head += ` · first seen ${first}`;
    if (last) head += ` · last seen ${last}`;
    lines.push(head);
    lines.push("");
    lines.push(
      `- Linked: ${counts.filed} listed in this issue, ${counts.signature} matched by error signature, `
        + `${counts.triage} added by triage, ${counts.manual} attached by members`,
    );
    const versions = breakdown(get(entry, "versions"), "version");
    if (versions.length) lines.push("- Versions: " + versions.map(([v, n]) => `${v} (${n})`).join(", "));
    const platforms = breakdown(get(entry, "platforms"), "platform");
    if (platforms.length) lines.push("- Platforms: " + platforms.map(([p, n]) => `${p} (${n})`).join(", "));
    const newest = get(entry, "newest");
    const rows: string[] = [];
    for (const r of (Array.isArray(newest) ? newest : []).slice(0, MAX_NEWEST)) {
      if (!isObject(r)) continue;
      const rid = get(r, "report_id");
      if (typeof rid !== "string" || !ID_RE.test(rid)) continue;
      let row = `- \`${rid}\` · ${version(get(r, "version"))}`;
      const d = day(get(r, "received_at"));
      if (d) row += ` · ${d}`;
      if (dashboard && UUID_RE.test(rid)) row += ` · [dashboard](${dashboard}/reports/${rid})`;
      rows.push(row);
    }
    if (rows.length) {
      lines.push("");
      lines.push("Newest reports (dashboard pages are for organization members; the full list is there):");
      lines.push(...rows);
    }
  }
  lines.push("");
  lines.push("_Kept up to date by the diagnostics ingest run and edited in place; report contents are never posted here._");
  const body = lines.join("\n") + "\n";
  if ([...body].length > MAX_BODY_CHARS) return null;
  return { issue, fingerprint, body };
}

/**
 * Renders the collector's answer (`render_summaries`).
 *
 * @param payload - The `GET /issues/summaries` answer (an object
 *   with `issues`) or its `issues` list.
 * @param dashboard - Validated collector origin for report
 *   links, or null.
 * @returns At most `MAX_SUMMARIES` comments, one per valid
 *   issue in the answer's order (a repeated issue keeps its first entry).
 */
export function renderSummaries(payload: unknown, dashboard: string | null = null): Rendered[] {
  const issues = isObject(payload) ? get(payload, "issues") : payload;
  if (!Array.isArray(issues)) return [];
  const out: Rendered[] = [];
  const seen = new Set<number>();
  for (const entry of issues) {
    if (out.length >= MAX_SUMMARIES) break;
    const rendered = renderSummary(entry, dashboard);
    if (rendered === null || seen.has(rendered.issue)) continue;
    seen.add(rendered.issue);
    out.push(rendered);
  }
  return out;
}
