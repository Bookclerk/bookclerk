/**
 * Reads the report IDs a diagnostics issue tracks.
 *
 * Triage (`scripts/validate_plan.py`), its fallback
 * (`scripts/triage-and-file.sh`) and regression issues
 * (`scripts/regression_issue.py`) end every issue they file with a hidden
 * report block: a `<!-- bookclerk-diagnostics: report ids` line, one bare
 * ID per line and a closing `-->` line. GitHub does not render it, so people
 * read the summary comment instead of an ID list that the dashboard's
 * corrections leave behind. Issues filed before the block existed list their
 * IDs under a visible `### Report IDs` heading, one `` - `<id>` `` line per
 * report, optionally followed by `` · [dashboard](<url>) ``; that list is
 * read only when the body has no block.
 *
 * Issue bodies can be edited by people with triage rights, so the body is
 * treated as untrusted: only the last block (or the lines directly under the
 * last heading) counts, a block without its closing line counts as empty,
 * each ID must look like a report UUID or a collector object name, and the
 * list is capped.
 */

/** First line of a triage-planned issue. */
export const TRIAGE_MARKER = "<!-- bookclerk-diagnostics: workers-ai triage -->";

/** First line of a fallback (untriaged batch) issue. */
export const FALLBACK_MARKER = "<!-- bookclerk-diagnostics: triage fallback -->";

/**
 * First line of a regression issue (`scripts/regression_issue.py`), naming
 * the closed issue whose fixed error came back: at most nine digits, no
 * leading zero.
 */
export const REGRESSION_MARKER_RE =
  /^<!-- bookclerk-diagnostics: regression of #([1-9]\d{0,8}) -->$/;

/** Most report IDs read from one issue. */
export const MAX_IDS_PER_ISSUE = 500;

/** Longest issue body read, in characters. */
export const MAX_BODY_CHARS = 65536;

/** Opening line of the hidden report block; only the last one counts. */
export const REPORT_BLOCK_OPEN = "<!-- bookclerk-diagnostics: report ids";

/** Closing line of the hidden report block. */
export const REPORT_BLOCK_CLOSE = "-->";

/**
 * One ID line inside the block. The character set has no `>`, so no listed
 * ID can end the HTML comment early and make the rest of the block render.
 */
const BLOCK_LINE_RE = /^([A-Za-z0-9._:/+-]{1,160})[ \t]*$/;

/**
 * A line without its trailing spaces and tabs, the only padding the block's
 * lines may carry (the same set `scripts/holding_issue.py` strips).
 *
 * @param line - Body line.
 * @returns The line without trailing spaces and tabs.
 */
function unpadded(line: string): string {
  return line.replace(/[ \t]+$/, "");
}

/** Lowercase report UUID. */
const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

/**
 * A list line under the legacy heading: `` - `id` ``, optionally with a trailing
 * `` · [dashboard](<url>) ``. Only the ID is read; the link is ignored.
 */
const LINE_RE =
  /^[-*] `([A-Za-z0-9._:/+-]{1,160})`(?: \u00b7 \[dashboard\]\([^\s()]{1,300}\))?\s*$/;

/**
 * Turns one listed ID into a report UUID: either the UUID itself or a
 * collector object name `diagnostics/<version>/<uuid>.json`.
 *
 * @param id - Listed ID.
 * @returns Lowercase UUID, or null when it is neither form.
 */
export function reportIdOf(id: string): string | null {
  const lower = id.toLowerCase();
  if (UUID_RE.test(lower)) return lower;
  const m = /^diagnostics\/[a-z0-9._+-]{1,64}\/([0-9a-f-]{36})\.json$/.exec(lower);
  const uuid = m?.[1];
  return uuid !== undefined && UUID_RE.test(uuid) ? uuid : null;
}

/**
 * Report UUIDs listed in an issue body, and what kind of issue it is.
 *
 * @param body - Issue body.
 * @returns Distinct lowercase UUIDs
 *   in listed order, at most `MAX_IDS_PER_ISSUE`, and for a regression
 *   issue the number of the closed issue its marker names.
 */
export function parseIssueReportIds(body: unknown): {
  kind: "triage" | "fallback" | "regression" | "other";
  ids: string[];
  regressionOf: number | null;
} {
  if (typeof body !== "string") return { kind: "other", ids: [], regressionOf: null };
  const text = body.slice(0, MAX_BODY_CHARS);
  const lines = text.split(/\r?\n/);
  const first = (lines[0] ?? "").trim();
  const regression = REGRESSION_MARKER_RE.exec(first);
  const kind =
    first === TRIAGE_MARKER
      ? "triage"
      : first === FALLBACK_MARKER
        ? "fallback"
        : regression
          ? "regression"
          : "other";
  const regressionOf = regression ? Number(regression[1]) : null;
  const block = lastIndex(lines, (line) => unpadded(line) === REPORT_BLOCK_OPEN);
  if (block >= 0) return { kind, ids: listed(lines, block + 1, BLOCK_LINE_RE, true), regressionOf };
  const heading = lastIndex(lines, (line) => /^###\s+Report IDs\s*$/.test(line));
  if (heading < 0) return { kind, ids: [], regressionOf };
  return { kind, ids: listed(lines, heading + 1, LINE_RE, false), regressionOf };
}

/**
 * Index of the last line that satisfies a test.
 *
 * @param lines - Body lines.
 * @param test - Line test.
 * @returns The index, or -1 when no line passes.
 */
function lastIndex(lines: readonly string[], test: (line: string) => boolean): number {
  for (let i = lines.length - 1; i >= 0; i--) if (test(lines[i] ?? "")) return i;
  return -1;
}

/**
 * Reads the run of ID lines that starts at one line.
 *
 * @param lines - Body lines.
 * @param start - First line of the run.
 * @param lineRe - Pattern of one ID line; group 1 is the ID.
 * @param closed - Whether the run must end on `REPORT_BLOCK_CLOSE` (the
 *   hidden block); a block that does not lists nothing.
 * @returns Distinct lowercase UUIDs in listed order, at most
 *   `MAX_IDS_PER_ISSUE`.
 */
function listed(
  lines: readonly string[],
  start: number,
  lineRe: RegExp,
  closed: boolean,
): string[] {
  const seen = new Set<string>();
  let i = start;
  for (; i < lines.length; i++) {
    const m = lineRe.exec(lines[i] ?? "");
    if (!m) break;
    const id = reportIdOf(m[1] ?? "");
    if (id && seen.size < MAX_IDS_PER_ISSUE) seen.add(id);
  }
  if (closed && unpadded(lines[i] ?? "") !== REPORT_BLOCK_CLOSE) return [];
  return [...seen];
}
