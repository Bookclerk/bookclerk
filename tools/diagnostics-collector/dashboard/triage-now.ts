/**
 * **Triage now** on a report page, sans-IO: the one-report batch the model
 * reads, the reading of its reply into a proposal a member confirms, and
 * the proposal's stored form.
 *
 * The reply is model output over report text anyone can submit, so it is
 * untrusted. A proposal files a public issue, so the reading is held to the
 * rules of the ingest run's plan validator (`scripts/validate_plan.py`):
 * only this report, a prefixed one-line title, a bounded body, no links,
 * mentions, cross-references, raw HTML or forged report blocks (`defang`,
 * shared with the summary comment and checked against Python there), and
 * the report block written here, never by the model. Attachments go only
 * to the issues the model was offered. `triage-now.fuzz.test.ts` holds the
 * reading to the validator itself: the validator's seed corpus, mutations
 * of it and built plans must give the same proposal on both sides.
 */

import { asObject, type JsonObject } from "../json.ts";
import { REPORT_BLOCK_CLOSE, REPORT_BLOCK_OPEN, TRIAGE_MARKER } from "./report-ids.ts";
import { defang } from "./summary.ts";

/** Prefix every filed issue title carries (`validate_plan.py`'s). */
export const TITLE_PREFIX = "[diagnostics] ";

/** Longest title, in code points, after the prefix is added. */
export const MAX_TITLE_CHARS = 120;

/** Longest model-written body, in code points, before the report block. */
export const MAX_BODY_CHARS = 8000;

/** Longest reply read; the model's output is capped well below it. */
export const MAX_REPLY_CHARS = 32_768;

/** Deepest JSON nesting read; a plan needs four levels. */
export const MAX_DEPTH = 32;

/** Most events, message characters and fields per event the model sees (the ingest batch's caps). */
const EVENT_CAPS = { events: 40, message: 500, fields: 20 } as const;

/** A lowercase report UUID. */
const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

/**
 * The characters Python's `str.split()` and `\s` treat as whitespace, so
 * titles and headings are read as `validate_plan.py` reads them.
 */
const PY_SPACE =
  "\\t\\n\\v\\f\\r\\x1c-\\x20\\x85\\xa0\\u1680\\u2000-\\u200a\\u2028\\u2029\\u202f\\u205f\\u3000";

/** A run of Python whitespace. */
const PY_SPACES = new RegExp(`[${PY_SPACE}]+`, "u");

/**
 * A legacy "Report IDs" heading, which only the ingest run may write, as
 * `validate_plan.py` removes it: `(?im)^\s*#+\s*report ids\b.*$`. Python
 * anchors lines only at `\n` and its whitespace may run across lines; its
 * case-insensitive letters are spelled out (`i` also matches `İ` and `ı`,
 * `s` matches `ſ`) and `\b` is a Unicode word boundary.
 */
const REPORT_IDS_HEADING = new RegExp(
  `(?:^|(?<=\\n))[${PY_SPACE}]*#+[${PY_SPACE}]*[Rr][Ee][Pp][Oo][Rr][Tt] [Ii\\u0130\\u0131][Dd][Ss\\u017f](?![\\p{L}\\p{N}_])[^\\n]*`,
  "gu",
);

/** An integer as JSON writes it, which Python reads as an `int`. */
const JSON_INT = /^-?(?:0|[1-9][0-9]*)$/;

/**
 * A `JSON.parse` reviver that turns any number not written as an integer
 * (`7.0`, `7e0`) into NaN, since Python reads those as floats and the plan
 * validator takes only ints as issue numbers.
 *
 * @param _key - Property name.
 * @param value - Parsed value.
 * @param context - The value's source text, where the runtime gives it.
 * @returns The value, or NaN.
 */
function intsOnly(_key: string, value: unknown, context?: { source?: string }): unknown {
  return typeof value === "number" &&
    typeof context?.source === "string" &&
    !JSON_INT.test(context.source)
    ? Number.NaN
    : value;
}

/** What triage proposes for one report. */
export type Proposal =
  | {
      /** File a new issue. */
      kind: "issue";
      /** Prefixed, defanged, one-line title. */
      title: string;
      /** Defanged body, without the marker line and report block. */
      body: string;
      /** Whether the issue gets the `bug` label. */
      bug: boolean;
    }
  | {
      /** Attach the report to an open issue the model was offered. */
      kind: "attach";
      /** That issue's number. */
      issue: number;
    }
  | {
      /** Nothing to file: the model judged the report noise. */
      kind: "none";
    };

