/**
 * GitHub calls for the dashboard: sign-in through the automation GitHub App's
 * user authorization, the organization membership check, and reads of
 * diagnostics issues and the pull requests that reference them.
 *
 * The App can write to the repository, so the Worker narrows what it holds:
 * - every installation token it mints is down-scoped in the request to
 *   `INSTALLATION_PERMISSIONS` on the one configured repository, so no token
 *   the Worker holds can write to pull requests or code; the one exception
 *   is the owner's "Update GitHub now" button, which mints a separate,
 *   uncached token with `SUMMARY_PERMISSIONS` (`issues: write`, nothing
 *   else), edits one issue's summary comment with it and drops it;
 * - the user token from sign-in is used for one `GET /user` call and then
 *   revoked, never stored, logged or sent to the browser;
 * - the private key and client secret are Worker secrets read only here.
 */

import { MARKER } from "./summary.ts";
import { reportIdOf } from "./report-ids.ts";
import type { Env } from "../env.ts";
import { isObject, isRecordLike, isSafeInteger, prop } from "../json.ts";

/** GitHub REST and GraphQL host. */
export const API = "https://api.github.com";

/** GitHub web host (OAuth endpoints). */
export const WEB = "https://github.com";

/**
 * Permissions every installation token is narrowed to. All read-only.
 * `contents` is for the release list and the compare call that find which
 * release tag contains a fix.
 */
export const INSTALLATION_PERMISSIONS = Object.freeze({
  metadata: "read",
  issues: "read",
  pull_requests: "read",
  members: "read",
  contents: "read",
});

/**
 * Permissions of the token that edits an issue's summary comment
 * (`writeSummaryComment`): the one `issues: write` the comment edit needs,
 * which the ingest workflow's own token also has. It is never cached.
 */
export const SUMMARY_PERMISSIONS = Object.freeze({
  issues: "write",
});

/**
 * Comments per page when looking for the summary comment (GitHub's most),
 * and the most comments `decodeComments` reads from one page.
 */
export const COMMENT_PAGE = 100;

/**
 * Most comment pages read to find the summary comment. An issue with more
 * comments than that is left to the ingest run, which pages through all of
 * them, rather than risk a second summary comment.
 */
const MAX_COMMENT_PAGES = 20;

/** Time allowed for one GitHub call, in milliseconds. */
const TIMEOUT_MS = 10_000;

/** Installation tokens are refreshed when fewer than this many ms remain. */
const TOKEN_REFRESH_MARGIN_MS = 5 * 60 * 1000;

/**
 * Dashboard GitHub settings, or null when the dashboard is not configured
 * (a fork without the App secrets): then every dashboard route is off.
 */
export type GitHubConfig = {
  /** The GitHub App's OAuth client ID. */
  clientId: string;
  /** The GitHub App's OAuth client secret. */
  clientSecret: string;
  /** The GitHub App's private key, PEM. */
  privateKey: string;
  /** Owner of the repository issues are filed in. */
  owner: string;
  /** Name of the repository issues are filed in. */
  repo: string;
};

/**
 * Reads the dashboard's GitHub settings from the Worker environment.
 *
 * @param env - Worker bindings.
 * @returns Settings, or null when any is missing or
 *   `GITHUB_REPOSITORY` is not `owner/name`.
 */
export function githubConfig(env: Env): GitHubConfig | null {
  const clientId =
    typeof env.GITHUB_APP_CLIENT_ID === "string" ? env.GITHUB_APP_CLIENT_ID.trim() : "";
  // Trimmed: a secret pasted with a trailing newline makes GitHub answer
  // incorrect_client_credentials.
  const clientSecret =
    typeof env.GITHUB_APP_CLIENT_SECRET === "string" ? env.GITHUB_APP_CLIENT_SECRET.trim() : "";
  const privateKey =
    typeof env.GITHUB_APP_PRIVATE_KEY === "string" ? env.GITHUB_APP_PRIVATE_KEY : "";
  const m = /^([A-Za-z0-9_.-]+)\/([A-Za-z0-9_.-]+)$/.exec(
    String(env.GITHUB_REPOSITORY || "").trim(),
  );
  if (!clientId || !clientSecret || !privateKey || !m) return null;
  if (!/^[A-Za-z0-9._-]{1,64}$/.test(clientId)) return null;
  const [, owner = "", repo = ""] = m;
  return { clientId, clientSecret, privateKey, owner, repo };
}

/**
 * Headers for every GitHub API call.
 *
 * @param token - Bearer token, or null / "" for an anonymous call.
 * @returns The headers.
 */
function apiHeaders(token: string | null): Record<string, string> {
  const h: Record<string, string> = {
    Accept: "application/vnd.github+json",
    "User-Agent": "bookclerk-diagnostics-worker",
    "X-GitHub-Api-Version": "2022-11-28",
  };
  if (token) h.Authorization = `Bearer ${token}`;
  return h;
}

/**
 * Fetch with the module's timeout.
 *
 * @param url - Absolute URL on api.github.com or github.com.
 * @param init - Request options.
 * @returns The response.
 */
function gh(url: string, init: RequestInit): Promise<Response> {
  return fetch(url, { ...init, signal: AbortSignal.timeout(TIMEOUT_MS) });
}

// ---------------------------------------------------------------------------
// App authentication

/**
 * DER length bytes.
 *
 * @param n - Content length.
 * @returns Encoded length.
 */
function derLength(n: number): number[] {
  if (n < 0x80) return [n];
  const bytes: number[] = [];
  for (let v = n; v > 0; v = Math.floor(v / 256)) bytes.unshift(v & 0xff);
  return [0x80 | bytes.length, ...bytes];
}

/**
 * DER element.
 *
 * @param tag - Tag byte.
 * @param content - Content bytes.
 * @returns Encoded element.
 */
function der(tag: number, content: Uint8Array | number[]): Uint8Array {
  return Uint8Array.from([tag, ...derLength(content.length), ...content]);
}

