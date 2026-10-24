// Which bookclerkd requests a demo visitor may make. Sans-IO: the Worker
// calls classify() before anything reaches the visitor's container.
//
// The list is default-deny and covers every operation in
// crates/bookclerkd/openapi.json; test/routes.test.js fails when the API
// gains an operation this table does not name, so a new route cannot reach
// the demo unreviewed. Acquire, downloads, node and cluster links, plugin
// installs, sign-in methods, users and settings writes are refused here
// (lock 1). The image itself has no media worker and no output plugin, so the
// host refuses acquire work too (lock 2).

/**
 * What the Worker does with a request.
 *
 * - `allow`: forward it to the visitor's container.
 * - `signin`: forward it, but count it as a store sign-in attempt first
 *   (password stores sign in through the container, so the demo must not
 *   become a free way to test stolen passwords).
 * - `deny`: answer 403 without contacting the container.
 *
 * @typedef {"allow" | "signin" | "deny"} Verdict
 */

/**
 * The classified operations: method, OpenAPI path template, verdict.
 *
 * @type {ReadonlyArray<readonly [string, string, Verdict]>}
 */
export const OPERATIONS = Object.freeze([
  // Health and the API description.
  ["GET", "/health", "allow"],
  ["GET", "/api/openapi.json", "allow"],
  ["GET", "/api/status", "allow"],

  // Sign-in and the visitor's own account. The demo signs the visitor in;
  // signing out, other sign-in methods and credential changes are refused.
  ["GET", "/api/auth/me", "allow"],
  ["GET", "/api/auth/signin", "allow"],
  ["GET", "/api/auth/sessions", "allow"],
  ["DELETE", "/api/auth/sessions/{id}", "deny"],
  ["POST", "/api/auth/bootstrap", "deny"],
  ["POST", "/api/auth/login", "deny"],
  ["POST", "/api/auth/logout", "deny"],
  ["POST", "/api/auth/elevate", "deny"],
  ["DELETE", "/api/auth/elevate", "deny"],
  ["POST", "/api/auth/impersonate", "deny"],
  ["DELETE", "/api/auth/impersonate", "deny"],
  ["GET", "/api/auth/branch-consent", "deny"],
  ["POST", "/api/auth/branch-consent", "deny"],
  ["POST", "/api/auth/branch-consent/step-up", "deny"],
  ["GET", "/api/auth/mfa-policy", "allow"],
  ["PUT", "/api/auth/mfa-policy", "deny"],
  ["GET", "/api/auth/oidc/callback", "deny"],
  ["POST", "/api/auth/oidc/callback", "deny"],
  ["GET", "/api/auth/oidc/clients", "deny"],
  ["POST", "/api/auth/oidc/clients", "deny"],
  ["PUT", "/api/auth/oidc/clients/{client_id}", "deny"],
  ["DELETE", "/api/auth/oidc/clients/{client_id}", "deny"],
  ["POST", "/api/auth/oidc/clients/{client_id}/rotate-secret", "deny"],
  ["GET", "/api/auth/oidc/config", "deny"],
  ["PUT", "/api/auth/oidc/config", "deny"],
  ["GET", "/api/auth/oidc/elevate", "deny"],
  ["GET", "/api/auth/oidc/identities", "allow"],
  ["GET", "/api/auth/oidc/login", "deny"],
  ["GET", "/api/auth/oidc/providers", "allow"],
  ["GET", "/api/auth/passkeys", "allow"],
  ["POST", "/api/auth/passkeys/elevate/begin", "deny"],
  ["POST", "/api/auth/passkeys/elevate/finish", "deny"],
  ["POST", "/api/auth/passkeys/login/begin", "deny"],
  ["POST", "/api/auth/passkeys/login/finish", "deny"],
  ["POST", "/api/auth/passkeys/register/begin", "deny"],
  ["POST", "/api/auth/passkeys/register/finish", "deny"],
  ["DELETE", "/api/auth/passkeys/{id}", "deny"],
  ["PUT", "/api/auth/password", "deny"],
  ["POST", "/api/auth/password", "deny"],
  ["PATCH", "/api/auth/profile", "deny"],
  ["PUT", "/api/auth/profile/avatar", "deny"],
  ["DELETE", "/api/auth/profile/avatar", "deny"],
  ["GET", "/api/auth/totp", "allow"],
  ["DELETE", "/api/auth/totp", "deny"],
  ["POST", "/api/auth/totp/enroll/begin", "deny"],
  ["POST", "/api/auth/totp/enroll/finish", "deny"],
  ["POST", "/api/auth/totp/login", "deny"],
  ["GET", "/api/auth/tray-handoff", "deny"],
  ["POST", "/api/auth/tray-handoff/prepare", "deny"],

  // Clusters, branches, clerks and nodes: read-only views, no links.
  ["GET", "/api/cluster/status", "allow"],
  ["GET", "/api/config/branches", "allow"],
  ["PUT", "/api/config/branches", "deny"],
  ["POST", "/api/config/branches/titles/remove", "deny"],
  ["GET", "/api/config/clerks", "allow"],
  ["GET", "/api/config/cluster-consents", "allow"],
  ["POST", "/api/config/cluster-consents/approve", "deny"],
  ["POST", "/api/config/cluster-consents/revoke", "deny"],
  ["POST", "/api/config/clusters/{cluster_id}/jobs", "deny"],
  ["POST", "/api/config/clusters/{cluster_id}/scan", "deny"],
  ["GET", "/api/config/host-enrollment", "allow"],
  ["PUT", "/api/config/host-enrollment", "deny"],
  ["GET", "/api/config/join", "allow"],
  ["POST", "/api/config/join/callback", "deny"],
  ["POST", "/api/config/join/inspect", "deny"],
  ["POST", "/api/config/join/invite", "deny"],
  ["POST", "/api/config/join/leave", "deny"],
  ["GET", "/api/config/join/sharing", "allow"],
  ["POST", "/api/config/join/sharing", "deny"],
  ["POST", "/api/config/join/start", "deny"],
  ["DELETE", "/api/config/node-credentials/{node_id}", "deny"],
  ["GET", "/api/config/node-invites", "allow"],
  ["POST", "/api/config/node-invites", "deny"],
  ["DELETE", "/api/config/node-invites/{id}", "deny"],
  ["GET", "/api/config/nodes", "allow"],
  ["DELETE", "/api/config/nodes/{node_id}", "deny"],
  ["PUT", "/api/config/nodes/{node_id}/labels", "deny"],
  ["PUT", "/api/config/nodes/{node_id}/owner", "deny"],
  ["PUT", "/api/config/nodes/{node_id}/runtime", "deny"],
  ["GET", "/api/node/v1/artifacts/{sha256}", "deny"],
  ["GET", "/api/node/v1/connect", "deny"],
  ["POST", "/api/node/v1/enroll", "deny"],
  ["POST", "/api/node/v1/enroll/oauth", "deny"],
  ["GET", "/api/node/v1/info", "deny"],
  ["POST", "/api/node/v1/uploads/{upload_id}/commit", "deny"],
  ["PUT", "/api/node/v1/uploads/{upload_id}/files/{index}", "deny"],

  // Host configuration: readable, never written. Writes could turn on
  // auto-acquire, add an output or point the host at a URL.
  ["GET", "/api/config/domains/core.events", "allow"],
  ["PUT", "/api/config/domains/core.events", "deny"],
  ["GET", "/api/config/domains/core.output", "allow"],
  ["PUT", "/api/config/domains/core.output", "deny"],
  ["POST", "/api/config/reload", "deny"],
  ["POST", "/api/database/migrate", "deny"],
  ["GET", "/api/settings", "allow"],
  ["PATCH", "/api/settings", "deny"],

  // Plugins: the image's own storefronts only; no installs or changes.
  ["GET", "/api/config/plugin-instances", "allow"],
  ["POST", "/api/config/plugin-instances", "deny"],
  ["DELETE", "/api/config/plugin-instances/{id}", "deny"],
  ["GET", "/api/config/plugin-instances/{id}/config", "allow"],
  ["PUT", "/api/config/plugin-instances/{id}/config", "deny"],
  ["PUT", "/api/config/plugin-instances/{id}/deployments/{node_id}", "deny"],
  ["PUT", "/api/config/plugin-instances/{id}/placement", "deny"],
  ["DELETE", "/api/config/plugin-instances/{id}/placement", "deny"],
  ["GET", "/api/config/plugins", "allow"],
  ["POST", "/api/config/plugins", "deny"],
  ["GET", "/api/config/plugins/{key}", "allow"],
  ["DELETE", "/api/config/plugins/{key}", "deny"],
  ["PUT", "/api/config/plugins/{key}/active", "deny"],
  ["POST", "/api/config/plugins/{key}/versions/{version}/approve", "deny"],
  ["GET", "/api/plugins/{id}/consent", "allow"],
  ["POST", "/api/plugins/{id}/consent", "deny"],
  ["GET", "/api/plugins/{kind}/{id}/logo", "allow"],

  // Discover: searching the stores is what the demo is for.
  ["POST", "/api/discover/purchase-hints", "allow"],
  ["POST", "/api/discover/purchase-hints/batch", "allow"],
  ["GET", "/api/discover/recommendations", "allow"],
  ["GET", "/api/discover/search", "allow"],
  ["POST", "/api/discover/sync-listening", "allow"],
  ["POST", "/api/discover/title-meta", "allow"],
  ["POST", "/api/discover/title-meta/batch", "allow"],
  ["POST", "/api/discover/title-reviews", "allow"],

  // Events: readable; deliveries are never retried or resumed.
  ["GET", "/api/events", "allow"],
  ["GET", "/api/events/deliveries", "allow"],
  ["POST", "/api/events/deliveries/{id}/acknowledge", "deny"],
  ["POST", "/api/events/deliveries/{id}/cancel", "deny"],
  ["POST", "/api/events/deliveries/{id}/resume", "deny"],
  ["POST", "/api/events/deliveries/{id}/retry", "deny"],

  // Jobs and the library: scan the visitor's accounts, never acquire.
  ["GET", "/api/jobs", "allow"],
  ["POST", "/api/jobs/{id}/cancel", "allow"],
  ["POST", "/api/library/acquire", "deny"],
  ["GET", "/api/library/books", "allow"],
  ["GET", "/api/library/books/{uuid}", "allow"],
  ["GET", "/api/library/books/{uuid}/cover", "allow"],
  ["POST", "/api/library/scan", "allow"],
  ["GET", "/api/live", "allow"],
  ["GET", "/api/queues", "allow"],
  ["GET", "/api/queues/{name}", "allow"],
  ["GET", "/api/request-queue", "allow"],
  ["GET", "/api/wishlist", "allow"],
  ["POST", "/api/wishlist", "allow"],
  ["DELETE", "/api/wishlist/{uuid}", "allow"],
  ["GET", "/api/preferences", "allow"],
  ["PATCH", "/api/preferences", "allow"],

  // Store accounts. Every way of starting a store sign-in is counted.
  ["GET", "/api/portal/me", "allow"],
  ["GET", "/api/portal/sources", "allow"],
  ["GET", "/api/portal/connections", "allow"],
  ["POST", "/api/portal/audible/start", "signin"],
  ["POST", "/api/portal/libro/login", "signin"],
  ["POST", "/api/portal/sources/{id}/login", "signin"],
  ["POST", "/api/portal/sources/{id}/oauth/start", "signin"],
  ["POST", "/api/portal/sources/{source}/connections/{account_id}/revoke", "allow"],
  ["POST", "/api/portal/login/integration", "deny"],
  ["POST", "/api/portal/logout", "deny"],
  ["POST", "/api/portal/redeem", "deny"],

  // Users: only the visitor exists; no invites or changes.
  ["GET", "/api/users", "allow"],
  ["POST", "/api/users", "deny"],
  ["DELETE", "/api/users/{id}", "deny"],
  ["PATCH", "/api/users/{id}", "deny"],
  ["GET", "/api/users/{id}/avatar", "allow"],
  ["POST", "/api/users/{id}/claim-ticket", "deny"],
  ["POST", "/api/users/{id}/reset-password", "deny"],

  // The host as an OpenID provider, and the legacy unprefixed routes.
  ["GET", "/.well-known/openid-configuration", "deny"],
  ["GET", "/oidc/authorize", "deny"],
  ["POST", "/oidc/authorize", "deny"],
  ["POST", "/oidc/revoke", "deny"],
  ["POST", "/oidc/token", "deny"],
  ["GET", "/oidc/userinfo", "deny"],
  ["POST", "/acquire", "deny"],
  ["POST", "/scan", "deny"],
  ["GET", "/jobs", "deny"],
  ["POST", "/jobs/{id}/cancel", "deny"],
  ["GET", "/status", "deny"],
  ["POST", "/integrations/{id}/scan", "deny"],
]);

