/**
 * Background work for the dashboard: the cron's issue and pull request sync,
 * the daily sweep, the owner's reindex from B2, and indexing a report right
 * after `/submit` stores it.
 *
 * The sync and the reindex run under the invocation's D1 call budget
 * (budget.js): each unit of work (an issue page, a GraphQL batch, a listing
 * page) is written in one or two batches that also save the cursor, and work
 * stops while enough budget is left for the invocation's last writes. What
 * is left over is picked up by the next run or press.
 */

import { metered } from "./budget.js";
import {
  all,
  getState,
  getStates,
  indexReport,
  reportInsert,
  setState,
  stateStatement,
  sweep,
  utcDayOf,
} from "./db.js";
import { installationToken, listDiagnosticsIssues, pullRequestsFor } from "./github.js";
import { parseIssueReportIds, reportIdOf } from "./report-ids.js";
import { summarizeReport } from "./signature.js";
import { listPage, MAX_STORED_REPORT_CHARS } from "./storage.js";

/** Most issue pages (100 each) one sync reads. */
const MAX_ISSUE_PAGES = 5;

/** Issues stored per D1 batch. */
const ISSUES_PER_BATCH = 50;

/** D1 calls one full issue page costs (100 issues, `ISSUES_PER_BATCH` each). */
const ISSUE_PAGE_CALLS = Math.ceil(100 / ISSUES_PER_BATCH);

/**
 * Most issues whose pull requests one sync refreshes. Issues past the limit
 * stay queued (`prs_synced_at` NULL) for the next run.
 */
const MAX_PR_REFRESH = 100;

/** Issues per GraphQL call, stored in one D1 batch. */
const PR_BATCH = 10;

/** D1 calls the sync keeps for its last write (the sync time). */
const SYNC_RESERVE = 1;

/** Report links inserted per statement (two bound values each). */
const LINK_CHUNK = 40;

/**
 * Most pull requests kept by number in one statement (one bound value each);
 * an issue with more has its links deleted and written again.
 */
const KEEP_CHUNK = 90;

/** Most stored reports one reindex request downloads. */
export const REINDEX_DOWNLOADS = 40;

/** Most listing pages (100 names each) one reindex request reads. */
const REINDEX_PAGES = 20;

/**
 * Most B2 requests (listing pages and downloads) one reindex request makes,
 * inside Workers Free's 50 subrequests per invocation with room for the
 * authorization call.
 */
const REINDEX_FETCHES = 45;

/** Names looked up per "already indexed?" query. */
const LOOKUP_CHUNK = 90;

/** D1 calls one listing page costs: two lookups and one write batch. */
const REINDEX_PAGE_CALLS = Math.ceil(100 / LOOKUP_CHUNK) + 1;

/**
 * D1 calls a reindex request keeps for the route's writes afterwards (the
 * result note and the access log).
 */
export const REINDEX_RESERVE = 2;

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
 * The statements that store one issue and the reports it lists, replacing
 * earlier links.
 *
 * @param {D1Database} db Database.
 * @param {object} issue Issue from the REST API.
 * @returns {D1PreparedStatement[]} Statements, empty for an invalid number.
 */
function issueStatements(db, issue) {
  const number = Number(issue.number);
  if (!Number.isSafeInteger(number) || number <= 0) return [];
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
  return stmts;
}

/**
 * Stores one issue and the reports it lists, replacing earlier links.
 *
 * @param {D1Database} db Database.
 * @param {object} issue Issue from the REST API.
 * @returns {Promise<void>}
 */
export async function storeIssue(db, issue) {
  const stmts = issueStatements(db, issue);
  if (stmts.length) await db.batch(stmts);
}

/**
 * Stores a page of issues and saves the sync cursor with the last of them,
 * in at most `ISSUE_PAGE_CALLS` batches.
 *
 * @param {D1Database} db Database.
 * @param {object[]} issues Issues from the REST API (at most 100).
 * @param {D1PreparedStatement[]} cursor Statements that save the cursor.
 * @returns {Promise<void>}
 */
async function storeIssuePage(db, issues, cursor) {
  const batches = [];
  for (let i = 0; i < issues.length; i += ISSUES_PER_BATCH) {
    batches.push(issues.slice(i, i + ISSUES_PER_BATCH).flatMap((issue) => issueStatements(db, issue)));
  }
  if (!batches.length) batches.push([]);
  batches[batches.length - 1].push(...cursor);
  for (const stmts of batches) await db.batch(stmts);
}

/**
 * The statements that replace the pull requests recorded for an issue and
 * mark it refreshed. Unchanged pull requests are left alone, so a refresh
 * that finds nothing new writes only the refresh time.
 *
 * @param {D1Database} db Database.
 * @param {number} number Issue number.
 * @param {{number: number, title: string, state: string, mergedAt: number | null}[]} prs
 *   Pull requests from `pullRequestsFor`.
 * @param {number} now Unix time in milliseconds.
 * @returns {D1PreparedStatement[]} Statements.
 */
