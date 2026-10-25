/**
 * The issue summary comment, rendered in the Worker for the owner's
 * "Update GitHub now" button. This is the same renderer as the ingest
 * Action's `scripts/summary_comment.py`, function for function: the Python
 * module is the oracle of `summary.fuzz.test.js`, which runs both on the
 * same inputs and requires identical output, so an edit to one without the
 * other fails the tests. Keep the two in step, including `defang`, which is
 * `validate_plan.defang`.
 *
 * Every string in a summary comes from reports anyone can submit (versions,
 * platforms), so it is validated or rewritten before it reaches a public
 * comment, and report IDs link only to their own dashboard page.
 */

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
 * @param {string} text Report-derived text.
 * @returns {string} The text with Markdown link syntax broken, every
 *   `scheme://` or `www.` URL removed, raw HTML escaped, and mentions and
 *   issue references broken by a zero-width space.
 */
export function defang(text) {
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
 * @param {unknown} raw The origin, possibly with a trailing slash.
 * @returns {string | null} The lowercase origin without a trailing slash, or
 *   null when it is missing or not a bare `https://` origin.
 */
export function dashboardOrigin(raw) {
  if (typeof raw !== "string" || raw === "") return null;
  const origin = raw.replace(ASCII_WS_ENDS, "").replace(/\/+$/, "").toLowerCase();
  if ([...origin].length > MAX_DASHBOARD_ORIGIN_CHARS || !DASHBOARD_RE.test(origin)) return null;
  return origin;
}

/** Whether a value is a plain object (Python's dict). */
const isDict = (v) => v !== null && typeof v === "object" && !Array.isArray(v);

/**
 * A field of a plain object, by own property only.
 *
 * @param {object} o Object.
 * @param {string} k Key.
 * @returns {unknown} The value, or undefined.
 */
const get = (o, k) => (Object.hasOwn(o, k) ? o[k] : undefined);

/**
 * A count or issue number (`_count`).
 *
 * @param {unknown} value Field value.
 * @returns {number | null} The integer from 0 to `MAX_NUMBER`, else null.
 */
function count(value) {
  if (typeof value !== "number" || !Number.isInteger(value) || value < 0 || value > MAX_NUMBER) return null;
  return value;
}

/**
 * A UTC date for a time in milliseconds (`_day`).
 *
 * @param {unknown} value Field value.
 * @returns {string | null} `YYYY-MM-DD`, or null.
 */
function day(value) {
  if (typeof value !== "number" || !Number.isInteger(value) || value < 0 || value > MAX_TIME_MS) return null;
  return new Date(value).toISOString().slice(0, 10);
}

/**
 * A version for the comment (`_version`).
 *
 * @param {unknown} value Field value.
 * @returns {string} Validated and rewritten, else `unknown`.
 */
function version(value) {
  if (typeof value === "string") {
    const text = value.replace(ASCII_WS_ENDS, "");
    if (text === "others" || VERSION_RE.test(text)) return defang(text);
  }
  return "unknown";
}

/**
 * A platform for the comment (`_platform`).
 *
 * @param {unknown} value Field value.
 * @returns {string} Validated and rewritten, else `unknown`.
 */
function platform(value) {
  if (typeof value === "string") {
    const text = value.split(ASCII_WS).filter((part) => part !== "").join(" ");
    if (text === "others" || PLATFORM_RE.test(text)) return defang(text);
  }
  return "unknown";
}

/**
 * The `(label, count)` pairs of a versions or platforms list (`_breakdown`).
 *
 * @param {unknown} value Field value.
 * @param {"version" | "platform"} name Which list.
 * @returns {Array<[string, number]>} Pairs.
 */
function breakdown(value, name) {
  if (!Array.isArray(value)) return [];
  const out = [];
  for (const entry of value.slice(0, MAX_BREAKDOWN)) {
    if (!isDict(entry)) continue;
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
 * @param {number} n Count.
 * @param {string} word Singular noun.
 * @returns {string} The phrase.
 */
const plural = (n, word) => `${n} ${word}${n === 1 ? "" : "s"}`;

/**
 * One summary comment to write.
 *
 * @typedef {{issue: number, fingerprint: string, body: string}} Rendered
 *   The issue's number, the collector's fingerprint of the figures
 *   rendered, and the comment starting with `MARKER`.
 */

/**
 * Renders one issue's summary (`render_summary`).
 *
 * @param {unknown} entry One element of the collector's `issues` list.
 * @param {string | null} dashboard Validated collector origin for report
 *   links, or null.
 * @returns {Rendered | null} The comment, or null when the entry lacks a
 *   valid issue number, fingerprint or report count, or renders too long.
 */
export function renderSummary(entry, dashboard) {
  if (!isDict(entry)) return null;
  const issue = count(get(entry, "issue"));
  const fingerprint = get(entry, "fingerprint");
  const reports = count(get(entry, "reports"));
  if (!issue || reports === null || typeof fingerprint !== "string" || !FINGERPRINT_RE.test(fingerprint)) return null;
  const installs = count(get(entry, "installs")) || 0;
  const first = day(get(entry, "first_seen"));
  const last = day(get(entry, "last_seen"));
  const sources = isDict(get(entry, "sources")) ? get(entry, "sources") : {};
  const counts = Object.fromEntries(["filed", "signature", "triage", "manual"].map((k) => [k, count(get(sources, k)) || 0]));
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
    const rows = [];
    for (const r of (Array.isArray(newest) ? newest : []).slice(0, MAX_NEWEST)) {
      if (!isDict(r)) continue;
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
 * @param {unknown} payload The `GET /issues/summaries` answer (an object
 *   with `issues`) or its `issues` list.
 * @param {string | null} [dashboard] Validated collector origin for report
 *   links, or null.
 * @returns {Rendered[]} At most `MAX_SUMMARIES` comments, one per valid
 *   issue in the answer's order (a repeated issue keeps its first entry).
 */
export function renderSummaries(payload, dashboard = null) {
  const issues = isDict(payload) ? get(payload, "issues") : payload;
  if (!Array.isArray(issues)) return [];
  const out = [];
  const seen = new Set();
  for (const entry of issues) {
    if (out.length >= MAX_SUMMARIES) break;
    const rendered = renderSummary(entry, dashboard);
    if (rendered === null || seen.has(rendered.issue)) continue;
    seen.add(rendered.issue);
    out.push(rendered);
  }
  return out;
}
