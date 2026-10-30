/**
 * Bookclerk diagnostics Worker (deployed via GitHub Actions + wrangler-action).
 *
 * POST /submit  — clients send redacted JSON; validated + enriched → B2
 * GET  /report  — GitHub Action pulls new objects since `since`, or since the
 *                 stored cursor with `cursor=stored` (secret key); with
 *                 `unfiled=1` it skips reports an issue already lists and
 *                 links reports whose error signature an open issue tracks
 *                 to that issue instead of returning them
 * POST /report/cursor — GitHub Action stores where its last pull ended
 *                 (secret key; see ingest-cursor.ts)
 * POST /triage  — GitHub Action sends the sanitized batch; Workers AI returns
 *                 an issue plan, under a daily neuron budget (secret key; see
 *                 triage.ts)
 * GET  /health  — liveness
 *
 * With the dashboard configured (see dashboard/routes.ts), `/` is the
 * GitHub sign-in page and `/dash`, `/reports`, `/reports/<id>` and `/issues`
 * serve organization members; without it `/` answers like `/health`.
 *
 * Object layout: diagnostics/<version>/<report_id>.json
 *
 * /report selects objects whose path version is the latest *stable* GitHub
 * release, newer (e.g. prerelease of a future version), or a packaging
 * derivative of that release. With no GitHub releases yet, all versions match.
 *
 * Secrets (set by deploy workflow from GitHub secrets):
 *   B2_KEY_ID, B2_APPLICATION_KEY, B2_BUCKET_ID
 *   REPORT_API_KEY          — required for /report, /report/cursor, /triage,
 *                             /issues/notify, /issues/attach,
 *                             /issues/summaries and /issues/summary
 *   GITHUB_TOKEN            — optional; higher GitHub API rate limits
 *   GITHUB_APP_CLIENT_ID, GITHUB_APP_CLIENT_SECRET, GITHUB_APP_PRIVATE_KEY
 *                           — optional; the automation App, for dashboard
 *                             sign-in, read-only issue sync and the owner's
 *                             "Update GitHub now" comment edit (issues: write)
 *
 * Vars:
 *   GITHUB_REPOSITORY       — owner/repo for releases/latest (e.g. Bookclerk/bookclerk)
 *   TRIAGE_MODEL            — optional; Workers AI model for /triage
 *   TRIAGE_DAILY_NEURONS    — optional; /triage neuron cap per UTC day
 *
 * Bindings: AI (Workers AI), TRIAGE_BUDGET (KV, the daily neuron ledger),
 * DASHBOARD_DB (D1, the dashboard's report index and sessions), ASSETS
 * (dashboard styles, script and fonts under /static/), LIVE (the LiveHub
 * Durable Object that pushes "changed" to open dashboard pages; see
 * dashboard/live.ts).
 *
 * REPORT_API_KEY only ever lives in the Worker and the GitHub Actions
 * secrets; Bookclerk builds carry the collector URL, never this key.
 */

import {
  MAX_ATTACH_BODY_CHARS,
  parseAttachRequest,
  parseIssueNotify,
  parseSummaryDone,
} from "./dashboard/github.ts";
import { reportIdOf } from "./dashboard/report-ids.ts";
import {
  dashboardFetch,
  dashboardFiledReportIds,
  dashboardIndexSubmitted,
  dashboardIssueNotify,
  dashboardTriageAttach,
  dashboardIssueSummaries,
  dashboardLinkBySignature,
  dashboardSummarySent,
  dashboardMatchClosed,
  dashboardScheduled,
} from "./dashboard/routes.ts";
import { MAX_CURSOR_CHARS, commitCursor, parseCursor, readCursor } from "./ingest-cursor.ts";
import { handleTriage, readCapped } from "./triage.ts";
import { extractVersionFromKey, normalizeVersion, versionAcceptable } from "./version-filter.ts";
import type { B2Access, B2Auth } from "./dashboard/storage.ts";
import type { Background, Env } from "./env.ts";
import { asObject, isObject, isRecordLike, type JsonObject } from "./json.ts";

// The Durable Object class must be exported from the main module.
export { LiveHub } from "./dashboard/live.ts";

/** One report `GET /report` returns. */
export type PulledReport = {
  /** The stored object's `report_id`, or null when it has none. */
  report_id: unknown;
  /** Object key. */
  file_name: string;
  /** B2 file ID. */
  file_id: string;
  /** Upload time, Unix ms. */
  upload_timestamp_ms: number;
  /** Size in bytes. */
  content_length: number;
  /** Version from the object key, or null. */
  object_version: string | null;
  /** The closed issue the report regresses, or null. */
  regression_of: {
    issue: number;
    fixed_in: string;
    pr_number: number | null;
    title: string;
  } | null;
  /** The stored object. */
  report: unknown;
};

/** A report `GET /report` linked to an issue instead of returning it. */
export type LinkedReport = {
  /** Report UUID. */
  report_id: string;
  /** The issue it joined. */
  issue: number | undefined;
  /** That issue's state. */
  state: "open" | "closed";
};

