/**
 * Sign-in routes: `GET /auth/github`, `GET /auth/callback`,
 * `POST /auth/logout`.
 *
 * Flow: a random `state` and PKCE verifier go into a 10-minute `__Host-`
 * cookie; GitHub returns a code; the Worker checks `state`, trades code and
 * verifier for a user token, reads `GET /user`, revokes the token, checks
 * organization membership with its own read-only installation token, and
 * creates a session for active members.
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

/** Cookie holding `state.verifier` between the redirect and the callback. */
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

/**
 * Checks a sign-in callback's inputs: the query GitHub sent back and the
 * request's cookies. Sans-IO, so the fuzz tests drive the same code as the
 * route.
 *
 * Accepts only when `state` and the cookie's state are the same 32-byte
 * base64url token, the cookie carries a verifier of the same form, and
 * `code` is 1–`MAX_CODE_CHARS` base64url characters.
 *
 * @param {string} search Query string of the callback URL (untrusted).
 * @param {string} cookieHeader `Cookie` header (untrusted).
 * @returns {{code: string, verifier: string} | null} What to trade with
 *   GitHub, or null when the callback must be refused.
 */
export function parseCallback(search, cookieHeader) {
  const q = new URLSearchParams(String(search));
  const code = q.get("code") || "";
  const state = q.get("state") || "";
  const parts = (cookieValue(cookieHeader, OAUTH_COOKIE) || "").split(".");
  if (parts.length !== 2) return null;
  const [cookieState, verifier] = parts;
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
  return { code, verifier };
}

/**
 * `GET /auth/github`: redirects to GitHub with fresh state and PKCE.
 *
 * @param {Request} request Incoming request.
 * @param {import("./github.js").GitHubConfig} cfg Settings.
 * @returns {Promise<Response>} 303 to github.com.
 */
export async function startSignIn(request, cfg) {
  const state = randomToken(32);
  const verifier = randomToken(32);
  const url = authorizeUrl(cfg, {
    redirectUri: callbackUrl(request),
    state,
    challenge: await pkceChallenge(verifier),
  });
  return redirect(url, [
    hostCookie(OAUTH_COOKIE, `${state}.${verifier}`, { maxAge: OAUTH_COOKIE_SECONDS, sameSite: "Lax" }),
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
 * @returns {Promise<Response>} A page that moves on to `/dash`, or an error
 *   page.
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
  return htmlResponse(
    html`<!doctype html><html lang="en"><head><meta charset="utf-8">
      <meta http-equiv="refresh" content="0;url=/dash"><title>Signing in</title>
      <link rel="stylesheet" href="/static/dashboard.css"></head>
      <body class="center"><p>Signed in. <a href="/dash">Open the dashboard</a>.</p></body></html>`,
    200,
    {},
    [clear, session],
  );
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
