/**
 * GitHub calls for the dashboard: sign-in through the automation GitHub App's
 * user authorization, the organization membership check, and reads of
 * diagnostics issues and the pull requests that reference them.
 *
 * The App can write to the repository, so the Worker narrows what it holds:
 * - every installation token it mints is down-scoped in the request to
 *   `INSTALLATION_PERMISSIONS` on the one configured repository, so no token
 *   the Worker ever holds can write;
 * - the user token from sign-in is used for one `GET /user` call and then
 *   revoked, never stored, logged or sent to the browser;
 * - the private key and client secret are Worker secrets read only here.
 */

/** GitHub REST and GraphQL host. */
export const API = "https://api.github.com";

/** GitHub web host (OAuth endpoints). */
export const WEB = "https://github.com";

/** Permissions every installation token is narrowed to. All read-only. */
export const INSTALLATION_PERMISSIONS = Object.freeze({
  metadata: "read",
  issues: "read",
  pull_requests: "read",
  members: "read",
});

/** Time allowed for one GitHub call, in milliseconds. */
const TIMEOUT_MS = 10_000;

/** Installation tokens are refreshed when fewer than this many ms remain. */
const TOKEN_REFRESH_MARGIN_MS = 5 * 60 * 1000;

/**
 * Dashboard GitHub settings, or null when the dashboard is not configured
 * (a fork without the App secrets): then every dashboard route is off.
 *
 * @typedef {{clientId: string, clientSecret: string, privateKey: string,
 *   owner: string, repo: string}} GitHubConfig
 */

/**
 * Reads the dashboard's GitHub settings from the Worker environment.
 *
 * @param {Record<string, unknown>} env Worker bindings.
 * @returns {GitHubConfig | null} Settings, or null when any is missing or
 *   `GITHUB_REPOSITORY` is not `owner/name`.
 */
export function githubConfig(env) {
  const clientId = typeof env.GITHUB_APP_CLIENT_ID === "string" ? env.GITHUB_APP_CLIENT_ID.trim() : "";
  // Trimmed: a secret pasted with a trailing newline makes GitHub answer
  // incorrect_client_credentials.
  const clientSecret = typeof env.GITHUB_APP_CLIENT_SECRET === "string" ? env.GITHUB_APP_CLIENT_SECRET.trim() : "";
  const privateKey = typeof env.GITHUB_APP_PRIVATE_KEY === "string" ? env.GITHUB_APP_PRIVATE_KEY : "";
  const m = /^([A-Za-z0-9_.-]+)\/([A-Za-z0-9_.-]+)$/.exec(String(env.GITHUB_REPOSITORY || "").trim());
  if (!clientId || !clientSecret || !privateKey || !m) return null;
  if (!/^[A-Za-z0-9._-]{1,64}$/.test(clientId)) return null;
  return { clientId, clientSecret, privateKey, owner: m[1], repo: m[2] };
}