/** The answer to `GET /report`. */
export type ReportAnswer = {
  /** Always true. */
  ok: true;
  /** Cursor the pull started from: upload time, Unix ms. */
  since: number;
  /** Cursor the pull started from: object name. */
  after: string;
  /** Where the starting cursor came from. */
  cursor_source: "stored" | "query";
  /** Cursor for the next pull: upload time. */
  next_since: number;
  /** Cursor for the next pull: object name. */
  next_after: string;
  /** Reports returned. */
  count: number;
  /** Whether more reports wait past this pull. */
  truncated: boolean;
  /** Whether the bucket listing hit its page cap. */
  list_truncated: boolean;
  /** Version reports must reach, or null for all. */
  baseline_version: string | null;
  /** Where the baseline came from. */
  baseline_source: string;
  /** Reports passed over for their version. */
  skipped_version: number;
  /** Reports passed over as unreadable. */
  skipped_corrupt: number;
  /** Reports passed over because an issue lists them. */
  skipped_filed: number;
  /** Whether the filed filter was on. */
  filed_filter: "on" | "off";
  /** Reports linked to an open issue by signature. */
  linked_signature: number;
  /** Reports linked instead of returned. */
  linked: LinkedReport[];
  /** Returned reports that regress a closed issue. */
  regressions: number;
  /** The reports. */
  reports: PulledReport[];
};

/** The answer to `POST /report/cursor`. */
export type CursorAnswer = {
  /** Always true. */
  ok: true;
  /** Whether the stored cursor moved. */
  advanced: boolean;
  /** Stored cursor: upload time, Unix ms. */
  since: number;
  /** Stored cursor: object name. */
  after: string;
};

/** An error answer. */
export type ErrorAnswer = {
  /** What went wrong. */
  error: string;
  /** The failed upstream stage, for callers with the report key. */
  detail?: string;
};

const MAX_BODY_BYTES = 256 * 1024;
const DIAGNOSTICS_PREFIX = "diagnostics/";
const MAX_REPORT_FILES = 100;

/**
 * A call to B2 or GitHub failed. The message names the stage, the HTTP
 * status and, for B2, its short error code, and never carries credentials or
 * response bodies, so it is safe to return to an authenticated caller.
 */
export class UpstreamError extends Error {
  /**
   * @param stage - Which call failed, e.g. "b2 list".
   * @param status - HTTP status of the failed response.
   * @param code - Upstream error code, kept only when it is a short
   *   lowercase identifier.
   */
  constructor(stage: string, status: number = 0, code: string | undefined = undefined) {
    const parts = [`${stage} failed`];
    if (status) parts.push(`HTTP ${status}`);
    if (typeof code === "string" && /^[a-z_]{1,40}$/.test(code)) parts.push(code);
    super(parts.join(": "));
    this.name = "UpstreamError";
  }
}

/** Longest B2 error body, in UTF-16 units, parsed when looking for its error code. */
const MAX_B2_ERROR_CHARS = 4096;

/**
 * Reads B2's error code from an error response body. Sans-IO, so the fuzz
 * test drives the same code the Worker runs.
 *
 * @param text - The response body (from B2, untrusted).
 * @returns The body's `code` when the body is a JSON
 *   object of at most `MAX_B2_ERROR_CHARS` UTF-16 units whose `code` is a
 *   lowercase identifier of 1 to 40 characters, else undefined.
 */
export function b2ErrorCode(text: string): string | undefined {
  if (typeof text !== "string" || text.length > MAX_B2_ERROR_CHARS) return undefined;
  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    return undefined;
  }
  if (!isObject(body)) return undefined;
  const code = body.code;
  return typeof code === "string" && /^[a-z_]{1,40}$/.test(code) ? code : undefined;
}

/**
 * Builds the UpstreamError for a failed B2 response.
 *
 * @param stage - Which call failed.
 * @param res - The failed response.
 * @returns The error to throw.
 */
async function b2Error(stage: string, res: Response): Promise<UpstreamError> {
  let text;
  try {
    // Stops reading past the limit (null), so an oversized error body
    // costs no more than the limit before it is dropped.
    text = (await readCapped(res, MAX_B2_ERROR_CHARS)) ?? "";
  } catch {
    text = "";
  }
  return new UpstreamError(stage, res.status, b2ErrorCode(text));
}

/**
 * Calls fetch, turning a rejected request (DNS, TLS, connection reset) into
 * an UpstreamError for `stage`. The runtime's own message is dropped, since
 * it can echo the URL or headers.
 *
 * @param stage - Which call this is, e.g. "b2 list".
 * @param url - Request URL.
 * @param init - Request options.
 * @returns The response, whatever its status.
 */
async function upstreamFetch(stage: string, url: string, init: RequestInit): Promise<Response> {
  try {
    return await fetch(url, init);
  } catch {
    throw new UpstreamError(stage);
  }
}

/**
 * One-line description of a thrown value for the Worker log.
 *
 * @param err - What was thrown.
 * @returns Its name and message, or its string form.
 */
function errorText(err: unknown): string {
  return err instanceof Error ? `${err.name}: ${err.message}` : String(err);
}

/**
 * The collector's robots.txt: no crawler may read any page, and the content
 * signals (Cloudflare's Content Signals Policy, contentsignals.org) refuse
 * search, AI input, AI training and any reuse. Nothing here is public
 * content: the dashboard is for maintainers and the rest is an API.
 */
export const ROBOTS_TXT = `# As a condition of accessing this website, you agree to abide by the
# following content signals:

# (a)  If a content-signal = yes, you may collect content for the
#      corresponding use.
# (b)  If a content-signal = no, you may not collect content for the
#      corresponding use.
# (c)  If the website operator does not include a content signal for a
#      corresponding use, the website operator neither grants nor restricts
#      permission via content signal with respect to the corresponding use.

# The content signals and their meanings are:

# search: building a search index and providing search results (e.g., returning
#         hyperlinks and short excerpts from your website's contents). Search
#         does not include providing AI-generated search summaries.
# ai-input: inputting content into one or more AI models (e.g., retrieval
#           augmented generation, grounding, or other real-time taking of
#           content for generative AI search answers).
# ai-train: training or fine-tuning AI models.
# use: what may be kept and reused after accessing the content. "immediate"
#      means interact, but store and reuse nothing.

# ANY RESTRICTIONS EXPRESSED VIA CONTENT SIGNALS ARE EXPRESS RESERVATIONS OF
# RIGHTS UNDER ARTICLE 4 OF THE EUROPEAN UNION DIRECTIVE 2019/790 ON COPYRIGHT
# AND RELATED RIGHTS IN THE DIGITAL SINGLE MARKET.

User-agent: *
Content-Signal: search=no, ai-input=no, ai-train=no, use=immediate
Disallow: /
`;

