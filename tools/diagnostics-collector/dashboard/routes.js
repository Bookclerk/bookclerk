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
import { DEFAULT_NEXT, finishSignIn, safeNext, sameSiteHop, signOut, startSignIn } from "./auth.js";
import { metered } from "./budget.js";
import {
  fixStatus,
  isRegressionVersion,
  issuesTrackingSignature,
  all,
  countsByBucket,
  DAY_MS,
  DEFAULT_INDEX_DAILY_ROWS,
  DEFAULT_RANGE,
  filterOptions,
  getReport,
  first,
  getState,
  indexCapReached,
  linkBySignature,
  linkFingerprint,
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
import { announceChange, closeAllLive, connectLive } from "./live.js";
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
import { indexSubmitted, reindexSlice, syncIssueNow, syncIssues } from "./sync.js";

/** Paths the dashboard owns (exact). */
const PATHS = new Set(["/", "/auth/github", "/auth/callback", "/auth/logout", "/dash", "/reports", "/issues", "/admin/reindex", "/admin/sync", "/admin/sign-out-all", "/live"]);

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
 * Whether a request is a top-level page load the member started on another
 * site by clicking a link (in a GitHub issue, chat or web mail). The browser
 * leaves the SameSite=Strict session cookie off such a request, so a
 * signed-in member would otherwise see the sign-in page.
 *
 * Requiring user activation keeps another site's scripts from loading
 * member pages with the cookie on its own (which would write report views
 * to the access log under the member's name), and stops the reload from
 * repeating: the reload the hop page starts is never user-activated.
 *
 * @param {Request} request Incoming request.
 * @returns {boolean} True when `Sec-Fetch-Site` is `cross-site`,
 *   `Sec-Fetch-Mode` is `navigate`, `Sec-Fetch-Dest` is `document` and
 *   `Sec-Fetch-User` is `?1`; browsers that send none of them get the
 *   sign-in page as before.
 */
export function crossSiteNavigation(request) {
  const h = request.headers;
  return (
    h.get("sec-fetch-site") === "cross-site" &&
    h.get("sec-fetch-mode") === "navigate" &&
    h.get("sec-fetch-dest") === "document" &&
    h.get("sec-fetch-user") === "?1"
  );
}

/**
 * The page that reloads `target` same-site (see `crossSiteNavigation`).
 *
 * @param {string} target Same-origin address from `safeNext` or `homeWith`.
 * @returns {Response} A 200 page with no cookies.
 */
function sameSiteReload(target) {
  return htmlResponse(sameSiteHop("Opening the dashboard", target, "Opening the dashboard."));
}

/**
 * The sign-in page's address with a return page.
 *
 * @param {string | null} next Page from `safeNext`, or null.
 * @returns {string} `/` or `/?next=<encoded page>`.
 */
function homeWith(next) {
  return next ? `/?next=${encodeURIComponent(next)}` : "/";
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
  // One D1 call budget for the whole request; the reindex stops within it.
  env = { ...env, DASHBOARD_DB: metered(env.DASHBOARD_DB) };
  const db = env.DASHBOARD_DB;
  const method = request.method;
  const url = new URL(request.url);

  if (path === "/auth/github" && method === "GET") return startSignIn(request, cfg);
  if (path === "/auth/callback" && method === "GET") return finishSignIn(request, env, cfg, ctx, now);
  if (path === "/auth/logout" && method === "POST") return signOut(request, env);

  const member = await currentMember(db, request, now);
  if (path === "/" && method === "GET") {
    const next = safeNext(url.searchParams.get("next"));
    if (member) return redirect(next ?? DEFAULT_NEXT);
    // Load the page again from a same-site navigation, which carries the
    // session cookie if there is one; without one it lands back here as a
    // same-origin request and gets the sign-in page.
    if (crossSiteNavigation(request)) return sameSiteReload(homeWith(next));
    return htmlResponse(signInPage(cfg, next));
  }
  if (!member) {
    if (method === "GET") {
      const here = safeNext(path + url.search);
      if (here && crossSiteNavigation(request)) return sameSiteReload(here);
      return redirect(homeWith(here));
    }
    return htmlResponse(messagePage("Signed out", html`<p>Your session ended. Sign in again.</p>`), 401);
  }

  if (method === "POST" && (path === "/admin/reindex" || path === "/admin/sync" || path === "/admin/sign-out-all")) {
    if (member.role !== "admin" || !sameOrigin(request)) {
      return htmlResponse(messagePage("Request refused", html`<p>Only organization owners can do this, from the dashboard's own buttons.</p>`), 403);
    }
    if (path === "/admin/sign-out-all") {
      await logAccess(db, member, "sign-out-all", null, now);
      await endAllSessions(db);
      await closeAllLive(env);
      return redirect("/");
    }
    if (path === "/admin/sync") {
      let note;
      try {
        const res = await syncAndAnnounce(env, cfg, now);
        note = `Issue sync: stored ${res.issues} changed issues, refreshed pull requests for ${res.refreshed}.`;
      } catch (err) {
        console.error(`issue sync failed: ${err instanceof Error ? err.message : String(err)}`);
        note = "Issue sync could not reach GitHub; try again.";
      }
      await setState(db, "reindex_note", `${new Date(now).toISOString().slice(0, 16).replace("T", " ")} UTC: ${note}`);
      await logAccess(db, member, "sync", null, now);
      return redirect("/dash");
    }
    let note;
    try {
      const res = await reindexSlice(env, b2);
      note = res.done
        ? `Reindex finished: added ${res.indexed} reports.`
        : `Reindex added ${res.indexed} reports; press again to continue.`;
      if (res.indexed > 0) await announceChange(env);
    } catch {
      note = "Reindex could not read storage; try again.";
    }
    await setState(db, "reindex_note", `${new Date(now).toISOString().slice(0, 16).replace("T", " ")} UTC: ${note}`);
    await logAccess(db, member, "reindex", null, now);
    return redirect("/dash");
  }

  if (method !== "GET") return null;

  if (path === "/live") {
    // Same-origin pages only: a cookie alone would let another site's page
    // open the socket (cross-site WebSocket hijacking), though it would
    // learn nothing but when the numbers change.
    if ((request.headers.get("upgrade") || "").toLowerCase() !== "websocket" || !sameOrigin(request)) {
      return new Response("expected a same-origin WebSocket upgrade", { status: 400 });
    }
    return connectLive(env, member.expiresAt);
  }

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
      fixStatus(db),
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
    // A live refresh (dashboard.js after a push) swaps in only the links and
    // counts, so it skips the stored report. It still logs the view unless
    // this member's view of this report is already logged in the last 8
    // hours (a session's length), so the header cannot hide a first view.
    const liveRefresh = request.headers.get("bookclerk-live") === "1"
      && Boolean(await first(db, "SELECT 1 AS seen FROM access_log WHERE user_id = ? AND action = 'view-report' AND report_id = ? AND at > ? LIMIT 1", member.userId, r.report_id, now - 8 * 3_600_000));
    const [stored, afterFix, similar, days, sameSignature] = await Promise.all([
      liveRefresh ? Promise.resolve(null) : readStoredReport(b2, String(r.object_key)).catch(() => null),
      fixStatus(db),
      r.signature
        ? all(db, "SELECT report_id, received_at, version, os, arch FROM reports WHERE signature = ? AND report_id <> ? ORDER BY received_at DESC LIMIT 8", r.signature, r.report_id)
        : Promise.resolve([]),
      r.signature ? signatureDays(db, r.signature, now - 14 * DAY_MS) : Promise.resolve(new Map()),
      r.signature ? issuesTrackingSignature(db, r.signature) : Promise.resolve([]),
    ]);
    if (!liveRefresh) await logAccess(db, member, "view-report", r.report_id, now);
    // The fix shown is a linked issue's: one with a fixed-in version first
    // (it can tell a regression), then the newest merge. A report no issue
    // lists yet shows the fix of an issue tracking its signature, so a
    // regression is visible before anyone links it.
    const linked = found.issues.map((i) => Number(i.number));
    const pick = (numbers) => numbers
      .filter((n) => afterFix.has(n))
      .map((n) => ({ ...afterFix.get(n), issue: n }))
      .sort((a, b) => Number(Boolean(b.fixedIn)) - Number(Boolean(a.fixedIn)) || (b.fixAt ?? 0) - (a.fixAt ?? 0))[0];
    const fix = pick(linked) ?? pick(sameSignature.filter((n) => !linked.includes(n))) ?? null;
    const regression = Boolean(fix?.fixedIn && isRegressionVersion(String(r.version), fix.fixedIn));
    const today = Math.floor(now / DAY_MS);
    const counts = dailySeries(now, 14, days);
    const sigDays = counts.map((n, i) => {
      const day = today - 13 + i;
      return { label: fmtDay(day * DAY_MS), n, after: Boolean(fix?.fixAt && (day + 1) * DAY_MS > fix.fixAt) };
    });
    const markerIndex = fix?.fixAt ? Math.floor(fix.fixAt / DAY_MS) - (today - 13) : -1;
    const fixPr = found.prs.filter((p) => p.state === "merged").sort((a, b) => Number(b.merged_at) - Number(a.merged_at))[0];
    return htmlResponse(
      reportPage({
        cfg, member, report: r, stored, issues: found.issues, prs: found.prs, fix, regression, similar, sigDays,
        sigMarker: fix?.fixAt && fixPr ? { index: markerIndex, label: `#${fixPr.pr_number} merged` } : null,
      }),
    );
  }

  if (path === "/issues") {
    const s = q.get("state");
    const state = s === "closed" || s === "all" ? s : "open";
    const [issues, afterFix, syncedAt] = await Promise.all([
      listIssues(db, state, now - 14 * DAY_MS, 100),
      fixStatus(db),
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
  await syncAndAnnounce({ ...env, DASHBOARD_DB: metered(env.DASHBOARD_DB) }, cfg, now);
}

/**
 * Runs `work` and tells open pages to refresh only when it changed what
 * they show (the link fingerprint differs afterwards). The sync keeps D1
 * calls in reserve for the two fingerprint reads.
 *
 * @template T
 * @param {{DASHBOARD_DB: D1Database}} env Worker bindings with a metered
 *   handle.
 * @param {() => Promise<T>} work The sync.
 * @returns {Promise<T>} What `work` returned.
 */
async function announceIfLinksChanged(env, work) {
  const before = await linkFingerprint(env.DASHBOARD_DB);
  try {
    return await work();
  } finally {
    // The work can store part of its result before it fails; announce that
    // too, without hiding the work's own error.
    try {
      if ((await linkFingerprint(env.DASHBOARD_DB)) !== before) await announceChange(env);
    } catch (err) {
      console.error(`live update check failed: ${err instanceof Error ? err.message : String(err)}`);
    }
  }
}

/**
 * The issue sync, with a live update when it changed anything visible.
 *
 * @param {Record<string, any>} env Worker bindings with a metered handle.
 * @param {import("./github.js").GitHubConfig} cfg Settings.
 * @param {number} now Unix time in milliseconds.
 * @returns {Promise<{issues: number, refreshed: number}>} The sync's counts.
 */
function syncAndAnnounce(env, cfg, now) {
  return announceIfLinksChanged(env, () => syncIssues(env, cfg, now));
}

/**
 * `POST /issues/notify` (ingest Action, `REPORT_API_KEY`): links a newly
 * filed issue right away instead of at the next cron.
 *
 * @param {Record<string, any>} env Worker bindings.
 * @param {number} number Issue number from `parseIssueNotify`.
 * @param {number} now Unix time in milliseconds.
 * @returns {Promise<"linked" | "dropped" | null>} What happened, or null
 *   when the dashboard is off.
 */
export async function dashboardIssueNotify(env, number, now) {
  const cfg = dashboardConfig(env);
  if (!cfg) return null;
  env = { ...env, DASHBOARD_DB: metered(env.DASHBOARD_DB) };
  return announceIfLinksChanged(env, () => syncIssueNow(env, cfg, number, now));
}

/**
 * `GET /report?unfiled=1`: the reports some issue already lists, so ingest
 * does not triage them again when its pull cursor is lost (the Actions cache
 * that holds it can be evicted) or a run re-pulls from the start.
 *
 * @param {Record<string, any>} env Worker bindings.
 * @returns {Promise<Set<string> | null>} Lowercase report UUIDs linked to at
 *   least one issue, or null when the dashboard database is not bound.
 */
export async function dashboardFiledReportIds(env) {
  if (!env.DASHBOARD_DB) return null;
  const rows = await all(env.DASHBOARD_DB, "SELECT DISTINCT report_id FROM report_issue WHERE source <> 'excluded'");
  return new Set(rows.map((r) => String(r.report_id)));
}

/**
 * `GET /report?unfiled=1`: links pulled reports to the open issue that
 * already tracks their signature (`linkBySignature`), so they join that
 * issue instead of going to triage, and tells open pages when any did.
 *
 * @param {Record<string, any>} env Worker bindings.
 * @param {string[]} reportIds Lowercase report UUIDs pulled by ingest.
 * @param {number} now Unix time in milliseconds.
 * @returns {Promise<Map<string, number>>} Report ID → issue number linked;
 *   empty when the dashboard database is not bound.
 */
export async function dashboardLinkBySignature(env, reportIds, now) {
  if (!env.DASHBOARD_DB || reportIds.length === 0) return new Map();
  const linked = await linkBySignature(env.DASHBOARD_DB, reportIds, now);
  if (linked.size) await announceChange(env);
  return linked;
}

/**
 * After `/submit`: indexes the stored report and, when a row was added,
 * tells open pages. Failures are the caller's to swallow.
 *
 * @param {Record<string, any>} env Worker bindings.
 * @param {object} enriched The object written to B2.
 * @param {string} objectKey Its key.
 * @returns {Promise<"indexed" | "capped">} What happened.
 */
export async function dashboardIndexSubmitted(env, enriched, objectKey) {
  const result = await indexSubmitted(env.DASHBOARD_DB, enriched, objectKey, indexCap(env));
  if (result === "indexed") await announceChange(env);
  return result;
}
