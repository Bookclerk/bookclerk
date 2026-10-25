/**
 * Sign-in routes: `GET /auth/github`, `GET /auth/callback`,
 * `POST /auth/logout`.
 *
 * Flow: a random `state`, PKCE verifier and the page to return to go into a
 * 10-minute `__Host-` cookie; GitHub returns a code; the Worker checks `state`, trades code and
 * verifier for a user token, reads `GET /user`, revokes the token, checks
 * organization membership with its own read-only installation token, and
 * creates a session for active members, then moves on to the page the
 * member asked for.
 */

import { logAccess } from "./db.js";
import {
  authorizeUrl,
  exchangeCode,
  fetchUser,
  membershipRole,
  revokeUserToken,
} from "./github.js";
import { html, htmlResponse, redirect } from "./html.js";
import { messagePage } from "./pages.js";
import {
  base64url,
  cookieValue,
  createSession,
  endSession,
  hostCookie,
  randomToken,
  sameOrigin,
} from "./session.js";

/**
 * Cookie holding `state.verifier[.next]` between the redirect and the
 * callback; `next` is the base64url of the page to return to.
 */
export const OAUTH_COOKIE = "__Host-bc_oauth";

/** Lifetime of the sign-in cookie, in seconds. */
const OAUTH_COOKIE_SECONDS = 600;

/** A base64url token of 32 random bytes. */
const TOKEN_RE = /^[A-Za-z0-9_-]{43}$/;

/**
 * Constant-time comparison of two strings.
 *
 * @param {string} a First.
 * @param {string} b Second.
 * @returns {boolean} True when equal.
 */
function equal(a, b) {
  if (a.length !== b.length) return false;
  let out = 0;
  for (let i = 0; i < a.length; i++) out |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return out === 0;
}

/**
 * PKCE S256 challenge for a verifier.
 *
 * @param {string} verifier Verifier.
 * @returns {Promise<string>} base64url SHA-256.
 */
export async function pkceChallenge(verifier) {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(verifier));
  return base64url(new Uint8Array(digest));
}

/**
 * The callback URL for a request's origin, so the same code serves the
 * custom domain and a fork's workers.dev host.
 *
 * @param {Request} request Incoming request.
 * @returns {string} `<origin>/auth/callback`.
 */
export function callbackUrl(request) {
  return `${new URL(request.url).origin}/auth/callback`;
}

/** Longest sign-in code accepted, in characters. */
export const MAX_CODE_CHARS = 128;

/** Page a sign-in lands on when it carries no valid return page. */
export const DEFAULT_NEXT = "/dash";

/** Longest return page accepted (path and query), in characters. */
export const MAX_NEXT_CHARS = 512;

/** Dashboard pages a sign-in may return to (GET pages only). */
const NEXT_PAGE = /^\/(?:dash|reports|issues|issues\/[1-9][0-9]{0,8}|reports\/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})$/;

/** A query string kept with the return page: URL-encoded filter text only. */
const NEXT_QUERY = /^\?[A-Za-z0-9._~%&=+/:@-]+$/;

/**
 * Validates a page to return to after sign-in (sans-IO; the value comes from
 * a query parameter or the sign-in cookie, both untrusted).
 *
 * Accepts only a dashboard page path (`/dash`, `/reports`, `/issues`,
 * `/issues/<number>`, `/reports/<uuid>`), so the result can never name another origin (no
 * scheme, no `//`, no backslash) or a sign-in, admin or WebSocket path. A
 * query of URL-encoded filter characters is kept; any other query is
 * dropped.
 *
 * @param {unknown} value Candidate path, such as `/reports?version=1.2`.
 * @returns {string | null} The path (and kept query), or null when the path
 *   is not a dashboard page or the value is longer than `MAX_NEXT_CHARS`.
 */
export function safeNext(value) {
  if (typeof value !== "string" || value.length > MAX_NEXT_CHARS) return null;
  const q = value.indexOf("?");
  const path = q < 0 ? value : value.slice(0, q);
  const query = q < 0 ? "" : value.slice(q);
  if (!NEXT_PAGE.test(path)) return null;
  return NEXT_QUERY.test(query) ? path + query : path;
}