export default {
  async fetch(request: Request, env: Env, ctx: Background): Promise<Response> {
    const url = new URL(request.url);
    const path = url.pathname.replace(/\/+$/, "") || "/";

    if (path === "/robots.txt" && (request.method === "GET" || request.method === "HEAD")) {
      return new Response(request.method === "HEAD" ? null : ROBOTS_TXT, {
        headers: {
          "Content-Type": "text/plain; charset=utf-8",
          "Cache-Control": "public, max-age=3600",
          "X-Content-Type-Options": "nosniff",
        },
      });
    }

    try {
      const page = await dashboardFetch(request, env, ctx, path, b2Access(env));
      if (page) return page;

      if (request.method === "GET" && (path === "/" || path === "/health")) {
        const origin = new URL(request.url).origin;
        return json({
          ok: true,
          service: "bookclerk-diagnostics",
          url: origin,
        });
      }

      // Awaited so a rejection lands in the catch below instead of escaping
      // as Cloudflare's opaque error 1101.
      if (request.method === "POST" && path === "/submit") {
        return await handleSubmit(request, env, ctx);
      }

      if (request.method === "GET" && path === "/report") {
        return await handleReport(request, env, url);
      }

      if (request.method === "POST" && path === "/report/cursor") {
        if (!requireReportAuth(request, env)) {
          return json({ error: "unauthorized" }, 401);
        }
        return await handleCursorCommit(request, env);
      }

      if (request.method === "POST" && path === "/issues/notify") {
        if (!requireReportAuth(request, env)) {
          return json({ error: "unauthorized" }, 401);
        }
        return await handleIssueNotify(request, env);
      }

      if (request.method === "POST" && path === "/issues/attach") {
        if (!requireReportAuth(request, env)) {
          return json({ error: "unauthorized" }, 401);
        }
        return await handleTriageAttach(request, env);
      }

      if (request.method === "GET" && path === "/issues/summaries") {
        if (!requireReportAuth(request, env)) {
          return json({ error: "unauthorized" }, 401);
        }
        const summaries = await dashboardIssueSummaries(env, Date.now());
        if (summaries === null) return new Response(null, { status: 204 });
        return json({ issues: summaries });
      }

      if (request.method === "POST" && path === "/issues/summary") {
        if (!requireReportAuth(request, env)) {
          return json({ error: "unauthorized" }, 401);
        }
        return await handleSummaryDone(request, env);
      }

      if (request.method === "POST" && path === "/triage") {
        if (!requireReportAuth(request, env)) {
          return json({ error: "unauthorized" }, 401);
        }
        return await handleTriage(request, env);
      }

      return json({ error: "not found" }, 404);
    } catch (err) {
      console.error(`${request.method} ${path} failed: ${errorText(err)}`);
      if (err instanceof UpstreamError) {
        // Only callers holding the report key learn which upstream failed;
        // the messages carry a stage, an HTTP status and a short code only.
        const detail = requireReportAuth(request, env) ? { detail: err.message } : {};
        return json({ error: "upstream failed", ...detail }, 502);
      }
      return json({ error: "internal error" }, 500);
    }
  },

  async scheduled(_controller, env, ctx): Promise<void> {
    ctx.waitUntil(dashboardScheduled(env, Date.now()));
  },
} satisfies ExportedHandler<Env>;

/**
 * The B2 helpers the dashboard reads stored reports with.
 *
 * @param env - Worker bindings.
 * @returns Helpers bound to this
 *   Worker's B2 credentials.
 */
function b2Access(env: Env): B2Access {
  return {
    authorize: () => b2Authorize(env),
    downloadById: (auth, fileId) => b2DownloadById(auth, fileId),
    bucketId: env.B2_BUCKET_ID ?? "",
  };
}

/**
 * `POST /report/cursor`: the ingest Action stores the cursor its last pull
 * ended at, so the next run starts there. A cursor earlier than the stored
 * one (a forced full pull) leaves it as is.
 *
 * @param request - Authenticated request with body
 *   `{"since": <ms>, "after": "<object name>"}`.
 * @param env - Worker bindings.
 * @returns 200 with the stored cursor, 400 for a
 *   malformed body, or 503 without the DASHBOARD_DB binding.
 */
async function handleCursorCommit(request: Request, env: Env): Promise<Response> {
  const text = await readCapped(request, MAX_CURSOR_CHARS);
  const next = text === null ? null : parseCursor(text);
  if (next === null) return json({ error: 'expected {"since": <ms>, "after": "<name>"}' }, 400);
  if (!env.DASHBOARD_DB)
    return json({ error: "no DASHBOARD_DB binding to store the cursor in" }, 503);
  const { advanced, cursor } = await commitCursor(env.DASHBOARD_DB, next);
  return json({
    ok: true,
    advanced,
    since: cursor.since,
    after: cursor.after,
  } satisfies CursorAnswer);
}

/**
 * `POST /issues/notify`: the ingest Action names an issue it just filed so
 * the dashboard links it at once. Answers 202 with what happened, 204 when
 * the dashboard is off (so forks without it do not fail the Action), or 400
 * for a malformed body.
 *
 * @param request - Authenticated request.
 * @param env - Worker bindings.
 * @returns The answer.
 */
