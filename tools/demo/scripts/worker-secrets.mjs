// Prints the demo Worker's secrets as JSON for `wrangler secret bulk`, so no
// value appears on a command line or in a log. Run by
// .github/workflows/demo-deploy.yml after each deploy:
//
//   node scripts/worker-secrets.mjs | npx wrangler secret bulk
//
// Environment:
//   CLOUDFLARE_API_TOKEN, CLOUDFLARE_ACCOUNT_ID  the deploy credentials (the
//     token needs Account > Turnstile > Edit)
//   DEMO_HOST          the demo's hostname, which the widget must allow
//   NEED_SESSION_KEY   "true" when the Worker has no SESSION_KEY yet; a new
//     random key is printed then. Rotating it would end every live session,
//     so an existing key is kept.
//
// The Turnstile widget "bookclerk-demo" is created on the first run and its
// hostnames kept in step with DEMO_HOST afterwards.

import { pathToFileURL } from "node:url";

/** The widget's name in the Cloudflare account. */
export const WIDGET_NAME = "bookclerk-demo";

/** Longest Cloudflare API response body read. */
export const MAX_RESPONSE_CHARS = 1024 * 1024;

/** A site key or secret: printable ASCII without spaces, 1 to 512 characters. */
const KEY_RE = /^[\x21-\x7e]{1,512}$/;

/** A hostname as Turnstile stores it: 1 to 253 printable ASCII characters. */
const HOST_RE = /^[\x21-\x7e]{1,253}$/;

/** Widget modes Cloudflare accepts on update. */
const MODES = ["managed", "non-interactive", "invisible"];

/** Most hostnames kept on the widget (Cloudflare allows ten). */
export const MAX_DOMAINS = 10;

/**
 * Reads a Cloudflare API answer. Sans-IO, so the fuzz test drives it.
 *
 * @param {number} status HTTP status.
 * @param {string} text Response body.
 * @returns {{ok: true, result: unknown} | {ok: false, codes: string}} The
 *   `result` when the status is 2xx and `success` is true; otherwise the
 *   numeric error codes (at most ten, comma-separated) for the log.
 */
export function decodeApiResponse(status, text) {
  let body;
  try {
    body = text.length <= MAX_RESPONSE_CHARS ? JSON.parse(text) : undefined;
  } catch {
    body = undefined;
  }
  const obj = body !== null && typeof body === "object" && !Array.isArray(body) ? body : {};
  if (status >= 200 && status < 300 && obj.success === true) return { ok: true, result: obj.result };
  const codes = Array.isArray(obj.errors)
    ? obj.errors
        .map((e) => (e !== null && typeof e === "object" ? e.code : undefined))
        .filter((c) => Number.isSafeInteger(c))
        .slice(0, 10)
        .join(",")
    : "";
  return { ok: false, codes };
}

/**
 * Picks the demo's widget out of a widget list `result`.
 *
 * @param {unknown} result The list call's `result`.
 * @returns {{sitekey: string, domains: string[], mode: string} | null} The
 *   first widget named {@link WIDGET_NAME} with a well-formed site key; its
 *   well-formed hostnames (deduplicated, at most {@link MAX_DOMAINS}) and its
 *   mode (`managed` unless a known one). Null when there is none.
 */
export function findWidget(result) {
  if (!Array.isArray(result)) return null;
  for (const w of result) {
    if (w === null || typeof w !== "object" || Array.isArray(w)) continue;
    if (w.name !== WIDGET_NAME || typeof w.sitekey !== "string" || !KEY_RE.test(w.sitekey)) continue;
    const domains = [];
    for (const d of Array.isArray(w.domains) ? w.domains : []) {
      if (typeof d === "string" && HOST_RE.test(d) && !domains.includes(d) && domains.length < MAX_DOMAINS) domains.push(d);
    }
    return { sitekey: w.sitekey, domains, mode: MODES.includes(w.mode) ? w.mode : "managed" };
  }
  return null;
}

/**
 * Reads a widget's keys out of a create or get `result`.
 *
 * @param {unknown} result The call's `result`.
 * @returns {{siteKey: string, secret: string} | null} Both keys when each is
 *   well-formed; otherwise null.
 */
