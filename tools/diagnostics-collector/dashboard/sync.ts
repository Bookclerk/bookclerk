/**
 * Background work for the dashboard: the cron's issue and pull request sync,
 * the daily sweep, the owner's reindex from B2, and indexing a report right
 * after `/submit` stores it.
 *
 * The sync and the reindex run under the invocation's D1 call budget
 * (budget.ts): each unit of work (an issue page, a GraphQL batch, a listing
 * page) is written in one or two batches that also save the cursor, and work
 * stops while enough budget is left for the invocation's last writes. What
 * is left over is picked up by the next run or press.
 */

import { metered, type MeteredD1 } from "./budget.ts";
import {
  all,
  getState,
  LATEST_FIX,
  getStates,
  indexReport,
  reportInsert,
  setState,
  stateStatement,
  sweep,
  utcDayOf,
} from "./db.ts";
import {
  fetchIssue,
  installationToken,
  listDiagnosticsIssues,
  listReleases,
  pullRequestsFor,
  tagContains,
  type GitHubConfig,
} from "./github.ts";
import { parseIssueReportIds, reportIdOf } from "./report-ids.ts";
import { summarizeReport } from "./signature.ts";
import { listPage, MAX_STORED_REPORT_CHARS, type B2Access } from "./storage.ts";
import { normalizeVersion, parseSemver } from "../version-filter.ts";
import type { DashboardEnv } from "../env.ts";
import { isRecordLike, isSafeInteger, type JsonObject } from "../json.ts";

/** A release's place in the order the fix lookup walks releases. */
type Release = {
  /** Publish time, Unix ms. */
  publishedAt: number;
  /** Tag name. */
  tag: string;
};

/** The object `/submit` writes to B2, as the index reads it. */
export type SubmittedReport = JsonObject & {
  /** Lowercase report UUID. */
  report_id: string;
  /** Receipt time, Unix ms. */
  received_at_unix_ms: number;
};

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

/**
 * D1 calls the sync keeps for its last write (the sync time), the live
 * update's before-and-after link fingerprints and, when an owner runs it from
 * the dashboard, the route's note and access-log writes.
 */
const SYNC_RESERVE = 5;

/** Waits, in ms, before re-reading an issue GitHub said it does not have. */
const NOT_FOUND_RETRY_MS = [1000, 2000];

/** Report links inserted per statement (two bound values each). */
const LINK_CHUNK = 40;

/** Most issues whose fixed-in version one sync resolves against releases. */
const FIX_RESOLVE_MAX = 5;

/** Most release tags compared against one issue's fix commit per sync. */
const FIX_COMPARES = 3;

/**
 * D1 calls the fixed-in step needs: the label read, the pending check, the
 * candidate read and one write batch.
 */
const FIX_CALLS = 4;

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
 * @param s - ISO-8601 text.
 * @returns Unix time in milliseconds.
 */
function isoMs(s: unknown): number | null {
  const n = typeof s === "string" ? Date.parse(s) : NaN;
  return Number.isFinite(n) ? n : null;
}

/**
 * The statements that store one issue and the reports it lists, replacing
 * the links an earlier copy of the body made.
 *
 * @param db - Database.
 * @param issue - Issue from the REST API.
 * @returns Statements, empty for an invalid number.
 */