async function handleIssueNotify(request: Request, env: Env): Promise<Response> {
  const text = await readCapped(request, 256);
  const number = text === null ? null : parseIssueNotify(text);
  if (number === null) return json({ error: 'expected {"issue": <number>}' }, 400);
  const result = await dashboardIssueNotify(env, number, Date.now());
  if (result === null) return new Response(null, { status: 204 });
  return json({ ok: true, issue: number, result }, 202);
}

/**
 * `POST /issues/attach`: the ingest Action hands over the model's validated
 * `attach` decisions, and the dashboard links each report to its open issue
 * as a `triage` row (never over a member's exclusion or an existing link).
 * Answers 200 with the counts and the report IDs it could not link because
 * their issue is closed, the holding issue or unknown; 204 when the
 * dashboard is off; or 400 for a malformed body.
 *
 * @param request - Authenticated request.
 * @param env - Worker bindings.
 * @returns The answer.
 */
async function handleTriageAttach(request: Request, env: Env): Promise<Response> {
  const text = await readCapped(request, MAX_ATTACH_BODY_CHARS);
  const links = text === null ? null : parseAttachRequest(text);
  if (links === null)
    return json(
      { error: 'expected {"links": [{"issue": <number>, "report_ids": ["<uuid>"]}]}' },
      400,
    );
  const result = await dashboardTriageAttach(env, links, Date.now());
  if (result === null) return new Response(null, { status: 204 });
  return json({ ok: true, ...result });
}

/**
 * `POST /issues/summary`: the ingest Action says which summary fingerprint
 * it wrote on an issue, so `GET /issues/summaries` stops handing that issue
 * back until its figures change. Answers 200, 204 when the dashboard is off,
 * 404 for an issue the dashboard does not know, or 400 for a malformed body.
 *
 * @param request - Authenticated request.
 * @param env - Worker bindings.
 * @returns The answer.
 */
async function handleSummaryDone(request: Request, env: Env): Promise<Response> {
  const text = await readCapped(request, 512);
  const done = text === null ? null : parseSummaryDone(text);
  if (done === null)
    return json({ error: 'expected {"issue": <number>, "fingerprint": "<sha-256 hex>"}' }, 400);
  const known = await dashboardSummarySent(env, done, Date.now());
  if (known === null) return new Response(null, { status: 204 });
  if (!known) return json({ error: "unknown issue" }, 404);
  return json({ ok: true, issue: done.issue });
}

/**
 * `POST /submit`: validates a client's redacted report, enriches it and
 * stores it in B2, then indexes it for the dashboard in the background.
 *
 * @param request - The client's request.
 * @param env - Worker bindings.
 * @param ctx - Execution context, for the background index write.
 * @returns 201 with the report's ID and object key, or 4xx for a body
 *   that is refused.
 */
async function handleSubmit(request: Request, env: Env, ctx: Background): Promise<Response> {
  const len = Number(request.headers.get("content-length") || "0");
  if (len > MAX_BODY_BYTES) {
    return json({ error: "payload too large" }, 413);
  }

  const raw = await request.arrayBuffer();
  if (raw.byteLength === 0 || raw.byteLength > MAX_BODY_BYTES) {
    return json({ error: "invalid payload size" }, 400);
  }

  let parsed: unknown;
  try {
    parsed = JSON.parse(new TextDecoder().decode(raw));
  } catch {
    return json({ error: "invalid json" }, 400);
  }

  const checked = validateClientPayload(parsed);
  if (typeof checked === "string") {
    return json({ error: checked }, 400);
  }
  const payload = checked;

  const asText = JSON.stringify(payload);
  if (looksLikeSecretLeak(asText)) {
    return json({ error: "payload rejected by secret heuristics" }, 400);
  }

  const receivedAt = Date.now();
  const reportId = crypto.randomUUID();
  const enriched = {
    schema_version: 1,
    report_id: reportId,
    received_at_unix_ms: receivedAt,
    worker: "bookclerk-diagnostics",
    user_agent: truncate(request.headers.get("user-agent") || "", 200),
    payload,
  };

  const bodyText = JSON.stringify(enriched);
  if (looksLikeSecretLeak(bodyText)) {
    return json({ error: "enriched payload rejected" }, 400);
  }

  const versionToken = versionPathToken(payload.version);
  const objectKey = `${DIAGNOSTICS_PREFIX}${versionToken}/${reportId}.json`;
  const auth = await b2Authorize(env);
  const upload = await b2GetUploadUrl(auth, env.B2_BUCKET_ID ?? "");
  await b2Upload(upload, objectKey, bodyText, {
    report_id: reportId,
    received_at_unix_ms: String(receivedAt),
    trigger: safeToken(payload.trigger, "report"),
    bookclerk_version: truncate(String(payload.version || ""), 64),
  });

  // Best-effort dashboard index row; a failure leaves the report for the
  // owner's reindex and never fails the upload.
  const db = env.DASHBOARD_DB;
  if (db) {
    ctx.waitUntil(
      dashboardIndexSubmitted({ ...env, DASHBOARD_DB: db }, enriched, objectKey).catch(() => {}),
    );
  }

  return json(
    {
      ok: true,
      report_id: reportId,
      key: objectKey,
      received_at_unix_ms: receivedAt,
    },
    201,
  );
}

/**
 * `GET /report`: the ingest Action pulls the stored reports after a cursor,
 * passing over versions older than the latest stable release and, with
 * `unfiled=1`, reports an issue already lists or tracks by signature.
 *
 * @param request - The Action's request (needs the report key).
 * @param env - Worker bindings.
 * @param url - The request URL.
 * @returns The reports and the next cursor, or an error answer.
 */