/** rsaEncryption AlgorithmIdentifier (OID 1.2.840.113549.1.1.1, NULL). */
const RSA_ALGORITHM = Uint8Array.from([
  0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00,
]);

/**
 * Converts the App's private key to PKCS#8 DER for WebCrypto. GitHub hands
 * out PKCS#1 (`BEGIN RSA PRIVATE KEY`); PKCS#8 (`BEGIN PRIVATE KEY`) is also
 * accepted, so the same Actions secret works either way.
 *
 * @param pem - PEM text.
 * @returns PKCS#8 DER.
 * @throws {Error} When the PEM holds neither form.
 */
export function pkcs8FromPem(pem: string): Uint8Array {
  const m = /-----BEGIN (RSA )?PRIVATE KEY-----([\s\S]+?)-----END (RSA )?PRIVATE KEY-----/.exec(
    pem,
  );
  if (!m || Boolean(m[1]) !== Boolean(m[3])) throw new Error("unrecognized private key");
  const bin = atob((m[2] ?? "").replace(/\s+/g, ""));
  const body = Uint8Array.from(bin, (c) => c.charCodeAt(0));
  if (!m[1]) return body;
  const version = Uint8Array.from([0x02, 0x01, 0x00]);
  const octets = der(0x04, body);
  return der(0x30, [...version, ...RSA_ALGORITHM, ...octets]);
}

/**
 * A short-lived JWT that authenticates as the App (RS256, 9 minutes).
 *
 * @param cfg - Settings.
 * @param now - Unix time in milliseconds.
 * @returns The JWT.
 */
