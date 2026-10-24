/**
 * Background work for the dashboard: the cron's issue and pull request sync,
 * the daily sweep, the owner's reindex from B2, and indexing a report right
 * after `/submit` stores it.
 */

import {
  all,
  getState,
  indexReport,
  run,
  setState,
  sweep,
} from "./db.js";
import { installationToken, listDiagnosticsIssues, pullRequestsFor } from "./github.js";
import { parseIssueReportIds, reportIdOf } from "./report-ids.js";
import { summarizeReport } from "./signature.js";
import { listPage, MAX_STORED_REPORT_CHARS } from "./storage.js";

/** Most issue pages (100 each) one sync reads. */
const MAX_ISSUE_PAGES = 5;

/**
 * Most issues whose pull requests one sync refreshes. Issues past the limit
 * stay queued (`prs_synced_at` NULL) for the next run.
 */
const MAX_PR_REFRESH = 100;

/** Issues per GraphQL call. */
const PR_BATCH = 10;

/** Report links inserted per statement (two bound values each). */
const LINK_CHUNK = 40;

/** Most stored reports one reindex request downloads. */
export const REINDEX_DOWNLOADS = 40;

/** Most listing pages (100 names each) one reindex request reads. */
const REINDEX_PAGES = 20;

/** Prefix of every stored report. */
const PREFIX = "diagnostics/";

/**
 * Parses an ISO time from GitHub.
 *
 * @param {unknown} s ISO-8601 text.
 * @returns {number | null} Unix time in milliseconds.
 */
function isoMs(s) {
  const n = typeof s === "string" ? Date.parse(s) : NaN;
  return Number.isFinite(n) ? n : null;
}

/**
 * Stores one issue and the reports it lists, replacing earlier links.
 *
 * @param {D1Database} db Database.
 * @param {object} issue Issue from the REST API.
 * @returns {Promise<void>}
 */
export async function storeIssue(db, issue) {
  const number = Number(issue.number);
  if (!Number.isSafeInteger(number) || number <= 0) return;
  const { kind, ids } = parseIssueReportIds(issue.body);
  const stmts = [
    db
      .prepare(
        `INSERT INTO issues (number, title, state, kind, created_at, updated_at, closed_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (number) DO UPDATE SET title = excluded.title, state = excluded.state,
           kind = excluded.kind, updated_at = excluded.updated_at, closed_at = excluded.closed_at,
           prs_synced_at = CASE WHEN issues.updated_at = excluded.updated_at
             THEN issues.prs_synced_at ELSE NULL END`,
      )
      .bind(
        number,
        String(issue.title ?? "").slice(0, 300),
        issue.state === "closed" ? "closed" : "open",
        kind,
        isoMs(issue.created_at) ?? 0,
        isoMs(issue.updated_at) ?? 0,
        isoMs(issue.closed_at),
      ),
    db.prepare("DELETE FROM report_issue WHERE issue_number = ?").bind(number),
  ];
  for (let i = 0; i < ids.length; i += LINK_CHUNK) {
    const chunk = ids.slice(i, i + LINK_CHUNK);
    stmts.push(
      db
        .prepare(`INSERT OR IGNORE INTO report_issue (report_id, issue_number) VALUES ${chunk.map(() => "(?, ?)").join(", ")}`)
        .bind(...chunk.flatMap((id) => [id, number])),
    );
  }
  await db.batch(stmts);
}

/**
 * Replaces the pull requests recorded for an issue and marks it refreshed.
 *
 * @param {D1Database} db Database.
 * @param {number} number Issue number.
 * @param {{number: number, title: string, state: string, mergedAt: number | null}[]} prs
 *   Pull requests from `pullRequestsFor`.
 * @param {number} now Unix time in milliseconds.
 * @returns {Promise<void>}
 */