function issueStatements(db: D1Database, issue: JsonObject): D1PreparedStatement[] {
  const number = Number(issue.number);
  if (!Number.isSafeInteger(number) || number <= 0) return [];
  const { kind, ids, regressionOf } = parseIssueReportIds(issue.body);
  const stmts = [
    db
      .prepare(
        `INSERT INTO issues (number, title, state, kind, created_at, updated_at, closed_at, fixed_in_label, regression_of)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (number) DO UPDATE SET title = excluded.title, state = excluded.state,
           kind = excluded.kind, updated_at = excluded.updated_at, closed_at = excluded.closed_at,
           fixed_in_label = excluded.fixed_in_label, regression_of = excluded.regression_of,
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
        typeof issue.fixed_in_label === "string" ? issue.fixed_in_label : null,
        // An issue cannot be a regression of itself or of a later issue.
        regressionOf !== null && regressionOf < number ? regressionOf : null,
      ),
    // Only the links the body lists are the body's to replace; links made by
    // signature, triage or a member live in the ledger alone.
    db.prepare("DELETE FROM report_issue WHERE issue_number = ? AND source = 'filed'").bind(number),
  ];
  for (let i = 0; i < ids.length; i += LINK_CHUNK) {
    const chunk = ids.slice(i, i + LINK_CHUNK);
    // A report linked another way that the body now lists becomes `filed`;
    // one a member excluded stays excluded.
    stmts.push(
      db
        .prepare(
          `INSERT INTO report_issue (report_id, issue_number, source, linked_by, linked_at)
           VALUES ${chunk.map(() => "(?, ?, 'filed', NULL, NULL)").join(", ")}
           ON CONFLICT (report_id, issue_number) DO UPDATE SET source = 'filed', linked_by = NULL, linked_at = NULL
           WHERE report_issue.source <> 'excluded'`,
        )
        .bind(...chunk.flatMap((id) => [id, number])),
    );
  }
  return stmts;
}

/**
 * Stores one issue and the reports it lists, replacing earlier links.
 *
 * @param db - Database.
 * @param issue - Issue from the REST API.
 */
export async function storeIssue(db: D1Database, issue: JsonObject): Promise<void> {
  const stmts = issueStatements(db, issue);
  if (stmts.length) await db.batch(stmts);
}

/**
 * Stores a page of issues and saves the sync cursor with the last of them,
 * in at most `ISSUE_PAGE_CALLS` batches.
 *
 * @param db - Database.
 * @param issues - Issues from the REST API (at most 100).
 * @param cursor - Statements that save the cursor.
 */
async function storeIssuePage(
  db: D1Database,
  issues: JsonObject[],
  cursor: D1PreparedStatement[],
): Promise<void> {
  const batches: D1PreparedStatement[][] = [];
  for (let i = 0; i < issues.length; i += ISSUES_PER_BATCH) {
    batches.push(
      issues.slice(i, i + ISSUES_PER_BATCH).flatMap((issue) => issueStatements(db, issue)),
    );
  }
  const last = batches.at(-1) ?? [];
  if (!batches.length) batches.push(last);
  last.push(...cursor);
  for (const stmts of batches) await db.batch(stmts);
}

/**
 * The statements that replace the pull requests recorded for an issue and
 * mark it refreshed. Unchanged pull requests are left alone, so a refresh
 * that finds nothing new writes only the refresh time.
 *
 * @param db - Database.
 * @param number - Issue number.
 * @param prs - Pull requests from `pullRequestsFor`.
 * @param now - Unix time in milliseconds.
 * @returns Statements.
 */
function pullRequestStatements(
  db: D1Database,
  number: number,
  prs: {
    number: number;
    title: string;
    state: string;
    mergedAt: number | null;
    mergeSha: string | null;
  }[],
  now: number,
): D1PreparedStatement[] {
  const stmts: D1PreparedStatement[] = [];
  if (prs.length === 0 || prs.length > KEEP_CHUNK) {
    stmts.push(db.prepare("DELETE FROM issue_pr WHERE issue_number = ?").bind(number));
  } else {
    stmts.push(
      db
        .prepare(
          `DELETE FROM issue_pr WHERE issue_number = ? AND pr_number NOT IN (${prs.map(() => "?").join(", ")})`,
        )
        .bind(number, ...prs.map((pr) => pr.number)),
    );
  }
  stmts.push(db.prepare("UPDATE issues SET prs_synced_at = ? WHERE number = ?").bind(now, number));
  for (const pr of prs) {
    stmts.push(
      db
        .prepare(
          `INSERT INTO issue_pr (issue_number, pr_number, title, state, merged_at, merge_sha) VALUES (?, ?, ?, ?, ?, ?)
           ON CONFLICT (issue_number, pr_number) DO UPDATE SET title = excluded.title,
             state = excluded.state, merged_at = excluded.merged_at, merge_sha = excluded.merge_sha
           WHERE issue_pr.title IS NOT excluded.title OR issue_pr.state IS NOT excluded.state
             OR issue_pr.merged_at IS NOT excluded.merged_at OR issue_pr.merge_sha IS NOT excluded.merge_sha`,
        )
        .bind(number, pr.number, pr.title, pr.state, pr.mergedAt, pr.mergeSha ?? null),
    );
  }
  return stmts;
}

/**
 * The statements that forget an issue that lost the `diagnostics` label,
 * with its report and pull request links.
 *
 * @param db - Database.
 * @param number - Issue number.
 * @returns Statements.
 */
function dropStatements(db: D1Database, number: number): D1PreparedStatement[] {
  return [
    db.prepare("DELETE FROM report_issue WHERE issue_number = ?").bind(number),
    db.prepare("DELETE FROM issue_pr WHERE issue_number = ?").bind(number),
    db.prepare("DELETE FROM issue_fix WHERE issue_number = ?").bind(number),
    db.prepare("DELETE FROM issues WHERE number = ?").bind(number),
  ];
}

/**
 * Records which version fixed each closed issue, in at most `FIX_CALLS` D1
 * calls and, when any issue needs a look, one release listing plus up to
 * `FIX_RESOLVE_MAX` × `FIX_COMPARES` compare calls to GitHub:
 *
 * - an issue whose `fixed-in:<version>` label names a version other than
 *   its latest row gets a `manual` row and is otherwise left alone; when
 *   the label is gone, the rows it wrote go too, so the lookup resumes;
 * - up to `FIX_RESOLVE_MAX` closed issues with a merged pull request whose
 *   merge commit is known, and no row yet or an `unreleased` row with
 *   releases not yet compared, get a `release` row naming the oldest release
 *   that contains the commit among the next `FIX_COMPARES` releases
 *   published after the merge and after the last one compared, or an
 *   `unreleased` row recording how far the comparison got (publish time
 *   and tag of the last release compared), so the next sync continues from
 *   there whether or not a new release appeared. Only
 *   releases whose tag is a version are considered, and the whole release
 *   list is read (`listReleases`); when it is longer than the pages read,
 *   nothing is resolved and a `fixed-in` label settles the version, rather
 *   than placing a fix in a release that may not be the first. A release
 *   row is final: a later pull request on the issue does not move it.
 *
 * A GitHub failure here is logged and leaves the rows as they were; the
 * next sync tries again.
 *
 * @param db - Metered database.
 * @param cfg - Settings.
 * @param token - Installation token.
 * @param now - Unix time in milliseconds.
 * @returns Statements written.
 */
export async function resolveFixes(
  db: MeteredD1,
  cfg: GitHubConfig,
  token: string,
  now: number,
): Promise<number> {
  const stmts: D1PreparedStatement[] = [];
  const insert = (
    issue: number,
    source: "manual" | "unreleased" | "release",
    fixedIn: string | null,
    prNumber: number | null,
    mergeSha: string | null,
    by: "label" | "sync",
    checked: Release | null = null,
  ): D1PreparedStatement =>
    db
      .prepare(
        `INSERT OR IGNORE INTO issue_fix (issue_number, recorded_at, source, fixed_in, pr_number, merge_sha, recorded_by, checked_until, checked_tag)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`,
      )
      .bind(
        issue,
        now,
        source,
        fixedIn,
        prNumber,
        mergeSha,
        by,
        checked?.publishedAt ?? null,
        checked?.tag ?? null,
      );
  // Labels: 'set' needs a manual row; 'gone' has label rows to drop.
  const labels = await all(
    db,
    `WITH ${LATEST_FIX}
     SELECT i.number AS number, i.fixed_in_label AS label,
       CASE WHEN i.fixed_in_label IS NOT NULL THEN 'set' ELSE 'gone' END AS kind
     FROM issues i LEFT JOIN latest l ON l.issue_number = i.number
     WHERE (i.fixed_in_label IS NOT NULL
         AND (l.issue_number IS NULL OR l.source <> 'manual' OR l.fixed_in IS NOT i.fixed_in_label))
       OR (i.fixed_in_label IS NULL AND l.source = 'manual' AND l.recorded_by = 'label')
     ORDER BY i.number LIMIT 100`,
  );
  for (const row of labels) {
    if (row.kind === "set")
      stmts.push(insert(Number(row.number), "manual", String(row.label), null, null, "label"));
    else
      stmts.push(
        db
          .prepare("DELETE FROM issue_fix WHERE issue_number = ? AND recorded_by = 'label'")
          .bind(Number(row.number)),
      );
  }

  // Closed issues with a merged fix the sync has not placed in a release.
  const CANDIDATES = `WITH ${LATEST_FIX}, newest AS (
       SELECT p.issue_number, p.pr_number, p.merged_at, p.merge_sha FROM issue_pr p
       WHERE p.state = 'merged' AND p.merged_at IS NOT NULL AND p.merge_sha IS NOT NULL
         AND p.merged_at = (SELECT MAX(q.merged_at) FROM issue_pr q
           WHERE q.issue_number = p.issue_number AND q.state = 'merged' AND q.merge_sha IS NOT NULL))
     SELECT i.number AS number, n.pr_number AS pr_number, n.merged_at AS merged_at, n.merge_sha AS merge_sha,
       l.source AS source, l.checked_until AS checked_until, l.checked_tag AS checked_tag
     FROM issues i JOIN newest n ON n.issue_number = i.number
     LEFT JOIN latest l ON l.issue_number = i.number
     WHERE i.state = 'closed' AND i.fixed_in_label IS NULL
       AND (l.issue_number IS NULL OR (l.source = 'unreleased'
         AND (COALESCE(l.checked_until, 0) < ?
           OR (COALESCE(l.checked_until, 0) = ? AND COALESCE(l.checked_tag, '') < ?))))
     ORDER BY i.closed_at, i.number LIMIT ?`;
  // Releases in the order the lookup walks them: publish time, then tag,
  // as decodeReleases sorts them.
  const after = (r: Release, cursor: Release): boolean =>
    r.publishedAt > cursor.publishedAt ||
    (r.publishedAt === cursor.publishedAt && r.tag > cursor.tag);
  // Cheap check before the GitHub call: anything at all to look at, taking
  // any release newer than an unreleased row's last comparison as possible.
  const pending = await all(db, CANDIDATES, Number.MAX_SAFE_INTEGER, -1, "", 1);
  if (pending.length) {
    try {
      // The whole release list is needed: releases are listed by creation
      // time and can be published late, so only the end of the list proves
      // the first release after a merge is in it. A list cut off by the
      // page cap leaves fixed-in versions to labels.
      const { releases: listed, complete } = await listReleases(cfg, token);
      if (!complete)
        throw new Error("the release list is longer than the lookup reads; set fixed-in labels");
      const releases = listed.filter((r) => parseSemver(r.tag) !== null);
      const newest: Release = releases.at(-1) ?? { publishedAt: 0, tag: "" };
      const candidates = await all(
        db,
        CANDIDATES,
        newest.publishedAt,
        newest.publishedAt,
        newest.tag,
        FIX_RESOLVE_MAX,
      );
      for (const c of candidates) {
        const mergedAt = Number(c.merged_at);
        const cursor =
          c.source === "unreleased"
            ? { publishedAt: Number(c.checked_until ?? 0), tag: String(c.checked_tag ?? "") }
            : { publishedAt: -Infinity, tag: "" };
        const sha = String(c.merge_sha);
        let fixedIn: string | null = null;
        // Where the comparison got to: the last release compared, or the
        // newest release when none was (nothing after the merge yet).
        let checked = newest;
        for (const r of releases
          .filter((x) => x.publishedAt >= mergedAt && after(x, cursor))
          .slice(0, FIX_COMPARES)) {
          checked = r;
          if (await tagContains(cfg, token, r.tag, sha)) {
            fixedIn = normalizeVersion(r.tag);
            break;
          }
        }
        stmts.push(
          fixedIn === null
            ? insert(
                Number(c.number),
                "unreleased",
                null,
                Number(c.pr_number),
                sha,
                "sync",
                checked,
              )
            : insert(Number(c.number), "release", fixedIn, Number(c.pr_number), sha, "sync"),
        );
      }
    } catch (err) {
      console.error(`fixed-in lookup failed: ${err instanceof Error ? err.message : String(err)}`);
    }
  }
  if (stmts.length) await db.batch(stmts);
  return stmts.length;
}

/**
 * Cron: pulls diagnostics issues changed since the last run, then refreshes
 * pull requests and the label check for up to `MAX_PR_REFRESH` issues:
 * queued ones (changed, or not reached by a previous run) first, then every
 * other issue least recently refreshed first, open or closed, so no issue
 * can starve the rest. Issues that lost the label are dropped. Old rows are
 * swept first.
 *
 * Issue pages are read by update time, oldest first. After each page,
 * `issues_since` moves to one second before the newest update on it and
 * reading starts again at that time's first page, so an issue updated
 * meanwhile (which moves to the end of the listing) cannot shift unread
 * issues behind the cursor, and a backlog longer than one run continues in
 * the next. Issues in the last second are read again; re-reading a stored
 * issue keeps its place in the refresh queue. Only a page whose updates all
 * fall within one second, where the time cannot move, pages on instead.
 *
 * @param env - Worker bindings; a handle already
 *   wrapped by `metered` shares that invocation's budget.
 * @param cfg - Settings.
 * @param now - Unix time in milliseconds.
 * @returns Issues stored and
 *   issues whose pull requests were refreshed.
 */
export async function syncIssues(
  env: Pick<DashboardEnv, "DASHBOARD_DB">,
  cfg: GitHubConfig,
  now: number,
): Promise<{ issues: number; refreshed: number }> {
  const db = metered(env.DASHBOARD_DB);
  const budget = db.budget;
  await sweep(db, now);
  const token = await installationToken(cfg, now);
  const state = await getStates(db, ["issues_since", "issues_page"]);
  let since = state.get("issues_since") ?? "";
  let page = Math.max(1, Number.parseInt(state.get("issues_page") ?? "1", 10) || 1);
  let stored = 0;
  for (let reads = 0; reads < MAX_ISSUE_PAGES; reads++) {
    if (budget.left() < ISSUE_PAGE_CALLS + SYNC_RESERVE) break;
    const result = await listDiagnosticsIssues(cfg, token, since, page);
    const newest = isoMs(result.newest);
    const back = newest === null ? since : new Date(newest - 1000).toISOString();
    if (back > since) {
      since = back;
      page = 1;
    } else if (result.full) {
      // The whole page sits in the second after `since`, so the time cannot
      // move: page on. When this run stops here, the next one reads this
      // page again (unless it was this run's only read, so every run moves
      // forward), in case updates meanwhile shifted the order.
      const more =
        reads + 1 < MAX_ISSUE_PAGES && budget.left() >= 2 * ISSUE_PAGE_CALLS + SYNC_RESERVE;
      page = more || reads === 0 ? page + 1 : page;
    } else {
      page = 1;
    }
    await storeIssuePage(db, result.issues, [
      stateStatement(db, "issues_since", since || null),
      stateStatement(db, "issues_page", page === 1 ? null : String(page)),
    ]);
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
    const numbers = queue
      .map((r) => Number(r.number))
      .filter((n) => Number.isSafeInteger(n) && n > 0);
    for (let i = 0; i < numbers.length && budget.left() > SYNC_RESERVE; i += PR_BATCH) {
      const batch = numbers.slice(i, i + PR_BATCH);
      const prs = await pullRequestsFor(cfg, token, batch);
      const stmts: D1PreparedStatement[] = [];
      for (const n of batch) {
        // No data for an issue (deleted, transferred): keep its old links but
        // move it to the back of the queue so it cannot starve the rest.
        const got = prs.get(n);
        if (!got) {
          stmts.push(
            db.prepare("UPDATE issues SET prs_synced_at = ? WHERE number = ?").bind(now, n),
          );
          continue;
        }
        stmts.push(
          ...(got.labeled ? pullRequestStatements(db, n, got.prs, now) : dropStatements(db, n)),
        );
        refreshed += 1;
      }
      await db.batch(stmts);
    }
  }
  if (budget.left() > FIX_CALLS + SYNC_RESERVE) await resolveFixes(db, cfg, token, now);
  await setState(db, "issues_synced_at", String(now));
  return { issues: stored, refreshed };
}

/**
 * Brings one issue up to date right away, for the ingest Action's notice
 * after it files an issue. Only the number comes from the caller: the issue,
 * its report IDs, its label and its pull requests are read from GitHub, so
 * a caller cannot link reports an issue does not list.
 *
 * @param env - Worker bindings.
 * @param cfg - Settings.
 * @param number - Issue number.
 * @param now - Unix time in milliseconds.
 * @param retryDelaysMs - Waits before reading again while GitHub
 *   has no such issue (tests pass none).
 * @returns Whether the issue is now stored
 *   with its links, or was forgotten (no such issue, or not labeled
 *   `diagnostics`).
 */
export async function syncIssueNow(
  env: Pick<DashboardEnv, "DASHBOARD_DB">,
  cfg: GitHubConfig,
  number: number,
  now: number,
  retryDelaysMs: number[] = NOT_FOUND_RETRY_MS,
): Promise<"linked" | "dropped"> {
  const db = metered(env.DASHBOARD_DB);
  const token = await installationToken(cfg, now);
  let got = await fetchIssue(cfg, token, number);
  // GitHub's reads can lag a create by a moment; the notice arrives right
  // after `gh issue create`, so a 404 is retried before it counts.
  for (const ms of retryDelaysMs) {
    if (got) break;
    await new Promise((resolve) => setTimeout(resolve, ms));
    got = await fetchIssue(cfg, token, number);
  }
  if (!got || !got.labeled) {
    await db.batch(dropStatements(db, number));
    return "dropped";
  }
  const prs = await pullRequestsFor(cfg, token, [number]);
  const stmts = issueStatements(db, got.issue);
  const refreshed = prs.get(number);
  if (refreshed?.labeled) stmts.push(...pullRequestStatements(db, number, refreshed.prs, now));
  await db.batch(stmts);
  return "linked";
}

/**
 * Indexes a report `/submit` just stored. Failures are swallowed by the
 * caller: the upload already succeeded and the reindex can pick it up.
 *
 * @param db - Database.
 * @param enriched - The object written to B2.
 * @param objectKey - Its key.
 * @param cap - Daily row cap.
 * @returns What happened.
 */
export async function indexSubmitted(
  db: D1Database,
  enriched: SubmittedReport,
  objectKey: string,
  cap: number,
): Promise<"indexed" | "capped"> {
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
 * @param env - Worker bindings; a handle already
 *   wrapped by `metered` shares that invocation's budget.
 * @param b2 - B2 helpers.
 * @returns Reports added and
 *   whether the listing reached its end (the cursor then starts over).
 */
export async function reindexSlice(
  env: Pick<DashboardEnv, "DASHBOARD_DB">,
  b2: B2Access,
): Promise<{ indexed: number; done: boolean }> {
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
    const page = await listPage(b2, auth, {
      prefix: PREFIX,
      startFileName: start,
      maxFileCount: 100,
    });
    const candidates = page.files.flatMap((f) => {
      const reportId = reportIdOf(f.fileName);
      return reportId ? [{ ...f, reportId }] : [];
    });
    const have = new Set<unknown>();
    for (let i = 0; i < candidates.length; i += LOOKUP_CHUNK) {
      const chunk = candidates.slice(i, i + LOOKUP_CHUNK);
      const rows = await all(
        db,
        `SELECT report_id FROM reports WHERE report_id IN (${chunk.map(() => "?").join(", ")})`,
        ...chunk.map((c) => c.reportId),
      );
      for (const r of rows) have.add(r.report_id);
    }
    const rows: Parameters<typeof reportInsert>[1][] = [];
    let stoppedAt: string | null = null;
    for (const f of candidates) {
      if (have.has(f.reportId)) continue;
      if (downloads >= REINDEX_DOWNLOADS || fetches >= REINDEX_FETCHES) {
        stoppedAt = f.fileName;
        break;
      }
      downloads += 1;
      fetches += 1;
      const text = await b2.downloadById(auth, f.fileId);
      let stored: unknown = null;
      try {
        stored = text.length <= MAX_STORED_REPORT_CHARS ? JSON.parse(text) : null;
      } catch {
        stored = null;
      }
      if (!isRecordLike(stored) || stored.report_id !== f.reportId) continue;
      const receivedAt = isSafeInteger(stored.received_at_unix_ms)
        ? stored.received_at_unix_ms
        : f.uploadTimestamp;
      if (!Number.isFinite(new Date(receivedAt).getTime())) continue;
      rows.push({
        reportId: f.reportId,
        objectKey: f.fileName,
        receivedAt,
        summary: await summarizeReport(stored),
      });
    }
    if (stoppedAt) start = stoppedAt;
    else if (!page.nextFileName) {
      start = null;
      done = true;
    } else start = page.nextFileName;

    // A report a concurrent /submit indexed meanwhile is ignored by the
    // insert but still counted in the ledger, which only errs toward the cap.
    const perDay = new Map<string, number>();
    for (const row of rows) {
      const day = utcDayOf(row.receivedAt);
      perDay.set(day, (perDay.get(day) ?? 0) + 1);
    }
    await db.batch([
      ...rows.map((row) => reportInsert(db, row)),
      ...[...perDay].map(([day, n]) =>
        db
          .prepare(
            "INSERT INTO index_days (day, rows) VALUES (?, ?) ON CONFLICT (day) DO UPDATE SET rows = rows + excluded.rows",
          )
          .bind(day, n),
      ),
      stateStatement(db, "reindex_cursor", start),
    ]);
    indexed += rows.length;
    if (stoppedAt || done) break;
  }
  return { indexed, done };
}
