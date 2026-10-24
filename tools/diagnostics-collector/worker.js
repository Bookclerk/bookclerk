/**
 * Bookclerk diagnostics Worker (deployed via GitHub Actions + wrangler-action).
 *
 * POST /submit  — clients send redacted JSON; validated + enriched → B2
 * GET  /report  — GitHub Action pulls new objects since `since`, or since the
 *                 stored cursor with `cursor=stored` (secret key); with
 *                 `unfiled=1` it skips reports an issue already lists
 * POST /report/cursor — GitHub Action stores where its last pull ended
 *                 (secret key; see ingest-cursor.js)
 * POST /triage  — GitHub Action sends the sanitized batch; Workers AI returns
 *                 an issue plan, under a daily neuron budget (secret key; see
 *                 triage.js)
 * GET  /health  — liveness
 *
 * With the dashboard configured (see dashboard/routes.js), `/` is the
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
 *   REPORT_API_KEY          — required for /report, /report/cursor, /triage
 *                             and /issues/notify
 *   CLIENT_IP_HASH_SALT     — optional; enables hashed client IP in enrichment
 *   GITHUB_TOKEN            — optional; higher GitHub API rate limits
 *   GITHUB_APP_CLIENT_ID, GITHUB_APP_CLIENT_SECRET, GITHUB_APP_PRIVATE_KEY
 *                           — optional; the automation App, for dashboard
 *                             sign-in and read-only issue sync
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
 * dashboard/live.js).
 *
 * REPORT_API_KEY only ever lives in the Worker and the GitHub Actions
 * secrets; Bookclerk builds carry the collector URL, never this key.
 */

import { parseIssueNotify } from "./dashboard/github.js";
import { reportIdOf } from "./dashboard/report-ids.js";
import {
  dashboardFetch,
  dashboardFiledReportIds,
  dashboardIndexSubmitted,
  dashboardIssueNotify,
  dashboardScheduled,
} from "./dashboard/routes.js";
import { MAX_CURSOR_CHARS, commitCursor, parseCursor, readCursor } from "./ingest-cursor.js";
import { handleTriage, readCapped } from "./triage.js";
import {
  extractVersionFromKey,
  normalizeVersion,
  versionAcceptable,
} from "./version-filter.js";

