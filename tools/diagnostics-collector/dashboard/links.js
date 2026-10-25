/**
 * Members' corrections to the report ledger, as the dashboard's forms post
 * them: attach a report to an issue, detach it (or dismiss a suggestion),
 * move it to another issue, or merge every report of one issue into
 * another. This module only reads the form; `db.js` writes the ledger and
 * `routes.js` checks the session, the origin and that the issue and report
 * exist.
 *
 * The form body is whatever a signed-in member's browser sent, so it is
 * parsed sans-IO here and fuzzed in `dashboard-input.fuzz.test.js`
 * (`corpus/link_action/`).
 */

/** Longest form body read, in characters; the forms are a few fields. */
export const MAX_FORM_CHARS = 2048;

/** Largest issue number accepted (GitHub's are far smaller). */
export const MAX_ISSUE_NUMBER = 1_000_000_000;

/** The corrections a member can post. */
export const LINK_ACTIONS = /** @type {const} */ (["attach", "detach", "move", "merge"]);

/** A report ID as the index stores it: a lowercase UUID. */
const REPORT_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

/**
 * An integer form field.
 *
 * @param {string | null} text Field value.
 * @returns {number | null} The number when it is a plain decimal integer
 *   from 1 to `MAX_ISSUE_NUMBER`.
 */
function issueNumber(text) {
  if (text === null || !/^[1-9][0-9]{0,9}$/.test(text)) return null;
  const n = Number(text);
  return n <= MAX_ISSUE_NUMBER ? n : null;
}

/**
 * One parsed correction.
 *
 * @typedef {{action: "attach" | "detach" | "move" | "merge", issue: number,
 *   report: string | null, to: number | null}} LinkAction
 *   `issue` is the issue acted on; `report` names the report for `attach`,
 *   `detach` and `move` (null for `merge`); `to` is the other issue for
 *   `move` and `merge` (null otherwise).
 */

/**
 * Reads a correction from a posted form body.
 *
 * Accepts only a body of at most `MAX_FORM_CHARS` characters whose `action`
 * is one of `LINK_ACTIONS`, whose `issue` is an issue number, whose
 * `report` is a lowercase UUID when the action needs one, and whose `to` is
 * a different issue number when the action needs one. Fields the action
 * does not use are ignored, as is any other field.
 *
 * @param {unknown} body The form body (`application/x-www-form-urlencoded`).
 * @returns {LinkAction | null} The correction, or null when the body is not
 *   one.
 */
export function parseLinkAction(body) {
  if (typeof body !== "string" || body.length > MAX_FORM_CHARS) return null;
  let form;
  try {
    form = new URLSearchParams(body);
  } catch {
    return null;
  }
  const action = form.get("action");
  if (action === null || !LINK_ACTIONS.includes(/** @type {any} */ (action))) return null;
  const issue = issueNumber(form.get("issue"));
  if (issue === null) return null;
  const needsReport = action !== "merge";
  const needsTarget = action === "move" || action === "merge";
  const report = form.get("report");
  if (needsReport && (report === null || !REPORT_ID.test(report))) return null;
  const to = issueNumber(form.get("to"));
  if (needsTarget && (to === null || to === issue)) return null;
  return {
    action: /** @type {LinkAction["action"]} */ (action),
    issue,
    report: needsReport ? report : null,
    to: needsTarget ? to : null,
  };
}
