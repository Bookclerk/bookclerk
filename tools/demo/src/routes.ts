// Which bookclerkd requests a demo visitor may make. Sans-IO: the Worker
// calls classify() before anything reaches the visitor's container.
//
// The list is default-deny and covers every operation in
// crates/bookclerkd/openapi.json; test/routes.test.ts fails when the API
// gains an operation this table does not name, so a new route cannot reach
// the demo unreviewed. Acquire, downloads, node and library links, plugin
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
 */
export type Verdict = "allow" | "signin" | "deny";

/**
 * The classified operations: method, OpenAPI path template, verdict.
 */
export const OPERATIONS: ReadonlyArray<readonly [string, string, Verdict]> = Object.freeze([
  // Health and the API description.
  ["GET", "/health", "allow"],
  ["GET", "/api/openapi.json", "allow"],
  ["GET", "/api/openapi-3.0.json", "allow"],
  ["GET", "/api/status", "allow"],
  // Read-only report of the formats this build decodes, reads and writes.
  ["GET", "/api/status/media", "allow"],

  // Sign-in and the visitor's own account. The demo signs the visitor in;
  // signing out, other sign-in methods and credential changes are refused.
  ["GET", "/api/auth/me", "allow"],
  ["GET", "/api/auth/signin", "allow"],
  ["GET", "/api/auth/sessions", "allow"],
  ["DELETE", "/api/auth/sessions/{id}", "deny"],
  ["POST", "/api/auth/bootstrap", "deny"],
  ["POST", "/api/auth/login", "deny"],
  ["POST", "/api/auth/logout", "deny"],
  // Owner tools: the visitor elevates with the password the banner shows
  // (VISITOR_PASSWORD) and may then act as a seeded user. Elevated, the
  // visitor still reaches only the routes this table allows.
  ["POST", "/api/auth/elevate", "allow"],
  ["DELETE", "/api/auth/elevate", "allow"],
  ["POST", "/api/auth/impersonate", "allow"],
  ["DELETE", "/api/auth/impersonate", "allow"],
  ["GET", "/api/auth/branch-consent", "deny"],
  ["POST", "/api/auth/branch-consent", "deny"],
  ["POST", "/api/auth/branch-consent/step-up", "deny"],
  ["GET", "/api/auth/clerk-consent", "deny"],
  ["POST", "/api/auth/clerk-consent", "deny"],
  // Player apps: making player passwords and approving an app's browser
  // sign-in are refused, so no app signs in to a demo.
  ["GET", "/api/auth/player-consent", "deny"],
  ["POST", "/api/auth/player-consent", "deny"],
  ["GET", "/api/auth/player-passwords", "deny"],
  ["POST", "/api/auth/player-passwords", "deny"],
  ["DELETE", "/api/auth/player-passwords/{id}", "deny"],
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
  ["GET", "/api/auth/profile/catalog-listing", "allow"],
  ["PUT", "/api/auth/profile/catalog-listing", "deny"],
  ["POST", "/api/auth/profile/email/verification", "deny"],
  ["GET", "/api/auth/totp", "allow"],
  ["DELETE", "/api/auth/totp", "deny"],
  ["POST", "/api/auth/totp/enroll/begin", "deny"],
  ["POST", "/api/auth/totp/enroll/finish", "deny"],
  ["POST", "/api/auth/totp/login", "deny"],
  ["GET", "/api/auth/tray-handoff", "deny"],
  ["POST", "/api/auth/tray-handoff/prepare", "deny"],
  ["POST", "/api/auth/verify-email", "deny"],

  // Libraries, branches, shared branches and nodes: read-only views, no links.
  ["GET", "/api/library-status", "allow"],
  ["GET", "/api/config/branches", "allow"],
  ["PUT", "/api/config/branches", "deny"],
  ["POST", "/api/config/branches/titles/remove", "deny"],
  ["PUT", "/api/config/branches/members-books", "deny"],
  // Asking a branch for a book acquires it there: never from the demo.
  ["POST", "/api/branch-requests", "deny"],
  // Asking the library to download a book acquires it: never from the demo.
  ["POST", "/api/download-requests", "deny"],
  ["POST", "/api/clerk/v1/token", "deny"],
  ["GET", "/api/clerk/v1/connect", "deny"],
  // A copy's files are a branch's or clerk's to download: never the demo's.
  ["GET", "/api/copies/v1/{copy_id}/files/{index}", "deny"],
  ["GET", "/api/config/shared-branches", "allow"],
  ["GET", "/api/config/clerk", "allow"],
  ["POST", "/api/config/clerk/callback", "deny"],
  ["POST", "/api/config/clerk/inspect", "deny"],
  ["POST", "/api/config/clerk/request", "deny"],
  ["POST", "/api/config/clerk/sign-out", "deny"],
  ["POST", "/api/config/clerk/start", "deny"],
  ["GET", "/api/config/library-consents", "allow"],
  ["POST", "/api/config/library-consents/approve", "deny"],
  ["POST", "/api/config/library-consents/revoke", "deny"],
  ["POST", "/api/config/libraries/{library_id}/jobs", "deny"],
  ["POST", "/api/config/libraries/{library_id}/scan", "deny"],
  ["GET", "/api/config/branch-enrollment", "allow"],
  ["PUT", "/api/config/branch-enrollment", "deny"],
  // The library key: the status may show; a recovery copy carries the key.
  ["GET", "/api/config/master-key", "allow"],
  ["POST", "/api/config/master-key/recovery-copy", "deny"],
  ["PUT", "/api/config/master-key/recovery-copy/state", "deny"],
  ["GET", "/api/config/join", "allow"],
  ["POST", "/api/config/join/callback", "deny"],
  ["POST", "/api/config/join/connections", "deny"],
  ["POST", "/api/config/join/connections/disconnect", "deny"],
  ["POST", "/api/config/join/inspect", "deny"],
  ["POST", "/api/config/join/invite", "deny"],
  ["POST", "/api/config/join/leave", "deny"],
  ["GET", "/api/config/join/sharing", "allow"],
  ["POST", "/api/config/join/sharing", "deny"],
  ["POST", "/api/config/join/capacity", "deny"],
  ["POST", "/api/config/join/start", "deny"],
  ["GET", "/api/config/join/window", "allow"],
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
  ["GET", "/api/config/ingress", "allow"],
  ["POST", "/api/config/ingress", "deny"],
  ["POST", "/api/config/reload", "deny"],
  ["POST", "/api/database/migrate", "deny"],
  ["GET", "/api/settings", "allow"],
  ["PATCH", "/api/settings", "deny"],
  // The visitor's own copy's sent diagnostics reports: read-only. Reading
  // one back asks the collector for that report with the copy's own access
  // code, which shows only what the copy sent. Deleting it would remove the
  // collector's copy for everyone working on the demo's bug: refused.
  ["GET", "/api/diagnostics/reports", "allow"],
  ["GET", "/api/diagnostics/reports/{id}", "allow"],
  ["DELETE", "/api/diagnostics/reports/{id}", "deny"],

  // Plugins: the image's own storefronts only; no installs or changes.
  ["GET", "/api/config/plugin-instances", "allow"],
  ["POST", "/api/config/plugin-instances", "deny"],
  ["DELETE", "/api/config/plugin-instances/{id}", "deny"],
  ["GET", "/api/config/plugin-instances/{id}/config", "allow"],
  ["PUT", "/api/config/plugin-instances/{id}/config", "deny"],
  ["PUT", "/api/config/plugin-instances/{id}/deployments/{node_id}", "deny"],
  ["GET", "/api/config/plugin-instances/{id}/filter", "allow"],
  ["PUT", "/api/config/plugin-instances/{id}/filter", "deny"],
  ["GET", "/api/config/plugin-instances/{id}/intake", "allow"],
  ["PUT", "/api/config/plugin-instances/{id}/intake", "deny"],
  ["DELETE", "/api/config/plugin-instances/{id}/intake", "deny"],
  // Guided setup calls out to the provider with a pasted token.
  ["GET", "/api/config/plugin-instances/{id}/provisioning", "allow"],
  ["POST", "/api/config/plugin-instances/{id}/provision", "deny"],
  ["POST", "/api/config/plugin-instances/{id}/deprovision", "deny"],
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
  ["POST", "/api/discover/refresh", "allow"],
  ["GET", "/api/discover/search", "allow"],
  ["GET", "/api/discover/status", "allow"],
  ["POST", "/api/discover/sync-listening", "allow"],
  // A read: places one book on the visitor's taste radar.
  ["POST", "/api/discover/taste/radar", "allow"],
  ["POST", "/api/discover/title-meta", "allow"],
  ["POST", "/api/discover/title-meta/batch", "allow"],
  // A store's public sample for a title: answers a signed same-origin path.
  ["POST", "/api/discover/title-sample", "allow"],
  ["POST", "/api/discover/title-reviews", "allow"],

  // Events: readable; deliveries are never retried or resumed.
  ["GET", "/api/events", "allow"],
  ["GET", "/api/events/deliveries", "allow"],
  ["POST", "/api/events/deliveries/{id}/acknowledge", "deny"],
  ["POST", "/api/events/deliveries/{id}/cancel", "deny"],
  ["POST", "/api/events/deliveries/{id}/resume", "deny"],
  ["POST", "/api/events/deliveries/{id}/retry", "deny"],
  // Plugin webhooks: the demo hands out no public URL, and a visitor must
  // not feed a guest deliveries it never asked for.
  ["POST", "/hooks/{token}/{name}", "deny"],

  // Jobs and the library: scan the visitor's accounts, never acquire or
  // convert (a convert writes a new copy to storage, like an acquire).
  ["GET", "/api/jobs", "allow"],
  ["POST", "/api/jobs/{id}/cancel", "allow"],
  ["POST", "/api/library/acquire", "deny"],
  ["POST", "/api/library/convert", "deny"],
  ["GET", "/api/library/works", "allow"],
  // Search by meaning and "more like this": reads over the sample
  // library's baked vectors, in the visitor's own container.
  ["GET", "/api/library/works/meaning", "allow"],
  ["GET", "/api/library/works/{id}", "allow"],
  ["GET", "/api/library/works/{id}/similar", "allow"],
  ["PUT", "/api/library/works/{id}/state", "allow"],
  ["GET", "/api/library/works/{id}/tags", "allow"],
  ["PUT", "/api/library/works/{id}/tags", "deny"],
  ["GET", "/api/library/collections", "allow"],
  ["PUT", "/api/library/collections", "deny"],
  ["GET", "/api/library/works/facets/{facet}", "allow"],
  ["GET", "/api/library/editions/{id}/cover", "allow"],
  // Chapters: an Administrator's edits stay in the visitor's own library.
  ["GET", "/api/library/editions/{id}/chapters", "allow"],
  ["PUT", "/api/library/editions/{id}/chapters", "allow"],
  ["DELETE", "/api/library/editions/{id}/chapters", "allow"],
  ["GET", "/api/library/editions/{id}/chapters/claims", "allow"],
  ["PUT", "/api/library/editions/{id}/chapters/pin", "allow"],
  ["DELETE", "/api/library/editions/{id}/chapters/pin", "allow"],
  // Re-align reads a stored copy and writes claims to the visitor's own
  // library; the image has no output plugin, so the job finds no copy.
  ["POST", "/api/library/editions/{id}/chapters/realign", "allow"],
  ["GET", "/api/library/conflicts", "allow"],
  ["PUT", "/api/library/listings/{id}/pin", "allow"],
  ["DELETE", "/api/library/listings/{id}/pin", "allow"],
  ["POST", "/api/library/works/{id}/split", "allow"],
  // Playback: a visitor's own listening state. The demo never acquires, so
  // no edition has audio to stream.
  ["GET", "/api/library/editions/{id}/playback", "allow"],
  // A few minutes cut from a held edition; records no listening.
  ["GET", "/api/library/editions/{id}/sample", "allow"],
  ["GET", "/api/library/editions/{id}/bookmarks", "allow"],
  ["POST", "/api/library/editions/{id}/bookmarks", "allow"],
  ["PUT", "/api/playback/bookmarks/{id}", "allow"],
  ["DELETE", "/api/playback/bookmarks/{id}", "allow"],
  ["GET", "/api/playback/progress", "allow"],
  ["PUT", "/api/playback/progress/{edition_id}", "allow"],
  ["DELETE", "/api/playback/progress/{edition_id}", "allow"],
  ["POST", "/api/playback/sessions", "allow"],
  ["POST", "/api/playback/sessions/{id}/sync", "allow"],
  ["POST", "/api/playback/sessions/{id}/close", "allow"],
  ["GET", "/api/playback/sessions/{id}/tracks/{index}", "allow"],

  // Notifications. Visitors may read settings, the catalog, the delivery
  // log and their inbox, and change routing choices. Anything that adds,
  // tests or resumes a destination is refused: a destination is where the
  // library would send messages, and the demo must not become a way to
  // send them anywhere.
  ["GET", "/api/notifications/catalog", "allow"],
  ["GET", "/api/notifications/channels", "allow"],
  ["POST", "/api/notifications/channels", "deny"],
  ["PATCH", "/api/notifications/channels/{id}", "deny"],
  ["DELETE", "/api/notifications/channels/{id}", "deny"],
  ["POST", "/api/notifications/channels/{id}/resume", "deny"],
  ["POST", "/api/notifications/channels/{id}/test", "deny"],
  ["POST", "/api/notifications/notifiers/{id}/deliverability", "deny"],
  // Account mail goes to real addresses, so the demo may only read it.
  ["GET", "/api/notifications/account-mail", "allow"],
  ["PUT", "/api/notifications/account-mail", "deny"],
  ["POST", "/api/notifications/account-mail/test", "deny"],
  ["GET", "/api/notifications/routes", "allow"],
  ["PUT", "/api/notifications/routes/{type}", "allow"],
  ["GET", "/api/notifications/offers", "allow"],
  ["PUT", "/api/notifications/offers/{type}", "allow"],
  ["GET", "/api/notifications/deliveries", "allow"],
  ["GET", "/api/me/notifications", "allow"],
  ["PUT", "/api/me/notifications/choices/{type}", "allow"],
  ["DELETE", "/api/me/notifications/choices/{type}", "allow"],
  ["POST", "/api/me/notifications/destinations", "deny"],
  ["DELETE", "/api/me/notifications/destinations/{id}", "deny"],
  ["POST", "/api/me/notifications/destinations/{id}/resume", "deny"],
  ["GET", "/api/me/notifications/inbox", "allow"],
  ["POST", "/api/me/notifications/inbox/read", "allow"],
  // Same-origin cover images; the token only names URLs the API handed out.
  ["GET", "/api/covers/{token}/{size}", "allow"],
  // Same-origin store samples; the token only names URLs the API handed out.
  ["GET", "/api/samples/{token}", "allow"],
  // Same-origin Gravatars (fixed host, validated hash) and store icons (from
  // a deployed plugin's brand), through the same cover service.
  ["GET", "/api/avatars/gravatar/{hash}", "allow"],
  // Signed profile pictures and remote logos, like a cover token.
  ["GET", "/api/images/{token}", "allow"],
  ["GET", "/api/stores/{id}/icon", "allow"],
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

  // Uploads: readable, never started; the demo stores no files.
  ["GET", "/api/uploads", "allow"],
  ["POST", "/api/uploads", "deny"],
  ["GET", "/api/uploads/review", "allow"],
  ["GET", "/api/uploads/{id}", "allow"],
  ["DELETE", "/api/uploads/{id}", "deny"],
  ["POST", "/api/uploads/{id}/commit", "deny"],
  ["PATCH", "/api/uploads/{id}/files/{member_no}", "deny"],

  // Store accounts. Every way of starting a store sign-in is counted.
  ["GET", "/api/portal/me", "allow"],
  ["GET", "/api/portal/sources", "allow"],
  ["GET", "/api/portal/connections", "allow"],
  // Browser (OAuth) sign-in is refused for now: the store's login page is
  // served from a callback port inside the VM that the Worker does not
  // bridge yet, so the link bookclerkd returns would point at the
  // visitor's own machine. Password sign-in works. Tracked in #415.
  ["POST", "/api/portal/audible/start", "deny"],
  ["POST", "/api/portal/libro/login", "signin"],
  ["POST", "/api/portal/sources/{id}/login", "signin"],
  ["POST", "/api/portal/sources/{id}/oauth/start", "deny"],
  ["POST", "/api/portal/sources/{source}/connections/{account_id}/revoke", "allow"],
  // A connection's download pace only changes how fast automatic acquires
  // run, and the demo refuses acquire work anyway.
  ["POST", "/api/portal/sources/{source}/connections/{account_id}/pace", "allow"],
  ["POST", "/api/portal/login/integration", "deny"],
  ["POST", "/api/portal/logout", "deny"],
  ["POST", "/api/portal/redeem", "deny"],

  // Users: only the visitor exists; no invites or changes.
  ["GET", "/api/users", "allow"],
  // Member controls: readable, never changed.
  ["GET", "/api/users/default-controls", "allow"],
  ["PATCH", "/api/users/default-controls", "deny"],
  ["GET", "/api/users/{id}/controls", "allow"],
  ["PATCH", "/api/users/{id}/controls", "deny"],
  // Administrator switches: readable, never changed.
  ["GET", "/api/users/default-admin-controls", "allow"],
  ["PATCH", "/api/users/default-admin-controls", "deny"],
  ["GET", "/api/users/{id}/admin-controls", "allow"],
  ["PATCH", "/api/users/{id}/admin-controls", "deny"],
  ["POST", "/api/users", "deny"],
  ["DELETE", "/api/users/{id}", "deny"],
  ["PATCH", "/api/users/{id}", "deny"],
  ["GET", "/api/users/{id}/avatar", "allow"],
  ["POST", "/api/users/{id}/claim-ticket", "deny"],
  ["POST", "/api/users/{id}/reset-password", "deny"],
  ["POST", "/api/users/{id}/invite-email", "deny"],
  // Patron scopes: readable, never changed.
  ["GET", "/api/users/default-scope", "allow"],
  ["PUT", "/api/users/default-scope", "deny"],
  ["GET", "/api/users/{id}/scope", "allow"],
  ["PUT", "/api/users/{id}/scope", "deny"],

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

  // The API player apps call: no app signs in to a demo, so none of it is
  // reachable.
  ["GET", "/player/status", "deny"],
  ["GET", "/player/ping", "deny"],
  ["GET", "/player/healthcheck", "deny"],
  ["POST", "/player/login", "deny"],
  ["POST", "/player/auth/refresh", "deny"],
  ["POST", "/player/logout", "deny"],
  ["GET", "/player/auth/openid", "deny"],
  ["GET", "/player/auth/openid/mobile-redirect", "deny"],
  ["GET", "/player/auth/openid/callback", "deny"],
  ["GET", "/player/socket.io/", "deny"],
  ["GET", "/player/socket.io", "deny"],
  ["POST", "/player/api/authorize", "deny"],
  ["GET", "/player/api/me", "deny"],
  ["GET", "/player/api/libraries", "deny"],
  ["GET", "/player/api/libraries/{id}", "deny"],
  ["GET", "/player/api/libraries/{id}/items", "deny"],
  ["GET", "/player/api/libraries/{id}/series", "deny"],
  ["GET", "/player/api/libraries/{id}/authors", "deny"],
  ["GET", "/player/api/libraries/{id}/narrators", "deny"],
  ["GET", "/player/api/libraries/{id}/collections", "deny"],
  ["GET", "/player/api/libraries/{id}/playlists", "deny"],
  ["GET", "/player/api/libraries/{id}/personalized", "deny"],
  ["GET", "/player/api/libraries/{id}/search", "deny"],
  ["GET", "/player/api/libraries/{id}/stats", "deny"],
  ["GET", "/player/api/libraries/{id}/filterdata", "deny"],
  ["GET", "/player/api/series/{id}", "deny"],
  ["GET", "/player/api/authors/{id}", "deny"],
  ["GET", "/player/api/items/{id}", "deny"],
  ["POST", "/player/api/items/batch/get", "deny"],
  ["GET", "/player/api/items/{id}/cover", "deny"],
  ["POST", "/player/api/items/{id}/play", "deny"],
  ["GET", "/player/api/items/{id}/file/{ino}", "deny"],
  ["GET", "/player/api/items/{id}/file/{ino}/download", "deny"],
  ["POST", "/player/api/session/{id}/sync", "deny"],
  ["POST", "/player/api/session/{id}/close", "deny"],
  ["POST", "/player/api/session/local", "deny"],
  ["POST", "/player/api/session/local-all", "deny"],
  ["GET", "/player/api/me/progress/{id}", "deny"],
  ["DELETE", "/player/api/me/progress/{id}", "deny"],
  ["PATCH", "/player/api/me/progress/{id}", "deny"],
  ["PATCH", "/player/api/me/progress/batch/update", "deny"],
  ["GET", "/player/api/me/items-in-progress", "deny"],
  ["GET", "/player/api/me/listening-sessions", "deny"],
  ["GET", "/player/api/me/listening-stats", "deny"],
  ["POST", "/player/api/me/item/{id}/bookmark", "deny"],
  ["PATCH", "/player/api/me/item/{id}/bookmark", "deny"],
  ["DELETE", "/player/api/me/item/{id}/bookmark/{time}", "deny"],
]);

