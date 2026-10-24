/**
 * Reads the report IDs a diagnostics issue tracks.
 *
 * Triage (`scripts/validate_plan.py`) and its fallback
 * (`scripts/triage-and-file.sh`) end every issue they file with a
 * `### Report IDs` heading followed by one `` - `<id>` `` line per report.
 * Issue bodies can be edited by people with triage rights, so the body is
 * treated as untrusted: only the lines directly under the last such heading
 * count, each ID must look like a report UUID or a collector object name, and
 * the list is capped.
 */

/** First line of a triage-planned issue. */
export const TRIAGE_MARKER = "<!-- bookclerk-diagnostics: workers-ai triage -->";

/** First line of a fallback (untriaged batch) issue. */
export const FALLBACK_MARKER = "<!-- bookclerk-diagnostics: triage fallback -->";

/** Most report IDs read from one issue. */
export const MAX_IDS_PER_ISSUE = 500;

/** Longest issue body read, in characters. */
export const MAX_BODY_CHARS = 65536;

/** Lowercase report UUID. */
const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

/** A list line under the heading: `` - `id` ``. */
const LINE_RE = /^[-*] `([A-Za-z0-9._:/+-]{1,160})`\s*$/;

/**
 * Turns one listed ID into a report UUID: either the UUID itself or a
 * collector object name `diagnostics/<version>/<uuid>.json`.
 *
 * @param {string} id Listed ID.
 * @returns {string | null} Lowercase UUID, or null when it is neither form.
 */
export function reportIdOf(id) {
  const lower = id.toLowerCase();
  if (UUID_RE.test(lower)) return lower;
  const m = /^diagnostics\/[a-z0-9._+-]{1,64}\/([0-9a-f-]{36})\.json$/.exec(lower);
  return m && UUID_RE.test(m[1]) ? m[1] : null;
}

/**
 * Report UUIDs listed in an issue body, and what kind of issue it is.
 *
 * @param {unknown} body Issue body.
 * @returns {{kind: "triage" | "fallback" | "other", ids: string[]}} Distinct
 *   lowercase UUIDs in listed order, at most `MAX_IDS_PER_ISSUE`.
 */
export function parseIssueReportIds(body) {
  if (typeof body !== "string") return { kind: "other", ids: [] };
  const text = body.slice(0, MAX_BODY_CHARS);
  const lines = text.split(/\r?\n/);
  const first = (lines[0] ?? "").trim();
  const kind = first === TRIAGE_MARKER ? "triage" : first === FALLBACK_MARKER ? "fallback" : "other";
  let start = -1;
  for (let i = lines.length - 1; i >= 0; i--) {
    if (/^###\s+Report IDs\s*$/.test(lines[i])) {
      start = i + 1;
      break;
    }
  }
  if (start < 0) return { kind, ids: [] };
  const seen = new Set();
  for (let i = start; i < lines.length && seen.size < MAX_IDS_PER_ISSUE; i++) {
    const m = LINE_RE.exec(lines[i]);
    if (!m) break;
    const id = reportIdOf(m[1]);
    if (id) seen.add(id);
  }
  return { kind, ids: [...seen] };
}