function pullRequestStatements(db, number, prs, now) {
  const stmts = [];
  if (prs.length === 0 || prs.length > KEEP_CHUNK) {
    stmts.push(db.prepare("DELETE FROM issue_pr WHERE issue_number = ?").bind(number));
  } else {
    stmts.push(
      db
        .prepare(`DELETE FROM issue_pr WHERE issue_number = ? AND pr_number NOT IN (${prs.map(() => "?").join(", ")})`)
        .bind(number, ...prs.map((pr) => pr.number)),
    );
  }
  stmts.push(db.prepare("UPDATE issues SET prs_synced_at = ? WHERE number = ?").bind(now, number));
  for (const pr of prs) {
    stmts.push(
      db
        .prepare(
          `INSERT INTO issue_pr (issue_number, pr_number, title, state, merged_at) VALUES (?, ?, ?, ?, ?)
           ON CONFLICT (issue_number, pr_number) DO UPDATE SET title = excluded.title,
             state = excluded.state, merged_at = excluded.merged_at
           WHERE issue_pr.title IS NOT excluded.title OR issue_pr.state IS NOT excluded.state
             OR issue_pr.merged_at IS NOT excluded.merged_at`,
        )
        .bind(number, pr.number, pr.title, pr.state, pr.mergedAt),
    );
  }
  return stmts;
}

/**
 * The statements that forget an issue that lost the `diagnostics` label,
 * with its report and pull request links.
 *
 * @param {D1Database} db Database.
 * @param {number} number Issue number.
 * @returns {D1PreparedStatement[]} Statements.
 */
function dropStatements(db, number) {
  return [
    db.prepare("DELETE FROM report_issue WHERE issue_number = ?").bind(number),
    db.prepare("DELETE FROM issue_pr WHERE issue_number = ?").bind(number),
    db.prepare("DELETE FROM issues WHERE number = ?").bind(number),
  ];
}

/**
 * Cron: pulls diagnostics issues changed since the last run, then refreshes
 * pull requests and the label check for up to `MAX_PR_REFRESH` issues:
 * queued ones (changed, or not reached by a previous run) first, then every
 * other issue least recently refreshed first, open or closed, so no issue
 * can starve the rest. Issues that lost the label are dropped. Old rows are
 * swept first.
 *
 * Issue pages are read as one pass over everything changed since
 * `issues_since`, saving the page reached after each page. A pass longer
 * than one run's budget continues in the next run, which reads the last
 * page again (issues updated meanwhile move to the end of the listing and
 * shift the rest back). Once the pass is drained, `issues_since` moves to
 * one second before the newest update seen, so issues sharing that second
 * are read again next run; re-reading a stored issue keeps its place in the
 * refresh queue.
 *
 * @param {{DASHBOARD_DB: D1Database}} env Worker bindings; a handle already
 *   wrapped by `metered` shares that invocation's budget.
 * @param {import("./github.js").GitHubConfig} cfg Settings.
 * @param {number} now Unix time in milliseconds.
 * @returns {Promise<{issues: number, refreshed: number}>} Issues stored and
 *   issues whose pull requests were refreshed.
 */
