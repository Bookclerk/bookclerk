// The demo Worker's request handling, apart from the container class so the
// tests can run it under Node. worker.js wires it to Cloudflare.
//
// Every visitor request goes through here: Worker endpoints under /_demo/,
// the launch page for a browser without a session, and otherwise the route
// check (routes.js) before the request reaches the visitor's own container.

import { classify } from "./routes.js";
import { admit, countSignin, finish, limitsFrom, overdue, prune, utcDay } from "./limits.js";
import {
  cookiesForContainer,
  PORTAL_COOKIE,
  readSessionToken,
  rewriteSetCookie,
  SESSION_COOKIE,
  siteverifyPassed,
} from "./inputs.js";
import { endedPage, htmlResponse, launchPage, REFUSALS, SECURITY_HEADERS, waitPage } from "./pages.js";

/** Turnstile's server-side check. */
const SITEVERIFY_URL = "https://challenges.cloudflare.com/turnstile/v0/siteverify";

/** The widget action the launch page sets and Siteverify must echo. */
export const TURNSTILE_ACTION = "demo_start";

/** Largest launch form body read (a Turnstile token is about 2 KB). */
const MAX_FORM_BYTES = 16 * 1024;

/** Largest web app HTML document the banner is added to. */
const MAX_BANNER_HTML_BYTES = 1024 * 1024;

/** Sessions the sweep ends per run. */
const SWEEP_BATCH = 20;

/** How far past its hard stop a session must be before the sweep ends it. */
const SWEEP_GRACE_SECONDS = 120;

/**
 * What the handler needs from outside, injectable for tests.
 *
 * @typedef {object} Deps
 * @property {typeof fetch} fetch Outbound fetch (Siteverify).
 * @property {() => number} now Current time in milliseconds.
 */

/** @type {Deps} */
const DEFAULT_DEPS = { fetch: (...a) => fetch(...a), now: () => Date.now() };

/** The imported HMAC key, cached per key text (one Worker serves one key). */
let hmacKey = { text: "", key: /** @type {Promise<CryptoKey> | null} */ (null) };

/**
 * Lowercase hex HMAC-SHA256.
 *
 * @param {string} key Key text.
 * @param {string} message Message text.
 * @returns {Promise<string>} The MAC in hex.
 */
async function hmacHex(key, message) {
  if (hmacKey.key === null || hmacKey.text !== key) {
    const imported = crypto.subtle.importKey("raw", new TextEncoder().encode(key), { name: "HMAC", hash: "SHA-256" }, false, [
      "sign",
    ]);
    hmacKey = { text: key, key: imported };
  }
  const k = await hmacKey.key;
  const mac = new Uint8Array(await crypto.subtle.sign("HMAC", k, new TextEncoder().encode(message)));
  return Array.from(mac, (x) => x.toString(16).padStart(2, "0")).join("");
}

/**
 * A new session token: 16 random bytes, unpadded base64url.
 *
 * @returns {string} The token.
 */