async function storePullRequests(db, number, prs, now) {
  const stmts = [
    db.prepare("DELETE FROM issue_pr WHERE issue_number = ?").bind(number),
    db.prepare("UPDATE issues SET prs_synced_at = ? WHERE number = ?").bind(now, number),
  ];
  for (const pr of prs) {
    stmts.push(
      db
        .prepare("INSERT OR REPLACE INTO issue_pr (issue_number, pr_number, title, state, merged_at) VALUES (?, ?, ?, ?, ?)")
        .bind(number, pr.number, pr.title, pr.state, pr.mergedAt),
    );
  }
  await db.batch(stmts);
}

/**
 * Forgets an issue that lost the `diagnostics` label, with its report and
 * pull request links.
 *
 * @param {D1Database} db Database.
 * @param {number} number Issue number.
 * @returns {Promise<void>}
 */
async function dropIssue(db, number) {
  await db.batch([
    db.prepare("DELETE FROM report_issue WHERE issue_number = ?").bind(number),
    db.prepare("DELETE FROM issue_pr WHERE issue_number = ?").bind(number),
    db.prepare("DELETE FROM issues WHERE number = ?").bind(number),
  ]);
}

/**
 * Cron: pulls diagnostics issues changed since the last run, then refreshes
 * pull requests and the label check for up to `MAX_PR_REFRESH` issues:
 * queued ones (changed, or not reached by a previous run) first, then every
 * other issue least recently refreshed first, open or closed, so no issue
 * can starve the rest. Issues that lost the label are dropped. Old rows are
 * swept first.
 *
 * @param {{DASHBOARD_DB: D1Database}} env Worker bindings.
 * @param {import("./github.js").GitHubConfig} cfg Settings.
 * @param {number} now Unix time in milliseconds.
 * @returns {Promise<{issues: number, refreshed: number}>} Issues stored and
 *   issues whose pull requests were refreshed.
 */
export async function syncIssues(env, cfg, now) {
  const db = env.DASHBOARD_DB;
  await sweep(db, now);
  const token = await installationToken(cfg, now);
  const since = (await getState(db, "issues_since")) ?? "";
  const startPage = Math.max(1, Number.parseInt((await getState(db, "issues_page")) ?? "1", 10) || 1);
  const stored = new Set();
  let newest = since;
  let drained = false;
  for (let page = startPage; page < startPage + MAX_ISSUE_PAGES; page++) {
    const result = await listDiagnosticsIssues(cfg, token, since, page);
    for (const issue of result.issues) {
      await storeIssue(db, issue);
      stored.add(Number(issue.number));
    }
    if (result.newest > newest) newest = result.newest;
    if (!result.full) {
      drained = true;
      break;
    }
  }
  // Resume one second before the newest update seen, so issues sharing that
  // second past a full last page are read next run. Re-reading the ones
  // already stored is harmless: an unchanged issue keeps its place in the
  // refresh queue. When every page read sits inside that second (more than
  // a run's worth of issues updated together), the time cannot move, so the
  // next run keeps it and continues from the last page read instead (read
  // again, in case updates shifted the order).
  const resume = isoMs(newest);
  const next = resume === null ? since : new Date(resume - 1000).toISOString();
  if (!drained && since && next <= since) {
    await setState(db, "issues_page", String(startPage + MAX_ISSUE_PAGES - 1));
  } else {
    if (next) await setState(db, "issues_since", next);
    await setState(db, "issues_page", "1");
  }

  const queue = await all(
    db,
    `SELECT number FROM issues
     ORDER BY prs_synced_at IS NOT NULL, prs_synced_at, number
     LIMIT ?`,
    MAX_PR_REFRESH,
  );
  const numbers = queue.map((r) => Number(r.number)).filter((n) => Number.isSafeInteger(n) && n > 0);
  let refreshed = 0;
  for (let i = 0; i < numbers.length; i += PR_BATCH) {
    const batch = numbers.slice(i, i + PR_BATCH);
    const prs = await pullRequestsFor(cfg, token, batch);
    for (const n of batch) {
      // No data for an issue (deleted, transferred): keep its old links but
      // move it to the back of the queue so it cannot starve the rest.
      if (!prs.has(n)) {
        await run(db, "UPDATE issues SET prs_synced_at = ? WHERE number = ?", now, n);
        continue;
      }
      const got = prs.get(n);
      if (got.labeled) await storePullRequests(db, n, got.prs, now);
      else await dropIssue(db, n);
      refreshed += 1;
    }
  }
  await setState(db, "issues_synced_at", String(now));
  return { issues: stored.size, refreshed };
}

