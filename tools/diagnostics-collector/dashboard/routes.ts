/**
 * Dashboard routing. `dashboardFetch` answers the dashboard's paths and
 * returns null for everything else, so worker.ts keeps handling `/submit`,
 * `/report`, `/triage` and `/health` as before.
 *
 * The dashboard is on only when the GitHub App settings and the
 * DASHBOARD_DB binding are both present; a fork without them gets the
 * collector exactly as it was (`/` answers the health JSON, dashboard paths
 * 404).
 */

import { BudgetLedger, DEFAULT_DAILY_NEURONS, MAX_DAILY_NEURONS, utcDay } from "../triage.ts";
import { DEFAULT_NEXT, finishSignIn, safeNext, sameSiteHop, signOut, startSignIn } from "./auth.ts";
import { metered } from "./budget.ts";
import {
  applyLinkAction,
  attachByTriage,
  fixStatus,
  isRegressionVersion,
  issueDetail,
  issuesTrackingSignature,
  issueSummaries,
  markSummarySent,
  claimSummaryWrite,
  handOutSummaries,
  releaseSummaryWrite,
  openIssueOptions,
  SUMMARY_BATCH,
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
  matchClosedBySignature,
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
  isRangeId,
  type IssueSummary,
  type RangeId,
  type ReportBriefRow,
  type ReportFilter,
} from "./db.ts";
import { githubConfig, writeSummaryComment, type AttachLink, type GitHubConfig, type SummaryWrite } from "./github.ts";
import { MAX_FORM_CHARS, issueNumber, parseLinkAction } from "./links.ts";
import { dashboardOrigin, renderSummary } from "./summary.ts";
import { announceChange, closeAllLive, connectLive } from "./live.ts";
import { fmtDay, html, htmlResponse, redirect } from "./html.ts";
import {
  dailySeries,
  issuePage,
  issuesPage,
  messagePage,
  overviewPage,
  reportPage,
  reportsPage,
  signInPage,
  type ReportFix,
} from "./pages.ts";
import { currentMember, endAllSessions, sameOrigin, type Member } from "./session.ts";
import { readStoredReport, type B2Access } from "./storage.ts";
import { indexSubmitted, reindexSlice, syncIssueNow, syncIssues, type SubmittedReport } from "./sync.ts";
import type { Background, DashboardEnv, Env } from "../env.ts";

/** Paths the dashboard owns (exact). */
const PATHS = new Set(["/", "/auth/github", "/auth/callback", "/auth/logout", "/dash", "/reports", "/issues", "/issues/link", "/admin/reindex", "/admin/sync", "/admin/sign-out-all", "/admin/update-issue", "/live"]);

/** `/reports/<uuid>`. */
const REPORT_PATH = /^\/reports\/([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})$/;

/** `/issues/<number>`. */
const ISSUE_PATH = /^\/issues\/([1-9][0-9]{0,8})$/;

/**
 * Cursor that holds what the owner's last "Update GitHub now" press said, as
 * `<issue number>\t<unix ms>\t<text>`, shown on that issue's page for
 * `UPDATE_NOTE_TTL_MS`.
 */
const UPDATE_NOTE = "issue_update_note";

/** How long the note of an "Update GitHub now" press stays on the issue page. */
const UPDATE_NOTE_TTL_MS = 10 * 60_000;

/**
 * The note of the last "Update GitHub now" press when it concerns this issue
 * and is recent.
 *
 * @param stored - The `UPDATE_NOTE` cursor.
 * @param issue - Issue number of the page.
 * @param now - Unix time in milliseconds.
 * @returns The note, or null.
 */
function updateNoteFor(stored: string | null, issue: number, now: number): { at: number; text: string; } | null {
  const [forIssue, at, ...rest] = (stored ?? "").split("\t");
  const when = Number(at);
  if (forIssue !== String(issue) || !Number.isFinite(when) || now - when > UPDATE_NOTE_TTL_MS || rest.length === 0) return null;
  return { at: when, text: rest.join("\t") };
}

/**
 * What the page tells the owner after a failed comment write, from the
 * error `writeSummaryComment` threw (its messages carry a stage and an HTTP
 * status, never a token).
 *
 * @param err - The error.
 * @returns One sentence.
 */