function newToken() {
  const b = new Uint8Array(16);
  crypto.getRandomValues(b);
  return btoa(String.fromCharCode(...b)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/**
 * The session id for a token. The container and the D1 row are named by it,
 * so knowing an id does not give anyone the cookie, and a guessed cookie
 * names nobody's container.
 *
 * @param {Record<string, any>} env Worker env.
 * @param {string} token Session token.
 * @returns {Promise<string>} The id (hex).
 */
function sessionId(env, token) {
  return hmacHex(env.SESSION_KEY, `session|${token}`);
}

/**
 * The address hash for rate limits, salted with the UTC day so rows cannot
 * be linked across days or reversed by trying addresses without the key.
 *
 * @param {Record<string, any>} env Worker env.
 * @param {string} ip Client address.
 * @param {number} nowSeconds Current Unix seconds.
 * @returns {Promise<string>} The hash (hex, 32 characters).
 */
async function ipHash(env, ip, nowSeconds) {
  return (await hmacHex(env.SESSION_KEY, `ip|${utcDay(nowSeconds)}|${ip}`)).slice(0, 32);
}

/**
 * The visitor's session object.
 *
 * @param {Record<string, any>} env Worker env.
 * @param {string} id Session id.
 * @returns {any} The Durable Object stub.
 */
function sessionStub(env, id) {
  return env.SESSIONS.get(env.SESSIONS.idFromName(id));
}

/**
 * Whether the Worker has everything it needs to admit visitors.
 *
 * @param {Record<string, any>} env Worker env.
 * @returns {boolean} True when the session key and Turnstile keys are set.
 */
function configured(env) {
  return Boolean(env.SESSION_KEY && env.TURNSTILE_SITE_KEY && env.TURNSTILE_SECRET);
}

/**
 * The website to link back to.
 *
 * @param {Record<string, any>} env Worker env.
 * @returns {string} `SITE_URL` when it is an https URL; otherwise empty.
 */
function siteUrl(env) {
  return typeof env.SITE_URL === "string" && /^https:\/\/[^\s"<>]+$/.test(env.SITE_URL) ? env.SITE_URL : "";
}

/**
 * A JSON response with the security headers.
 *
 * @param {unknown} body Body.
 * @param {number} status HTTP status.
 * @param {Record<string, string>} [extra] More headers.
 * @returns {Response} The response.
 */
function json(body, status, extra = {}) {
  return Response.json(body, { status, headers: { "Cache-Control": "no-store", ...SECURITY_HEADERS, ...extra } });
}

/**
 * A redirect, optionally setting cookies.
 *
 * @param {string} location Target path.
 * @param {string[]} [cookies] `Set-Cookie` lines.
 * @returns {Response} 303 See Other.
 */
function redirect(location, cookies = []) {
  const headers = new Headers({ Location: location, "Cache-Control": "no-store" });
  for (const c of cookies) headers.append("Set-Cookie", c);
  return new Response(null, { status: 303, headers });
}

/** Clears the Worker's session cookie and bookclerkd's portal session. */
const CLEAR_COOKIES = [
  `${SESSION_COOKIE}=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0`,
  `${PORTAL_COOKIE}=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0`,
];

/**
 * Whether a request is a browser loading a page (rather than the app's API calls).
 *
 * @param {Request} request Request.
 * @returns {boolean} True for a GET that accepts HTML.
 */
function isNavigation(request) {
  return request.method === "GET" && (request.headers.get("Accept") ?? "").includes("text/html");
}

/**
 * Whether a state-changing request came from a page on this host. The
 * Worker refuses cross-site writes itself, before any reach a container;
 * bookclerkd checks `Origin` again against its `public_origin`, which is
 * this origin.
 *
 * @param {Request} request Request.
 * @param {URL} url The request's URL.
 * @returns {boolean} True when `Origin` (or, without it, `Referer`) is this origin.
 */
function sameOrigin(request, url) {
  const origin = request.headers.get("Origin");
  if (origin !== null) return origin === url.origin;
  const referer = request.headers.get("Referer");
  if (referer === null) return false;
  try {
    return new URL(referer).origin === url.origin;
  } catch {
    return false;
  }
}

/**
 * Reads a body up to `max` bytes, without buffering more than that.
 *
 * @param {ReadableStream<Uint8Array>} body The body.
 * @param {number} max Most bytes to hold.
 * @returns {Promise<{chunks: Uint8Array[], size: number, done: boolean,
 *   reader: ReadableStreamDefaultReader<Uint8Array>}>} The chunks read, their
 *   total size, whether the body ended within `max` (otherwise `chunks` holds
 *   just over `max` bytes and `reader` the rest).
 */
async function readCapped(body, max) {
  const reader = body.getReader();
  const chunks = [];
  let size = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) return { chunks, size, done: true, reader };
    chunks.push(value);
    size += value.byteLength;
    if (size > max) return { chunks, size, done: false, reader };
  }
}

/**
 * Joins chunks into one UTF-8 string.
 *
 * @param {Uint8Array[]} chunks The chunks.
 * @returns {string} The text.
 */
function decodeChunks(chunks) {
  const decoder = new TextDecoder();
  return chunks.map((c) => decoder.decode(c, { stream: true })).join("") + decoder.decode();
}

/**
 * Reads the Turnstile token from the launch form.
 *
 * @param {Request} request The form POST.
 * @returns {Promise<string | null>} The token, or null when absent or oversized.
 */
async function turnstileToken(request) {
  const type = request.headers.get("Content-Type") ?? "";
  if (!type.startsWith("application/x-www-form-urlencoded") || !request.body) return null;
  const read = await readCapped(request.body, MAX_FORM_BYTES);
  if (!read.done) {
    await read.reader.cancel();
    return null;
  }
  const text = decodeChunks(read.chunks);
  const token = new URLSearchParams(text).get("cf-turnstile-response");
  return token && token.length <= 4096 ? token : null;
}

/**
 * `POST /_demo/start`: checks Turnstile and the limits, then opens a session.
 *
 * @param {Request} request The launch form POST.
 * @param {URL} url Its URL.
 * @param {Record<string, any>} env Worker env.
 * @param {Deps} deps Outside dependencies.
 * @returns {Promise<Response>} A redirect to the waiting page, or the launch
 *   page with the reason the session was refused.
 */
async function start(request, url, env, deps) {
  const page = (message, status) =>
    htmlResponse(launchPage({ siteKey: configured(env) ? env.TURNSTILE_SITE_KEY : null, message, siteUrl: siteUrl(env) }), status);
  if (!configured(env)) return page(undefined, 503);
  if (!sameOrigin(request, url)) return page(REFUSALS.origin, 403);
  const token = await turnstileToken(request);
  const ip = request.headers.get("CF-Connecting-IP") ?? "";
  if (!token) return page(REFUSALS.turnstile, 403);
  const verify = await deps.fetch(SITEVERIFY_URL, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ secret: env.TURNSTILE_SECRET, response: token, remoteip: ip }),
  });
  if (!siteverifyPassed(await verify.text(), { hostname: url.hostname, action: TURNSTILE_ACTION })) {
    return page(REFUSALS.turnstile, 403);
  }
  // A visitor who starts again (a second tab, the back button) gives up
  // their running session rather than holding two.
  const previous = readSessionToken(request.headers.get("Cookie"));
  if (previous) await sessionStub(env, await sessionId(env, previous)).end("restart");
  const now = Math.floor(deps.now() / 1000);
  const sessionToken = newToken();
  const id = await sessionId(env, sessionToken);
  const version = String(env.DEMO_VERSION ?? "");
  const admitted = await admit(env.DB, { id, ipHash: await ipHash(env, ip, now), now, version }, limitsFrom(env));
  if (!admitted.ok) return page(REFUSALS[admitted.reason], 429);
  await sessionStub(env, id).open({ id, endsAt: admitted.endsAt, version, origin: url.origin });
  const maxAge = admitted.endsAt - now;
  return redirect("/_demo/wait", [`${SESSION_COOKIE}=${sessionToken}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=${maxAge}`]);
}

