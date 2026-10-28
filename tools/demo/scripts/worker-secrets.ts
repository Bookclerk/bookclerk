// Prints the demo Worker's secrets as JSON for `wrangler secret bulk`, so no
// value appears on a command line or in a log. Run by
// .github/workflows/demo-deploy.yml after each deploy:
//
//   node scripts/worker-secrets.ts | npx wrangler secret bulk
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

/** The one kind of fetch the script makes: a string URL with options. */
export type FetchFn = (url: string, init: RequestInit) => Promise<Response>;

/** Most hostnames kept on the widget (Cloudflare allows ten). */
export const MAX_DOMAINS = 10;

/**
 * Whether a parsed JSON value is an object (not null, not an array).
 *
 * @param value - Parsed JSON.
 * @returns True for a plain JSON object.
 */
function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

/**
 * Reads a Cloudflare API answer. Sans-IO, so the fuzz test drives it.
 *
 * @param status - HTTP status.
 * @param text - Response body.
 * @returns The
 *   `result` when the status is 2xx and `success` is true; otherwise the
 *   numeric error codes (at most ten, comma-separated) for the log.
 */
export function decodeApiResponse(
  status: number,
  text: string,
): { ok: true; result: unknown } | { ok: false; codes: string } {
  let body: unknown;
  try {
    body = text.length <= MAX_RESPONSE_CHARS ? JSON.parse(text) : undefined;
  } catch {
    body = undefined;
  }
  const obj = isRecord(body) ? body : {};
  if (status >= 200 && status < 300 && obj.success === true)
    return { ok: true, result: obj.result };
  const codes = Array.isArray(obj.errors)
    ? obj.errors
        .map((e: unknown) => (isRecord(e) ? e.code : undefined))
        .filter((c: unknown) => Number.isSafeInteger(c))
        .slice(0, 10)
        .join(",")
    : "";
  return { ok: false, codes };
}

/**
 * Picks the demo's widget out of a widget list `result`.
 *
 * @param result - The list call's `result`.
 * @returns The
 *   first widget named {@link WIDGET_NAME} with a well-formed site key; its
 *   well-formed hostnames (deduplicated, at most {@link MAX_DOMAINS}) and its
 *   mode (`managed` unless a known one). Null when there is none.
 */
export function findWidget(
  result: unknown,
): { sitekey: string; domains: string[]; mode: string } | null {
  if (!Array.isArray(result)) return null;
  for (const w of result) {
    if (!isRecord(w)) continue;
    if (w.name !== WIDGET_NAME || typeof w.sitekey !== "string" || !KEY_RE.test(w.sitekey))
      continue;
    const domains: string[] = [];
    for (const d of Array.isArray(w.domains) ? (w.domains as unknown[]) : []) {
      if (
        typeof d === "string" &&
        HOST_RE.test(d) &&
        !domains.includes(d) &&
        domains.length < MAX_DOMAINS
      )
        domains.push(d);
    }
    return {
      sitekey: w.sitekey,
      domains,
      mode: typeof w.mode === "string" && MODES.includes(w.mode) ? w.mode : "managed",
    };
  }
  return null;
}

/**
 * Reads a widget's keys out of a create or get `result`.
 *
 * @param result - The call's `result`.
 * @returns Both keys when each is
 *   well-formed; otherwise null.
 */
export function widgetKeys(result: unknown): { siteKey: string; secret: string } | null {
  if (!isRecord(result)) return null;
  const { sitekey, secret } = result;
  if (
    typeof sitekey !== "string" ||
    typeof secret !== "string" ||
    !KEY_RE.test(sitekey) ||
    !KEY_RE.test(secret)
  ) {
    return null;
  }
  return { siteKey: sitekey, secret };
}

/**
 * Calls the Cloudflare API and returns `result`.
 *
 * @param fetchFn - Fetch.
 * @param token - API token.
 * @param path - Path under /client/v4.
 * @param init - Method and body.
 * @returns The response's `result`.
 * @throws {Error} When the call fails; the message names the path and
 *   Cloudflare's error codes, never a secret.
 */
async function api(
  fetchFn: FetchFn,
  token: string,
  path: string,
  init: RequestInit = {},
): Promise<unknown> {
  const res = await fetchFn(`https://api.cloudflare.com/client/v4${path}`, {
    ...init,
    headers: { Authorization: `Bearer ${token}`, "Content-Type": "application/json" },
  });
  const answer = decodeApiResponse(res.status, await res.text());
  if (!answer.ok) {
    throw new Error(
      `Cloudflare API ${init.method ?? "GET"} ${path.replace(/\/widgets\/.+$/, "/widgets/…")} failed: ${res.status} ${answer.codes}`,
    );
  }
  return answer.result;
}

/**
 * Finds or creates the demo's Turnstile widget, allows `host` on it, and
 * returns its keys.
 *
 * @param p - Fetch, API token, account id and the demo's hostname.
 * @returns The widget's keys.
 * @throws {Error} When the API refuses, answers without keys, or the widget
 *   already has the most hostnames it can take.
 */
export async function turnstileKeys({
  fetchFn,
  token,
  account,
  host,
}: {
  fetchFn: FetchFn;
  token: string;
  account: string;
  host: string;
}): Promise<{ siteKey: string; secret: string }> {
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
      if (found.domains.length >= MAX_DOMAINS)
        throw new Error("Turnstile widget already allows the most hostnames");
      await api(fetchFn, token, `${base}/${encodeURIComponent(found.sitekey)}`, {
        method: "PUT",
        body: JSON.stringify({
          name: WIDGET_NAME,
          domains: [...found.domains, host],
          mode: found.mode,
        }),
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
 * @returns The key.
 */
export function newSessionKey(): string {
  const b = new Uint8Array(32);
  crypto.getRandomValues(b);
  return Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
}

/**
 * Builds the secrets object for `wrangler secret bulk`.
 *
 * @param env - Environment (see header).
 * @param fetchFn - Fetch.
 * @returns Secret names and values.
 * @throws {Error} When a required variable is missing or the API fails.
 */
export async function workerSecrets(
  env: Record<string, string | undefined>,
  fetchFn: FetchFn,
): Promise<Record<string, string>> {
  const required = (name: string): string => {
    const value = env[name];
    if (!value) throw new Error(`${name} is not set`);
    return value;
  };
  const token = required("CLOUDFLARE_API_TOKEN");
  const account = required("CLOUDFLARE_ACCOUNT_ID");
  const host = required("DEMO_HOST");
  const { siteKey, secret } = await turnstileKeys({ fetchFn, token, account, host });
  const out: Record<string, string> = { TURNSTILE_SITE_KEY: siteKey, TURNSTILE_SECRET: secret };
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
