/**
 * Triage state in D1: members mark reports seen or noise, mute error
 * signatures, and the index ends a version-bound mute when a report from
 * that version arrives. Forms are parsed in `triage-forms.ts`.
 *
 * A report needs triage while it has no state, no issue lists it and no
 * mute on its signature is in force (`ReportFilter.triage` "needs" in
 * `db.ts`). A mute also keeps its reports out of the ingest Action's
 * unfiled pull (`dashboardFiledReportIds` in `routes.ts`), so nothing files
 * an issue for them.
 */

import { compareSemver } from "../version-filter.ts";
import { all, first, run } from "./db.ts";
import type { MuteAction, TriageAction } from "./triage-forms.ts";

/** One day in milliseconds. */
const DAY_MS = 86_400_000;

/** Report IDs bound per statement. */
const CHUNK = 90;

/**
 * Sets the triage state of reports. "clear" puts them back in the queue.
 * Reports that are not indexed are skipped.
 *
 * @param db - Dashboard database.
 * @param reports - Lowercase report UUIDs.
 * @param action - The state to set.
 * @param login - The member's login.
 * @param now - Unix time in milliseconds.
 * @returns How many reports changed.
 */
export async function setTriageState(
  db: D1Database,
  reports: string[],
  action: TriageAction,
  login: string,
  now: number,
): Promise<number> {
  const stmts = [];
  for (let i = 0; i < reports.length; i += CHUNK) {
    const chunk = reports.slice(i, i + CHUNK);
    const marks = chunk.map(() => "?").join(", ");
    stmts.push(
      action === "clear"
        ? db
            .prepare(
              `UPDATE reports SET triage_state = '', triaged_by = NULL, triaged_at = NULL
               WHERE report_id IN (${marks}) AND triage_state <> ''`,
            )
            .bind(...chunk)
        : db
            .prepare(
              `UPDATE reports SET triage_state = ?, triaged_by = ?, triaged_at = ?
               WHERE report_id IN (${marks}) AND triage_state <> ?`,
            )
            .bind(action, login, now, ...chunk, action),
    );
  }
  if (stmts.length === 0) return 0;
  const results = await db.batch(stmts);
  return results.reduce((n, r) => n + Number(r.meta?.changes ?? 0), 0);
}

/** A mute in force, as the report page shows it. */
export type SignatureMute = {
  /** Why it was muted. */
  reason: string;
  /** Member who muted it. */
  muted_by: string;
  /** When, Unix ms. */
  muted_at: number;
  /** When it ends, Unix ms, or null. */
  until: number | null;
  /** Version whose reports end it, or null. */
  until_version: string | null;
};

/**
 * Mutes or unmutes a signature. A new mute replaces an older one.
 *
 * @param db - Dashboard database.
 * @param a - The parsed form.
 * @param login - The member's login.
 * @param now - Unix time in milliseconds.
 */
export async function applyMute(
  db: D1Database,
  a: MuteAction,
  login: string,
  now: number,
): Promise<void> {
  if (a.action === "unmute") {
    await run(db, "DELETE FROM signature_mutes WHERE signature = ?", a.signature);
    return;
  }
  await run(
    db,
    `INSERT OR REPLACE INTO signature_mutes (signature, reason, muted_by, muted_at, until, until_version)
     VALUES (?, ?, ?, ?, ?, ?)`,
    a.signature,
    a.reason,
    login,
    now,
    a.days === null ? null : now + a.days * DAY_MS,
    a.untilVersion,
  );
}

/**
 * The mute in force on a signature.
 *
 * @param db - Dashboard database.
 * @param signature - 16 hex characters.
 * @param now - Unix time in milliseconds.
 * @returns The mute, or null when none is in force.
 */
export async function getMute(
  db: D1Database,
  signature: string,
  now: number,
): Promise<SignatureMute | null> {
  return first<SignatureMute>(
    db,
    `SELECT reason, muted_by, muted_at, until, until_version FROM signature_mutes
     WHERE signature = ? AND (until IS NULL OR until > ?)`,
    signature,
    now,
  );
}

/**
 * Ends a version-bound mute once a report from that version or a later one
 * arrives with the signature: the failure is back where it was meant to be
 * fixed, so it needs eyes again. Run after a report is indexed.
 *
 * @param db - Dashboard database.
 * @param signature - The report's signature, or null.
 * @param version - The report's version.
 * @returns True when a mute was ended.
 */
export async function endMuteReachedBy(
  db: D1Database,
  signature: string | null,
  version: string,
): Promise<boolean> {
  if (signature === null) return false;
  const mute = await first<{ until_version: string | null }>(
    db,
    "SELECT until_version FROM signature_mutes WHERE signature = ?",
    signature,
  );
  if (!mute?.until_version) return false;
  if ((compareSemver(version, mute.until_version) ?? -1) < 0) return false;
  await run(
    db,
    "DELETE FROM signature_mutes WHERE signature = ? AND until_version = ?",
    signature,
    mute.until_version,
  );
  return true;
}

/**
 * {@link endMuteReachedBy} for a batch of reports indexed together (the
 * owner's reindex): one read for the batch's muted signatures, then one
 * batch of deletes for the mutes their versions reached.
 *
 * @param db - Dashboard database.
 * @param reports - Each indexed report's signature (or null) and version.
 * @returns How many mutes were ended.
 */
export async function endMutesReachedBy(
  db: D1Database,
  reports: Array<{ signature: string | null; version: string }>,
): Promise<number> {
  const newest = new Map<string, string[]>();
  for (const r of reports) {
    if (r.signature === null) continue;
    newest.set(r.signature, [...(newest.get(r.signature) ?? []), r.version]);
  }
  const signatures = [...newest.keys()].slice(0, CHUNK);
  if (signatures.length === 0) return 0;
  const mutes = await all(
    db,
    `SELECT signature, until_version FROM signature_mutes
     WHERE until_version IS NOT NULL AND signature IN (${signatures.map(() => "?").join(", ")})`,
    ...signatures,
  );
  const ended = mutes.filter((m) => {
    const until = typeof m.until_version === "string" ? m.until_version : "";
    const versions = newest.get(String(m.signature)) ?? [];
    return until !== "" && versions.some((v) => (compareSemver(v, until) ?? -1) >= 0);
  });
  if (ended.length === 0) return 0;
  await db.batch(
    ended.map((m) =>
      db
        .prepare("DELETE FROM signature_mutes WHERE signature = ? AND until_version = ?")
        .bind(m.signature, m.until_version),
    ),
  );
  return ended.length;
}

/**
 * Distinct installs behind reports with a signature, counted by install key
 * (only reports from installs that share usage carry one). Keys change each
 * month, so an install counts once per month it reported in.
 *
 * @param db - Dashboard database.
 * @param signature - 16 hex characters.
 * @returns The count.
 */
export async function signatureInstalls(db: D1Database, signature: string): Promise<number> {
  const row = await first(
    db,
    "SELECT COUNT(DISTINCT install_key) AS n FROM reports WHERE signature = ? AND install_key <> ''",
    signature,
  );
  return Number(row?.n ?? 0);
}