async function handleReport(request: Request, env: Env, url: URL): Promise<Response> {
  if (!requireReportAuth(request, env)) {
    return json({ error: "unauthorized" }, 401);
  }

  let since: number;
  let afterName: string;
  let cursorSource: "stored" | "query";
  if (url.searchParams.get("cursor") === "stored" && !url.searchParams.has("since")) {
    if (!env.DASHBOARD_DB) return json({ error: "no DASHBOARD_DB binding holding a cursor" }, 503);
    ({ since, after: afterName } = await readCursor(env.DASHBOARD_DB));
    cursorSource = "stored";
  } else {
    const sinceRaw = url.searchParams.get("since");
    since = sinceRaw != null && sinceRaw !== "" ? Number(sinceRaw) : 0;
    if (!Number.isFinite(since) || since < 0) {
      return json({ error: "invalid since" }, 400);
    }
    // Secondary cursor: exclusive fileName within the same uploadTimestamp as `since`.
    afterName = url.searchParams.get("after") || "";
    cursorSource = "query";
  }
  // Reports an issue already lists are passed over (and the cursor moves past
  // them) so a re-pull cannot file them a second time. Off when the dashboard
  // database is not bound: nothing records the links then.
  const filed = url.searchParams.get("unfiled") === "1" ? await dashboardFiledReportIds(env) : null;

  // Optional override for tests/ops; otherwise resolve latest stable from GitHub.
  let baselineVersion: string | null = null;
  let baselineSource = "all";
  if (url.searchParams.has("baseline_version")) {
    const raw = url.searchParams.get("baseline_version") ?? "";
    baselineVersion = normalizeVersion(raw);
    baselineSource = baselineVersion ? "query" : "all";
  } else {
    const resolved = await resolveLatestStableBaseline(env);
    baselineVersion = resolved.version;
    baselineSource = resolved.source;
  }

  const auth = await b2Authorize(env);
  const listed = await b2ListIncoming(auth, env.B2_BUCKET_ID ?? "");
  const files = listed.files;
  const newer = files
    .filter((f) => f.action === "upload" && isAfterCursor(f, since, afterName))
    .sort(compareUploadOrder);

  // First pass: pick the objects this pull covers and move the cursor past
  // them; downloads wait until the dashboard has had a chance to link some
  // of them to an issue by signature, which needs only the index.
  const picked: { f: B2File; ts: number; objectVersion: string | null; listedId: string | null }[] =
    [];
  let maxTs = since;
  let nextAfter = afterName;
  let skippedVersion = 0;
  let skippedFiled = 0;
  let truncated = false;
  for (const f of newer) {
    const ts = Number(f.uploadTimestamp);
    const objectVersion = extractVersionFromKey(f.fileName);
    if (!versionAcceptable(objectVersion, baselineVersion)) {
      // Advance past rejected versions while scanning so they are not stuck forever.
      ({ maxTs, nextAfter } = advanceCursor(maxTs, nextAfter, ts, f.fileName));
      skippedVersion += 1;
      continue;
    }
    const listedId = reportIdOf(f.fileName);
    if (filed && listedId && filed.has(listedId)) {
      ({ maxTs, nextAfter } = advanceCursor(maxTs, nextAfter, ts, f.fileName));
      skippedFiled += 1;
      continue;
    }
    if (picked.length >= MAX_REPORT_FILES) {
      truncated = true;
      break;
    }
    ({ maxTs, nextAfter } = advanceCursor(maxTs, nextAfter, ts, f.fileName));
    picked.push({ f, ts, objectVersion, listedId });
  }

  // Reports an open issue already tracks by signature join that issue here
  // and are not returned: nothing to triage. Only with the filed filter on,
  // so a `force_full` run still re-triages everything.
  let linkedBySignature = new Map<string, number>();
  if (filed) {
    try {
      linkedBySignature = await dashboardLinkBySignature(
        env,
        picked.map((p) => p.listedId).filter((id) => id !== null),
        Date.now(),
      );
    } catch (err) {
      // The pull needs only B2; without the links these reports go to
      // triage as before.
      console.error(
        `signature linking failed: ${err instanceof Error ? err.message : String(err)}`,
      );
    }
  }

  // The rest are sorted against closed issues: a report from the version
  // that fixed one, or later, is returned as a regression of it for ingest
  // to open a new issue over; one from an older build on the same version
  // line joins the closed issue quietly and is not returned.
  let closed: Awaited<ReturnType<typeof dashboardMatchClosed>> = {
    regressions: new Map(),
    linked: new Map(),
  };
  if (filed) {
    try {
      closed = await dashboardMatchClosed(
        env,
        picked
          .map((p) => p.listedId)
          .filter((id): id is string => id !== null && !linkedBySignature.has(id)),
        Date.now(),
      );
    } catch (err) {
      console.error(
        `closed-issue matching failed: ${err instanceof Error ? err.message : String(err)}`,
      );
    }
  }

  const reports: PulledReport[] = [];
  const linked: LinkedReport[] = [];
  let regressions = 0;
  let skippedCorrupt = 0;
  for (const { f, ts, objectVersion, listedId } of picked) {
    if (listedId && linkedBySignature.has(listedId)) {
      linked.push({ report_id: listedId, issue: linkedBySignature.get(listedId), state: "open" });
      continue;
    }
    if (listedId && closed.linked.has(listedId)) {
      linked.push({ report_id: listedId, issue: closed.linked.get(listedId), state: "closed" });
      continue;
    }
    const downloaded = await b2DownloadById(auth, f.fileId);
    let parsed: unknown;
    try {
      parsed = JSON.parse(downloaded);
    } catch {
      // Corrupt objects must not stall the cursor (it is already past them).
      skippedCorrupt += 1;
      continue;
    }
    const regression = listedId ? closed.regressions.get(listedId) : undefined;
    if (regression) regressions += 1;
    reports.push({
      report_id: asObject(parsed).report_id || null,
      file_name: f.fileName,
      file_id: f.fileId,
      upload_timestamp_ms: ts,
      content_length: f.contentLength,
      object_version: objectVersion,
      // The closed issue this report regresses, for ingest to name in the
      // issue it opens (scripts/regression_issue.py); absent otherwise.
      regression_of: regression
        ? {
            issue: regression.issue,
            fixed_in: regression.fixedIn,
            pr_number: regression.prNumber,
            title: regression.title,
          }
        : null,
      report: parsed,
    });
  }

  const advanced = picked.length > 0 || skippedVersion > 0 || skippedFiled > 0;
  return json({
    ok: true,
    since,
    after: afterName,
    cursor_source: cursorSource,
    next_since: advanced ? maxTs : since,
    next_after: advanced ? nextAfter : afterName,
    count: reports.length,
    truncated: truncated || listed.listTruncated,
    list_truncated: listed.listTruncated,
    baseline_version: baselineVersion || null,
    baseline_source: baselineSource,
    skipped_version: skippedVersion,
    skipped_corrupt: skippedCorrupt,
    skipped_filed: skippedFiled,
    filed_filter: filed ? "on" : "off",
    linked_signature: linked.length,
    linked,
    regressions,
    reports,
  } satisfies ReportAnswer);
}