export async function syncIssues(env, cfg, now) {
  const db = metered(env.DASHBOARD_DB);
  const budget = db.budget;
  await sweep(db, now);
  const token = await installationToken(cfg, now);
  const state = await getStates(db, ["issues_since", "issues_page", "issues_newest"]);
  const since = state.get("issues_since") ?? "";
  const startPage = Math.max(1, Number.parseInt(state.get("issues_page") ?? "1", 10) || 1);
  let newest = state.get("issues_newest") ?? since;
  let stored = 0;
  for (let page = startPage; page < startPage + MAX_ISSUE_PAGES; page++) {
    if (budget.left() < ISSUE_PAGE_CALLS + SYNC_RESERVE) break;
    const result = await listDiagnosticsIssues(cfg, token, since, page);
    if (result.newest > newest) newest = result.newest;
    let cursor;
    if (result.full) {
      const more = page + 1 < startPage + MAX_ISSUE_PAGES && budget.left() >= 2 * ISSUE_PAGE_CALLS + SYNC_RESERVE;
      // Stopping mid-pass: read this page again next run, unless it is the
      // only page this run read, so every run moves forward.
      const resumeAt = more || page === startPage ? page + 1 : page;
      cursor = [stateStatement(db, "issues_page", String(resumeAt)), stateStatement(db, "issues_newest", newest)];
    } else {
      const resume = isoMs(newest);
      const back = resume === null ? since : new Date(resume - 1000).toISOString();
      const next = back > since ? back : since;
      cursor = [
        stateStatement(db, "issues_since", next || null),
        stateStatement(db, "issues_page", null),
        stateStatement(db, "issues_newest", null),
      ];
    }
    await storeIssuePage(db, result.issues, cursor);
    stored += result.issues.length;
    if (!result.full) break;
  }

  let refreshed = 0;
  if (budget.left() > 1 + SYNC_RESERVE) {
    const queue = await all(
      db,
      `SELECT number FROM issues
       ORDER BY prs_synced_at IS NOT NULL, prs_synced_at, number
       LIMIT ?`,
      MAX_PR_REFRESH,
    );
    const numbers = queue.map((r) => Number(r.number)).filter((n) => Number.isSafeInteger(n) && n > 0);
    for (let i = 0; i < numbers.length && budget.left() > SYNC_RESERVE; i += PR_BATCH) {
      const batch = numbers.slice(i, i + PR_BATCH);
      const prs = await pullRequestsFor(cfg, token, batch);
      const stmts = [];
      for (const n of batch) {
        // No data for an issue (deleted, transferred): keep its old links but
        // move it to the back of the queue so it cannot starve the rest.
        if (!prs.has(n)) {
          stmts.push(db.prepare("UPDATE issues SET prs_synced_at = ? WHERE number = ?").bind(now, n));
          continue;
        }
        const got = prs.get(n);
        stmts.push(...(got.labeled ? pullRequestStatements(db, n, got.prs, now) : dropStatements(db, n)));
        refreshed += 1;
      }
      await db.batch(stmts);
    }
  }
  await setState(db, "issues_synced_at", String(now));
  return { issues: stored, refreshed };
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
 * per request, resuming where the last request stopped. Each listing page's
 * new reports, their days' ledger counts and the cursor are written in one
 * batch, and pages stop while `REINDEX_RESERVE` D1 calls are left for the
 * route and before `REINDEX_FETCHES` B2 requests are spent.
 *
 * @param {{DASHBOARD_DB: D1Database}} env Worker bindings; a handle already
 *   wrapped by `metered` shares that invocation's budget.
 * @param {import("./storage.js").B2Access} b2 B2 helpers.
 * @returns {Promise<{indexed: number, done: boolean}>} Reports added and
 *   whether the listing reached its end (the cursor then starts over).
 */
export async function reindexSlice(env, b2) {
  const db = metered(env.DASHBOARD_DB);
  const budget = db.budget;
  const auth = await b2.authorize();
  let start = (await getState(db, "reindex_cursor")) || null;
  let indexed = 0;
  let downloads = 0;
  let fetches = 0;
  let done = false;
  for (let pages = 0; pages < REINDEX_PAGES && downloads < REINDEX_DOWNLOADS; pages++) {
    if (fetches >= REINDEX_FETCHES || budget.left() < REINDEX_PAGE_CALLS + REINDEX_RESERVE) break;
    fetches += 1;
    const page = await listPage(b2, auth, { prefix: PREFIX, startFileName: start, maxFileCount: 100 });
    const candidates = page.files
      .map((f) => ({ ...f, reportId: reportIdOf(f.fileName) }))
      .filter((f) => f.reportId);
    const have = new Set();
    for (let i = 0; i < candidates.length; i += LOOKUP_CHUNK) {
      const chunk = candidates.slice(i, i + LOOKUP_CHUNK);
      const rows = await all(
        db,
        `SELECT report_id FROM reports WHERE report_id IN (${chunk.map(() => "?").join(", ")})`,
        ...chunk.map((c) => c.reportId),
      );
      for (const r of rows) have.add(r.report_id);
    }
    const rows = [];
    let stoppedAt = null;
    for (const f of candidates) {
      if (have.has(f.reportId)) continue;
      if (downloads >= REINDEX_DOWNLOADS || fetches >= REINDEX_FETCHES) {
        stoppedAt = f.fileName;
        break;
      }
      downloads += 1;
      fetches += 1;
      const text = await b2.downloadById(auth, f.fileId);
      let stored = null;
      try {
        stored = text.length <= MAX_STORED_REPORT_CHARS ? JSON.parse(text) : null;
      } catch {
        stored = null;
      }
      if (!stored || typeof stored !== "object" || stored.report_id !== f.reportId) continue;
      const receivedAt = Number.isSafeInteger(stored.received_at_unix_ms) ? stored.received_at_unix_ms : f.uploadTimestamp;
      if (!Number.isFinite(new Date(receivedAt).getTime())) continue;
      rows.push({ reportId: f.reportId, objectKey: f.fileName, receivedAt, summary: await summarizeReport(stored) });
    }
    if (stoppedAt) start = stoppedAt;
    else if (!page.nextFileName) {
      start = null;
      done = true;
    } else start = page.nextFileName;

    // A report a concurrent /submit indexed meanwhile is ignored by the
    // insert but still counted in the ledger, which only errs toward the cap.
    const perDay = new Map();
    for (const row of rows) {
      const day = utcDayOf(row.receivedAt);
      perDay.set(day, (perDay.get(day) ?? 0) + 1);
    }
    await db.batch([
      ...rows.map((row) => reportInsert(db, row)),
      ...[...perDay].map(([day, n]) =>
        db
          .prepare("INSERT INTO index_days (day, rows) VALUES (?, ?) ON CONFLICT (day) DO UPDATE SET rows = rows + excluded.rows")
          .bind(day, n),
      ),
      stateStatement(db, "reindex_cursor", start),
    ]);
    indexed += rows.length;
    if (stoppedAt || done) break;
  }
  return { indexed, done };
}