/**
 * The one-report batch the model reads, shaped as the ingest run shapes
 * each report (`scripts/triage-and-file.sh`): the fields triage uses, the
 * first 40 events with messages cut to 500 characters and 20 fields.
 *
 * @param reportId - Lowercase report UUID.
 * @param fileName - The stored object's name.
 * @param stored - The stored report (untrusted payload).
 * @returns The batch entry.
 */
export function triageItem(reportId: string, fileName: string, stored: unknown): JsonObject {
  const s = asObject(stored);
  const p = asObject(s.payload);
  const text = (v: unknown, fallback: string | null): string | null =>
    typeof v === "string" ? v : fallback;
  const events = Array.isArray(p.events) ? p.events : [];
  const item = {
    report_id: reportId,
    file_name: fileName,
    trigger: text(p.trigger, "report"),
    version: text(p.version, "?"),
    os: text(p.os, "?"),
    arch: text(p.arch, null),
    distro: text(p.distro, null),
    rustc_channel: text(p.rustc_channel, null),
    received_at_unix_ms: typeof s.received_at_unix_ms === "number" ? s.received_at_unix_ms : null,
    event_count: events.length,
    events: events.slice(0, EVENT_CAPS.events).map((e) => {
      const ev = asObject(e);
      return {
        level: ev.level ?? null,
        target: ev.target ?? null,
        message: typeof ev.message === "string" ? ev.message.slice(0, EVENT_CAPS.message) : "",
        fields: Array.isArray(ev.fields) ? ev.fields.slice(0, EVENT_CAPS.fields) : [],
      };
    }),
  };
  return item;
}

/**
 * Where the JSON object that opens at `start` ends, scanning strings and
 * nesting.
 *
 * @param text - Reply text.
 * @param start - Index of a `{`.
 * @returns The index after its closing `}`, `-1` when it never closes, or
 *   `-2` when it nests deeper than `MAX_DEPTH`.
 */
function objectEnd(text: string, start: number): number {
  let depth = 0;
  let inString = false;
  for (let i = start; i < text.length; i++) {
    const ch = text[i];
    if (inString) {
      if (ch === "\\") i++;
      else if (ch === '"') inString = false;
    } else if (ch === '"') {
      inString = true;
    } else if (ch === "{" || ch === "[") {
      if (++depth > MAX_DEPTH) return -2;
    } else if (ch === "}" || ch === "]") {
      if (--depth === 0) return i + 1;
      if (depth < 0) return -1;
    }
  }
  return -1;
}

/**
 * The first JSON object with an `issues` list in a reply, as
 * `validate_plan.py`'s `find_plan` finds it.
 *
 * @param text - The model's reply.
 * @returns The plan, or null when there is none, its `issues` is not a
 *   list, or the reply is too long or nests too deeply.
 */
export function findPlan(text: string): JsonObject | null {
  if (typeof text !== "string" || text.length > MAX_REPLY_CHARS) return null;
  for (let i = text.indexOf("{"); i >= 0; i = text.indexOf("{", i + 1)) {
    const end = objectEnd(text, i);
    if (end === -2) return null;
    if (end < 0) continue;
    let obj: unknown;
    try {
      obj = JSON.parse(text.slice(i, end), intsOnly);
    } catch {
      continue;
    }
    if (obj !== null && typeof obj === "object" && !Array.isArray(obj) && "issues" in obj) {
      const plan = obj as JsonObject;
      return Array.isArray(plan.issues) ? plan : null;
    }
  }
  return null;
}

/**
 * Text cut to a number of code points.
 *
 * @param text - Input.
 * @param max - Most code points kept.
 * @returns The prefix.
 */
function codePoints(text: string, max: number): string {
  const points = Array.from(text);
  return points.length > max ? points.slice(0, max).join("") : text;
}

/**
 * A model-written title made publishable: one line, prefixed, defanged and
 * cut to `MAX_TITLE_CHARS`.
 *
 * @param raw - Model text.
 * @returns The title.
 */
export function issueTitle(raw: string): string {
  let title = raw.split(PY_SPACES).filter(Boolean).join(" ");
  if (!title.startsWith(TITLE_PREFIX)) title = TITLE_PREFIX + title;
  return codePoints(defang(title), MAX_TITLE_CHARS);
}