/**
 * Whether an object comes after the cursor in upload order.
 *
 * @param file - A listed object.
 * @param since - Cursor upload time, in milliseconds.
 * @param afterName - Last object name read at `since`.
 * @returns True when the object is newer.
 */
function isAfterCursor(file: B2File, since: number, afterName: string): boolean {
  const ts = Number(file.uploadTimestamp);
  if (ts > since) return true;
  if (ts === since && file.fileName > afterName) return true;
  return false;
}

/**
 * Orders listed objects by upload time, then by name.
 *
 * @param a - One object.
 * @param b - Another.
 * @returns Negative, zero or positive, as for `Array.prototype.sort`.
 */
function compareUploadOrder(a: B2File, b: B2File): number {
  const dt = Number(a.uploadTimestamp) - Number(b.uploadTimestamp);
  if (dt !== 0) return dt;
  if (a.fileName < b.fileName) return -1;
  if (a.fileName > b.fileName) return 1;
  return 0;
}

/**
 * Moves the cursor past one object when it is later than the cursor.
 *
 * @param maxTs - Cursor upload time, in milliseconds.
 * @param nextAfter - Cursor object name.
 * @param ts - The object's upload time, in milliseconds.
 * @param fileName - The object's name.
 * @returns The cursor after the object.
 */
function advanceCursor(
  maxTs: number,
  nextAfter: string,
  ts: number,
  fileName: string,
): { maxTs: number; nextAfter: string } {
  if (ts > maxTs) {
    return { maxTs: ts, nextAfter: fileName };
  }
  if (ts === maxTs && fileName > nextAfter) {
    return { maxTs, nextAfter: fileName };
  }
  return { maxTs, nextAfter };
}

/**
 * Latest non-prerelease GitHub release tag, or null when none exist yet.
 *
 * @param env - Worker bindings.
 * @returns The normalized version and where it came from.
 */
async function resolveLatestStableBaseline(
  env: Env,
): Promise<{ version: string | null; source: string }> {
  const repo = (env.GITHUB_REPOSITORY || "").trim();
  if (!repo || !/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repo)) {
    return { version: null, source: "all" };
  }

  const headers: Record<string, string> = {
    Accept: "application/vnd.github+json",
    "User-Agent": "bookclerk-diagnostics-worker",
    "X-GitHub-Api-Version": "2022-11-28",
  };
  if (env.GITHUB_TOKEN) {
    headers.Authorization = `Bearer ${env.GITHUB_TOKEN}`;
  }

  const res = await upstreamFetch(
    "github releases/latest",
    `https://api.github.com/repos/${repo}/releases/latest`,
    {
      headers,
    },
  );

  // No releases published yet → ingest every version.
  if (res.status === 404) {
    return { version: null, source: "all" };
  }
  if (!res.ok) {
    throw new UpstreamError("github releases/latest", res.status);
  }

  const data: unknown = await res.json();
  if (!isObject(data) || data.prerelease || data.draft) {
    return { version: null, source: "all" };
  }
  const version = normalizeVersion(typeof data.tag_name === "string" ? data.tag_name : "");
  if (!version) {
    return { version: null, source: "all" };
  }
  return { version, source: "github_latest_stable" };
}

/**
 * Whether a request carries the report key, as a Bearer token or in
 * `X-Bookclerk-Report-Key`.
 *
 * @param request - Incoming request.
 * @param env - Worker bindings.
 * @returns False as well when the Worker has no key of at least 16
 *   characters.
 */
function requireReportAuth(request: Request, env: Env): boolean {
  const expected = env.REPORT_API_KEY;
  if (!expected || typeof expected !== "string" || expected.length < 16) {
    return false;
  }
  const header = request.headers.get("authorization") || "";
  const bearer = header.toLowerCase().startsWith("bearer ") ? header.slice(7).trim() : "";
  const alt = (request.headers.get("x-bookclerk-report-key") || "").trim();
  return timingSafe.equal(bearer, expected) || timingSafe.equal(alt, expected);
}

/** Constant-time string compare for equal-length secrets. */
const timingSafe = {
  /**
   * Compares two strings in time that depends only on their length.
   *
   * @param a - One string.
   * @param b - Another.
   * @returns True when both are strings and equal.
   */
  equal(a: unknown, b: unknown): boolean {
    if (typeof a !== "string" || typeof b !== "string" || a.length !== b.length) {
      return false;
    }
    let out = 0;
    for (let i = 0; i < a.length; i++) {
      out |= a.charCodeAt(i) ^ b.charCodeAt(i);
    }
    return out === 0;
  },
};