/**
 * Adds the demo banner (time left, Start over, Get Bookclerk) to the web
 * app's HTML document, just before `</head>`.
 *
 * @param {string} html The app's document.
 * @returns {string} The document with the banner's stylesheet and script.
 */
export function withBanner(html) {
  const tags = '<link rel="stylesheet" href="/_demo/banner.css"><script src="/_demo/banner.js" defer></script>';
  const at = html.search(/<\/head\s*>/i);
  return at === -1 ? tags + html : html.slice(0, at) + tags + html.slice(at);
}

/**
 * Turns the container's answer into the visitor's response: rewritten
 * cookies, security headers, and the banner on HTML documents.
 *
 * @param {Response} upstream The container's response.
 * @returns {Promise<Response>} The response for the browser.
 */
async function forwardResponse(upstream) {
  if (upstream.webSocket) return upstream;
  const headers = new Headers(upstream.headers);
  headers.delete("Set-Cookie");
  for (const line of upstream.headers.getSetCookie()) {
    const rewritten = rewriteSetCookie(line);
    if (rewritten) headers.append("Set-Cookie", rewritten);
  }
  for (const [k, v] of Object.entries(SECURITY_HEADERS)) headers.set(k, v);
  const init = { status: upstream.status, statusText: upstream.statusText, headers };
  const type = headers.get("Content-Type") ?? "";
  if (!type.startsWith("text/html") || !upstream.body) return new Response(upstream.body, init);
  // The body is decoded here, so its length changes; the runtime encodes it
  // again for the browser as Content-Encoding says.
  headers.delete("Content-Length");
  const read = await readCapped(upstream.body, MAX_BANNER_HTML_BYTES);
  if (read.done) return new Response(withBanner(decodeChunks(read.chunks)), init);
  // Too big for the banner: send what was read, then the rest as it comes.
  const { chunks, reader } = read;
  const rest = new ReadableStream({
    start(controller) {
      for (const c of chunks) controller.enqueue(c);
    },
    async pull(controller) {
      const { done, value } = await reader.read();
      if (done) controller.close();
      else controller.enqueue(value);
    },
    cancel(reason) {
      return reader.cancel(reason);
    },
  });
  return new Response(rest, init);
}

