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
  const clientSecret = typeof env.GITHUB_APP_CLIENT_SECRET === "string" ? env.GITHUB_APP_CLIENT_SECRET : "";
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
  const { id } = await inst.json();
  if (!Number.isSafeInteger(id)) throw new Error("app installation id missing");
  const res = await gh(`${API}/app/installations/${id}/access_tokens`, {
    method: "POST",
    headers: { ...apiHeaders(jwt), "Content-Type": "application/json" },
    body: JSON.stringify({ repositories: [cfg.repo], permissions: INSTALLATION_PERMISSIONS }),
  });
  if (!res.ok) throw new Error(`installation token failed: HTTP ${res.status}`);
  const data = await res.json();
  const expiresAt = Date.parse(data.expires_at);
  if (typeof data.token !== "string" || !Number.isFinite(expiresAt)) {
    throw new Error("installation token response malformed");
  }
  cachedToken = { key, token: data.token, expiresAt };
  return data.token;
}

/** Forgets the cached installation token (tests). */
export function clearTokenCache() {
  cachedToken = null;
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
 * @returns {Promise<string | null>} The user token, or null when GitHub
 *   refuses the code.
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
  if (!res.ok) return null;
  const data = await res.json().catch(() => null);
  return data && typeof data.access_token === "string" && data.access_token ? data.access_token : null;
}

/**
 * Who a user token belongs to.
 *
 * @param {string} userToken Token from `exchangeCode`.
 * @returns {Promise<{id: number, login: string} | null>} Numeric id and
 *   login, or null when GitHub refuses the token.
 */
export async function fetchUser(userToken) {
  const res = await gh(`${API}/user`, { headers: apiHeaders(userToken) });
  if (!res.ok) return null;
  const u = await res.json().catch(() => null);
  if (!u || !Number.isSafeInteger(u.id) || typeof u.login !== "string") return null;
  if (!/^[A-Za-z0-9-]{1,39}$/.test(u.login)) return null;
  return { id: u.id, login: u.login };
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
  if (res.status === 404) {
    // Not a member, or the owner is a user account.
    return user.login.toLowerCase() === cfg.owner.toLowerCase() ? "admin" : null;
  }
  if (!res.ok) throw new Error(`membership check failed: HTTP ${res.status}`);
  const m = await res.json().catch(() => null);
  if (!m || m.state !== "active" || m.user?.id !== user.id) return null;
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
 * @returns {Promise<object[]>} Issues (pull requests left out).
 */
export async function listDiagnosticsIssues(cfg, token, sinceIso, page) {
  const q = new URLSearchParams({
    labels: "diagnostics",
    state: "all",
    sort: "updated",
    direction: "asc",
    per_page: "100",
    page: String(page),
  });
  if (sinceIso) q.set("since", sinceIso);
  const res = await gh(`${API}/repos/${cfg.owner}/${cfg.repo}/issues?${q}`, { headers: apiHeaders(token) });
  if (!res.ok) throw new Error(`issue list failed: HTTP ${res.status}`);
  const items = await res.json();
  return Array.isArray(items) ? items.filter((i) => i && !i.pull_request) : [];
}

/** GraphQL fields read for a referencing pull request. */
const PR_FIELDS = "number title state mergedAt repository { nameWithOwner }";

/**
 * Pull requests that close or cross-reference each issue, in this repository.
 *
 * @param {GitHubConfig} cfg Settings.
 * @param {string} token Installation token.
 * @param {number[]} numbers Issue numbers (at most 10 per call).
 * @returns {Promise<Map<number, {number: number, title: string,
 *   state: "open" | "closed" | "merged", mergedAt: number | null}[]>>}
 *   Issue number → distinct pull requests. An issue GitHub returned no data
 *   for is left out, so the caller keeps what it stored before.
 * @throws {Error} When the call fails or the response carries GraphQL
 *   errors, so a failed refresh never reads as "no pull requests".
 */
export async function pullRequestsFor(cfg, token, numbers) {
  const fields = numbers
    .map(
      (n, i) => `i${i}: issue(number: ${Number(n)}) {
        closedByPullRequestsReferences(first: 10, includeClosedPrs: true) { nodes { ${PR_FIELDS} } }
        timelineItems(first: 50, itemTypes: [CROSS_REFERENCED_EVENT, CONNECTED_EVENT]) { nodes {
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
  const data = await res.json();
  if (Array.isArray(data?.errors) && data.errors.length) throw new Error("graphql returned errors");
  const repoData = data?.data?.repository;
  if (!repoData || typeof repoData !== "object") throw new Error("graphql returned no repository");
  const self = `${cfg.owner}/${cfg.repo}`.toLowerCase();
  const out = new Map();
  numbers.forEach((n, i) => {
    const issue = repoData[`i${i}`];
    if (!issue || typeof issue !== "object") return;
    const found = new Map();
    const take = (pr) => {
      if (!pr || !Number.isSafeInteger(pr.number)) return;
      if (String(pr.repository?.nameWithOwner ?? "").toLowerCase() !== self) return;
      const state = { OPEN: "open", CLOSED: "closed", MERGED: "merged" }[pr.state];
      if (!state) return;
      const mergedAt = pr.mergedAt ? Date.parse(pr.mergedAt) : null;
      found.set(pr.number, {
        number: pr.number,
        title: String(pr.title ?? "").slice(0, 300),
        state,
        mergedAt: Number.isFinite(mergedAt) ? mergedAt : null,
      });
    };
    for (const pr of issue?.closedByPullRequestsReferences?.nodes ?? []) take(pr);
    for (const ev of issue?.timelineItems?.nodes ?? []) take(ev?.source ?? ev?.subject);
    out.set(n, [...found.values()]);
  });
  return out;
}