/** A client report that passed `validateClientPayload`. */
type ClientPayload = JsonObject & {
  /** What made the client send it. */
  trigger: string;
  /** The client's bookclerk version. */
  version: string;
  /** The client's operating system. */
  os: string;
  /** The client's log events. */
  events: unknown[];
};

/**
 * Checks the shape of a client report.
 *
 * @param p - The parsed body.
 * @returns The report, or a short reason it is refused.
 */
function validateClientPayload(p: unknown): ClientPayload | string {
  if (!isObject(p)) return "body must be an object";
  if (typeof p.trigger !== "string" || !p.trigger || p.trigger.length > 64) {
    return "trigger required";
  }
  if (typeof p.version !== "string" || !p.version.trim() || p.version.length > 64) {
    return "version required";
  }
  if (typeof p.os !== "string" || p.os.length > 64) return "os required";
  if (typeof p.archived_at_unix_ms !== "number" && typeof p.archived_at_unix_ms !== "undefined") {
    return "archived_at_unix_ms invalid";
  }
  if (!Array.isArray(p.events)) return "events must be an array";
  if (p.events.length > 500) return "too many events";
  for (const ev of p.events as unknown[]) {
    // Any non-null object passes, arrays included, as before the TypeScript
    // conversion; the fields checked below are read off it as a record.
    if (!isRecordLike(ev)) return "invalid event";
    if (typeof ev.message === "string" && ev.message.length > 8000) {
      return "event message too long";
    }
    if (typeof ev.level === "string" && ev.level.length > 32) return "event level too long";
    if (typeof ev.target === "string" && ev.target.length > 256) return "event target too long";
  }
  return { ...p, trigger: p.trigger, version: p.version, os: p.os, events: p.events };
}

/**
 * Whether text holds something shaped like a credential the client should
 * have redacted.
 *
 * @param text - Serialized report.
 * @returns True when any known token or key pattern matches.
 */
function looksLikeSecretLeak(text: string): boolean {
  return (
    /\bAtna\|[A-Za-z0-9._\-+/=]+/.test(text) ||
    /\bAtnr\|[A-Za-z0-9._\-+/=]+/.test(text) ||
    /\bBearer\s+[A-Za-z0-9._\-+/=]{20,}/.test(text) ||
    /\bgh[pousr]_[A-Za-z0-9_]{20,}/.test(text) ||
    /\bgithub_pat_[A-Za-z0-9_]{20,}/.test(text) ||
    /\bAKIA[0-9A-Z]{16}\b/.test(text) ||
    /-----BEGIN [A-Z ]*PRIVATE KEY-----/.test(text)
  );
}

/**
 * A lowercase token for B2 file info.
 *
 * @param s - Any value.
 * @param fallback - Used when nothing is left after cleaning.
 * @returns At most 48 characters of `[a-z0-9_-]`.
 */
function safeToken(s: unknown, fallback: string = "report"): string {
  const cleaned = String(s || "")
    .toLowerCase()
    .replace(/[^a-z0-9_-]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 48);
  return cleaned || fallback;
}

/**
 * Preserve semver-ish characters (dots, plus) in B2 path segments.
 *
 * @param s - The client's version.
 * @returns At most 64 characters, "unknown" when nothing is left.
 */
function versionPathToken(s: unknown): string {
  const cleaned = String(s || "")
    .trim()
    .replace(/^v(?=\d)/i, "")
    .toLowerCase()
    .replace(/[^a-z0-9._+-]+/g, "-")
    .replace(/^[-.]+|[-.]+$/g, "")
    .slice(0, 64);
  return cleaned || "unknown";
}

/**
 * Percent-encode a B2 object key, keeping `/` as the folder delimiter.
 *
 * @param fileName - Object key.
 * @returns The encoded key.
 */
function b2EncodeFileName(fileName: string): string {
  return String(fileName)
    .split("/")
    .map((segment) => encodeURIComponent(segment))
    .join("/");
}

/**
 * The first characters of a string.
 *
 * @param s - Text.
 * @param n - Most UTF-16 units kept.
 * @returns The text, cut to `n`.
 */
function truncate(s: string, n: number): string {
  return s.length <= n ? s : s.slice(0, n);
}

/**
 * Authorizes the B2 account.
 *
 * @param env - Worker bindings.
 * @returns API and download URLs and the token.
 * @throws {UpstreamError} Without credentials or when B2 refuses.
 */
async function b2Authorize(env: Env): Promise<B2Auth> {
  const endpoint = (env.B2_ENDPOINT || "https://api.backblazeb2.com").replace(/\/$/, "");
  const id = env.B2_KEY_ID;
  const key = env.B2_APPLICATION_KEY;
  if (!id || !key || !env.B2_BUCKET_ID) {
    throw new UpstreamError("b2 credentials check");
  }
  const basic = btoa(`${id}:${key}`);
  const res = await upstreamFetch("b2 authorize", `${endpoint}/b2api/v2/b2_authorize_account`, {
    headers: { Authorization: `Basic ${basic}` },
  });
  if (!res.ok) throw await b2Error("b2 authorize", res);
  const data = asObject(await res.json());
  return {
    apiUrl: String(data.apiUrl),
    authorizationToken: String(data.authorizationToken),
    downloadUrl: String(data.downloadUrl),
  };
}

/** Where and with which token to upload one object. */
type B2Upload = {
  /** Upload URL. */
  uploadUrl: string;
  /** Token for the upload. */
  authorizationToken: string;
};