export function widgetKeys(result) {
  if (result === null || typeof result !== "object" || Array.isArray(result)) return null;
  const { sitekey, secret } = result;
  if (typeof sitekey !== "string" || typeof secret !== "string" || !KEY_RE.test(sitekey) || !KEY_RE.test(secret)) {
    return null;
  }
  return { siteKey: sitekey, secret };
}

/**
 * Calls the Cloudflare API and returns `result`.
 *
 * @param {typeof fetch} fetchFn Fetch.
 * @param {string} token API token.
 * @param {string} path Path under /client/v4.
 * @param {RequestInit} [init] Method and body.
 * @returns {Promise<unknown>} The response's `result`.
 * @throws {Error} When the call fails; the message names the path and
 *   Cloudflare's error codes, never a secret.
 */
async function api(fetchFn, token, path, init = {}) {
  const res = await fetchFn(`https://api.cloudflare.com/client/v4${path}`, {
    ...init,
    headers: { Authorization: `Bearer ${token}`, "Content-Type": "application/json" },
  });
  const answer = decodeApiResponse(res.status, await res.text());
  if (!answer.ok) {
    throw new Error(`Cloudflare API ${init.method ?? "GET"} ${path.replace(/\/widgets\/.+$/, "/widgets/…")} failed: ${res.status} ${answer.codes}`);
  }
  return answer.result;
}

/**
 * Finds or creates the demo's Turnstile widget, allows `host` on it, and
 * returns its keys.
 *
 * @param {{fetchFn: typeof fetch, token: string, account: string, host: string}} p
 *   Fetch, API token, account id and the demo's hostname.
 * @returns {Promise<{siteKey: string, secret: string}>} The widget's keys.
 * @throws {Error} When the API refuses, answers without keys, or the widget
 *   already has the most hostnames it can take.
 */
export async function turnstileKeys({ fetchFn, token, account, host }) {
  const base = `/accounts/${encodeURIComponent(account)}/challenges/widgets`;
  const found = findWidget(await api(fetchFn, token, `${base}?per_page=100`));
  let result;
  if (!found) {
    result = await api(fetchFn, token, base, {
      method: "POST",
      body: JSON.stringify({ name: WIDGET_NAME, domains: [host], mode: "managed" }),
    });
  } else {
    if (!found.domains.includes(host)) {
      if (found.domains.length >= MAX_DOMAINS) throw new Error("Turnstile widget already allows the most hostnames");
      await api(fetchFn, token, `${base}/${encodeURIComponent(found.sitekey)}`, {
        method: "PUT",
        body: JSON.stringify({ name: WIDGET_NAME, domains: [...found.domains, host], mode: found.mode }),
      });
    }
    result = await api(fetchFn, token, `${base}/${encodeURIComponent(found.sitekey)}`);
  }
  const keys = widgetKeys(result);
  if (!keys) throw new Error("Turnstile widget has no site key or secret");
  return keys;
}

/**
 * A new session key: 32 random bytes in hex.
 *
 * @returns {string} The key.
 */
export function newSessionKey() {
  const b = new Uint8Array(32);
  crypto.getRandomValues(b);
  return Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
}

/**
 * Builds the secrets object for `wrangler secret bulk`.
 *
 * @param {Record<string, string | undefined>} env Environment (see header).
 * @param {typeof fetch} fetchFn Fetch.
 * @returns {Promise<Record<string, string>>} Secret names and values.
 * @throws {Error} When a required variable is missing or the API fails.
 */
export async function workerSecrets(env, fetchFn) {
  for (const name of ["CLOUDFLARE_API_TOKEN", "CLOUDFLARE_ACCOUNT_ID", "DEMO_HOST"]) {
    if (!env[name]) throw new Error(`${name} is not set`);
  }
  const { siteKey, secret } = await turnstileKeys({
    fetchFn,
    token: env.CLOUDFLARE_API_TOKEN,
    account: env.CLOUDFLARE_ACCOUNT_ID,
    host: env.DEMO_HOST,
  });
  const out = { TURNSTILE_SITE_KEY: siteKey, TURNSTILE_SECRET: secret };
  if (env.NEED_SESSION_KEY === "true") out.SESSION_KEY = newSessionKey();
  return out;
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) {
  try {
    process.stdout.write(JSON.stringify(await workerSecrets(process.env, fetch)));
  } catch (err) {
    console.error(String(err instanceof Error ? err.message : err));
    process.exit(1);
  }
}