/**
 * Sends a visitor's request to their container, after the route check.
 *
 * @param {Request} request The visitor's request.
 * @param {URL} url Its URL.
 * @param {Record<string, any>} env Worker env.
 * @param {Deps} deps Outside dependencies.
 * @param {string} id Session id.
 * @returns {Promise<Response>} The container's answer, a refusal, or the
 *   page for a session that is not running.
 */
async function proxy(request, url, env, deps, id) {
  const { verdict } = classify(request.method, url.pathname);
  if (verdict === "deny") {
    return json({ error: "This is not available in the Bookclerk demo." }, 403);
  }
  if (request.method !== "GET" && request.method !== "HEAD" && !sameOrigin(request, url)) {
    return json({ error: "Cross-origin request refused." }, 403);
  }
  if (verdict === "signin") {
    const now = Math.floor(deps.now() / 1000);
    const ip = request.headers.get("CF-Connecting-IP") ?? "";
    if (!(await countSignin(env.DB, { id, ipHash: await ipHash(env, ip, now), now }, limitsFrom(env)))) {
      return json({ error: "You have reached the demo's limit on store sign-in attempts." }, 429);
    }
  }
  const forwarded = new Request(request);
  // The visitor's IP never reaches the container, and nothing the visitor
  // sends can claim one for them. `Origin` and `Referer` stay: bookclerkd
  // checks them against its public origin (this one).
  for (const h of [
    "Authorization",
    "Cookie",
    "CF-Connecting-IP",
    "True-Client-IP",
    "Forwarded",
    "X-Forwarded-For",
    "X-Forwarded-Host",
    "X-Forwarded-Proto",
    "X-Real-IP",
  ]) {
    forwarded.headers.delete(h);
  }
  const cookies = cookiesForContainer(request.headers.get("Cookie"));
  if (cookies) forwarded.headers.set("Cookie", cookies);
  const upstream = await sessionStub(env, id).fetch(forwarded);
  const state = upstream.headers.get("X-Demo-State");
  if (upstream.status === 503 && state !== null) {
    return notRunning(request, state, env);
  }
  return forwardResponse(upstream);
}

/**
 * The answer for a session that is not running: wait while it starts,
 * otherwise clear the cookies and say it ended. Only this answer clears the
 * cookies; a 410 from bookclerkd itself (an expired invite link) does not.
 *
 * @param {Request} request The visitor's request.
 * @param {string} state The session's state.
 * @param {Record<string, any>} [env] Worker env, for the website link.
 * @returns {Response} A page for a browser, JSON for the app.
 */
