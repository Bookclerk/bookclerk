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

/**
 * Calls the Cloudflare API and returns `result`.
 *
 * @param {typeof fetch} fetchFn Fetch.
 * @param {string} token API token.
 * @param {string} path Path under /client/v4.
 * @param {RequestInit} [init] Method and body.
 * @returns {Promise<any>} The response's `result`.
 * @throws {Error} When the call fails; the message names the path and
 *   Cloudflare's error codes, never a secret.
 */
async function api(fetchFn, token, path, init = {}) {
  const res = await fetchFn(`https://api.cloudflare.com/client/v4${path}`, {
    ...init,
    headers: { Authorization: `Bearer ${token}`, "Content-Type": "application/json" },
  });
  const body = await res.json().catch(() => ({}));
  if (!res.ok || body.success !== true) {
    const codes = Array.isArray(body.errors) ? body.errors.map((e) => e?.code).join(",") : "";
    throw new Error(`Cloudflare API ${init.method ?? "GET"} ${path.replace(/\/widgets\/.+$/, "/widgets/…")} failed: ${res.status} ${codes}`);
  }
  return body.result;
}

/**
 * Finds or creates the demo's Turnstile widget, allows `host` on it, and
 * returns its keys.
 *
 * @param {{fetchFn: typeof fetch, token: string, account: string, host: string}} p
 *   Fetch, API token, account id and the demo's hostname.
 * @returns {Promise<{siteKey: string, secret: string}>} The widget's keys.
 * @throws {Error} When the API refuses or answers without keys.
 */
export async function turnstileKeys({ fetchFn, token, account, host }) {
  const base = `/accounts/${encodeURIComponent(account)}/challenges/widgets`;
  const widgets = await api(fetchFn, token, `${base}?per_page=100`);
  const found = Array.isArray(widgets) ? widgets.find((w) => w?.name === WIDGET_NAME) : undefined;
  let widget;
  if (!found) {
    widget = await api(fetchFn, token, base, {
      method: "POST",
      body: JSON.stringify({ name: WIDGET_NAME, domains: [host], mode: "managed" }),
    });
  } else {
    const domains = Array.isArray(found.domains) ? found.domains : [];
    if (!domains.includes(host)) {
      await api(fetchFn, token, `${base}/${encodeURIComponent(found.sitekey)}`, {
        method: "PUT",
        body: JSON.stringify({ name: WIDGET_NAME, domains: [...domains, host], mode: found.mode ?? "managed" }),
      });
    }
    widget = await api(fetchFn, token, `${base}/${encodeURIComponent(found.sitekey)}`);
  }
  if (typeof widget?.sitekey !== "string" || typeof widget?.secret !== "string" || !widget.sitekey || !widget.secret) {
    throw new Error("Turnstile widget has no site key or secret");
  }
  return { siteKey: widget.sitekey, secret: widget.secret };
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