/**
 * Base64url-decodes ASCII text (the `next` part of the sign-in cookie).
 *
 * @param {string} text Base64url without padding (untrusted).
 * @returns {string | null} The decoded text, or null when it is not
 *   base64url.
 */
function fromBase64url(text) {
  if (!/^[A-Za-z0-9_-]+$/.test(text)) return null;
  try {
    return atob(text.replace(/-/g, "+").replace(/_/g, "/"));
  } catch {
    return null;
  }
}

/**
 * Checks a sign-in callback's inputs: the query GitHub sent back and the
 * request's cookies. Sans-IO, so the fuzz tests drive the same code as the
 * route.
 *
 * Accepts only when `state` and the cookie's state are the same 32-byte
 * base64url token, the cookie carries a verifier of the same form, and
 * `code` is 1–`MAX_CODE_CHARS` base64url characters. The cookie's optional
 * third part is the return page; one that does not decode to a page
 * `safeNext` accepts falls back to `DEFAULT_NEXT` rather than refusing the
 * sign-in.
 *
 * @param {string} search Query string of the callback URL (untrusted).
 * @param {string} cookieHeader `Cookie` header (untrusted).
 * @returns {{code: string, verifier: string, next: string} | null} What to
 *   trade with GitHub and the page to land on, or null when the callback
 *   must be refused.
 */
export function parseCallback(search, cookieHeader) {
  const q = new URLSearchParams(String(search));
  const code = q.get("code") || "";
  const state = q.get("state") || "";
  const parts = (cookieValue(cookieHeader, OAUTH_COOKIE) || "").split(".");
  if (parts.length !== 2 && parts.length !== 3) return null;
  const [cookieState, verifier, nextPart] = parts;
  if (
    !TOKEN_RE.test(state) ||
    !TOKEN_RE.test(cookieState) ||
    !TOKEN_RE.test(verifier) ||
    !equal(state, cookieState) ||
    !/^[A-Za-z0-9_-]+$/.test(code) ||
    code.length > MAX_CODE_CHARS
  ) {
    return null;
  }
  const next = (nextPart === undefined ? null : safeNext(fromBase64url(nextPart))) ?? DEFAULT_NEXT;
  return { code, verifier, next };
}

/**
 * `GET /auth/github`: redirects to GitHub with fresh state and PKCE. A
 * `next` query parameter that `safeNext` accepts rides along in the sign-in
 * cookie, so the callback returns the member to the page they opened.
 *
 * @param {Request} request Incoming request.
 * @param {import("./github.js").GitHubConfig} cfg Settings.
 * @returns {Promise<Response>} 303 to github.com.
 */
export async function startSignIn(request, cfg) {
  const state = randomToken(32);
  const verifier = randomToken(32);
  const next = safeNext(new URL(request.url).searchParams.get("next"));
  const value = next ? `${state}.${verifier}.${base64url(new TextEncoder().encode(next))}` : `${state}.${verifier}`;
  const url = authorizeUrl(cfg, {
    redirectUri: callbackUrl(request),
    state,
    challenge: await pkceChallenge(verifier),
  });
  return redirect(url, [
    hostCookie(OAUTH_COOKIE, value, { maxAge: OAUTH_COOKIE_SECONDS, sameSite: "Lax" }),
  ]);
}

/**
 * The page for a sign-in GitHub refused, naming the step and GitHub's short
 * reason so an owner can tell a wrong client secret from an expired code.
 * The reason is logged too.
 *
 * @param {string} step Which call failed: "code exchange" or "account lookup".
 * @param {string} reason GitHub's OAuth error code or an HTTP status, from
 *   `exchangeCode` / `fetchUser` (never credentials or response text).
 * @param {string} clear Set-Cookie value that clears the sign-in cookie.
 * @returns {Response} A 400 error page.
 */
function signInFailed(step, reason, clear) {
  console.error(`sign-in ${step} failed: ${reason}`);
  return htmlResponse(
    messagePage(
      "Sign-in failed",
      html`<p>GitHub refused the sign-in at the ${step} step. Start again from the sign-in page.</p>
        <p class="notice">GitHub's reason: <code>${reason}</code></p>`,
    ),
    400,
    {},
    [clear],
  );
}