/** One object as `b2_list_file_names` lists it. */
type B2File = {
  /** Object key. */
  fileName: string;
  /** B2 file ID, for downloads. */
  fileId: string;
  /** Upload time, in milliseconds. */
  uploadTimestamp: number;
  /** "upload" for a stored object (others are hide markers and folders). */
  action: string;
  /** Size in bytes. */
  contentLength: number;
};

/**
 * An upload URL for the bucket.
 *
 * @param auth - Authorization.
 * @param bucketId - Bucket.
 * @returns URL and token.
 * @throws {UpstreamError} When B2 refuses.
 */
async function b2GetUploadUrl(auth: B2Auth, bucketId: string): Promise<B2Upload> {
  const res = await upstreamFetch(
    "b2 get_upload_url",
    `${auth.apiUrl}/b2api/v2/b2_get_upload_url`,
    {
      method: "POST",
      headers: {
        Authorization: auth.authorizationToken,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ bucketId }),
    },
  );
  if (!res.ok) throw await b2Error("b2 get_upload_url", res);
  const data = asObject(await res.json());
  return { uploadUrl: String(data.uploadUrl), authorizationToken: String(data.authorizationToken) };
}

/**
 * Uploads one object.
 *
 * @param upload - From `b2GetUploadUrl`.
 * @param fileName - Object key.
 * @param bodyText - Content (JSON).
 * @param info - B2 file info, sent as `X-Bz-Info-*` headers.
 * @throws {UpstreamError} When B2 refuses.
 */
async function b2Upload(
  upload: B2Upload,
  fileName: string,
  bodyText: string,
  info: Record<string, string> = {},
): Promise<void> {
  const bytes = new TextEncoder().encode(bodyText);
  const hashBuffer = await crypto.subtle.digest("SHA-1", bytes);
  const sha1 = [...new Uint8Array(hashBuffer)].map((b) => b.toString(16).padStart(2, "0")).join("");

  const headers: Record<string, string> = {
    Authorization: upload.authorizationToken,
    // B2: percent-encode UTF-8 but leave `/` as the path delimiter.
    "X-Bz-File-Name": b2EncodeFileName(fileName),
    "Content-Type": "application/json",
    "Content-Length": String(bytes.byteLength),
    "X-Bz-Content-Sha1": sha1,
  };
  for (const [k, v] of Object.entries(info)) {
    headers[`X-Bz-Info-${k}`] = encodeURIComponent(String(v));
  }

  const res = await upstreamFetch("b2 upload", upload.uploadUrl, {
    method: "POST",
    headers,
    body: bytes,
  });
  if (!res.ok) throw await b2Error("b2 upload", res);
}

/**
 * Every object under the diagnostics prefix, page by page.
 *
 * @param auth - Authorization.
 * @param bucketId - Bucket.
 * @returns The objects, and whether the page cap cut the listing short.
 * @throws {UpstreamError} When B2 refuses.
 */
async function b2ListIncoming(
  auth: B2Auth,
  bucketId: string,
): Promise<{ files: B2File[]; listTruncated: boolean }> {
  const files: B2File[] = [];
  let startFileName: string | null = null;
  // Paginate until exhausted. Soft cap avoids runaway Worker CPU on huge buckets;
  // /report surfaces list_truncated when hit so ingest can alert.
  const maxPages = 500;
  for (let i = 0; i < maxPages; i++) {
    const body: { bucketId: string; prefix: string; maxFileCount: number; startFileName?: string } =
      {
        bucketId,
        prefix: DIAGNOSTICS_PREFIX,
        maxFileCount: 1000,
      };
    if (startFileName) body.startFileName = startFileName;
    const res = await upstreamFetch("b2 list", `${auth.apiUrl}/b2api/v2/b2_list_file_names`, {
      method: "POST",
      headers: {
        Authorization: auth.authorizationToken,
        "Content-Type": "application/json",
      },
      body: JSON.stringify(body),
    });
    if (!res.ok) throw await b2Error("b2 list", res);
    const data = asObject(await res.json());
    for (const entry of Array.isArray(data.files) ? data.files : []) {
      const f = asObject(entry);
      // An entry without a string name and ID cannot be downloaded or
      // matched against the cursor, so it is skipped.
      if (typeof f.fileName !== "string" || typeof f.fileId !== "string") continue;
      files.push({
        fileName: f.fileName,
        fileId: f.fileId,
        uploadTimestamp: Number(f.uploadTimestamp),
        action: String(f.action),
        contentLength: Number(f.contentLength),
      });
    }
    if (!data.nextFileName) {
      return { files, listTruncated: false };
    }
    startFileName = String(data.nextFileName);
  }
  return { files, listTruncated: true };
}

/**
 * Downloads one object's text.
 *
 * @param auth - Authorization.
 * @param fileId - B2 file ID.
 * @returns The content.
 * @throws {UpstreamError} When B2 refuses.
 */
async function b2DownloadById(auth: B2Auth, fileId: string): Promise<string> {
  const res = await upstreamFetch(
    "b2 download",
    `${auth.downloadUrl}/b2api/v2/b2_download_file_by_id?fileId=${encodeURIComponent(fileId)}`,
    {
      headers: { Authorization: auth.authorizationToken },
    },
  );
  if (!res.ok) throw await b2Error("b2 download", res);
  return res.text();
}

/**
 * A JSON answer that is never cached.
 *
 * @param obj - Body.
 * @param status - HTTP status.
 * @returns The response.
 */
function json(obj: unknown, status: number = 200): Response {
  return new Response(JSON.stringify(obj), {
    status,
    headers: {
      "Content-Type": "application/json",
      "Cache-Control": "no-store",
    },
  });
}
