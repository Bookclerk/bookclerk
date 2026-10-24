/**
 * Dashboard routing. `dashboardFetch` answers the dashboard's paths and
 * returns null for everything else, so worker.js keeps handling `/submit`,
 * `/report`, `/triage` and `/health` as before.
 *
 * The dashboard is on only when the GitHub App settings and the
 * DASHBOARD_DB binding are both present; a fork without them gets the
 * collector exactly as it was (`/` answers the health JSON, dashboard paths
 * 404).
 */

import { BudgetLedger, DEFAULT_DAILY_NEURONS, MAX_DAILY_NEURONS, utcDay } from "../triage.js";
import { finishSignIn, signOut, startSignIn } from "./auth.js";
import {
  afterFixCounts,
  all,
  countsByBucket,
  DAY_MS,
  DEFAULT_INDEX_DAILY_ROWS,
  DEFAULT_RANGE,
  filterOptions,
  getReport,
  getState,
  indexCapReached,
  listIssues,
  listReports,
  logAccess,
  overviewStats,
  PAGE_SIZE,
  RANGES,
  recentAccess,
  setState,
  signatureDays,
  topBy,
  topSignatures,
} from "./db.js";
import { githubConfig } from "./github.js";
import { fmtDay, html, htmlResponse, redirect } from "./html.js";
import {
  dailySeries,
  issuesPage,
  messagePage,
  overviewPage,
  reportPage,
  reportsPage,
  signInPage,
} from "./pages.js";
import { currentMember, endAllSessions, sameOrigin } from "./session.js";
import { readStoredReport } from "./storage.js";
import { reindexSlice, syncIssues } from "./sync.js";

/** Paths the dashboard owns (exact). */
const PATHS = new Set(["/", "/auth/github", "/auth/callback", "/auth/logout", "/dash", "/reports", "/issues", "/admin/reindex", "/admin/sign-out-all"]);

/** `/reports/<uuid>`. */
const REPORT_PATH = /^\/reports\/([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})$/;

/**
 * Whether the dashboard is configured in this deployment.
 *
 * @param {Record<string, unknown>} env Worker bindings.
 * @returns {import("./github.js").GitHubConfig | null} Settings when on.
 */
export function dashboardConfig(env) {
  return env.DASHBOARD_DB ? githubConfig(env) : null;
}

/**
 * Whether a path belongs to the dashboard (used to 404 it when off).
 *
 * @param {string} path Normalized path.
 * @returns {boolean} True for dashboard paths other than `/`.
 */
export function isDashboardPath(path) {
  return (path !== "/" && PATHS.has(path)) || REPORT_PATH.test(path);
}

/**
 * Daily index cap from `DASHBOARD_INDEX_DAILY_ROWS`.
 *
 * @param {Record<string, unknown>} env Worker bindings.
 * @returns {number} Positive cap.
 */
export function indexCap(env) {
  const n = Number(env.DASHBOARD_INDEX_DAILY_ROWS);
  return Number.isSafeInteger(n) && n > 0 ? n : DEFAULT_INDEX_DAILY_ROWS;
}

/**
 * Bounded filter text from a query parameter.
 *
 * @param {URLSearchParams} q Query.
 * @param {string} name Parameter.
 * @returns {string} At most 128 characters, no control characters.
 */
function param(q, name) {
  return (q.get(name) || "").replace(/[\u0000-\u001f\u007f]/g, "").slice(0, 128);
}

/**
 * Range id from the query, or the default.
 *
 * @param {URLSearchParams} q Query.
 * @returns {string} A key of `RANGES`.
 */
function rangeOf(q) {
  const id = q.get("range") || "";
  return Object.hasOwn(RANGES, id) ? id : DEFAULT_RANGE;
}

/**
 * Today's triage spend and cap, read from #388's TRIAGE_BUDGET ledger.
 *
 * @param {Record<string, unknown>} env Worker bindings.
 * @param {number} now Unix time in milliseconds.
 * @returns {Promise<{spent: number, cap: number}>} Neurons.
 */
