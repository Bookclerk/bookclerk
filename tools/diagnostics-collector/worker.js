/**
 * Bookclerk diagnostics Worker (deployed via GitHub Actions + wrangler-action).
 *
 * POST /submit  — clients send redacted JSON; validated + enriched → B2
 * GET  /report  — GitHub Action pulls new objects since `since` (secret key)
 * GET  /health  — liveness
 *
 * Object layout: diagnostics/<version>/<report_id>.json
 *
 * /report selects objects whose path version is the latest *stable* GitHub
 * release, newer (e.g. prerelease of a future version), or a packaging
 * derivative of that release. With no GitHub releases yet, all versions match.
 *
 * Secrets (set by deploy workflow from GitHub secrets):
 *   B2_KEY_ID, B2_APPLICATION_KEY, B2_BUCKET_ID
 *   REPORT_API_KEY          — required for /report
 *   CLIENT_IP_HASH_SALT     — optional; enables hashed client IP in enrichment
 *   GITHUB_TOKEN            — optional; higher GitHub API rate limits
 *
 * Vars:
 *   GITHUB_REPOSITORY       — owner/repo for releases/latest (e.g. fritz-fritz/bookclerk)
 */

import {
  extractVersionFromKey,
  normalizeVersion,
  versionAcceptable,
} from "./version-filter.js";

const MAX_BODY_BYTES = 256 * 1024;
const DIAGNOSTICS_PREFIX = "diagnostics/";
const MAX_REPORT_FILES = 100;

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    const path = url.pathname.replace(/\/+$/, "") || "/";

    try {
      if (request.method === "GET" && (path === "/" || path === "/health")) {
        const origin = new URL(request.url).origin;
        return json({
          ok: true,
          service: "bookclerk-diagnostics",
          url: origin,
        });
      }

      if (request.method === "POST" && path === "/submit") {
        return handleSubmit(request, env);
      }

      if (request.method === "GET" && path === "/report") {
        return handleReport(request, env, url);
      }

      return json({ error: "not found" }, 404);
    } catch (err) {
      return json({ error: "internal error" }, 500);
    }
  },
};

async function handleSubmit(request, env) {
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

  const sinceRaw = url.searchParams.get("since");
  const since = sinceRaw != null && sinceRaw !== "" ? Number(sinceRaw) : 0;
  if (!Number.isFinite(since) || since < 0) {
    return json({ error: "invalid since" }, 400);
  }
  // Secondary cursor: exclusive fileName within the same uploadTimestamp as `since`.
  const afterName = url.searchParams.get("after") || "";

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

  const advanced = reports.length > 0 || skippedVersion > 0 || skippedCorrupt > 0;
  return json({
    ok: true,
    since,
    after: afterName,
    next_since: advanced ? maxTs : since,
    next_after: advanced ? nextAfter : afterName,
    count: reports.length,
    truncated: truncated || listed.listTruncated,
    list_truncated: listed.listTruncated,
    baseline_version: baselineVersion || null,
    baseline_source: baselineSource,
    skipped_version: skippedVersion,
    skipped_corrupt: skippedCorrupt,
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

  const res = await fetch(`https://api.github.com/repos/${repo}/releases/latest`, {
    headers,
  });

  // No releases published yet → ingest every version.
  if (res.status === 404) {
    return { version: null, source: "all" };
  }
  if (!res.ok) {
    throw new Error(`github releases/latest failed: HTTP ${res.status}`);
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
    throw new Error("missing B2 credentials");
  }
  const basic = btoa(`${id}:${key}`);
  const res = await fetch(`${endpoint}/b2api/v2/b2_authorize_account`, {
    headers: { Authorization: `Basic ${basic}` },
  });
  if (!res.ok) throw new Error("b2 authorize failed");
  return res.json();
}

async function b2GetUploadUrl(auth, bucketId) {
  const res = await fetch(`${auth.apiUrl}/b2api/v2/b2_get_upload_url`, {
    method: "POST",
    headers: {
      Authorization: auth.authorizationToken,
      "Content-Type": "application/json",
    },
    body: JSON.stringify({ bucketId }),
  });
  if (!res.ok) throw new Error("b2 get_upload_url failed");
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

  const res = await fetch(upload.uploadUrl, {
    method: "POST",
    headers,
    body: bytes,
  });
  if (!res.ok) throw new Error("b2 upload failed");
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
    const res = await fetch(`${auth.apiUrl}/b2api/v2/b2_list_file_names`, {
      method: "POST",
      headers: {
        Authorization: auth.authorizationToken,
        "Content-Type": "application/json",
      },
      body: JSON.stringify(body),
    });
    if (!res.ok) throw new Error("b2 list failed");
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
  const res = await fetch(`${auth.downloadUrl}/b2api/v2/b2_download_file_by_id?fileId=${encodeURIComponent(fileId)}`, {
    headers: { Authorization: auth.authorizationToken },
  });
  if (!res.ok) throw new Error("b2 download failed");
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