// The Durable Object class must be exported from the main module.
export { LiveHub } from "./dashboard/live.js";

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
   * @param {string} stage Which call failed, e.g. "b2 list".
   * @param {number} [status] HTTP status of the failed response.
   * @param {string} [code] Upstream error code, kept only when it is a short
   *   lowercase identifier.
   */
  constructor(stage, status, code) {
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
 * @param {string} text The response body (from B2, untrusted).
 * @returns {string | undefined} The body's `code` when the body is a JSON
 *   object of at most `MAX_B2_ERROR_CHARS` UTF-16 units whose `code` is a
 *   lowercase identifier of 1 to 40 characters, else undefined.
 */
export function b2ErrorCode(text) {
  if (typeof text !== "string" || text.length > MAX_B2_ERROR_CHARS) return undefined;
  let body;
  try {
    body = JSON.parse(text);
  } catch {
    return undefined;
  }
  if (!body || typeof body !== "object" || Array.isArray(body)) return undefined;
  const code = body.code;
  return typeof code === "string" && /^[a-z_]{1,40}$/.test(code) ? code : undefined;
}

/**
 * Builds the UpstreamError for a failed B2 response.
 *
 * @param {string} stage Which call failed.
 * @param {Response} res The failed response.
 * @returns {Promise<UpstreamError>} The error to throw.
 */
async function b2Error(stage, res) {
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
 * @param {string} stage Which call this is, e.g. "b2 list".
 * @param {string} url Request URL.
 * @param {RequestInit} [init] Request options.
 * @returns {Promise<Response>} The response, whatever its status.
 */
async function upstreamFetch(stage, url, init) {
  try {
    return await fetch(url, init);
  } catch {
    throw new UpstreamError(stage);
  }
}

/**
 * One-line description of a thrown value for the Worker log.
 *
 * @param {unknown} err What was thrown.
 * @returns {string} Its name and message, or its string form.
 */
function errorText(err) {
  return err instanceof Error ? `${err.name}: ${err.message}` : String(err);
}

export default {
  async fetch(request, env, ctx) {
    const url = new URL(request.url);
    const path = url.pathname.replace(/\/+$/, "") || "/";

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

  async scheduled(event, env, ctx) {
    ctx.waitUntil(dashboardScheduled(env, Date.now()));
  },
};

/**
 * The B2 helpers the dashboard reads stored reports with.
 *
 * @param {Record<string, any>} env Worker bindings.
 * @returns {import("./dashboard/storage.js").B2Access} Helpers bound to this
 *   Worker's B2 credentials.
 */
function b2Access(env) {
  return {
    authorize: () => b2Authorize(env),
    downloadById: (auth, fileId) => b2DownloadById(auth, fileId),
    bucketId: env.B2_BUCKET_ID,
  };
}

/**
 * `POST /report/cursor`: the ingest Action stores the cursor its last pull
 * ended at, so the next run starts there. A cursor earlier than the stored
 * one (a forced full pull) leaves it as is.
 *
 * @param {Request} request Authenticated request with body
 *   `{"since": <ms>, "after": "<object name>"}`.
 * @param {Record<string, any>} env Worker bindings.
 * @returns {Promise<Response>} 200 with the stored cursor, 400 for a
 *   malformed body, or 503 without the DASHBOARD_DB binding.
 */
async function handleCursorCommit(request, env) {
  const text = await readCapped(request, MAX_CURSOR_CHARS);
  const next = text === null ? null : parseCursor(text);
  if (next === null) return json({ error: "expected {\"since\": <ms>, \"after\": \"<name>\"}" }, 400);
  if (!env.DASHBOARD_DB) return json({ error: "no DASHBOARD_DB binding to store the cursor in" }, 503);
  const { advanced, cursor } = await commitCursor(env.DASHBOARD_DB, next);
  return json({ ok: true, advanced, since: cursor.since, after: cursor.after });
}

/**
 * `POST /issues/notify`: the ingest Action names an issue it just filed so
 * the dashboard links it at once. Answers 202 with what happened, 204 when
 * the dashboard is off (so forks without it do not fail the Action), or 400
 * for a malformed body.
 *
 * @param {Request} request Authenticated request.
 * @param {Record<string, any>} env Worker bindings.
 * @returns {Promise<Response>} The answer.
 */
async function handleIssueNotify(request, env) {
  const text = await readCapped(request, 256);
  const number = text === null ? null : parseIssueNotify(text);
  if (number === null) return json({ error: "expected {\"issue\": <number>}" }, 400);
  const result = await dashboardIssueNotify(env, number, Date.now());
  if (result === null) return new Response(null, { status: 204 });
  return json({ ok: true, issue: number, result }, 202);
}

async function handleSubmit(request, env, ctx) {
  const len = Number(request.headers.get("content-length") || "0");
  if (len > MAX_BODY_BYTES) {
    return json({ error: "payload too large" }, 413);
  }

  const raw = await request.arrayBuffer();
  if (raw.byteLength === 0 || raw.byteLength > MAX_BODY_BYTES) {
    return json({ error: "invalid payload size" }, 400);
  }

  let payload;
  try {
    payload = JSON.parse(new TextDecoder().decode(raw));
  } catch {
    return json({ error: "invalid json" }, 400);
  }

  const verr = validateClientPayload(payload);
  if (verr) {
    return json({ error: verr }, 400);
  }

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
    client_ip_hash: await maybeHashIp(request, env),
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
  const upload = await b2GetUploadUrl(auth, env.B2_BUCKET_ID);
  await b2Upload(upload, objectKey, bodyText, {
    report_id: reportId,
    received_at_unix_ms: String(receivedAt),
    trigger: safeToken(payload.trigger, "report"),
    bookclerk_version: truncate(String(payload.version || ""), 64),
  });

  // Best-effort dashboard index row; a failure leaves the report for the
  // owner's reindex and never fails the upload.
  if (env.DASHBOARD_DB && ctx) {
    ctx.waitUntil(dashboardIndexSubmitted(env, enriched, objectKey).catch(() => {}));
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

async function handleReport(request, env, url) {
  if (!requireReportAuth(request, env)) {
    return json({ error: "unauthorized" }, 401);
  }

  let since;
  let afterName;
  let cursorSource;
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
  let baselineVersion = null;
  let baselineSource = "all";
  if (url.searchParams.has("baseline_version")) {
    const raw = url.searchParams.get("baseline_version");
    baselineVersion = normalizeVersion(raw);
    baselineSource = baselineVersion ? "query" : "all";
  } else {
    const resolved = await resolveLatestStableBaseline(env);
    baselineVersion = resolved.version;
    baselineSource = resolved.source;
  }

  const auth = await b2Authorize(env);
  const listed = await b2ListIncoming(auth, env.B2_BUCKET_ID);
  const files = listed.files;
  const newer = files
    .filter((f) => f.action === "upload" && isAfterCursor(f, since, afterName))
    .sort(compareUploadOrder);

  const reports = [];
  let maxTs = since;
  let nextAfter = afterName;
  let skippedVersion = 0;
  let skippedCorrupt = 0;
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
    if (reports.length >= MAX_REPORT_FILES) {
      truncated = true;
      break;
    }
    const downloaded = await b2DownloadById(auth, f.fileId);
    let parsed;
    try {
      parsed = JSON.parse(downloaded);
    } catch {
      // Corrupt objects must not stall the cursor.
      ({ maxTs, nextAfter } = advanceCursor(maxTs, nextAfter, ts, f.fileName));
      skippedCorrupt += 1;
      continue;
    }
    ({ maxTs, nextAfter } = advanceCursor(maxTs, nextAfter, ts, f.fileName));
    reports.push({
      report_id: parsed.report_id || null,
      file_name: f.fileName,
      file_id: f.fileId,
      upload_timestamp_ms: ts,
      content_length: f.contentLength,
      object_version: objectVersion,
      report: parsed,
    });
  }

  const advanced = reports.length > 0 || skippedVersion > 0 || skippedCorrupt > 0 || skippedFiled > 0;
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
    reports,
  });
}

function isAfterCursor(file, since, afterName) {
  const ts = Number(file.uploadTimestamp);
  if (ts > since) return true;
  if (ts === since && file.fileName > afterName) return true;
  return false;
}

function compareUploadOrder(a, b) {
  const dt = Number(a.uploadTimestamp) - Number(b.uploadTimestamp);
  if (dt !== 0) return dt;
  if (a.fileName < b.fileName) return -1;
  if (a.fileName > b.fileName) return 1;
  return 0;
}

function advanceCursor(maxTs, nextAfter, ts, fileName) {
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
 * @returns {Promise<{ version: string | null, source: string }>}
 */
async function resolveLatestStableBaseline(env) {
  const repo = (env.GITHUB_REPOSITORY || "").trim();
  if (!repo || !/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repo)) {
    return { version: null, source: "all" };
  }

  const headers = {
    Accept: "application/vnd.github+json",
    "User-Agent": "bookclerk-diagnostics-worker",
    "X-GitHub-Api-Version": "2022-11-28",
  };
  if (env.GITHUB_TOKEN) {
    headers.Authorization = `Bearer ${env.GITHUB_TOKEN}`;
  }

  const res = await upstreamFetch("github releases/latest", `https://api.github.com/repos/${repo}/releases/latest`, {
    headers,
  });

  // No releases published yet → ingest every version.
  if (res.status === 404) {
    return { version: null, source: "all" };
  }
  if (!res.ok) {
    throw new UpstreamError("github releases/latest", res.status);
  }

  const data = await res.json();
  if (!data || data.prerelease || data.draft) {
    return { version: null, source: "all" };
  }
  const version = normalizeVersion(data.tag_name || "");
  if (!version) {
    return { version: null, source: "all" };
  }
  return { version, source: "github_latest_stable" };
}

function requireReportAuth(request, env) {
  const expected = env.REPORT_API_KEY;
  if (!expected || typeof expected !== "string" || expected.length < 16) {
    return false;
  }
  const header = request.headers.get("authorization") || "";
  const bearer = header.toLowerCase().startsWith("bearer ")
    ? header.slice(7).trim()
    : "";
  const alt = (request.headers.get("x-bookclerk-report-key") || "").trim();
  return timingSafe.equal(bearer, expected) || timingSafe.equal(alt, expected);
}

/** Constant-time string compare for equal-length secrets. */
const timingSafe = {
  equal(a, b) {
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

function validateClientPayload(p) {
  if (!p || typeof p !== "object" || Array.isArray(p)) return "body must be an object";
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
  for (const ev of p.events) {
    if (!ev || typeof ev !== "object") return "invalid event";
    if (typeof ev.message === "string" && ev.message.length > 8000) {
      return "event message too long";
    }
    if (typeof ev.level === "string" && ev.level.length > 32) return "event level too long";
    if (typeof ev.target === "string" && ev.target.length > 256) return "event target too long";
  }
  return null;
}

function looksLikeSecretLeak(text) {
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

async function maybeHashIp(request, env) {
  const salt = env.CLIENT_IP_HASH_SALT;
  if (!salt) return null;
  const ip = request.headers.get("cf-connecting-ip") || "";
  if (!ip) return null;
  const data = new TextEncoder().encode(`${salt}:${ip}`);
  const digest = await crypto.subtle.digest("SHA-256", data);
  return [...new Uint8Array(digest)]
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("")
    .slice(0, 32);
}

function safeToken(s, fallback = "report") {
  const cleaned = String(s || "")
    .toLowerCase()
    .replace(/[^a-z0-9_-]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 48);
  return cleaned || fallback;
}

/** Preserve semver-ish characters (dots, plus) in B2 path segments. */
function versionPathToken(s) {
  const cleaned = String(s || "")
    .trim()
    .replace(/^v(?=\d)/i, "")
    .toLowerCase()
    .replace(/[^a-z0-9._+-]+/g, "-")
    .replace(/^[-.]+|[-.]+$/g, "")
    .slice(0, 64);
  return cleaned || "unknown";
}

/** Percent-encode a B2 object key, keeping `/` as the folder delimiter. */
function b2EncodeFileName(fileName) {
  return String(fileName)
    .split("/")
    .map((segment) => encodeURIComponent(segment))
    .join("/");
}

function truncate(s, n) {
  return s.length <= n ? s : s.slice(0, n);
}

async function b2Authorize(env) {
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
  return res.json();
}

async function b2GetUploadUrl(auth, bucketId) {
  const res = await upstreamFetch("b2 get_upload_url", `${auth.apiUrl}/b2api/v2/b2_get_upload_url`, {
    method: "POST",
    headers: {
      Authorization: auth.authorizationToken,
      "Content-Type": "application/json",
    },
    body: JSON.stringify({ bucketId }),
  });
  if (!res.ok) throw await b2Error("b2 get_upload_url", res);
  return res.json();
}

async function b2Upload(upload, fileName, bodyText, info = {}) {
  const bytes = new TextEncoder().encode(bodyText);
  const hashBuffer = await crypto.subtle.digest("SHA-1", bytes);
  const sha1 = [...new Uint8Array(hashBuffer)].map((b) => b.toString(16).padStart(2, "0")).join("");

  const headers = {
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

async function b2ListIncoming(auth, bucketId) {
  const files = [];
  let startFileName = null;
  // Paginate until exhausted. Soft cap avoids runaway Worker CPU on huge buckets;
  // /report surfaces list_truncated when hit so ingest can alert.
  const maxPages = 500;
  for (let i = 0; i < maxPages; i++) {
    const body = {
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
    const data = await res.json();
    files.push(...(data.files || []));
    if (!data.nextFileName) {
      return { files, listTruncated: false };
    }
    startFileName = data.nextFileName;
  }
  return { files, listTruncated: true };
}

async function b2DownloadById(auth, fileId) {
  const res = await upstreamFetch("b2 download", `${auth.downloadUrl}/b2api/v2/b2_download_file_by_id?fileId=${encodeURIComponent(fileId)}`, {
    headers: { Authorization: auth.authorizationToken },
  });
  if (!res.ok) throw await b2Error("b2 download", res);
  return res.text();
}

function json(obj, status = 200) {
  return new Response(JSON.stringify(obj), {
    status,
    headers: {
      "Content-Type": "application/json",
      "Cache-Control": "no-store",
    },
  });
}
