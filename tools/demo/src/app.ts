// The demo Worker's request handling, apart from the container class so the
// tests can run it under Node. worker.ts wires it to Cloudflare.
//
// Every visitor request goes through here: Worker endpoints under /_demo/,
// the launch page for a browser without a session, and otherwise the route
// check (routes.ts) before the request reaches the visitor's own container.

import { classify } from "./routes.ts";
import {
  admit,
  countSignin,
  finish,
  limitsFrom,
  overdue,
  prune,
  startupEstimate,
  utcDay,
} from "./limits.ts";
import {
  cookiesForContainer,
  decodeLaunchForm,
  MAX_FORM_BYTES,
  PORTAL_COOKIE,
  readSessionToken,
  rewriteSetCookie,
  SESSION_COOKIE,
  siteverifyPassed,
} from "./inputs.ts";
import type { Env, SessionApi } from "./env.ts";
import {
  endedPage,
  htmlResponse,
  launchPage,
  REFUSALS,
  SECURITY_HEADERS,
  waitPage,
} from "./pages.ts";
import { LIVE_PATH, liveRequest, NO_SESSION, siteUrl } from "./live.ts";

/** Turnstile's server-side check. */
const SITEVERIFY_URL = "https://challenges.cloudflare.com/turnstile/v0/siteverify";

/** The widget action the launch page sets and Siteverify must echo. */
export const TURNSTILE_ACTION = "demo_start";

/** Largest web app HTML document the banner is added to. */
const MAX_BANNER_HTML_BYTES = 1024 * 1024;

/** Sessions the sweep ends per run. */
const SWEEP_BATCH = 20;

/** How far past its hard stop a session must be before the sweep ends it. */
const SWEEP_GRACE_SECONDS = 120;

/**
 * How far past its hard stop a session whose object keeps failing to end is
 * closed in D1 anyway, so it stops holding a live slot (about as long as the
 * object itself retries destroying the VM).
 */
const SWEEP_GIVE_UP_SECONDS = 3600;

/** What the handler needs from outside, injectable for tests. */
export type Deps = {
  /** Outbound fetch (Siteverify): a string URL with options. */
  fetch: (url: string, init: RequestInit) => Promise<Response>;
  /** Current time in milliseconds. */
  now: () => number;
};

const DEFAULT_DEPS: Deps = { fetch: (url, init) => fetch(url, init), now: () => Date.now() };

/** The imported HMAC key, cached per key text (one Worker serves one key). */
let hmacKey: { text: string; key: Promise<CryptoKey> | null } = { text: "", key: null };

/**
 * Lowercase hex HMAC-SHA256.
 *
 * @param key - Key text.
 * @param message - Message text.
 * @returns The MAC in hex.
 */
async function hmacHex(key: string, message: string): Promise<string> {
  let imported = hmacKey.text === key ? hmacKey.key : null;
  if (imported === null) {
    imported = crypto.subtle.importKey(
      "raw",
      new TextEncoder().encode(key),
      { name: "HMAC", hash: "SHA-256" },
      false,
      ["sign"],
    );
    hmacKey = { text: key, key: imported };
  }
  const k = await imported;
  const mac = new Uint8Array(
    await crypto.subtle.sign("HMAC", k, new TextEncoder().encode(message)),
  );
  return Array.from(mac, (x) => x.toString(16).padStart(2, "0")).join("");
}

/**
 * A new session token: 16 random bytes, unpadded base64url.
 *
 * @returns The token.
 */
function newToken(): string {
  const b = new Uint8Array(16);
  crypto.getRandomValues(b);
  return btoa(String.fromCharCode(...b))
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
}

/**
 * The session id for a token. The container and the D1 row are named by it,
 * so knowing an id does not give anyone the cookie, and a guessed cookie
 * names nobody's container.
 *
 * @param env - Worker env.
 * @param token - Session token.
 * @returns The id (hex).
 */
function sessionId(env: ConfiguredEnv, token: string): Promise<string> {
  return hmacHex(env.SESSION_KEY, `session|${token}`);
}

/**
 * The address hash for rate limits, salted with the UTC day so rows cannot
 * be linked across days or reversed by trying addresses without the key.
 *
 * @param env - Worker env.
 * @param ip - Client address.
 * @param nowSeconds - Current Unix seconds.
 * @returns The hash (hex, 32 characters).
 */
async function ipHash(env: ConfiguredEnv, ip: string, nowSeconds: number): Promise<string> {
  return (await hmacHex(env.SESSION_KEY, `ip|${utcDay(nowSeconds)}|${ip}`)).slice(0, 32);
}