/**
 * First path segments that belong to the host's API rather than the web
 * app, compared in lower case. A path under one of these that no operation
 * matches is refused, so an undocumented route is never reachable.
 */
const API_ROOTS = new Set([".well-known", "acquire", "api", "integrations", "jobs", "oidc", "scan", "status"]);

/** Longest path the Worker classifies; anything longer is refused. */
export const MAX_PATH_CHARS = 2048;

/**
 * Splits a path template into segments, with `null` for a `{param}` segment.
 *
 * @param {string} template OpenAPI path template.
 * @returns {Array<string | null>} Literal segments, or null for parameters.
 */
function templateSegments(template) {
  return template
    .split("/")
    .slice(1)
    .map((s) => (s.startsWith("{") && s.endsWith("}") ? null : s));
}

/** The operation table compiled for matching: method, segments, verdict, template. */
const COMPILED = OPERATIONS.map(([method, template, verdict]) => ({
  method,
  segments: templateSegments(template),
  verdict,
  template,
}));

/**
 * Percent-decodes one path segment, refusing segments a server could read as
 * a different path: undecodable escapes, decoded separators, control
 * characters, and `.` / `..`.
 *
 * @param {string} raw Segment as it appears in the URL.
 * @returns {string | null} The decoded segment, or null when it is refused.
 */