function updateFailure(err: unknown): string {
  const text = err instanceof Error ? err.message : String(err);
  if (text.startsWith("installation token failed")) {
    return `GitHub refused the token to edit the comment (${text}); the App needs Issues: write on the repository. The scheduled ingest run will write it.`;
  }
  return `GitHub did not take the comment (${text}); try again later, or the scheduled ingest run will write it.`;
}

/**
 * The owner's "Update GitHub now" press: renders the issue's summary from
 * the ledger and writes it to GitHub as the ingest run would, then records
 * the fingerprint written so the run does not write it again. A refusal for
 * good (locked or gone issue) is recorded the same way, as the run does.
 *
 * @param env - Worker bindings.
 * @param cfg - Settings.
 * @param request - The form post.
 * @param issue - Issue number.
 * @param now - Unix time in milliseconds.
 * @returns The note for the issue page.
 */
async function updateIssueNow(env: DashboardEnv, cfg: GitHubConfig, request: Request, issue: number, now: number): Promise<string> {
  const db = env.DASHBOARD_DB;
  // The ingest run and this press take turns per issue, so neither writes
  // over the other's comment and then records the other's figures.
  if (!(await claimSummaryWrite(db, issue, now))) {
    return "The ingest run (or another owner) is writing this comment now; look again in a few minutes.";
  }
  try {
    const [summary] = await issueSummaries(db, 1, issue);
    if (!summary) return "GitHub already shows the current figures for this issue.";
    // Report links point at this deployment: the Worker serves on exactly one
    // origin (the custom domain, or workers.dev without one), which is the
    // collector URL the ingest run renders with, so both write the same links.
    const rendered = renderSummary(summary, dashboardOrigin(new URL(request.url).origin));
    if (!rendered) return "The summary could not be rendered; the scheduled ingest run will report it.";
    let result: SummaryWrite;
    try {
      result = await writeSummaryComment(cfg, now, issue, rendered.body);
    } catch (err) {
      console.error(`summary comment write failed: ${err instanceof Error ? err.message : String(err)}`);
      return updateFailure(err);
    }
    await markSummarySent(db, issue, rendered.fingerprint, now);
    await announceChange(env);
    switch (result.outcome) {
      case "updated":
        return "The summary comment on GitHub was updated.";
      case "created":
        return "The summary comment was written to GitHub.";
      case "refused":
        return result.reason === "locked"
          ? "The issue is locked, so GitHub refuses the comment; it is not retried until the figures change."
          : "GitHub has no such issue any more (deleted or transferred); the comment is not retried until the figures change.";
      default:
        return "";
    }
  } finally {
    await releaseSummaryWrite(db, issue);
  }
}

/**
 * Whether the dashboard is configured in this deployment.
 *
 * @param env - Worker bindings.
 * @returns Settings when on.
 */
export function dashboardConfig(env: Env): GitHubConfig | null {
  return env.DASHBOARD_DB ? githubConfig(env) : null;
}

/**
 * Whether a path belongs to the dashboard (used to 404 it when off).
 *
 * @param path - Normalized path.
 * @returns True for dashboard paths other than `/`.
 */