/** Headers for every GitHub API call. */
function apiHeaders(token) {
  const h = {
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
 * @param {string} url Absolute URL on api.github.com or github.com.
 * @param {RequestInit} init Request options.
 * @returns {Promise<Response>} The response.
 */
function gh(url, init) {
  return fetch(url, { ...init, signal: AbortSignal.timeout(TIMEOUT_MS) });
}

// ---------------------------------------------------------------------------
// App authentication

/**
 * DER length bytes.
 *
 * @param {number} n Content length.
 * @returns {number[]} Encoded length.
 */
function derLength(n) {
  if (n < 0x80) return [n];
  const bytes = [];
  for (let v = n; v > 0; v = Math.floor(v / 256)) bytes.unshift(v & 0xff);
  return [0x80 | bytes.length, ...bytes];
}

/**
 * DER element.
 *
 * @param {number} tag Tag byte.
 * @param {Uint8Array | number[]} content Content bytes.
 * @returns {Uint8Array} Encoded element.
 */
function der(tag, content) {
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
 * @param {string} pem PEM text.
 * @returns {Uint8Array} PKCS#8 DER.
 * @throws {Error} When the PEM holds neither form.
 */
export function pkcs8FromPem(pem) {
  const m = /-----BEGIN (RSA )?PRIVATE KEY-----([\s\S]+?)-----END (RSA )?PRIVATE KEY-----/.exec(pem);
  if (!m || Boolean(m[1]) !== Boolean(m[3])) throw new Error("unrecognized private key");
  const bin = atob(m[2].replace(/\s+/g, ""));
  const body = Uint8Array.from(bin, (c) => c.charCodeAt(0));
  if (!m[1]) return body;
  const version = Uint8Array.from([0x02, 0x01, 0x00]);
  const octets = der(0x04, body);
  return der(0x30, [...version, ...RSA_ALGORITHM, ...octets]);
}

/**
 * A short-lived JWT that authenticates as the App (RS256, 9 minutes).
 *
 * @param {GitHubConfig} cfg Settings.
 * @param {number} now Unix time in milliseconds.
 * @returns {Promise<string>} The JWT.
 */
export async function appJwt(cfg, now) {
  const key = await crypto.subtle.importKey(
    "pkcs8",
    pkcs8FromPem(cfg.privateKey),
    { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const enc = (obj) => b64url(new TextEncoder().encode(JSON.stringify(obj)));
  const iat = Math.floor(now / 1000) - 60;
  const head = `${enc({ alg: "RS256", typ: "JWT" })}.${enc({ iat, exp: iat + 600, iss: cfg.clientId })}`;
  const sig = await crypto.subtle.sign("RSASSA-PKCS1-v1_5", key, new TextEncoder().encode(head));
  return `${head}.${b64url(new Uint8Array(sig))}`;
}

/**
 * Base64url without padding.
 *
 * @param {Uint8Array} bytes Input.
 * @returns {string} Encoded text.
 */
function b64url(bytes) {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/** Cached down-scoped installation token for this isolate. */
let cachedToken = null;

/**
 * A read-only installation token for the configured repository, cached while
 * it has more than five minutes left.
 *
 * @param {GitHubConfig} cfg Settings.
 * @param {number} now Unix time in milliseconds.
 * @returns {Promise<string>} The token.
 * @throws {Error} When the App is not installed on the repository or GitHub
 *   refuses the narrowed permissions (for example Members is not granted).
 */
export async function installationToken(cfg, now) {
  const key = `${cfg.clientId}/${cfg.owner}/${cfg.repo}`;
  if (cachedToken && cachedToken.key === key && cachedToken.expiresAt - TOKEN_REFRESH_MARGIN_MS > now) {
    return cachedToken.token;
  }
  const jwt = await appJwt(cfg, now);
  const inst = await gh(`${API}/repos/${cfg.owner}/${cfg.repo}/installation`, { headers: apiHeaders(jwt) });
  if (!inst.ok) throw new Error(`app installation lookup failed: HTTP ${inst.status}`);
  const id = decodeInstallationId(await inst.json().catch(() => null));
  if (id === null) throw new Error("app installation id missing");
  const res = await gh(`${API}/app/installations/${id}/access_tokens`, {
    method: "POST",
    headers: { ...apiHeaders(jwt), "Content-Type": "application/json" },
    body: JSON.stringify({ repositories: [cfg.repo], permissions: INSTALLATION_PERMISSIONS }),
  });
  if (!res.ok) throw new Error(`installation token failed: HTTP ${res.status}`);
  const got = decodeInstallationToken(await res.json().catch(() => null));
  if (!got) throw new Error("installation token response malformed");
  cachedToken = { key, ...got };
  return got.token;
}

/** Forgets the cached installation token (tests). */
export function clearTokenCache() {
  cachedToken = null;
}

// ---------------------------------------------------------------------------
// Response decoders: sans-IO, so the fuzzer drives the same code as the
// requests above. None of them throws.

/** Whether a value is a non-null object. */
const isObject = (v) => v !== null && typeof v === "object";

/** A GitHub login: letters, digits and hyphens, at most 39. */
const LOGIN = /^[A-Za-z0-9-]{1,39}$/;

/**
 * Reads `GET /repos/{owner}/{repo}/installation`.
 *
 * @param {unknown} data Parsed body.
 * @returns {number | null} The installation id when it is a positive safe
 *   integer.
 */
export function decodeInstallationId(data) {
  const id = isObject(data) ? data.id : undefined;
  return Number.isSafeInteger(id) && id > 0 ? id : null;
}

/**
 * Reads `POST /app/installations/{id}/access_tokens`.
 *
 * @param {unknown} data Parsed body.
 * @returns {{token: string, expiresAt: number} | null} A non-empty token
 *   and its expiry in milliseconds, or null when either is missing.
 */
export function decodeInstallationToken(data) {
  if (!isObject(data) || typeof data.token !== "string" || data.token === "" || typeof data.expires_at !== "string") return null;
  const expiresAt = Date.parse(data.expires_at);
  return Number.isFinite(expiresAt) ? { token: data.token, expiresAt } : null;
}

/**
 * Reads the sign-in code exchange answer.
 *
 * @param {unknown} data Parsed body.
 * @returns {string | null} A non-empty `access_token`, or null.
 */
export function decodeAccessToken(data) {
  return isObject(data) && typeof data.access_token === "string" && data.access_token !== "" ? data.access_token : null;
}

/**
 * Reads GitHub's OAuth error code (`bad_verification_code`,
 * `incorrect_client_credentials`, `redirect_uri_mismatch`, ...) from a code
 * exchange answer, for the sign-in error page and the Worker log. The
 * free-text `error_description` is never read.
 *
 * @param {unknown} data Parsed body.
 * @returns {string | null} The `error` field when it is 1 to 40 lowercase
 *   letters and underscores, else null.
 */
export function oauthErrorCode(data) {
  return isObject(data) && typeof data.error === "string" && /^[a-z_]{1,40}$/.test(data.error) ? data.error : null;
}

/**
 * Reads `GET /user`.
 *
 * @param {unknown} data Parsed body.
 * @returns {{id: number, login: string} | null} A positive safe integer id
 *   and a GitHub-shaped login, or null.
 */
export function decodeUser(data) {
  if (!isObject(data) || !Number.isSafeInteger(data.id) || data.id <= 0) return null;
  if (typeof data.login !== "string" || !LOGIN.test(data.login)) return null;
  return { id: data.id, login: data.login };
}

/**
 * An issue as the sync stores it.
 *
 * @typedef {{number: number, title: string, state: "open" | "closed",
 *   body: string, created_at: string, updated_at: string,
 *   closed_at: string | null}} SyncedIssue
 */

/**
 * Reads one page of `GET /repos/{owner}/{repo}/issues`.
 *
 * @param {unknown} data Parsed body.
 * @returns {{issues: SyncedIssue[], full: boolean, newest: string}} Issues
 *   with a positive safe integer number (pull requests left out; text
 *   fields that are not strings become ""), whether the page held 100
 *   entries (counted before filtering, so paging continues past a page of
 *   pull requests), and the newest valid `updated_at` on the page as
 *   ISO-8601, or "" when none parses.
 */
export function decodeIssuePage(data) {
  const list = Array.isArray(data) ? data : [];
  const str = (v) => (typeof v === "string" ? v : "");
  let newestMs = -Infinity;
  const issues = [];
  for (const i of list) {
    if (!isObject(i)) continue;
    const t = Date.parse(str(i.updated_at));
    if (Number.isFinite(t) && t > newestMs) newestMs = t;
    if (i.pull_request !== undefined && i.pull_request !== null) continue;
    if (!Number.isSafeInteger(i.number) || i.number <= 0) continue;
    issues.push({
      number: i.number,
      title: str(i.title),
      state: i.state === "closed" ? "closed" : "open",
      body: str(i.body),
      created_at: str(i.created_at),
      updated_at: str(i.updated_at),
      closed_at: typeof i.closed_at === "string" ? i.closed_at : null,
    });
  }
  return { issues, full: list.length >= 100, newest: Number.isFinite(newestMs) ? new Date(newestMs).toISOString() : "" };
}

// ---------------------------------------------------------------------------
// Sign-in

/**
 * The GitHub authorize URL that starts sign-in.
 *
 * @param {GitHubConfig} cfg Settings.
 * @param {{redirectUri: string, state: string, challenge: string}} p
 *   Callback URL, CSRF state and PKCE S256 challenge.
 * @returns {string} URL on github.com.
 */
export function authorizeUrl(cfg, p) {
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
 * @param {GitHubConfig} cfg Settings.
 * @param {{code: string, redirectUri: string, verifier: string}} p Code,
 *   callback URL and PKCE verifier.
 * @returns {Promise<{token: string} | {error: string}>} The user token, or
 *   why GitHub refused the code: its OAuth error code, or the HTTP status
 *   when it gave none. Neither carries credentials.
 */
export async function exchangeCode(cfg, p) {
  const res = await gh(`${WEB}/login/oauth/access_token`, {
    method: "POST",
    headers: { Accept: "application/json", "Content-Type": "application/json", "User-Agent": "bookclerk-diagnostics-worker" },
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
 * @param {string} userToken Token from `exchangeCode`.
 * @returns {Promise<{user: {id: number, login: string}} | {error: string}>}
 *   Numeric id and login, or the HTTP status when GitHub refuses the token
 *   or answers with something else.
 */
export async function fetchUser(userToken) {
  const res = await gh(`${API}/user`, { headers: apiHeaders(userToken) });
  const user = res.ok ? decodeUser(await res.json().catch(() => null)) : null;
  return user ? { user } : { error: `HTTP ${res.status}` };
}

/**
 * Revokes a user token so it cannot be used after sign-in.
 *
 * @param {GitHubConfig} cfg Settings.
 * @param {string} userToken Token from `exchangeCode`.
 * @returns {Promise<void>} Resolves whether or not GitHub accepted it.
 */
export async function revokeUserToken(cfg, userToken) {
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
 * @param {GitHubConfig} cfg Settings.
 * @param {{id: number, login: string}} user Signed-in user.
 * @param {number} now Unix time in milliseconds.
 * @returns {Promise<"admin" | "member" | null>} Role, or null when not an
 *   active member.
 * @throws {Error} When GitHub cannot answer (token or permission problem),
 *   so a GitHub outage is not reported as "not a member".
 */
export async function membershipRole(cfg, user, now) {
  const token = await installationToken(cfg, now);
  const res = await gh(
    `${API}/orgs/${cfg.owner}/memberships/${encodeURIComponent(user.login)}`,
    { headers: apiHeaders(token) },
  );
  const body = res.status === 404 || !res.ok ? "" : await res.text().catch(() => "");
  return membershipDecision(res.status, body, user, cfg.owner);
}

/**
 * Decides, from GitHub's answer to the organization membership request,
 * whether a user may use the dashboard and with which role. Sans-IO, so the
 * fuzzer drives the same code sign-in does.
 *
 * @param {number} status HTTP status of `GET /orgs/{owner}/memberships/{login}`.
 * @param {string} body Response body text (read only for a 2xx status).
 * @param {{id: number, login: string}} user Signed-in user from `GET /user`.
 * @param {string} owner Repository owner (organization or user login).
 * @returns {"admin" | "member" | null} Role, or null when not an active
 *   member: a 404 passes only the owner itself (a user-owned repository);
 *   a 2xx passes only an `active` membership for the same user id, as
 *   "admin" exactly when GitHub says `role: "admin"`.
 * @throws {Error} For any other status, so an outage or permission problem
 *   is not reported as "not a member".
 */
export function membershipDecision(status, body, user, owner) {
  if (status === 404) {
    // Not a member, or the owner is a user account.
    return typeof user?.login === "string" && typeof owner === "string" && user.login.toLowerCase() === owner.toLowerCase()
      ? "admin"
      : null;
  }
  if (!(status >= 200 && status < 300)) throw new Error(`membership check failed: HTTP ${status}`);
  let m;
  try {
    m = JSON.parse(body);
  } catch {
    return null;
  }
  if (m === null || typeof m !== "object" || m.state !== "active") return null;
  if (!Number.isSafeInteger(user?.id) || user.id <= 0 || m.user?.id !== user.id) return null;
  return m.role === "admin" ? "admin" : "member";
}

// ---------------------------------------------------------------------------
// Issues and pull requests

/**
 * One page of diagnostics issues updated since a time, oldest update first.
 *
 * @param {GitHubConfig} cfg Settings.
 * @param {string} token Installation token.
 * @param {string} sinceIso ISO-8601 time, or "" for all.
 * @param {number} page 1-based page.
 * @returns {Promise<{issues: object[], full: boolean, newest: string}>}
 *   Issues (pull requests left out); whether GitHub returned a full page, so
 *   another may follow; and the latest `updated_at` on the page, pull
 *   requests included, so the sync cursor moves past them too.
 */
export async function listDiagnosticsIssues(cfg, token, sinceIso, page) {
  const q = new URLSearchParams({
    labels: DIAGNOSTICS_LABEL,
    state: "all",
    sort: "updated",
    direction: "asc",
    per_page: "100",
    page: String(page),
  });
  if (sinceIso) q.set("since", sinceIso);
  const res = await gh(`${API}/repos/${cfg.owner}/${cfg.repo}/issues?${q}`, { headers: apiHeaders(token) });
  if (!res.ok) throw new Error(`issue list failed: HTTP ${res.status}`);
  return decodeIssuePage(await res.json().catch(() => null));
}

/** Label that marks a diagnostics issue. */
export const DIAGNOSTICS_LABEL = "diagnostics";

/** GraphQL fields read for a referencing pull request. */
const PR_FIELDS = "number title state mergedAt repository { nameWithOwner }";

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
 * @param {GitHubConfig} cfg Settings.
 * @param {string} token Installation token.
 * @param {number[]} numbers Issue numbers (at most 10 per call).
 * @returns {Promise<Map<number, {labeled: boolean, prs: {number: number,
 *   title: string, state: "open" | "closed" | "merged",
 *   mergedAt: number | null}[]}>>} Issue number → label check and distinct
 *   pull requests. An issue GitHub returned no data for is left out, so the
 *   caller keeps what it stored before.
 * @throws {Error} When the call fails or the response carries GraphQL
 *   errors, so a failed refresh never reads as "no pull requests".
 */
export async function pullRequestsFor(cfg, token, numbers) {
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
 * @param {unknown} data Parsed response body.
 * @param {number[]} numbers Issue numbers, in the order of the `i0`, `i1`, …
 *   aliases.
 * @param {string} repository `owner/name`; pull requests elsewhere are
 *   ignored.
 * @returns {Map<number, {labeled: boolean, prs: {number: number,
 *   title: string, state: "open" | "closed" | "merged",
 *   mergedAt: number | null}[]}>} As `pullRequestsFor` returns.
 * @throws {Error} When the response carries GraphQL errors or no
 *   repository object.
 */
export function decodePullRequests(data, numbers, repository) {
  const text = (v) => (typeof v === "string" ? v : "");
  if (Array.isArray(data?.errors) && data.errors.length) throw new Error("graphql returned errors");
  const repoData = data?.data?.repository;
  if (!repoData || typeof repoData !== "object") throw new Error("graphql returned no repository");
  const self = repository.toLowerCase();
  const out = new Map();
  numbers.forEach((n, i) => {
    const issue = repoData[`i${i}`];
    if (!issue || typeof issue !== "object") return;
    const found = new Map();
    const take = (pr) => {
      if (!pr || !Number.isSafeInteger(pr.number) || pr.number <= 0) return;
      if (text(pr.repository?.nameWithOwner).toLowerCase() !== self) return;
      const state = pr.state === "OPEN" ? "open" : pr.state === "CLOSED" ? "closed" : pr.state === "MERGED" ? "merged" : null;
      if (!state) return;
      const mergedAt = Date.parse(text(pr.mergedAt));
      found.set(pr.number, {
        number: pr.number,
        title: text(pr.title).slice(0, 300),
        state,
        mergedAt: Number.isFinite(mergedAt) ? mergedAt : null,
      });
    };
    const list = (v) => (Array.isArray(v) ? v : []);
    for (const pr of list(issue.closedByPullRequestsReferences?.nodes)) take(pr);
    for (const ev of list(issue.timelineItems?.nodes)) take(ev?.source ?? ev?.subject);
    const labels = Array.isArray(issue.labels?.nodes) ? issue.labels.nodes : null;
    // Without a complete label list the check is unknown: keep the issue.
    const complete = labels !== null && issue.labels?.pageInfo?.hasNextPage === false;
    const labeled = !complete || labels.some((l) => text(l?.name).toLowerCase() === DIAGNOSTICS_LABEL);
    out.set(n, { labeled, prs: [...found.values()] });
  });
  return out;
}