export function decodeSegment(raw) {
  let s;
  try {
    s = decodeURIComponent(raw);
  } catch {
    return null;
  }
  if (s === "." || s === "..") return null;
  for (const ch of s) {
    const c = ch.codePointAt(0);
    if (ch === "/" || ch === "\\" || c < 0x20 || c === 0x7f) return null;
  }
  return s;
}

/**
 * Classifies a request by method and URL path.
 *
 * Paths are compared after percent-decoding each segment, so an encoded
 * spelling of a refused route (`/api/library/%61cquire`) is refused too.
 * `HEAD` is treated as `GET`. Paths outside the API roots are the web app's
 * own files and pages: `GET` and `HEAD` only.
 *
 * @param {string} method HTTP method, any case.
 * @param {string} pathname URL path, starting with `/`, still percent-encoded.
 * @returns {{verdict: Verdict, template: string | null}} The verdict, and the
 *   matched operation's template (null for app files and refused unknowns).
 */
export function classify(method, pathname) {
  const deny = { verdict: /** @type {Verdict} */ ("deny"), template: null };
  if (typeof method !== "string" || typeof pathname !== "string") return deny;
  if (!pathname.startsWith("/") || pathname.length > MAX_PATH_CHARS) return deny;
  let m = method.toUpperCase();
  if (m === "HEAD") m = "GET";
  const raw = pathname.split("/").slice(1);
  const segments = [];
  for (let i = 0; i < raw.length; i++) {
    // An empty segment is only allowed as the very last one (a trailing slash
    // or the root path itself); `//` inside a path is refused.
    if (raw[i] === "" && i !== raw.length - 1) return deny;
    const s = decodeSegment(raw[i]);
    if (s === null) return deny;
    segments.push(s);
  }
  for (const op of COMPILED) {
    if (op.method !== m || op.segments.length !== segments.length) continue;
    if (op.segments.every((lit, i) => (lit === null ? segments[i] !== "" : lit === segments[i]))) {
      return { verdict: op.verdict, template: op.template };
    }
  }
  if (API_ROOTS.has((segments[0] ?? "").toLowerCase())) return deny;
  // The web app: its index, assets and client-side routes.
  if (m === "GET") return { verdict: "allow", template: null };
  return deny;
}