/**
 * A model-written body made publishable: cut to `MAX_BODY_CHARS`, any
 * "Report IDs" heading removed, then defanged (which also breaks any
 * `<!--`, so no report block can be forged).
 *
 * @param raw - Model text.
 * @returns The body, without marker or report block.
 */
export function proposalBody(raw: string): string {
  return defang(codePoints(raw, MAX_BODY_CHARS).replace(REPORT_IDS_HEADING, ""));
}

/**
 * The full issue body filed for a proposal: the triage marker, the body,
 * and the report block listing only this report.
 *
 * @param body - A `proposalBody` result.
 * @param reportId - Lowercase report UUID.
 * @returns The body to file.
 */
export function issueBody(body: string, reportId: string): string {
  return `${TRIAGE_MARKER}\n${body}\n\n${REPORT_BLOCK_OPEN}\n${reportId}\n${REPORT_BLOCK_CLOSE}\n`;
}

/**
 * Reads the model's reply for one report into a proposal.
 *
 * An attachment that names the report and an offered issue wins, as the
 * ingest run takes an attached report out of new issues. Otherwise the
 * first of the first five issues that lists the report with a string title
 * and body is proposed. An empty plan is the model's judgment that nothing
 * needs filing.
 *
 * @param text - The model's reply (untrusted).
 * @param reportId - Lowercase report UUID the batch held.
 * @param offered - Open issue numbers the model was offered for `attach`.
 * @returns The proposal, or null when the reply holds no plan, or a plan
 *   that names issues or attachments yet none that can be used.
 */
export function proposalFor(
  text: string,
  reportId: string,
  offered: readonly number[],
): Proposal | null {
  if (!UUID_RE.test(reportId)) return null;
  const plan = findPlan(text);
  if (!plan) return null;
  const listsReport = (ids: unknown): boolean => Array.isArray(ids) && ids.includes(reportId);
  const attach = Array.isArray(plan.attach) ? plan.attach : [];
  for (const entry of attach) {
    const a = asObject(entry);
    if (
      typeof a.issue === "number" &&
      Number.isSafeInteger(a.issue) &&
      offered.includes(a.issue) &&
      listsReport(a.report_ids)
    ) {
      return { kind: "attach", issue: a.issue };
    }
  }
  const issues = plan.issues as unknown[];
  for (const entry of issues.slice(0, 5)) {
    const e = asObject(entry);
    if (typeof e.title !== "string" || typeof e.body !== "string") continue;
    if (!listsReport(e.report_ids)) continue;
    return {
      kind: "issue",
      title: issueTitle(e.title),
      body: proposalBody(e.body),
      bug: e.bug === true,
    };
  }
  return issues.length === 0 && attach.length === 0 ? { kind: "none" } : null;
}

/** A stored proposal: when triage answered, and what it proposed. */
export type StoredProposal = {
  /** Unix ms. */
  at: number;
  /** The proposal. */
  proposal: Proposal;
};

/**
 * The stored form of a proposal (a `sync_state` value).
 *
 * @param stored - Proposal and time.
 * @returns JSON text.
 */
export function encodeProposal(stored: StoredProposal): string {
  return JSON.stringify(stored);
}

/**
 * Reads a stored proposal back, checking its shape again, since it decides
 * what is filed.
 *
 * @param raw - The `sync_state` value, or null.
 * @returns The proposal, or null when absent or malformed.
 */
export function decodeProposal(raw: string | null): StoredProposal | null {
  if (raw === null) return null;
  let data: unknown;
  try {
    data = JSON.parse(raw);
  } catch {
    return null;
  }
  const d = asObject(data);
  const p = asObject(d.proposal);
  if (typeof d.at !== "number" || !Number.isFinite(d.at)) return null;
  switch (p.kind) {
    case "issue":
      if (
        typeof p.title !== "string" ||
        typeof p.body !== "string" ||
        typeof p.bug !== "boolean" ||
        Array.from(p.title).length > MAX_TITLE_CHARS ||
        !p.title.startsWith(TITLE_PREFIX)
      )
        return null;
      return { at: d.at, proposal: { kind: "issue", title: p.title, body: p.body, bug: p.bug } };
    case "attach":
      return typeof p.issue === "number" && Number.isSafeInteger(p.issue) && p.issue > 0
        ? { at: d.at, proposal: { kind: "attach", issue: p.issue } }
        : null;
    case "none":
      return { at: d.at, proposal: { kind: "none" } };
    default:
      return null;
  }
}