/**
 * The visitor's session object.
 *
 * @param env - Worker env.
 * @param id - Session id.
 * @returns The Durable Object stub.
 */
function sessionStub(env: Env, id: string): SessionApi {
  return env.SESSIONS.get(env.SESSIONS.idFromName(id));
}

/** An env with the session key and both Turnstile keys set. */
type ConfiguredEnv = Env & {
  SESSION_KEY: string;
  TURNSTILE_SITE_KEY: string;
  TURNSTILE_SECRET: string;
};

/**
 * Whether the Worker has everything it needs to admit visitors.
 *
 * @param env - Worker env.
 * @returns True when the session key and Turnstile keys are set.
 */
function configured(env: Env): env is ConfiguredEnv {
  return Boolean(env.SESSION_KEY && env.TURNSTILE_SITE_KEY && env.TURNSTILE_SECRET);
}

/** How long one isolate reuses the startup estimate before asking D1 again. */
const ESTIMATE_TTL_MS = 60_000;

/**
 * The startup estimate, cached per isolate so the launch page costs no D1
 * read per view. The pending lookup is cached too, so requests arriving
 * together share one query.
 */
let estimateCache: { at: number; seconds: Promise<number | null> } | null = null;

/**
 * How long a start usually takes (limits.ts `startupEstimate`), cached for
 * a minute.
 *
 * @param env - Worker env.
 * @param deps - Outside dependencies (the clock).
 * @returns Seconds, or null while too few starts are recorded or D1 fails.
 */
async function estimate(env: Env, deps: Deps): Promise<number | null> {
  const now = deps.now();
  if (estimateCache !== null && now - estimateCache.at < ESTIMATE_TTL_MS)
    return estimateCache.seconds;
  const seconds = startupEstimate(env.DB, Math.floor(now / 1000)).catch((err: unknown) => {
    // The pages read fine without an estimate.
    console.error(`startup estimate failed: ${String(err)}`);
    return null;
  });
  estimateCache = { at: now, seconds };
  return seconds;
}

/** Forgets the cached startup estimate (tests). */
export function resetEstimateCache(): void {
  estimateCache = null;
}

/**
 * The launch page, with this demo's session length and startup estimate.
 *
 * @param env - Worker env.
 * @param deps - Outside dependencies.
 * @param message - Why the last start was refused, if it was.
 * @returns The document.
 */
async function launch(env: Env, deps: Deps, message?: string): Promise<string> {
  return launchPage({
    siteKey: configured(env) ? env.TURNSTILE_SITE_KEY : null,
    siteUrl: siteUrl(env),
    sessionMinutes: Math.round(limitsFrom(env).sessionSeconds / 60),
    estimateSeconds: await estimate(env, deps),
    ...(message === undefined ? {} : { message }),
  });
}

/**
 * A JSON response with the security headers.
 *
 * @param body - Body.
 * @param status - HTTP status.
 * @param extra - More headers.
 * @returns The response.
 */
function json(body: unknown, status: number, extra: Record<string, string> = {}): Response {
  return Response.json(body, {
    status,
    headers: { "Cache-Control": "no-store", ...SECURITY_HEADERS, ...extra },
  });
}

/**
 * A redirect, optionally setting cookies.
 *
 * @param location - Target path.
 * @param cookies - `Set-Cookie` lines.
 * @returns 303 See Other.
 */