/**
 * Indexes a report `/submit` just stored. Failures are swallowed by the
 * caller: the upload already succeeded and the reindex can pick it up.
 *
 * @param {D1Database} db Database.
 * @param {object} enriched The object written to B2.
 * @param {string} objectKey Its key.
 * @param {number} cap Daily row cap.
 * @returns {Promise<"indexed" | "capped">} What happened.
 */
export async function indexSubmitted(db, enriched, objectKey, cap) {
  return indexReport(
    db,
    {
      reportId: enriched.report_id,
      objectKey,
      receivedAt: enriched.received_at_unix_ms,
      summary: await summarizeReport(enriched),
    },
    cap,
  );
}

/**
 * Owner action: indexes stored reports the index is missing, a bounded slice
 * per request, resuming where the last request stopped.
 *
 * @param {{DASHBOARD_DB: D1Database}} env Worker bindings.
 * @param {import("./storage.js").B2Access} b2 B2 helpers.
 * @returns {Promise<{indexed: number, done: boolean}>} Reports added and
 *   whether the listing reached its end (the cursor then starts over).
 */
export async function reindexSlice(env, b2) {
  const db = env.DASHBOARD_DB;
  const auth = await b2.authorize();
  let start = (await getState(db, "reindex_cursor")) || null;
  let indexed = 0;
  let downloads = 0;
  let done = false;
  for (let pages = 0; pages < REINDEX_PAGES && downloads < REINDEX_DOWNLOADS; pages++) {
    const page = await listPage(b2, auth, { prefix: PREFIX, startFileName: start, maxFileCount: 100 });
    const candidates = page.files
      .map((f) => ({ ...f, reportId: reportIdOf(f.fileName) }))
      .filter((f) => f.reportId);
    const have = new Set();
    for (let i = 0; i < candidates.length; i += 90) {
      const chunk = candidates.slice(i, i + 90);
      const rows = await all(
        db,
        `SELECT report_id FROM reports WHERE report_id IN (${chunk.map(() => "?").join(", ")})`,
        ...chunk.map((c) => c.reportId),
      );
      for (const r of rows) have.add(r.report_id);
    }
    let stoppedAt = null;
    for (const f of candidates) {
      if (have.has(f.reportId)) continue;
      if (downloads >= REINDEX_DOWNLOADS) {
        stoppedAt = f.fileName;
        break;
      }
      downloads += 1;
      const text = await b2.downloadById(auth, f.fileId);
      let stored = null;
      try {
        stored = text.length <= MAX_STORED_REPORT_CHARS ? JSON.parse(text) : null;
      } catch {
        stored = null;
      }
      if (!stored || typeof stored !== "object" || stored.report_id !== f.reportId) continue;
      const receivedAt = Number.isSafeInteger(stored.received_at_unix_ms) ? stored.received_at_unix_ms : f.uploadTimestamp;
      await indexReport(
        db,
        { reportId: f.reportId, objectKey: f.fileName, receivedAt, summary: await summarizeReport(stored) },
        Number.MAX_SAFE_INTEGER,
      );
      indexed += 1;
    }
    if (stoppedAt) {
      start = stoppedAt;
      break;
    }
    if (!page.nextFileName) {
      start = null;
      done = true;
      break;
    }
    start = page.nextFileName;
  }
  if (start) await setState(db, "reindex_cursor", start);
  else await run(db, "DELETE FROM sync_state WHERE key = 'reindex_cursor'");
  return { indexed, done };
}