function notRunning(request, state, env = {}) {
  if (state === "starting") {
    return isNavigation(request) ? redirect("/_demo/wait") : json({ error: "Your demo is still starting." }, 503);
  }
  const response = isNavigation(request)
    ? htmlResponse(endedPage({ siteUrl: siteUrl(env) }), 410, {})
    : json({ error: "Your demo has ended." }, 410);
  for (const c of CLEAR_COOKIES) response.headers.append("Set-Cookie", c);
  return response;
}

/**
 * Handles one request to the demo Worker.
 *
 * @param {Request} request The request.
 * @param {Record<string, any>} env Worker env: `DB` (D1), `SESSIONS` (the
 *   session class), `SESSION_KEY`, `TURNSTILE_SITE_KEY`, `TURNSTILE_SECRET`,
 *   `DEMO_VERSION`, `SITE_URL` and the limit vars.
 * @param {Deps} [deps] Outside dependencies.
 * @returns {Promise<Response>} The response.
 */
export async function handle(request, env, deps = DEFAULT_DEPS) {
  const url = new URL(request.url);
  const path = url.pathname;
  if (path === "/_demo/health") {
    return json({ ok: true, version: String(env.DEMO_VERSION ?? ""), configured: configured(env) }, 200);
  }
  if (path === "/_demo/start") {
    return request.method === "POST" ? start(request, url, env, deps) : redirect("/");
  }
  const token = configured(env) ? readSessionToken(request.headers.get("Cookie")) : null;
  const id = token ? await sessionId(env, token) : null;

  if (path === "/_demo/wait") {
    return id ? htmlResponse(waitPage()) : redirect("/");
  }
  if (path === "/_demo/session") {
    const status = id ? await sessionStub(env, id).status() : { state: "none", endsAt: 0, version: "" };
    return json({ ...status, siteUrl: siteUrl(env) }, 200);
  }
  if (path === "/_demo/enter") {
    const portal = id ? await sessionStub(env, id).portalSession() : null;
    if (!portal) return redirect(id ? "/_demo/wait" : "/");
    return redirect("/", [`${PORTAL_COOKIE}=${portal}; Path=/; HttpOnly; Secure; SameSite=Lax`]);
  }
  if (path === "/_demo/end") {
    if (request.method !== "POST") return redirect("/");
    if (!sameOrigin(request, url)) return json({ error: "Cross-origin request refused." }, 403);
    if (id) await sessionStub(env, id).end("visitor");
    return redirect("/", CLEAR_COOKIES);
  }
  if (path.startsWith("/_demo/")) {
    return json({ error: "Not found." }, 404);
  }

  if (!id) {
    if (path === "/" && request.method === "GET") {
      return htmlResponse(
        launchPage({ siteKey: configured(env) ? env.TURNSTILE_SITE_KEY : null, siteUrl: siteUrl(env) }),
        200,
      );
    }
    return isNavigation(request) ? redirect("/") : json({ error: "Start the demo first." }, 401);
  }
  return proxy(request, url, env, deps, id);
}

/**
 * The five-minute sweep: ends sessions still marked live past their hard
 * stop (their own timer did not fire), and prunes old rows.
 *
 * @param {Record<string, any>} env Worker env.
 * @param {Deps} [deps] Outside dependencies.
 * @returns {Promise<number>} Sessions the sweep asked to end.
 */
export async function sweep(env, deps = DEFAULT_DEPS) {
  const now = Math.floor(deps.now() / 1000);
  const ids = await overdue(env.DB, now, SWEEP_GRACE_SECONDS, SWEEP_BATCH);
  for (const id of ids) {
    try {
      await sessionStub(env, id).end("sweep");
    } catch (err) {
      console.error(`sweep could not end session ${id}: ${String(err)}`);
    }
    // The object records its own end; this closes the row when it could not
    // (it had already ended, or never opened), so no row stays live forever.
    await finish(env.DB, { id, now, reason: "sweep", revoked: 0, failed: 0 });
  }
  await prune(env.DB, now);
  return ids.length;
}
