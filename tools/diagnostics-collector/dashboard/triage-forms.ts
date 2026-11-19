/**
 * Members' triage forms: mark reports seen or noise (one report or a page
 * of them), mute or unmute an error signature, and the owner's close
 * actions on an issue. This module only reads the forms; `triage-state.ts`
 * writes D1, `github.ts` writes GitHub, and `routes.ts` checks the session,
 * the origin and the role.
 *
 * Each body is whatever a signed-in member's browser sent, so it is parsed
 * sans-IO here and fuzzed in `dashboard-input.fuzz.test.ts`
 * (`corpus/triage_form/`, `corpus/mute_form/`, `corpus/close_form/`).
 */

import { MAX_FORM_CHARS, issueNumber } from "./links.ts";

/** Triage states a member can set; "clear" puts a report back in the queue. */
export const TRIAGE_ACTIONS = ["seen", "noise", "clear"] as const;

/** One of {@link TRIAGE_ACTIONS}. */
export type TriageAction = (typeof TRIAGE_ACTIONS)[number];

/** Most reports one triage form marks: a page of the explorer and spare. */
export const MAX_TRIAGE_REPORTS = 40;

/** Longest mute reason kept, in characters. */
export const MAX_MUTE_REASON = 200;

/** Longest mute, in days. */
export const MAX_MUTE_DAYS = 365;

/** A report ID as the index stores it: a lowercase UUID. */
const REPORT_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

/** An error signature: 16 lowercase hex characters. */
const SIGNATURE = /^[0-9a-f]{16}$/;

/** A release version: `MAJOR.MINOR.PATCH` with an optional prerelease. */
const VERSION = /^[0-9]{1,6}\.[0-9]{1,6}\.[0-9]{1,6}(?:-[0-9A-Za-z.-]{1,40})?$/;

/** Control characters, which no reason may carry. */
const CONTROL = /[\u0000-\u001f\u007f]/;

/**
 * Parses a form body.
 *
 * @param body - The posted body.
 * @returns The fields, or null when the body is not a string of at most
 *   `MAX_FORM_CHARS` characters.
 */
function form(body: unknown): URLSearchParams | null {
  if (typeof body !== "string" || body.length > MAX_FORM_CHARS) return null;
  try {
    return new URLSearchParams(body);
  } catch {
    return null;
  }
}

/**
 * Whether a value is one of {@link TRIAGE_ACTIONS}.
 *
 * @param v - Form value.
 * @returns True for a known action.
 */
function isTriageAction(v: string | null): v is TriageAction {
  return v !== null && (TRIAGE_ACTIONS as readonly string[]).includes(v);
}

/** A parsed triage mark. */
export type TriageMark = {
  /** The state to set. */
  state: TriageAction;
  /** Distinct report IDs, in form order. */
  reports: string[];
};

/**
 * Reads a triage mark: `state` is one of {@link TRIAGE_ACTIONS} and every
 * `report` field is a lowercase UUID, one to {@link MAX_TRIAGE_REPORTS} of
 * them once repeats are dropped. Other fields are ignored.
 *
 * @param body - The form body (`application/x-www-form-urlencoded`).
 * @returns The mark, or null when the body is not one.
 */
export function parseTriageMark(body: unknown): TriageMark | null {
  const f = form(body);
  if (!f) return null;
  const state = f.get("state");
  if (!isTriageAction(state)) return null;
  const reports = [...new Set(f.getAll("report"))];
  if (reports.length === 0 || reports.length > MAX_TRIAGE_REPORTS) return null;
  if (!reports.every((r) => REPORT_ID.test(r))) return null;
  return { state, reports };
}

/** A parsed mute or unmute. */
export type MuteAction =
  | {
      /** Mute the signature. */
      action: "mute";
      /** The signature. */
      signature: string;
      /** Why, trimmed: 1 to {@link MAX_MUTE_REASON} characters. */
      reason: string;
      /** Days the mute lasts, or null when it lasts until a version. */
      days: number | null;
      /** Version whose reports end the mute, or null when it lasts `days`. */
      untilVersion: string | null;
    }
  | {
      /** Lift the mute. */
      action: "unmute";
      /** The signature. */
      signature: string;
    };

/**
 * Reads a mute form. `action` is "mute" or "unmute" and `signature` is 16
 * lowercase hex characters. A mute also needs a `reason` (1 to
 * {@link MAX_MUTE_REASON} characters after trimming, no control characters)
 * and exactly one end: `days` (1 to {@link MAX_MUTE_DAYS}) or
 * `until_version` (a release version, an optional leading `v` dropped). An
 * empty field counts as absent.
 *
 * @param body - The form body.
 * @returns The action, or null when the body is not one.
 */
export function parseMuteAction(body: unknown): MuteAction | null {
  const f = form(body);
  if (!f) return null;
  const action = f.get("action");
  const signature = f.get("signature");
  if (signature === null || !SIGNATURE.test(signature)) return null;
  if (action === "unmute") return { action, signature };
  if (action !== "mute") return null;
  const reason = (f.get("reason") ?? "").trim();
  if (!reason || reason.length > MAX_MUTE_REASON || CONTROL.test(reason)) return null;
  const daysText = f.get("days") ?? "";
  const versionText = (f.get("until_version") ?? "").replace(/^v/, "");
  if ((daysText === "") === (versionText === "")) return null;
  if (daysText !== "") {
    if (!/^[1-9][0-9]{0,2}$/.test(daysText) || Number(daysText) > MAX_MUTE_DAYS) return null;
    return { action, signature, reason, days: Number(daysText), untilVersion: null };
  }
  if (!VERSION.test(versionText)) return null;
  return { action, signature, reason, days: null, untilVersion: versionText };
}

/** A parsed close action. */
export type CloseAction = {
  /** The issue to close. */
  issue: number;
  /** "not_planned", or "fixed" with the version that fixed it. */
  how: "not_planned" | "fixed";
  /** The fixing version for "fixed", else null. */
  version: string | null;
};

/**
 * Reads the owner's close form: `issue` is an issue number, `how` is
 * "not_planned" or "fixed", and "fixed" needs `version` (a release
 * version, an optional leading `v` dropped). Other fields are ignored.
 *
 * @param body - The form body.
 * @returns The action, or null when the body is not one.
 */
export function parseCloseAction(body: unknown): CloseAction | null {
  const f = form(body);
  if (!f) return null;
  const issue = issueNumber(f.get("issue"));
  if (issue === null) return null;
  const how = f.get("how");
  if (how === "not_planned") return { issue, how, version: null };
  if (how !== "fixed") return null;
  const version = (f.get("version") ?? "").replace(/^v/, "");
  if (!VERSION.test(version)) return null;
  return { issue, how, version };
}