async function triageBudget(env, now) {
  const n = Number(env.TRIAGE_DAILY_NEURONS);
  const cap = Math.min(Number.isSafeInteger(n) && n > 0 ? n : DEFAULT_DAILY_NEURONS, MAX_DAILY_NEURONS);
  if (!env.TRIAGE_BUDGET) return { spent: 0, cap };
  try {
    return { spent: await new BudgetLedger(env.TRIAGE_BUDGET).spent(utcDay(now)), cap };
  } catch {
    return { spent: 0, cap };
  }
}

/**
 * Answers a dashboard request.
 *
 * @param {Request} request Incoming request.
 * @param {Record<string, any>} env Worker bindings.
 * @param {{waitUntil(p: Promise<unknown>): void}} ctx Execution context.
 * @param {string} path Normalized path.
 * @param {import("./storage.js").B2Access} b2 B2 helpers from worker.js.
 * @param {number} [now] Unix time in milliseconds (tests).
 * @returns {Promise<Response | null>} The response, or null when the path is
 *   not the dashboard's or the dashboard is off.
 */
export async function dashboardFetch(request, env, ctx, path, b2, now = Date.now()) {
  const cfg = dashboardConfig(env);
  if (!cfg || !(PATHS.has(path) || REPORT_PATH.test(path))) return null;
  const db = env.DASHBOARD_DB;
  const method = request.method;
  const url = new URL(request.url);

  if (path === "/auth/github" && method === "GET") return startSignIn(request, cfg);
  if (path === "/auth/callback" && method === "GET") return finishSignIn(request, env, cfg, ctx, now);
  if (path === "/auth/logout" && method === "POST") return signOut(request, env);

  const member = await currentMember(db, request, now);
  if (path === "/" && method === "GET") return member ? redirect("/dash") : htmlResponse(signInPage(cfg));
  if (!member) {
    if (method === "GET") return redirect("/");
    return htmlResponse(messagePage("Signed out", html`<p>Your session ended. Sign in again.</p>`), 401);
  }

  if (method === "POST" && (path === "/admin/reindex" || path === "/admin/sign-out-all")) {
    if (member.role !== "admin" || !sameOrigin(request)) {
      return htmlResponse(messagePage("Request refused", html`<p>Only organization owners can do this, from the dashboard's own buttons.</p>`), 403);
    }
    if (path === "/admin/sign-out-all") {
      await logAccess(db, member, "sign-out-all", null, now);
      await endAllSessions(db);
      return redirect("/");
    }
    let note;
    try {
      const res = await reindexSlice(env, b2);
      note = res.done
        ? `Reindex finished: added ${res.indexed} reports.`
        : `Reindex added ${res.indexed} reports; press again to continue.`;
    } catch {
      note = "Reindex could not read storage; try again.";
    }
    await setState(db, "reindex_note", `${new Date(now).toISOString().slice(0, 16).replace("T", " ")} UTC: ${note}`);
    await logAccess(db, member, "reindex", null, now);
    return redirect("/dash");
  }

  if (method !== "GET") return null;
  const q = url.searchParams;

  if (path === "/dash") {
    const rangeId = rangeOf(q);
    const range = RANGES[rangeId];
    const since = now - range.ms;
    const filter = { version: param(q, "version") };
    const [stats, counts, platforms, versions, signatures, recent, afterFix, budget, versionOptions, capReached] = await Promise.all([
      overviewStats(db, now, since, filter),
      countsByBucket(db, since, range.bucket, filter),
      topBy(db, "platform", since, filter, 6),
      topBy(db, "version", since, {}, 6),
      topSignatures(db, since, filter, 6),
      listReports(db, since, filter, 8, 0).then((r) => r.rows),
      afterFixCounts(db),
      triageBudget(env, now),
      topBy(db, "version", now - 90 * DAY_MS, {}, 30).then((r) => r.map((x) => String(x.key)).filter(Boolean)),
      indexCapReached(db, now, indexCap(env)),
    ]);
    const totals = await all(
      db,
      "SELECT CAST(received_at / ? AS INTEGER) AS day, COUNT(*) AS n FROM reports WHERE received_at >= ? GROUP BY day",
      DAY_MS,
      now - 14 * DAY_MS,
    );
    const admin = member.role === "admin";
    const [access, reindexNote] = admin
      ? await Promise.all([recentAccess(db, 10), getState(db, "reindex_note")])
      : [[], null];
    return htmlResponse(
      overviewPage({
        cfg, member, url, now, rangeId, filter, stats, counts, platforms, versions, signatures, recent,
        afterFix, budget, versionOptions, capReached, access, reindexNote: reindexNote ?? "",
        totalSeries: dailySeries(now, 14, new Map(totals.map((t) => [Number(t.day), Number(t.n)]))),
      }),
    );
  }

  if (path === "/reports") {
    const rangeId = rangeOf(q);
    const since = now - RANGES[rangeId].ms;
    const filed = q.get("filed");
    const signature = param(q, "signature");
    const filter = {
      version: param(q, "version"),
      platform: param(q, "platform"),
      trigger: param(q, "trigger"),
      filed: filed === "yes" || filed === "no" ? filed : "",
      signature: /^[0-9a-f]{16}$/.test(signature) ? signature : "",
    };
    const page = Math.min(Math.max(1, Number.parseInt(q.get("page") || "1", 10) || 1), 10_000);
    const [list, options] = await Promise.all([
      listReports(db, since, filter, PAGE_SIZE, (page - 1) * PAGE_SIZE),
      filterOptions(db, since),
    ]);
    return htmlResponse(
      reportsPage({ cfg, member, url, rangeId, filter, options, page, pageSize: PAGE_SIZE, rows: list.rows, total: list.total }),
    );
  }

  const m = REPORT_PATH.exec(path);
  if (m) {
    const found = await getReport(db, m[1]);
    if (!found) {
      return htmlResponse(messagePage("Report not found", html`<p>No indexed report has that ID. It may be older than the index; an owner can index older reports from the overview.</p>`), 404);
    }
    const r = found.report;
    const [stored, afterFix, similar, days] = await Promise.all([
      readStoredReport(b2, String(r.object_key)).catch(() => null),
      afterFixCounts(db),
      r.signature
        ? all(db, "SELECT report_id, received_at, version, os, arch FROM reports WHERE signature = ? AND report_id <> ? ORDER BY received_at DESC LIMIT 8", r.signature, r.report_id)
        : Promise.resolve([]),
      r.signature ? signatureDays(db, r.signature, now - 14 * DAY_MS) : Promise.resolve(new Map()),
    ]);
    await logAccess(db, member, "view-report", r.report_id, now);
    const fixes = found.issues.map((i) => afterFix.get(Number(i.number))).filter(Boolean);
    const fix = fixes.sort((a, b) => b.fixAt - a.fixAt)[0] ?? null;
    const today = Math.floor(now / DAY_MS);
    const counts = dailySeries(now, 14, days);
    const sigDays = counts.map((n, i) => {
      const day = today - 13 + i;
      return { label: fmtDay(day * DAY_MS), n, after: Boolean(fix && (day + 1) * DAY_MS > fix.fixAt) };
    });
    const markerIndex = fix ? Math.floor(fix.fixAt / DAY_MS) - (today - 13) : -1;
    const fixPr = found.prs.filter((p) => p.state === "merged").sort((a, b) => Number(b.merged_at) - Number(a.merged_at))[0];
    return htmlResponse(
      reportPage({
        cfg, member, report: r, stored, issues: found.issues, prs: found.prs, fix, similar, sigDays,
        sigMarker: fix && fixPr ? { index: markerIndex, label: `#${fixPr.pr_number} merged` } : null,
      }),
    );
  }

  if (path === "/issues") {
    const s = q.get("state");
    const state = s === "closed" || s === "all" ? s : "open";
    const [issues, afterFix, syncedAt] = await Promise.all([
      listIssues(db, state, now - 14 * DAY_MS, 100),
      afterFixCounts(db),
      getState(db, "issues_synced_at"),
    ]);
    return htmlResponse(issuesPage({ cfg, member, now, state, issues, afterFix, syncedAt: syncedAt ? Number(syncedAt) : null }));
  }
  return null;
}

/**
 * Cron entry: issue and pull request sync when the dashboard is on.
 *
 * @param {Record<string, any>} env Worker bindings.
 * @param {number} now Unix time in milliseconds.
 * @returns {Promise<void>}
 */
export async function dashboardScheduled(env, now) {
  const cfg = dashboardConfig(env);
  if (!cfg) return;
  await syncIssues(env, cfg, now);
}