export function isDashboardPath(path: string): boolean {
  return (path !== "/" && PATHS.has(path)) || REPORT_PATH.test(path) || ISSUE_PATH.test(path);
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
 * @param request - Incoming request.
 * @returns True when `Sec-Fetch-Site` is `cross-site`,
 *   `Sec-Fetch-Mode` is `navigate`, `Sec-Fetch-Dest` is `document` and
 *   `Sec-Fetch-User` is `?1`; browsers that send none of them get the
 *   sign-in page as before.
 */
export function crossSiteNavigation(request: Request): boolean {
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
 * @param target - Same-origin address from `safeNext` or `homeWith`.
 * @returns A 200 page with no cookies.
 */
function sameSiteReload(target: string): Response {
  return htmlResponse(sameSiteHop("Opening the dashboard", target, "Opening the dashboard."));
}

/**
 * The sign-in page's address with a return page.
 *
 * @param next - Page from `safeNext`, or null.
 * @returns `/` or `/?next=<encoded page>`.
 */
function homeWith(next: string | null): string {
  return next ? `/?next=${encodeURIComponent(next)}` : "/";
}

/**
 * Daily index cap from `DASHBOARD_INDEX_DAILY_ROWS`.
 *
 * @param env - Worker bindings.
 * @returns Positive cap.
 */
export function indexCap(env: Env): number {
  const n = Number(env.DASHBOARD_INDEX_DAILY_ROWS);
  return Number.isSafeInteger(n) && n > 0 ? n : DEFAULT_INDEX_DAILY_ROWS;
}

/**
 * Bounded filter text from a query parameter.
 *
 * @param q - Query.
 * @param name - Parameter.
 * @returns At most 128 characters, no control characters.
 */
function param(q: URLSearchParams, name: string): string {
  return (q.get(name) || "").replace(/[\u0000-\u001f\u007f]/g, "").slice(0, 128);
}

/**
 * Range id from the query, or the default.
 *
 * @param q - Query.
 * @returns A key of `RANGES`.
 */
function rangeOf(q: URLSearchParams): RangeId {
  const id = q.get("range") || "";
  return isRangeId(id) ? id : DEFAULT_RANGE;
}

/**
 * Today's triage spend and cap, read from #388's TRIAGE_BUDGET ledger.
 *
 * @param env - Worker bindings.
 * @param now - Unix time in milliseconds.
 * @returns Neurons.
 */
async function triageBudget(env: Env, now: number): Promise<{ spent: number; cap: number; }> {
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
 * @param request - Incoming request.
 * @param env - Worker bindings.
 * @param ctx - Execution context.
 * @param path - Normalized path.
 * @param b2 - B2 helpers from worker.ts.
 * @param now - Unix time in milliseconds (tests).
 * @returns The response, or null when the path is
 *   not the dashboard's or the dashboard is off.
 */
export async function dashboardFetch(request: Request, bindings: Env, ctx: Background, path: string, b2: B2Access, now: number = Date.now()): Promise<Response | null> {
  const cfg = dashboardConfig(bindings);
  if (!cfg || !bindings.DASHBOARD_DB || !(PATHS.has(path) || REPORT_PATH.test(path) || ISSUE_PATH.test(path))) return null;
  // One D1 call budget for the whole request; the reindex stops within it.
  const env: DashboardEnv = { ...bindings, DASHBOARD_DB: metered(bindings.DASHBOARD_DB) };
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

  if (method === "POST" && path === "/issues/link") {
    if (!sameOrigin(request)) {
      return htmlResponse(messagePage("Request refused", html`<p>Corrections are made from the dashboard's own buttons.</p>`), 403);
    }
    return correctLinks(request, env, member, now);
  }

  if (method === "POST" && (path === "/admin/reindex" || path === "/admin/sync" || path === "/admin/sign-out-all" || path === "/admin/update-issue")) {
    if (member.role !== "admin" || !sameOrigin(request)) {
      return htmlResponse(messagePage("Request refused", html`<p>Only organization owners can do this, from the dashboard's own buttons.</p>`), 403);
    }
    if (path === "/admin/update-issue") {
      const form = await readForm(request);
      const back = safeNext(formField(form, "back")) ?? "/issues";
      const issue = issueNumber(formField(form, "issue") ?? "");
      if (issue === null) {
        return htmlResponse(messagePage("Request refused", html`<p>The form names no issue.</p>`), 400);
      }
      const note = await updateIssueNow(env, cfg, request, issue, now);
      await setState(db, UPDATE_NOTE, `${issue}\t${now}\t${note}`);
      await logAccess(db, member, "update-issue", null, now);
      return redirect(back);
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
    const filter: ReportFilter = {
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
    const found = await getReport(db, m[1] ?? "");
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
        ? all<ReportBriefRow>(db, "SELECT report_id, received_at, version, os, arch FROM reports WHERE signature = ? AND report_id <> ? ORDER BY received_at DESC LIMIT 8", r.signature, r.report_id)
        : Promise.resolve([]),
      r.signature ? signatureDays(db, r.signature, now - 14 * DAY_MS) : Promise.resolve(new Map()),
      r.signature ? issuesTrackingSignature(db, r.signature) : Promise.resolve([]),
    ]);
    if (!liveRefresh) await logAccess(db, member, "view-report", r.report_id, now);
    // The fix shown is the tracking issue's, the issue the report counts
    // under, so the page agrees with the counts. A report no issue lists
    // yet shows the fix of an issue tracking its signature (one with a
    // fixed-in version first, since it can tell a regression, then the
    // newest merge), so a regression is visible before anyone links it.
    const pick = (numbers: number[]): ReportFix | undefined => numbers
      .flatMap((n) => {
        const status = afterFix.get(n);
        return status ? [{ ...status, issue: n }] : [];
      })
      .sort((a, b) => Number(Boolean(b.fixedIn)) - Number(Boolean(a.fixedIn)) || (b.fixAt ?? 0) - (a.fixAt ?? 0))[0];
    const fix = (r.issue_number == null ? pick(sameSignature) : pick([Number(r.issue_number)])) ?? null;
    const regression = Boolean(fix?.fixedIn && isRegressionVersion(String(r.version), fix.fixedIn));
    const today = Math.floor(now / DAY_MS);
    const counts = dailySeries(now, 14, days);
    const sigDays = counts.map((n, i) => {
      const day = today - 13 + i;
      return { label: fmtDay(day * DAY_MS), n, after: Boolean(fix?.fixAt && (day + 1) * DAY_MS > fix.fixAt) };
    });
    const markerIndex = fix?.fixAt ? Math.floor(fix.fixAt / DAY_MS) - (today - 13) : -1;
    const fixPr = found.prs.filter((p) => p.state === "merged").sort((a, b) => Number(b.merged_at) - Number(a.merged_at))[0];
    // A report no issue tracks gets a picker to attach it; one issue is the
    // normal case and needs none.
    const openIssues = found.issues.length === 0 ? await openIssueOptions(db) : [];
    return htmlResponse(
      reportPage({
        cfg, member, report: r, stored, issues: found.issues, prs: found.prs, fix, regression, similar, sigDays,
        sigMarker: fix?.fixAt && fixPr ? { index: markerIndex, label: `#${fixPr.pr_number} merged` } : null,
        openIssues,
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

  const im = ISSUE_PATH.exec(path);
  if (im) {
    const number = Number(im[1]);
    const [detail, afterFix, openIssues, updateNote] = await Promise.all([
      issueDetail(db, number, now - 14 * DAY_MS),
      fixStatus(db),
      openIssueOptions(db),
      member.role === "admin" ? getState(db, UPDATE_NOTE) : Promise.resolve(null),
    ]);
    if (!detail) {
      return htmlResponse(messagePage("Issue not found", html`<p>No diagnostics issue with that number is synced. The sync lists issues labeled <code>diagnostics</code>; an owner can run it from the overview.</p>`), 404);
    }
    return htmlResponse(
      issuePage({
        cfg, member, now, detail, fix: afterFix.get(number) ?? null, updateNote: updateNoteFor(updateNote, number, now),
        openIssues: openIssues.filter((i) => i.number !== number),
      }),
    );
  }
  return null;
}

/**
 * Reads a posted form body, bounded.
 *
 * @param request - Incoming POST.
 * @returns The body, or null when it is longer
 *   than `MAX_FORM_CHARS` bytes or cannot be read.
 */
async function readForm(request: Request): Promise<string | null> {
  const len = Number(request.headers.get("content-length") || "0");
  if (!Number.isFinite(len) || len > MAX_FORM_CHARS) return null;
  try {
    const text = await request.text();
    return text.length > MAX_FORM_CHARS ? null : text;
  } catch {
    return null;
  }
}

/**
 * One field of a form body.
 *
 * @param body - From `readForm`.
 * @param name - Field name.
 * @returns Its value.
 */
function formField(body: string | null, name: string): string | null {
  if (body === null) return null;
  try {
    return new URLSearchParams(body).get(name);
  } catch {
    return null;
  }
}

/**
 * `POST /issues/link` (members, same origin): applies one correction to the
 * ledger, logs it under the member's name and tells open pages. The issues
 * named must be synced and the report indexed; a correction that names
 * neither answers 404 so a stale form cannot write a dangling link.
 *
 * @param request - Incoming POST.
 * @param env - Worker bindings with a metered handle.
 * @param member - Signed-in member.
 * @param now - Unix time in milliseconds.
 * @returns A redirect back to the page the form named
 *   (a dashboard page), else to the issue's page.
 */
async function correctLinks(request: Request, env: DashboardEnv, member: Member, now: number): Promise<Response> {
  const db = env.DASHBOARD_DB;
  const body = await readForm(request);
  const change = parseLinkAction(body);
  if (!change) {
    return htmlResponse(messagePage("Request not understood", html`<p>The correction form was incomplete. Go back and try again.</p>`), 400);
  }
  const numbers = change.to === null ? [change.issue] : [change.issue, change.to];
  const [issues, report] = await Promise.all([
    all(db, `SELECT number, kind FROM issues WHERE number IN (${numbers.map(() => "?").join(", ")})`, ...numbers),
    change.report === null ? Promise.resolve({ ok: 1 }) : first(db, "SELECT 1 AS ok FROM reports WHERE report_id = ?", change.report),
  ]);
  if (issues.length !== numbers.length || !report) {
    return htmlResponse(messagePage("Not found", html`<p>That issue is not synced or that report is not indexed, so nothing was changed.</p>`), 404);
  }
  // Reports are attached to triaged issues only: the holding issue for
  // untriaged reports is ingest's, and a report given to it by hand would
  // never reach a summary comment.
  const destination = change.action === "attach" ? change.issue : change.to;
  if (destination !== null && issues.some((i) => Number(i.number) === destination && i.kind === "fallback")) {
    return htmlResponse(messagePage("Request refused", html`<p>Reports cannot be attached to the holding issue for untriaged reports; attach them to a triaged issue instead.</p>`), 400);
  }
  await applyLinkAction(db, change, member.login, now);
  await logAccess(db, member, change.action, change.report, now);
  await announceChange(env);
  const back = safeNext(formField(body, "back"));
  return redirect(back ?? `/issues/${change.action === "merge" ? change.to : change.issue}`);
}

/**
 * Cron entry: issue and pull request sync when the dashboard is on.
 *
 * @param env - Worker bindings.
 * @param now - Unix time in milliseconds.
 */
export async function dashboardScheduled(env: Env, now: number): Promise<void> {
  const cfg = dashboardConfig(env);
  if (!cfg || !env.DASHBOARD_DB) return;
  await syncAndAnnounce({ ...env, DASHBOARD_DB: metered(env.DASHBOARD_DB) }, cfg, now);
}

/**
 * Runs `work` and tells open pages to refresh only when it changed what
 * they show (the link fingerprint differs afterwards). The sync keeps D1
 * calls in reserve for the two fingerprint reads.
 *
 * @param env - Worker bindings with a metered
 *   handle.
 * @param work - The sync.
 * @returns What `work` returned.
 */
async function announceIfLinksChanged<T>(env: DashboardEnv, work: () => Promise<T>): Promise<T> {
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
 * @param env - Worker bindings with a metered handle.
 * @param cfg - Settings.
 * @param now - Unix time in milliseconds.
 * @returns The sync's counts.
 */
function syncAndAnnounce(env: DashboardEnv, cfg: GitHubConfig, now: number): Promise<{ issues: number; refreshed: number; }> {
  return announceIfLinksChanged(env, () => syncIssues(env, cfg, now));
}

/**
 * `POST /issues/notify` (ingest Action, `REPORT_API_KEY`): links a newly
 * filed issue right away instead of at the next cron.
 *
 * @param env - Worker bindings.
 * @param number - Issue number from `parseIssueNotify`.
 * @param now - Unix time in milliseconds.
 * @returns What happened, or null
 *   when the dashboard is off.
 */
export async function dashboardIssueNotify(bindings: Env, number: number, now: number): Promise<"linked" | "dropped" | null> {
  const cfg = dashboardConfig(bindings);
  if (!cfg || !bindings.DASHBOARD_DB) return null;
  const env: DashboardEnv = { ...bindings, DASHBOARD_DB: metered(bindings.DASHBOARD_DB) };
  return announceIfLinksChanged(env, () => syncIssueNow(env, cfg, number, now));
}

/**
 * `POST /issues/attach` (ingest Action, `REPORT_API_KEY`): records the
 * model's decision that reports belong to issues already open, as `triage`
 * ledger rows, with a live update when it linked anything.
 *
 * @param bindings - Worker bindings.
 * @param links - From `parseAttachRequest`.
 * @param now - Unix time in milliseconds.
 * @returns What `attachByTriage` did, or null when the dashboard database
 *   is not bound.
 */
export async function dashboardTriageAttach(bindings: Env, links: readonly AttachLink[], now: number): Promise<{ linked: number; existing: number; unlinked: string[]; } | null> {
  if (!bindings.DASHBOARD_DB) return null;
  const env: DashboardEnv = { ...bindings, DASHBOARD_DB: metered(bindings.DASHBOARD_DB) };
  return announceIfLinksChanged(env, () => attachByTriage(env.DASHBOARD_DB, links, now));
}

/**
 * `GET /issues/summaries` (ingest Action, `REPORT_API_KEY`): the issues
 * whose summary comment is out of date, with the figures to render.
 *
 * @param env - Worker bindings.
 * @param now - Unix time in milliseconds.
 * @returns Summaries, or null when the dashboard database is not bound.
 */
export async function dashboardIssueSummaries(env: Env, now: number): Promise<IssueSummary[] | null> {
  if (!env.DASHBOARD_DB) return null;
  const db = metered(env.DASHBOARD_DB);
  return handOutSummaries(db, await issueSummaries(db, SUMMARY_BATCH), now);
}

/**
 * `POST /issues/summary` (ingest Action, `REPORT_API_KEY`): records the
 * summary fingerprint the Action rendered on an issue.
 *
 * @param env - Worker bindings.
 * @param done - From `parseSummaryDone`.
 * @param now - Unix time in milliseconds.
 * @returns Whether the issue exists, or null when
 *   the dashboard database is not bound.
 */
export async function dashboardSummarySent(env: Env, done: { issue: number; fingerprint: string; }, now: number): Promise<boolean | null> {
  if (!env.DASHBOARD_DB) return null;
  return markSummarySent(metered(env.DASHBOARD_DB), done.issue, done.fingerprint, now);
}

/**
 * `GET /report?unfiled=1`: the reports some issue already lists, so ingest
 * does not triage them again when its pull cursor is lost (the Actions cache
 * that holds it can be evicted) or a run re-pulls from the start.
 *
 * @param env - Worker bindings.
 * @returns Lowercase report UUIDs linked to at
 *   least one issue, or null when the dashboard database is not bound.
 */
export async function dashboardFiledReportIds(env: Env): Promise<Set<string> | null> {
  if (!env.DASHBOARD_DB) return null;
  const rows = await all(env.DASHBOARD_DB, "SELECT DISTINCT report_id FROM report_issue WHERE source <> 'excluded'");
  return new Set(rows.map((r) => String(r.report_id)));
}

/**
 * `GET /report?unfiled=1`: links pulled reports to the open issue that
 * already tracks their signature (`linkBySignature`), so they join that
 * issue instead of going to triage, and tells open pages when any did.
 *
 * @param env - Worker bindings.
 * @param reportIds - Lowercase report UUIDs pulled by ingest.
 * @param now - Unix time in milliseconds.
 * @returns Report ID → issue number linked;
 *   empty when the dashboard database is not bound.
 */
export async function dashboardLinkBySignature(env: Env, reportIds: string[], now: number): Promise<Map<string, number>> {
  if (!env.DASHBOARD_DB || reportIds.length === 0) return new Map();
  const linked = await linkBySignature(env.DASHBOARD_DB, reportIds, now);
  if (linked.size) await announceChange(env);
  return linked;
}

/**
 * `GET /report?unfiled=1`, after `dashboardLinkBySignature`: sorts the
 * reports still unlinked against closed issues (`matchClosedBySignature`),
 * so a regression is handed to ingest with the issue it goes back to and a
 * report from an old build joins the closed issue quietly; tells open pages
 * when any was linked.
 *
 * @param env - Worker bindings.
 * @param reportIds - Lowercase report UUIDs pulled by ingest and
 *   not linked to an open issue.
 * @param now - Unix time in milliseconds.
 * @returns As `matchClosedBySignature`; both maps empty when the dashboard
 *   database is not bound.
 */
export async function dashboardMatchClosed(env: Env, reportIds: string[], now: number): Promise<Awaited<ReturnType<typeof matchClosedBySignature>>> {
  if (!env.DASHBOARD_DB || reportIds.length === 0) return { regressions: new Map(), linked: new Map() };
  const out = await matchClosedBySignature(env.DASHBOARD_DB, reportIds, now);
  if (out.linked.size) await announceChange(env);
  return out;
}

/**
 * After `/submit`: indexes the stored report and, when a row was added,
 * tells open pages. Failures are the caller's to swallow.
 *
 * @param env - Worker bindings.
 * @param enriched - The object written to B2.
 * @param objectKey - Its key.
 * @returns What happened.
 */
export async function dashboardIndexSubmitted(env: DashboardEnv, enriched: SubmittedReport, objectKey: string): Promise<"indexed" | "capped"> {
  const result = await indexSubmitted(env.DASHBOARD_DB, enriched, objectKey, indexCap(env));
  if (result === "indexed") await announceChange(env);
  return result;
}