function redirect(location: string, cookies: string[] = []): Response {
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
 * @param request - Request.
 * @returns True for a GET that accepts HTML.
 */
function isNavigation(request: Request): boolean {
  return request.method === "GET" && (request.headers.get("Accept") ?? "").includes("text/html");
}

/**
 * Whether a state-changing request came from a page on this host. The
 * Worker refuses cross-site writes itself, before any reach a container;
 * bookclerkd checks `Origin` again against its `public_origin`, which is
 * this origin.
 *
 * @param request - Request.
 * @param url - The request's URL.
 * @returns True when `Origin` (or, without it, `Referer`) is this origin.
 */
function sameOrigin(request: Request, url: URL): boolean {
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
 * @param body - The body.
 * @param max - Most bytes to hold.
 * @returns The chunks read, their
 *   total size, whether the body ended within `max` (otherwise `chunks` holds
 *   just over `max` bytes and `reader` the rest).
 */
async function readCapped(
  body: ReadableStream<Uint8Array>,
  max: number,
): Promise<{
  chunks: Uint8Array[];
  size: number;
  done: boolean;
  reader: ReadableStreamDefaultReader<Uint8Array>;
}> {
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
 * @param chunks - The chunks.
 * @returns The text.
 */
function decodeChunks(chunks: Uint8Array[]): string {
  const decoder = new TextDecoder();
  return chunks.map((c) => decoder.decode(c, { stream: true })).join("") + decoder.decode();
}

/**
 * Reads the Turnstile token from the launch form.
 *
 * @param request - The form POST.
 * @returns The token, or null when absent or oversized.
 */
async function turnstileToken(request: Request): Promise<string | null> {
  const type = request.headers.get("Content-Type") ?? "";
  if (!type.startsWith("application/x-www-form-urlencoded") || !request.body) return null;
  const read = await readCapped(request.body, MAX_FORM_BYTES);
  if (!read.done) {
    await read.reader.cancel();
    return null;
  }
  const bytes = new Uint8Array(read.size);
  let at = 0;
  for (const c of read.chunks) {
    bytes.set(c, at);
    at += c.byteLength;
  }
  return decodeLaunchForm(bytes);
}

/**
 * `POST /_demo/start`: checks Turnstile and the limits, then opens a session.
 *
 * @param request - The launch form POST.
 * @param url - Its URL.
 * @param env - Worker env.
 * @param deps - Outside dependencies.
 * @returns A redirect to the waiting page, or the launch
 *   page with the reason the session was refused.
 */
async function start(request: Request, url: URL, env: Env, deps: Deps): Promise<Response> {
  const page = async (message: string | undefined, status: number): Promise<Response> =>
    htmlResponse(await launch(env, deps, message), status);
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
  if (
    !siteverifyPassed(await verify.text(), { hostname: url.hostname, action: TURNSTILE_ACTION })
  ) {
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
  const admitted = await admit(
    env.DB,
    { id, ipHash: await ipHash(env, ip, now), now, version },
    limitsFrom(env),
  );
  if (!admitted.ok) return page(REFUSALS[admitted.reason], 429);
  await sessionStub(env, id).open({ id, endsAt: admitted.endsAt, version, origin: url.origin });
  const maxAge = admitted.endsAt - now;
  return redirect("/_demo/wait", [
    `${SESSION_COOKIE}=${sessionToken}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=${maxAge}`,
  ]);
}

/**
 * Adds the demo banner (time left, Start over, Get Bookclerk) to the web
 * app's HTML document, just before `</head>`.
 *
 * @param html - The app's document.
 * @returns The document with the banner's stylesheet and script.
 */
export function withBanner(html: string): string {
  const tags =
    '<link rel="stylesheet" href="/_demo/banner.css"><script src="/_demo/live.js" defer></script><script src="/_demo/banner.js" defer></script>';
  const at = html.search(/<\/head\s*>/i);
  return at === -1 ? tags + html : html.slice(0, at) + tags + html.slice(at);
}

/**
 * Turns the container's answer into the visitor's response: rewritten
 * cookies, security headers, and the banner on HTML documents.
 *
 * @param upstream - The container's response.
 * @returns The response for the browser.
 */
async function forwardResponse(upstream: Response): Promise<Response> {
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
 * @param request - The visitor's request.
 * @param url - Its URL.
 * @param env - Worker env.
 * @param deps - Outside dependencies.
 * @param id - Session id.
 * @returns The container's answer, a refusal, or the
 *   page for a session that is not running.
 */
async function proxy(
  request: Request,
  url: URL,
  env: ConfiguredEnv,
  deps: Deps,
  id: string,
): Promise<Response> {
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
    if (
      !(await countSignin(env.DB, { id, ipHash: await ipHash(env, ip, now), now }, limitsFrom(env)))
    ) {
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
 * @param request - The visitor's request.
 * @param state - The session's state.
 * @param env - Worker env, for the website link.
 * @returns A page for a browser, JSON for the app.
 */
function notRunning(request: Request, state: string, env: Pick<Env, "SITE_URL"> = {}): Response {
  if (state === "starting") {
    return isNavigation(request)
      ? redirect("/_demo/wait")
      : json({ error: "Your demo is still starting." }, 503);
  }
  const response = isNavigation(request)
    ? htmlResponse(endedPage({ siteUrl: siteUrl(env) }), 410, {})
    : json({ error: "Your demo has ended." }, 410);
  for (const c of CLEAR_COOKIES) response.headers.append("Set-Cookie", c);
  return response;
}

/**
 * `GET /_demo/live`: hands a page's WebSocket to its session object, which
 * pushes the session's status over it (live.ts). Only pages on this origin
 * may open one: a socket carries the visitor's cookie, and browsers send
 * `Origin` on every WebSocket handshake.
 *
 * @param request - The upgrade request.
 * @param url - Its URL.
 * @param env - Worker env.
 * @param id - Session id, if the visitor has a session.
 * @returns The session object's upgrade, or a refusal.
 */
async function live(request: Request, url: URL, env: Env, id: string | null): Promise<Response> {
  const verdict = liveRequest({
    method: request.method,
    upgrade: request.headers.get("Upgrade"),
    origin: request.headers.get("Origin"),
    ownOrigin: url.origin,
    hasSession: id !== null,
  });
  switch (verdict) {
    case "not_websocket":
      return json({ error: "Open this as a WebSocket." }, 426, { Upgrade: "websocket" });
    case "cross_origin":
      return json({ error: "Cross-origin request refused." }, 403);
    case "no_session":
      return json({ error: "Start the demo first." }, 401);
    case "open":
      break;
  }
  // "open" implies a session; this only tells tsc so.
  if (id === null) return json({ error: "Start the demo first." }, 401);
  // A fresh request: nothing the browser sent (cookies, headers) reaches
  // the object, which needs only the path and the upgrade.
  return sessionStub(env, id).fetch(
    new Request(`https://session${LIVE_PATH}`, { headers: { Upgrade: "websocket" } }),
  );
}

/**
 * Handles one request to the demo Worker.
 *
 * @param request - The request.
 * @param env - Worker env: `DB` (D1), `SESSIONS` (the
 *   session class), `SESSION_KEY`, `TURNSTILE_SITE_KEY`, `TURNSTILE_SECRET`,
 *   `DEMO_VERSION`, `SITE_URL` and the limit vars.
 * @param deps - Outside dependencies.
 * @returns The response.
 */
export async function handle(
  request: Request,
  env: Env,
  deps: Deps = DEFAULT_DEPS,
): Promise<Response> {
  const url = new URL(request.url);
  const path = url.pathname;
  if (path === "/_demo/health") {
    return json(
      { ok: true, version: String(env.DEMO_VERSION ?? ""), configured: configured(env) },
      200,
    );
  }
  if (path === "/_demo/start") {
    return request.method === "POST" ? start(request, url, env, deps) : redirect("/");
  }
  const token = configured(env) ? readSessionToken(request.headers.get("Cookie")) : null;
  const id = token && configured(env) ? await sessionId(env, token) : null;

  if (path === "/_demo/wait") {
    return id
      ? htmlResponse(
          waitPage({ estimateSeconds: await estimate(env, deps), siteUrl: siteUrl(env) }),
        )
      : redirect("/");
  }
  if (path === "/_demo/session") {
    const status = id ? await sessionStub(env, id).status() : NO_SESSION;
    return json({ ...status, siteUrl: siteUrl(env) }, 200);
  }
  if (path === LIVE_PATH) {
    return live(request, url, env, id);
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

  // An id implies a configured env; the second test only tells tsc so.
  if (!id || !configured(env)) {
    if (path === "/" && request.method === "GET") {
      return htmlResponse(await launch(env, deps), 200);
    }
    return isNavigation(request) ? redirect("/") : json({ error: "Start the demo first." }, 401);
  }
  return proxy(request, url, env, deps, id);
}

/**
 * The five-minute sweep: ends sessions still marked live past their hard
 * stop (their own timer did not fire), and prunes old rows.
 *
 * @param env - Worker env.
 * @param deps - Outside dependencies.
 * @returns Sessions the sweep asked to end.
 */
export async function sweep(env: Env, deps: Deps = DEFAULT_DEPS): Promise<number> {
  const now = Math.floor(deps.now() / 1000);
  const rows = await overdue(env.DB, now, SWEEP_GRACE_SECONDS, SWEEP_BATCH);
  for (const { id, endsAt } of rows) {
    try {
      await sessionStub(env, id).end("sweep");
    } catch (err) {
      console.error(`sweep could not end session ${id}: ${String(err)}`);
      // Leave the row live so the next sweep tries the teardown again, until
      // it is so late that the slot matters more.
      if (now - endsAt < SWEEP_GIVE_UP_SECONDS) continue;
      await finish(env.DB, { id, now, reason: "sweep", revoked: 0, failed: 1 });
      continue;
    }
    // The object records its own end; this closes the row when it could not
    // (it had already ended, or never opened), so no row stays live forever.
    await finish(env.DB, { id, now, reason: "sweep", revoked: 0, failed: 0 });
  }
  await prune(env.DB, now);
  return rows.length;
}