/**
 * `GET /auth/callback`: finishes sign-in.
 *
 * @param {Request} request Incoming request.
 * @param {{DASHBOARD_DB: D1Database}} env Worker bindings.
 * @param {import("./github.js").GitHubConfig} cfg Settings.
 * @param {{waitUntil(p: Promise<unknown>): void}} ctx Execution context.
 * @param {number} now Unix time in milliseconds.
 * @returns {Promise<Response>} A page that moves on to the page the sign-in
 *   started from (or `/dash`), or an error page.
 */
export async function finishSignIn(request, env, cfg, ctx, now) {
  const clear = hostCookie(OAUTH_COOKIE, "", { maxAge: 0, sameSite: "Lax" });
  const parsed = parseCallback(new URL(request.url).search, request.headers.get("cookie") || "");
  if (!parsed) {
    return htmlResponse(
      messagePage("Sign-in expired", html`<p>The sign-in link was used already or took too long. Start again from the sign-in page.</p>`),
      400,
      {},
      [clear],
    );
  }

  let user;
  let role;
  try {
    const { code, verifier } = parsed;
    const exchanged = await exchangeCode(cfg, { code, redirectUri: callbackUrl(request), verifier });
    if ("error" in exchanged) return signInFailed("code exchange", exchanged.error, clear);
    let got;
    try {
      got = await fetchUser(exchanged.token);
    } finally {
      ctx.waitUntil(revokeUserToken(cfg, exchanged.token));
    }
    if ("error" in got) return signInFailed("account lookup", got.error, clear);
    user = got.user;
    role = await membershipRole(cfg, user, now);
  } catch (err) {
    console.error(`sign-in GitHub call failed: ${err instanceof Error ? err.message : String(err)}`);
    return htmlResponse(
      messagePage("GitHub could not be reached", html`<p>Bookclerk could not check your organization membership just now. Try again in a minute.</p>`),
      502,
      {},
      [clear],
    );
  }

  if (!role) {
    return htmlResponse(
      messagePage(
        "This dashboard is for organization members",
        html`<p>You signed in as <b>${user.login}</b>, who is not an active member of the ${cfg.owner} organization.</p>
          <p class="notice">Diagnostics issues are public on the ${cfg.owner}/${cfg.repo} repository. If you were just invited, accept the invitation on GitHub, then sign in again.</p>`,
      ),
      403,
      {},
      [clear],
    );
  }

  const who = { userId: user.id, login: user.login, role };
  const session = await createSession(env.DASHBOARD_DB, who, now);
  await logAccess(env.DASHBOARD_DB, who, "sign-in", null, now);
  // A 200 page that moves on by itself: the SameSite=Strict cookie set here
  // is not sent on a redirect chain that began on github.com, but it is on a
  // navigation this page starts.
  return htmlResponse(sameSiteHop("Signing in", parsed.next, "Signed in."), 200, {}, [clear, session]);
}

/**
 * A page that moves on to `target` by itself (meta refresh). The browser
 * counts the navigation it starts as same-site, so it carries the
 * SameSite=Strict session cookie that a navigation from another site (a
 * GitHub issue, chat or email link, or the sign-in redirect chain) leaves
 * out.
 *
 * @param {string} title Page title.
 * @param {string} target Same-origin path from `safeNext`, a dashboard
 *   path, or `/` with such a `next` (escaped here all the same).
 * @param {string} lead Text before the fallback link.
 * @returns {import("./html.js").SafeHtml} The document.
 */
export function sameSiteHop(title, target, lead) {
  return html`<!doctype html><html lang="en"><head><meta charset="utf-8">
      <meta http-equiv="refresh" content="0;url=${target}"><title>${title}</title>
      <link rel="stylesheet" href="/static/dashboard.css"></head>
      <body class="center"><p>${lead} <a href="${target}">Open the dashboard</a>.</p></body></html>`;
}

/**
 * `POST /auth/logout`: deletes the session.
 *
 * @param {Request} request Incoming request.
 * @param {{DASHBOARD_DB: D1Database}} env Worker bindings.
 * @returns {Promise<Response>} 303 to the sign-in page, or 403 when the
 *   request did not come from this origin.
 */
export async function signOut(request, env) {
  if (!sameOrigin(request)) {
    return htmlResponse(messagePage("Request refused", html`<p>Sign out from the dashboard's own button.</p>`), 403);
  }
  return redirect("/", [await endSession(env.DASHBOARD_DB, request)]);
}