export async function appJwt(cfg: GitHubConfig, now: number): Promise<string> {
  const key = await crypto.subtle.importKey(
    "pkcs8",
    pkcs8FromPem(cfg.privateKey),
    { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const enc = (obj: unknown): string => b64url(new TextEncoder().encode(JSON.stringify(obj)));
  const iat = Math.floor(now / 1000) - 60;
  const head = `${enc({ alg: "RS256", typ: "JWT" })}.${enc({ iat, exp: iat + 600, iss: cfg.clientId })}`;
  const sig = await crypto.subtle.sign("RSASSA-PKCS1-v1_5", key, new TextEncoder().encode(head));
  return `${head}.${b64url(new Uint8Array(sig))}`;
}

/**
 * Base64url without padding.
 *
 * @param bytes - Input.
 * @returns Encoded text.
 */
function b64url(bytes: Uint8Array): string {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/** Cached down-scoped installation token for this isolate. */
let cachedToken: { key: string; token: string; expiresAt: number } | null = null;

/**
 * A read-only installation token for the configured repository, cached while
 * it has more than five minutes left.
 *
 * @param cfg - Settings.
 * @param now - Unix time in milliseconds.
 * @returns The token.
 * @throws {Error} When the App is not installed on the repository or GitHub
 *   refuses the narrowed permissions (for example Members is not granted).
 */
export async function installationToken(cfg: GitHubConfig, now: number): Promise<string> {
  const key = `${cfg.clientId}/${cfg.owner}/${cfg.repo}`;
  if (
    cachedToken &&
    cachedToken.key === key &&
    cachedToken.expiresAt - TOKEN_REFRESH_MARGIN_MS > now
  ) {
    return cachedToken.token;
  }
  const got = await mintToken(cfg, now, INSTALLATION_PERMISSIONS);
  cachedToken = { key, ...got };
  return got.token;
}

/**
 * Mints an installation token narrowed to `permissions` on the configured
 * repository.
 *
 * @param cfg - Settings.
 * @param now - Unix time in milliseconds.
 * @param permissions - Permissions asked
 *   for; GitHub refuses any the installation does not grant.
 * @returns The token and when
 *   it expires.
 * @throws {Error} When the App is not installed on the repository or GitHub
 *   refuses the permissions.
 */
async function mintToken(
  cfg: GitHubConfig,
  now: number,
  permissions: Readonly<Record<string, string>>,
): Promise<{ token: string; expiresAt: number }> {
  const jwt = await appJwt(cfg, now);
  const inst = await gh(`${API}/repos/${cfg.owner}/${cfg.repo}/installation`, {
    headers: apiHeaders(jwt),
  });
  if (!inst.ok) throw new Error(`app installation lookup failed: HTTP ${inst.status}`);
  const id = decodeInstallationId(await inst.json().catch(() => null));
  if (id === null) throw new Error("app installation id missing");
  const res = await gh(`${API}/app/installations/${id}/access_tokens`, {
    method: "POST",
    headers: { ...apiHeaders(jwt), "Content-Type": "application/json" },
    body: JSON.stringify({ repositories: [cfg.repo], permissions }),
  });
  if (!res.ok) throw new Error(`installation token failed: HTTP ${res.status}`);
  const got = decodeInstallationToken(await res.json().catch(() => null));
  if (!got) throw new Error("installation token response malformed");
  return got;
}

/**
 * One outcome of `writeSummaryComment`.
 *
 * `updated` when the existing summary comment was edited, `created` when
 * one was posted, `refused` when GitHub will not take the comment for
 * good: the issue is locked (`reason` "locked") or gone (deleted or
 * transferred, `reason` "gone").
 */
export type SummaryWrite =
  { outcome: "updated" | "created" } | { outcome: "refused"; reason: "locked" | "gone" };

/**
 * Writes an issue's summary comment, for the owner's "Update GitHub now"
 * button, as the ingest Action's `update-summaries.sh` does with the
 * workflow's token: the first comment a bot account posted that starts
 * with the summary marker is edited in place (editing notifies nobody), else
 * the comment is posted; both write as a bot, so either finds the other's
 * comment. The token is minted for this call alone with
 * `SUMMARY_PERMISSIONS` and not kept.
 *
 * @param cfg - Settings.
 * @param now - Unix time in milliseconds.
 * @param issue - Issue number.
 * @param body - The comment, starting with the marker.
 * @returns What happened.
 * @throws {Error} When the App lacks `issues: write` on the repository, the
 *   comments cannot be listed (any failure, like the run, which skips the
 *   issue and retries), the issue has more than `MAX_COMMENT_PAGES` pages
 *   of comments, or GitHub answers the write with anything but success or
 *   a refusal for good (a rate limit, an outage). Messages carry a stage
 *   and an HTTP status, never the token.
 */
export async function writeSummaryComment(
  cfg: GitHubConfig,
  now: number,
  issue: number,
  body: string,
): Promise<SummaryWrite> {
  const { token } = await mintToken(cfg, now, SUMMARY_PERMISSIONS);
  const base = `${API}/repos/${cfg.owner}/${cfg.repo}/issues`;
  let found = null;
  let last = false;
  for (let page = 1; page <= MAX_COMMENT_PAGES && found === null && !last; page++) {
    const res = await gh(`${base}/${issue}/comments?per_page=${COMMENT_PAGE}&page=${page}`, {
      headers: apiHeaders(token),
    });
    if (!res.ok) throw new Error(`comment listing failed: HTTP ${res.status}`);
    const comments = decodeComments(await res.json().catch(() => null));
    if (comments === null) throw new Error("comment listing malformed");
    found = comments.summary;
    last = comments.count < COMMENT_PAGE;
  }
  if (found === null && !last)
    throw new Error(`comment listing longer than ${MAX_COMMENT_PAGES} pages`);
  const write = (method: "PATCH" | "POST", url: string): Promise<Response> =>
    gh(url, {
      method,
      headers: { ...apiHeaders(token), "Content-Type": "application/json" },
      body: JSON.stringify({ body }),
    });
  if (found !== null) {
    const res = await write("PATCH", `${base}/comments/${found}`);
    if (res.ok) return { outcome: "updated" };
    // 404 or 410 here means the comment went away since the listing (someone
    // deleted it), not the issue: a new one is posted below.
    if (res.status !== 404 && res.status !== 410)
      return refusal("edit", res, await res.text().catch(() => ""));
  }
  const res = await write("POST", `${base}/${issue}/comments`);
  if (res.ok) return { outcome: "created" };
  if (res.status === 404 || res.status === 410) return { outcome: "refused", reason: "gone" };
  return refusal("post", res, await res.text().catch(() => ""));
}

/**
 * Classifies a failed comment write: a locked issue is a refusal for good,
 * anything else is thrown for a retry.
 *
 * @param verb - Which write.
 * @param res - The failed response.
 * @param text - Its body.
 * @returns The refusal.
 * @throws {Error} For every other failure.
 */
function refusal(verb: "edit" | "post", res: Response, text: string): SummaryWrite {
  if (res.status === 403) {
    let data: unknown = null;
    try {
      data = JSON.parse(text);
    } catch {
      data = null;
    }
    if (isLockedMessage(data)) return { outcome: "refused", reason: "locked" };
  }
  throw new Error(`comment ${verb} failed: HTTP ${res.status}`);
}

/** Forgets the cached installation token (tests). */
export function clearTokenCache(): void {
  cachedToken = null;
}

// ---------------------------------------------------------------------------
// Response decoders: sans-IO, so the fuzzer drives the same code as the
// requests above. None of them throws.

/** A GitHub login: letters, digits and hyphens, at most 39. */
const LOGIN = /^[A-Za-z0-9-]{1,39}$/;

/**
 * Reads `GET /repos/{owner}/{repo}/installation`.
 *
 * @param data - Parsed body.
 * @returns The installation id when it is a positive safe
 *   integer.
 */
export function decodeInstallationId(data: unknown): number | null {
  const id = prop(data, "id");
  return isSafeInteger(id) && id > 0 ? id : null;
}

/**
 * Reads `POST /app/installations/{id}/access_tokens`.
 *
 * @param data - Parsed body.
 * @returns A non-empty token
 *   and its expiry in milliseconds, or null when either is missing.
 */
export function decodeInstallationToken(
  data: unknown,
): { token: string; expiresAt: number } | null {
  if (
    !isRecordLike(data) ||
    typeof data.token !== "string" ||
    data.token === "" ||
    typeof data.expires_at !== "string"
  )
    return null;
  const expiresAt = Date.parse(data.expires_at);
  return Number.isFinite(expiresAt) ? { token: data.token, expiresAt } : null;
}

/**
 * Reads one page of `GET /repos/{owner}/{repo}/issues/{n}/comments` for the
 * summary comment: the first comment on the page that a bot account posted
 * (`user.type` "Bot": the ingest workflow's token and the App both write as
 * one) and whose body starts with the summary marker.
 *
 * @param data - Parsed body.
 * @returns The comment's
 *   id when one is on the page, and how many comments the page holds (at
 *   most `COMMENT_PAGE`; a shorter page is the last); null when the body is
 *   not a list.
 */
export function decodeComments(data: unknown): { summary: number | null; count: number } | null {
  if (!Array.isArray(data)) return null;
  const page = data.slice(0, COMMENT_PAGE);
  for (const c of page) {
    if (!isRecordLike(c) || !isSafeInteger(c.id) || c.id <= 0) continue;
    if (!isRecordLike(c.user) || c.user.type !== "Bot") continue;
    if (typeof c.body === "string" && c.body.startsWith(MARKER))
      return { summary: c.id, count: page.length };
  }
  return { summary: null, count: page.length };
}

/** What GitHub's 403 says when an issue is locked (the one wording treated as final). */
const LOCKED_MESSAGE = /\bissue is locked\b/i;

/**
 * Reads the body of a 403 to a comment write: whether GitHub refused
 * because the issue is locked ("Unable to create comment because issue is
 * locked."), which no retry changes. A rate limit answers 403 too, with
 * another message, and is retried.
 *
 * @param data - Parsed body.
 * @returns True only for an object whose `message` is a string
 *   saying the issue is locked.
 */
export function isLockedMessage(data: unknown): boolean {
  return (
    isRecordLike(data) &&
    typeof data.message === "string" &&
    data.message.length <= 1000 &&
    LOCKED_MESSAGE.test(data.message)
  );
}

/**
 * Reads the sign-in code exchange answer.
 *
 * @param data - Parsed body.
 * @returns A non-empty `access_token`, or null.
 */
export function decodeAccessToken(data: unknown): string | null {
  return isRecordLike(data) && typeof data.access_token === "string" && data.access_token !== ""
    ? data.access_token
    : null;
}

/**
 * Reads GitHub's OAuth error code (`bad_verification_code`,
 * `incorrect_client_credentials`, `redirect_uri_mismatch`, ...) from a code
 * exchange answer, for the sign-in error page and the Worker log. The
 * free-text `error_description` is never read.
 *
 * @param data - Parsed body.
 * @returns The `error` field when it is 1 to 40 lowercase
 *   letters and underscores, else null.
 */
export function oauthErrorCode(data: unknown): string | null {
  return isRecordLike(data) && typeof data.error === "string" && /^[a-z_]{1,40}$/.test(data.error)
    ? data.error
    : null;
}

/**
 * Reads `GET /user`.
 *
 * @param data - Parsed body.
 * @returns A positive safe integer id
 *   and a GitHub-shaped login, or null.
 */
export function decodeUser(data: unknown): { id: number; login: string } | null {
  if (!isRecordLike(data) || !isSafeInteger(data.id) || data.id <= 0) return null;
  if (typeof data.login !== "string" || !LOGIN.test(data.login)) return null;
  return { id: data.id, login: data.login };
}

/**
 * An issue as the sync stores it.
 */
export type SyncedIssue = {
  /** Issue number. */
  number: number;
  /** Issue title. */
  title: string;
  /** Issue state. */
  state: "open" | "closed";
  /** Issue body, "" when empty. */
  body: string;
  /** Creation time, ISO 8601. */
  created_at: string;
  /** Last update time, ISO 8601. */
  updated_at: string;
  /** Close time, ISO 8601, or null while open. */
  closed_at: string | null;
  /** Version of the issue's `fixed-in` label, or null without one. */
  fixed_in_label: string | null;
};

/**
 * Reads one page of `GET /repos/{owner}/{repo}/issues`.
 *
 * @param data - Parsed body.
 * @returns Issues
 *   with a positive safe integer number (pull requests left out; text
 *   fields that are not strings become ""), whether the page held 100
 *   entries (counted before filtering, so paging continues past a page of
 *   pull requests), and the newest valid `updated_at` on the page as
 *   ISO-8601, or "" when none parses.
 */
export function decodeIssuePage(data: unknown): {
  issues: SyncedIssue[];
  full: boolean;
  newest: string;
} {
  const list = Array.isArray(data) ? data : [];
  const str = (v: unknown): string => (typeof v === "string" ? v : "");
  let newestMs = -Infinity;
  const issues: SyncedIssue[] = [];
  for (const i of list) {
    if (!isRecordLike(i)) continue;
    const t = Date.parse(str(i.updated_at));
    if (Number.isFinite(t) && t > newestMs) newestMs = t;
    if (i.pull_request !== undefined && i.pull_request !== null) continue;
    if (!isSafeInteger(i.number) || i.number <= 0) continue;
    issues.push({
      number: i.number,
      title: str(i.title),
      state: i.state === "closed" ? "closed" : "open",
      body: str(i.body),
      created_at: str(i.created_at),
      updated_at: str(i.updated_at),
      closed_at: typeof i.closed_at === "string" ? i.closed_at : null,
      fixed_in_label: fixedInLabel(i.labels),
    });
  }
  return {
    issues,
    full: list.length >= 100,
    newest: Number.isFinite(newestMs) ? new Date(newestMs).toISOString() : "",
  };
}

/**
 * Reads `GET /repos/{owner}/{repo}/issues/{number}`.
 *
 * @param data - Parsed body.
 * @returns The issue as
 *   `decodeIssuePage` reads it and whether its labels include
 *   `diagnostics`, or null for a pull request or a malformed answer.
 */
export function decodeIssue(data: unknown): { issue: SyncedIssue; labeled: boolean } | null {
  const { issues } = decodeIssuePage([data]);
  const [issue] = issues;
  if (issues.length !== 1 || issue === undefined) return null;
  const rawLabels = prop(data, "labels");
  const labels: unknown[] = Array.isArray(rawLabels) ? rawLabels : [];
  const labeled = labels.some(
    (l) => l === DIAGNOSTICS_LABEL || prop(l, "name") === DIAGNOSTICS_LABEL,
  );
  return { issue, labeled };
}

/**
 * Reads the body of `POST /issues/notify`: `{"issue": <number>}`.
 *
 * @param text - Request body (from the ingest Action; authenticated,
 *   still parsed defensively).
 * @returns An issue number from 1 to 10^9, or null.
 */
export function parseIssueNotify(text: string): number | null {
  if (typeof text !== "string" || text.length > 256) return null;
  let data: unknown;
  try {
    data = JSON.parse(text);
  } catch {
    return null;
  }
  if (!isObject(data)) return null;
  const n = data.issue;
  return isSafeInteger(n) && n > 0 && n <= 1e9 ? n : null;
}

/** A `issueSummaries` fingerprint: lowercase hex SHA-256. */
const FINGERPRINT = /^[0-9a-f]{64}$/;

/**
 * Reads the ingest Action's `POST /issues/summary` body, which says which
 * issue it wrote a summary comment on and which `GET /issues/summaries`
 * fingerprint it rendered.
 *
 * @param text - Request body, at most 512 characters.
 * @returns The pair, or null
 *   when the body is not a JSON object with a positive integer `issue` of at
 *   most 1e9 and a 64-character lowercase hex `fingerprint`.
 */
export function parseSummaryDone(text: unknown): { issue: number; fingerprint: string } | null {
  if (typeof text !== "string" || text.length > 512) return null;
  let data: unknown;
  try {
    data = JSON.parse(text);
  } catch {
    return null;
  }
  if (!isObject(data)) return null;
  const n = data.issue;
  const fp = data.fingerprint;
  if (!isSafeInteger(n) || n < 1 || n > 1e9) return null;
  if (typeof fp !== "string" || !FINGERPRINT.test(fp)) return null;
  return { issue: n, fingerprint: fp };
}

/** Longest `POST /issues/attach` body, in characters. */
export const MAX_ATTACH_BODY_CHARS = 64 * 1024;

/** Most issues one `POST /issues/attach` body may name. */
export const MAX_ATTACH_ISSUES = 20;

/** Most report IDs one `POST /issues/attach` body may carry, over all issues. */
export const MAX_ATTACH_IDS = 500;

/** One issue and the reports triage attaches to it. */
export type AttachLink = {
  /** Issue number. */
  issue: number;
  /** Distinct lowercase report UUIDs, in listed order. */
  reportIds: string[];
};

/**
 * Reads the ingest Action's `POST /issues/attach` body, the model's
 * validated `attach` decisions:
 * `{"links": [{"issue": <number>, "report_ids": ["<uuid>", ...]}, ...]}`.
 * Strict, since a malformed body means the Action and the Worker disagree
 * and the Action then tracks those reports in its holding issue instead.
 *
 * @param text - Request body, at most `MAX_ATTACH_BODY_CHARS` characters.
 * @returns One link per issue, at most `MAX_ATTACH_ISSUES`, each issue
 *   once, each report once (under the first issue naming it), at most
 *   `MAX_ATTACH_IDS` reports in all; or null when the body is not that
 *   shape, an issue is not a positive integer of at most 1e9, a report ID
 *   is neither a UUID nor a collector object name (`reportIdOf`), or a
 *   link lists no report.
 */
export function parseAttachRequest(text: unknown): AttachLink[] | null {
  if (typeof text !== "string" || text.length > MAX_ATTACH_BODY_CHARS) return null;
  let data: unknown;
  try {
    data = JSON.parse(text);
  } catch {
    return null;
  }
  if (!isObject(data) || !Array.isArray(data.links) || data.links.length > MAX_ATTACH_ISSUES)
    return null;
  const out: AttachLink[] = [];
  const issues = new Set<number>();
  const seen = new Set<string>();
  for (const entry of data.links) {
    if (!isObject(entry)) return null;
    const n = entry.issue;
    if (!isSafeInteger(n) || n < 1 || n > 1e9 || issues.has(n)) return null;
    if (!Array.isArray(entry.report_ids) || entry.report_ids.length === 0) return null;
    issues.add(n);
    const reportIds: string[] = [];
    for (const raw of entry.report_ids) {
      const id = typeof raw === "string" && raw.length <= 160 ? reportIdOf(raw) : null;
      if (id === null) return null;
      if (seen.has(id)) continue;
      seen.add(id);
      if (seen.size > MAX_ATTACH_IDS) return null;
      reportIds.push(id);
    }
    if (reportIds.length > 0) out.push({ issue: n, reportIds });
  }
  return out;
}

// ---------------------------------------------------------------------------
// Sign-in

/**
 * The GitHub authorize URL that starts sign-in.
 *
 * @param cfg - Settings.
 * @param p - Callback URL, CSRF state and PKCE S256 challenge.
 * @returns URL on github.com.
 */
export function authorizeUrl(
  cfg: GitHubConfig,
  p: { redirectUri: string; state: string; challenge: string },
): string {
  const q = new URLSearchParams({
    client_id: cfg.clientId,
    redirect_uri: p.redirectUri,
    state: p.state,
    code_challenge: p.challenge,
    code_challenge_method: "S256",
    allow_signup: "false",
  });
  return `${WEB}/login/oauth/authorize?${q}`;
}

/**
 * Trades a sign-in code for a user token.
 *
 * @param cfg - Settings.
 * @param p - Code,
 *   callback URL and PKCE verifier.
 * @returns The user token, or
 *   why GitHub refused the code: its OAuth error code, or the HTTP status
 *   when it gave none. Neither carries credentials.
 */
export async function exchangeCode(
  cfg: GitHubConfig,
  p: { code: string; redirectUri: string; verifier: string },
): Promise<{ token: string } | { error: string }> {
  const res = await gh(`${WEB}/login/oauth/access_token`, {
    method: "POST",
    headers: {
      Accept: "application/json",
      "Content-Type": "application/json",
      "User-Agent": "bookclerk-diagnostics-worker",
    },
    body: JSON.stringify({
      client_id: cfg.clientId,
      client_secret: cfg.clientSecret,
      code: p.code,
      redirect_uri: p.redirectUri,
      code_verifier: p.verifier,
    }),
  });
  const data = await res.json().catch(() => null);
  const token = res.ok ? decodeAccessToken(data) : null;
  if (token) return { token };
  return { error: oauthErrorCode(data) ?? `HTTP ${res.status}` };
}

/**
 * Who a user token belongs to.
 *
 * @param userToken - Token from `exchangeCode`.
 * @returns Numeric id and login, or the HTTP status when GitHub refuses the token
 *   or answers with something else.
 */
export async function fetchUser(
  userToken: string,
): Promise<{ user: { id: number; login: string } } | { error: string }> {
  const res = await gh(`${API}/user`, { headers: apiHeaders(userToken) });
  const user = res.ok ? decodeUser(await res.json().catch(() => null)) : null;
  return user ? { user } : { error: `HTTP ${res.status}` };
}

/**
 * Revokes a user token so it cannot be used after sign-in.
 *
 * @param cfg - Settings.
 * @param userToken - Token from `exchangeCode`.
 * @returns Resolves whether or not GitHub accepted it.
 */
export async function revokeUserToken(cfg: GitHubConfig, userToken: string): Promise<void> {
  try {
    await gh(`${API}/applications/${cfg.clientId}/token`, {
      method: "DELETE",
      headers: {
        ...apiHeaders(null),
        Authorization: `Basic ${btoa(`${cfg.clientId}:${cfg.clientSecret}`)}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ access_token: userToken }),
    });
  } catch {
    // The token also expires on its own; a failed revoke must not fail sign-in.
  }
}

/**
 * Whether a GitHub user may use the dashboard, and with which role.
 *
 * An organization owner's active members pass, with their organization role.
 * When the repository belongs to a user rather than an organization, only
 * that user passes.
 *
 * @param cfg - Settings.
 * @param user - Signed-in user.
 * @param now - Unix time in milliseconds.
 * @returns Role, or null when not an
 *   active member.
 * @throws {Error} When GitHub cannot answer (token or permission problem),
 *   so a GitHub outage is not reported as "not a member".
 */
export async function membershipRole(
  cfg: GitHubConfig,
  user: { id: number; login: string },
  now: number,
): Promise<"admin" | "member" | null> {
  const token = await installationToken(cfg, now);
  const res = await gh(`${API}/orgs/${cfg.owner}/memberships/${encodeURIComponent(user.login)}`, {
    headers: apiHeaders(token),
  });
  const body = res.status === 404 || !res.ok ? "" : await res.text().catch(() => "");
  return membershipDecision(res.status, body, user, cfg.owner);
}

/**
 * Decides, from GitHub's answer to the organization membership request,
 * whether a user may use the dashboard and with which role. Sans-IO, so the
 * fuzzer drives the same code sign-in does.
 *
 * @param status - HTTP status of `GET /orgs/{owner}/memberships/{login}`.
 * @param body - Response body text (read only for a 2xx status).
 * @param user - Signed-in user from `GET /user`.
 * @param owner - Repository owner (organization or user login).
 * @returns Role, or null when not an active
 *   member: a 404 passes only the owner itself (a user-owned repository);
 *   a 2xx passes only an `active` membership for the same user id, as
 *   "admin" exactly when GitHub says `role: "admin"`.
 * @throws {Error} For any other status, so an outage or permission problem
 *   is not reported as "not a member".
 */
export function membershipDecision(
  status: number,
  body: string,
  user: { id: number; login: string },
  owner: string,
): "admin" | "member" | null {
  if (status === 404) {
    // Not a member, or the owner is a user account.
    return typeof user?.login === "string" &&
      typeof owner === "string" &&
      user.login.toLowerCase() === owner.toLowerCase()
      ? "admin"
      : null;
  }
  if (!(status >= 200 && status < 300)) throw new Error(`membership check failed: HTTP ${status}`);
  let m: unknown;
  try {
    m = JSON.parse(body);
  } catch {
    return null;
  }
  if (!isRecordLike(m) || m.state !== "active") return null;
  if (!isSafeInteger(user?.id) || user.id <= 0 || prop(m.user, "id") !== user.id) return null;
  return m.role === "admin" ? "admin" : "member";
}

// ---------------------------------------------------------------------------
// Issues and pull requests

/**
 * One page of diagnostics issues updated since a time, oldest update first.
 *
 * @param cfg - Settings.
 * @param token - Installation token.
 * @param sinceIso - ISO-8601 time, or "" for all.
 * @param page - 1-based page.
 * @returns Issues (pull requests left out); whether GitHub returned a full page, so
 *   another may follow; and the latest `updated_at` on the page, pull
 *   requests included, so the sync cursor moves past them too.
 */
export async function listDiagnosticsIssues(
  cfg: GitHubConfig,
  token: string,
  sinceIso: string,
  page: number,
): Promise<{ issues: SyncedIssue[]; full: boolean; newest: string }> {
  const q = new URLSearchParams({
    labels: DIAGNOSTICS_LABEL,
    state: "all",
    sort: "updated",
    direction: "asc",
    per_page: "100",
    page: String(page),
  });
  if (sinceIso) q.set("since", sinceIso);
  const res = await gh(`${API}/repos/${cfg.owner}/${cfg.repo}/issues?${q}`, {
    headers: apiHeaders(token),
  });
  if (!res.ok) throw new Error(`issue list failed: HTTP ${res.status}`);
  return decodeIssuePage(await res.json().catch(() => null));
}

/**
 * One issue, read again from GitHub.
 *
 * @param cfg - Settings.
 * @param token - Installation token.
 * @param number - Issue number.
 * @returns The
 *   issue, or null when GitHub has none by that number (deleted,
 *   transferred, or a pull request) or answers with another number.
 * @throws {Error} When GitHub answers with another failure.
 */
export async function fetchIssue(
  cfg: GitHubConfig,
  token: string,
  number: number,
): Promise<{ issue: SyncedIssue; labeled: boolean } | null> {
  // A transferred issue answers 301 to its new home in another repository;
  // that is not this repository's issue, so the redirect is not followed.
  const res = await gh(`${API}/repos/${cfg.owner}/${cfg.repo}/issues/${number}`, {
    headers: apiHeaders(token),
    redirect: "manual",
  });
  if (res.status === 404 || res.status === 410 || res.status === 301) return null;
  if (!res.ok) throw new Error(`issue read failed: HTTP ${res.status}`);
  const got = decodeIssue(await res.json().catch(() => null));
  return got && got.issue.number === number ? got : null;
}

/** Label that marks a diagnostics issue. */
export const DIAGNOSTICS_LABEL = "diagnostics";

/**
 * Prefix of the label that sets an issue's fixed-in version by hand, as in
 * `fixed-in:0.4.2`. It overrides the version the sync derives from releases.
 */
export const FIXED_IN_LABEL = "fixed-in:";

/**
 * The version a `fixed-in:<version>` label names: `MAJOR.MINOR.PATCH` with
 * an optional prerelease and build, with or without a leading `v`, which is
 * dropped. Longer or odder values are not versions.
 */
const FIXED_IN_VERSION =
  /^v?(\d{1,6}\.\d{1,6}\.\d{1,6}(?:-[0-9A-Za-z.-]{1,64})?(?:\+[0-9A-Za-z.-]{1,64})?)$/;

/**
 * The fixed-in version an issue's labels set.
 *
 * @param labels - `labels` from the REST API (objects with `name`,
 *   or names).
 * @returns The version of the first well-formed
 *   `fixed-in:<version>` label, without a leading `v`, or null.
 */
export function fixedInLabel(labels: unknown): string | null {
  if (!Array.isArray(labels)) return null;
  for (const l of labels) {
    const lname = prop(l, "name");
    const name = typeof l === "string" ? l : typeof lname === "string" ? lname : "";
    if (name.length > 100 || !name.startsWith(FIXED_IN_LABEL)) continue;
    const m = FIXED_IN_VERSION.exec(name.slice(FIXED_IN_LABEL.length));
    if (m?.[1] !== undefined) return m[1];
  }
  return null;
}

/** Releases per list page (GitHub's maximum). */
const RELEASE_PAGE = 100;

/**
 * Most release pages one lookup reads, so a repository with a very long
 * release history cannot make the sync page through all of it; past this
 * the lookup leaves fixed-in versions to labels.
 */
const RELEASE_PAGES = 5;

/**
 * Reads `GET /repos/{owner}/{repo}/releases`.
 *
 * @param data - Parsed body.
 * @returns Published, non-draft releases whose tag is a plain name (letters,
 *   digits, `.`, `_`, `-`; at most 100 characters) with a parseable
 *   `published_at`, one per tag (the first kept), oldest first.
 */
export function decodeReleases(
  data: unknown,
): { tag: string; publishedAt: number; prerelease: boolean }[] {
  const list = Array.isArray(data) ? data : [];
  const seen = new Set<string>();
  const out: { tag: string; publishedAt: number; prerelease: boolean }[] = [];
  for (const r of list) {
    if (!isRecordLike(r) || r.draft === true) continue;
    const tag = typeof r.tag_name === "string" ? r.tag_name : "";
    if (!/^[A-Za-z0-9._-]{1,100}$/.test(tag) || seen.has(tag)) continue;
    const publishedAt = Date.parse(typeof r.published_at === "string" ? r.published_at : "");
    if (!Number.isFinite(publishedAt)) continue;
    seen.add(tag);
    out.push({ tag, publishedAt, prerelease: r.prerelease === true });
  }
  return out.sort(
    (a, b) => a.publishedAt - b.publishedAt || (a.tag < b.tag ? -1 : a.tag > b.tag ? 1 : 0),
  );
}

/**
 * The repository's releases, read to the end of the list.
 *
 * GitHub lists releases by creation time, and a release can be published
 * long after it was created, so no page short of the last one proves that
 * every release published after a given time has been read. Pages are
 * therefore read until a short page ends the list or `RELEASE_PAGES` pages
 * were read, and the caller treats a list cut off by the cap as incomplete.
 *
 * @param cfg - Settings.
 * @param token - Installation token.
 * @returns `releases` as `decodeReleases` returns them over all pages read;
 *   `complete` is whether the end of the list was reached.
 * @throws {Error} When a call fails.
 */
export async function listReleases(
  cfg: GitHubConfig,
  token: string,
): Promise<{ releases: ReturnType<typeof decodeReleases>; complete: boolean }> {
  const raw = [];
  let complete = false;
  for (let page = 1; page <= RELEASE_PAGES && !complete; page++) {
    const res = await gh(
      `${API}/repos/${cfg.owner}/${cfg.repo}/releases?per_page=${RELEASE_PAGE}&page=${page}`,
      {
        headers: apiHeaders(token),
      },
    );
    if (!res.ok) throw new Error(`release list failed: HTTP ${res.status}`);
    const body = await res.json().catch(() => null);
    const items = Array.isArray(body) ? body : [];
    raw.push(...items);
    complete = items.length < RELEASE_PAGE;
  }
  return { releases: decodeReleases(raw), complete };
}

/**
 * Reads `GET /repos/{owner}/{repo}/compare/{tag}...{sha}`: whether the tag
 * contains the commit.
 *
 * @param data - Parsed body.
 * @returns True for `identical` or `behind` (the commit
 *   is the tag or one of its ancestors, so the tag contains it; the compare
 *   is `{tag}...{sha}`, base first), false for `ahead` (the commit is newer
 *   than the tag) or `diverged`, null for any other answer.
 */
export function decodeCompare(data: unknown): boolean | null {
  const raw = prop(data, "status");
  const status = typeof raw === "string" ? raw : "";
  if (status === "identical" || status === "behind") return true;
  if (status === "ahead" || status === "diverged") return false;
  return null;
}

/**
 * Whether a release tag contains a commit.
 *
 * @param cfg - Settings.
 * @param token - Installation token.
 * @param tag - A tag from `decodeReleases`.
 * @param sha - A merge commit (40 hex characters).
 * @returns As `decodeCompare` returns; null also
 *   when GitHub has no such tag or commit (404).
 * @throws {Error} When the call fails otherwise.
 */
export async function tagContains(
  cfg: GitHubConfig,
  token: string,
  tag: string,
  sha: string,
): Promise<boolean | null> {
  if (!/^[A-Za-z0-9._-]{1,100}$/.test(tag) || !/^[0-9a-f]{40}$/.test(sha)) return null;
  const res = await gh(
    `${API}/repos/${cfg.owner}/${cfg.repo}/compare/${encodeURIComponent(tag)}...${sha}`,
    { headers: apiHeaders(token) },
  );
  if (res.status === 404) return null;
  if (!res.ok) throw new Error(`compare failed: HTTP ${res.status}`);
  return decodeCompare(await res.json().catch(() => null));
}

/** GraphQL fields read for a referencing pull request. */
const PR_FIELDS = "number title state mergedAt mergeCommit { oid } repository { nameWithOwner }";

/**
 * Pull requests that close or cross-reference each issue, in this
 * repository, and whether each issue still carries the `diagnostics` label
 * (the label-filtered issue listing cannot report a removal). An issue
 * counts as unlabeled only when GitHub returns its complete label list
 * without `diagnostics`.
 *
 * Cross-references are read newest first (the last 50 timeline events), so
 * a long-lived issue still shows the pull requests that mention it lately.
 *
 * @param cfg - Settings.
 * @param token - Installation token.
 * @param numbers - Issue numbers (at most 10 per call).
 * @returns Issue number →
 *   label check and distinct pull requests. An issue GitHub returned no data
 *   for is left out, so the caller keeps what it stored before.
 * @throws {Error} When the call fails or the response carries GraphQL
 *   errors, so a failed refresh never reads as "no pull requests".
 */
export async function pullRequestsFor(
  cfg: GitHubConfig,
  token: string,
  numbers: number[],
): Promise<
  Map<
    number,
    {
      labeled: boolean;
      prs: {
        number: number;
        title: string;
        state: "open" | "closed" | "merged";
        mergedAt: number | null;
        mergeSha: string | null;
      }[];
    }
  >
> {
  const fields = numbers
    .map(
      (n, i) => `i${i}: issue(number: ${Number(n)}) {
        labels(first: 50) { nodes { name } pageInfo { hasNextPage } }
        closedByPullRequestsReferences(first: 10, includeClosedPrs: true) { nodes { ${PR_FIELDS} } }
        timelineItems(last: 50, itemTypes: [CROSS_REFERENCED_EVENT, CONNECTED_EVENT]) { nodes {
          ... on CrossReferencedEvent { source { ... on PullRequest { ${PR_FIELDS} } } }
          ... on ConnectedEvent { subject { ... on PullRequest { ${PR_FIELDS} } } }
        } }
      }`,
    )
    .join("\n");
  const query = `query($owner: String!, $name: String!) { repository(owner: $owner, name: $name) { ${fields} } }`;
  const res = await gh(`${API}/graphql`, {
    method: "POST",
    headers: { ...apiHeaders(token), "Content-Type": "application/json" },
    body: JSON.stringify({ query, variables: { owner: cfg.owner, name: cfg.repo } }),
  });
  if (!res.ok) throw new Error(`graphql failed: HTTP ${res.status}`);
  return decodePullRequests(await res.json(), numbers, `${cfg.owner}/${cfg.repo}`);
}

/**
 * Reads the GraphQL answer `pullRequestsFor` asked for. Sans-IO, so the
 * fuzzer drives the same code the sync does.
 *
 * @param data - Parsed response body.
 * @param numbers - Issue numbers, in the order of the `i0`, `i1`, …
 *   aliases.
 * @param repository - `owner/name`; pull requests elsewhere are
 *   ignored.
 * @returns As
 *   `pullRequestsFor` returns; `mergeSha` is the merge commit (40 lowercase
 *   hex characters) of a merged pull request, else null.
 * @throws {Error} When the response carries GraphQL errors or no
 *   repository object.
 */
export function decodePullRequests(
  data: unknown,
  numbers: number[],
  repository: string,
): Map<
  number,
  {
    labeled: boolean;
    prs: {
      number: number;
      title: string;
      state: "open" | "closed" | "merged";
      mergedAt: number | null;
      mergeSha: string | null;
    }[];
  }
> {
  const text = (v: unknown): string => (typeof v === "string" ? v : "");
  const errors = prop(data, "errors");
  if (Array.isArray(errors) && errors.length) throw new Error("graphql returned errors");
  const repoData = prop(prop(data, "data"), "repository");
  if (!isRecordLike(repoData)) throw new Error("graphql returned no repository");
  const self = repository.toLowerCase();
  type Pr = {
    number: number;
    title: string;
    state: "open" | "closed" | "merged";
    mergedAt: number | null;
    mergeSha: string | null;
  };
  const out = new Map<number, { labeled: boolean; prs: Pr[] }>();
  numbers.forEach((n, i) => {
    const issue = repoData[`i${i}`];
    if (!isRecordLike(issue)) return;
    const found = new Map<number, Pr>();
    const take = (pr: unknown): void => {
      if (!isRecordLike(pr) || !isSafeInteger(pr.number) || pr.number <= 0) return;
      if (text(prop(pr.repository, "nameWithOwner")).toLowerCase() !== self) return;
      const state =
        pr.state === "OPEN"
          ? "open"
          : pr.state === "CLOSED"
            ? "closed"
            : pr.state === "MERGED"
              ? "merged"
              : null;
      if (!state) return;
      const mergedAt = Date.parse(text(pr.mergedAt));
      const oid = text(prop(pr.mergeCommit, "oid"));
      found.set(pr.number, {
        number: pr.number,
        title: text(pr.title).slice(0, 300),
        state,
        mergedAt: Number.isFinite(mergedAt) ? mergedAt : null,
        mergeSha: state === "merged" && /^[0-9a-f]{40}$/.test(oid) ? oid : null,
      });
    };
    const list = (v: unknown): unknown[] => (Array.isArray(v) ? v : []);
    for (const pr of list(prop(issue.closedByPullRequestsReferences, "nodes"))) take(pr);
    for (const ev of list(prop(issue.timelineItems, "nodes")))
      take(prop(ev, "source") ?? prop(ev, "subject"));
    const nodes = prop(issue.labels, "nodes");
    const labels: unknown[] | null = Array.isArray(nodes) ? nodes : null;
    // Without a complete label list the check is unknown: keep the issue.
    const complete =
      labels !== null && prop(prop(issue.labels, "pageInfo"), "hasNextPage") === false;
    const labeled =
      !complete ||
      (labels ?? []).some((l) => text(prop(l, "name")).toLowerCase() === DIAGNOSTICS_LABEL);
    out.set(n, { labeled, prs: [...found.values()] });
  });
  return out;
}