/**
 * First path segments that belong to the host's API rather than the web
 * app, compared in lower case. A path under one of these that no operation
 * matches is refused, so an undocumented route is never reachable.
 */
const API_ROOTS = new Set([
  ".well-known",
  "acquire",
  "api",
  "integrations",
  "jobs",
  "oidc",
  "player",
  "scan",
  "status",
]);

/**
 * Longest path the Worker classifies; anything longer is refused. A signed
 * image path for the longest URL bookclerkd signs (2048 bytes) is about
 * 2,780 characters, so this leaves room for it.
 */
export const MAX_PATH_CHARS = 3072;

/**
 * Splits a path template into segments, with `null` for a `{param}` segment.
 *
 * @param template - OpenAPI path template.
 * @returns Literal segments, or null for parameters.
 */
function templateSegments(template: string): Array<string | null> {
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
 * @param raw - Segment as it appears in the URL.
 * @returns The decoded segment, or null when it is refused.
 */
export function decodeSegment(raw: string): string | null {
  let s;
  try {
    s = decodeURIComponent(raw);
  } catch {
    return null;
  }
  if (s === "." || s === "..") return null;
  for (const ch of s) {
    // A character from a string iterator always has a code point.
    const c = ch.codePointAt(0) ?? 0;
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
 * @param method - HTTP method, any case.
 * @param pathname - URL path, starting with `/`, still percent-encoded.
 * @returns The verdict, and the
 *   matched operation's template (null for app files and refused unknowns).
 */
export function classify(
  method: string,
  pathname: string,
): { verdict: Verdict; template: string | null } {
  const deny: { verdict: Verdict; template: string | null } = { verdict: "deny", template: null };
  if (typeof method !== "string" || typeof pathname !== "string") return deny;
  if (!pathname.startsWith("/") || pathname.length > MAX_PATH_CHARS) return deny;
  let m = method.toUpperCase();
  if (m === "HEAD") m = "GET";
  const raw = pathname.split("/").slice(1);
  const segments: string[] = [];
  for (const [i, part] of raw.entries()) {
    // An empty segment is only allowed as the very last one (a trailing slash
    // or the root path itself); `//` inside a path is refused.
    if (part === "" && i !== raw.length - 1) return deny;
    const s = decodeSegment(part);
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
