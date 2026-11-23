/**
 * Standard Webhooks delivery, with no I/O of its own.
 *
 * Everything the notifier reads from outside is parsed here: the endpoint
 * URL and signing secret (the operator's settings or a patron's address
 * document) and the receiver's answer (its status and `Retry-After`). The
 * request is planned here too, signed per the Standard Webhooks scheme
 * (https://www.standardwebhooks.com): a `webhook-id`, a `webhook-timestamp`
 * in Unix seconds and a `webhook-signature` of
 * `v1,<base64 HMAC-SHA256(key, "<id>.<timestamp>.<body>")>`. `index.ts`
 * only makes the call, so the unit tests and the fuzzer drive the same code
 * the guest runs.
 *
 * No error text carries the URL, the secret or the message: a URL may hold
 * a token, and a message may be sensitive.
 */

import type { NotificationMessage } from "@bookclerk/plugin-sdk/workerd";

/** Longest endpoint URL accepted, in characters. */
export const MAX_URL_CHARS = 2048;

/** Prefix of a Standard Webhooks secret. */
export const SECRET_PREFIX = "whsec_";

/** Fewest key bytes a secret may decode to. */
export const MIN_SECRET_BYTES = 24;

/** Most key bytes a secret may decode to. */
export const MAX_SECRET_BYTES = 64;

/** Longest wait a `Retry-After` may ask for, in milliseconds (the host's cap). */
export const MAX_RETRY_AFTER_MS = 3_600_000;

/** Longest address document accepted, in UTF-8 bytes (the ABI's cap). */
export const MAX_ADDRESS_BYTES = 4096;

/** How long one delivery may take before it counts as unavailable, in milliseconds. */
export const SEND_TIMEOUT_MS = 20_000;

/** A failure with the plugin error code it maps to. */
export class WebhookFailure extends Error {
  /** `PluginErrorCode` wire string. */
  readonly code: string;
  /** For `rate_limited`: how long the receiver asked to wait, in milliseconds. */
  readonly retryAfterMs: number | undefined;

  /**
   * Creates a failure.
   *
   * @param code - `PluginErrorCode` wire string.
   * @param message - Operator-facing text, never holding a URL, secret or message.
   * @param retryAfterMs - Wait the receiver asked for, in milliseconds.
   */
  constructor(code: string, message: string, retryAfterMs?: number) {
    super(message);
    this.name = "WebhookFailure";
    this.code = code;
    this.retryAfterMs = retryAfterMs;
  }
}

/** Where one delivery goes and the key that signs it. */
export interface Endpoint {
  /** The URL posted to: `https:` or `http:`, no credentials or fragment. */
  url: URL;
  /** HMAC-SHA256 key, `MIN_SECRET_BYTES` to `MAX_SECRET_BYTES` long. */
  key: Uint8Array;
}

/**
 * Reads an endpoint URL.
 *
 * @param raw - The URL as configured or entered.
 * @param what - What it is, for the error (`the url setting`).
 * @returns The parsed URL.
 * @throws {WebhookFailure} `invalid_params` when it is empty, too long, not
 *   `http:` or `https:`, or carries credentials or a fragment.
 */
export function parseUrl(raw: string, what: string): URL {
  const text = raw.trim();
  if (text === "") throw new WebhookFailure("invalid_params", `${what} is empty`);
  if (text.length > MAX_URL_CHARS) {
    throw new WebhookFailure(
      "invalid_params",
      `${what} is longer than ${MAX_URL_CHARS} characters`,
    );
  }
  let url: URL;
  try {
    url = new URL(text);
  } catch {
    throw new WebhookFailure("invalid_params", `${what} is not a URL`);
  }
  if (url.protocol !== "https:" && url.protocol !== "http:") {
    throw new WebhookFailure("invalid_params", `${what} must start with https:// or http://`);
  }
  if (url.username !== "" || url.password !== "") {
    throw new WebhookFailure("invalid_params", `${what} must not carry a user name or password`);
  }
  if (url.hash !== "" || text.includes("#")) {
    throw new WebhookFailure("invalid_params", `${what} must not carry a fragment`);
  }
  if (url.hostname === "") throw new WebhookFailure("invalid_params", `${what} has no host`);
  return url;
}

/**
 * Reads a Standard Webhooks secret into its key bytes.
 *
 * @param raw - `whsec_` and the key in standard base64 (padding optional).
 * @param what - What it is, for the error.
 * @returns The key.
 * @throws {WebhookFailure} `invalid_params` when the prefix is missing, the
 *   rest is not base64, or the key is not `MIN_SECRET_BYTES` to
 *   `MAX_SECRET_BYTES` long.
 */
export function parseSecret(raw: string, what: string): Uint8Array {
  const text = raw.trim();
  if (!text.startsWith(SECRET_PREFIX)) {
    throw new WebhookFailure("invalid_params", `${what} must start with ${SECRET_PREFIX}`);
  }
  const body = text.slice(SECRET_PREFIX.length);
  const bare = body.replace(/=+$/, "");
  const padding = body.length - bare.length;
  // Canonical base64: no stray padding, and no set bits past the last byte.
  if (
    !/^[A-Za-z0-9+/]+$/.test(bare) ||
    bare.length % 4 === 1 ||
    (padding > 0 && (bare.length + padding) % 4 !== 0)
  ) {
    throw new WebhookFailure("invalid_params", `${what} is not base64 after ${SECRET_PREFIX}`);
  }
  const key = decodeBase64(bare);
  if (encodeBase64(key).replace(/=+$/, "") !== bare) {
    throw new WebhookFailure("invalid_params", `${what} is not base64 after ${SECRET_PREFIX}`);
  }
  if (key.length < MIN_SECRET_BYTES || key.length > MAX_SECRET_BYTES) {
    throw new WebhookFailure(
      "invalid_params",
      `${what} must hold ${MIN_SECRET_BYTES} to ${MAX_SECRET_BYTES} bytes`,
    );
  }
  return key;
}

/**
 * Decodes standard base64 the caller already checked.
 *
 * @param bare - Base64 without padding, never 1 more than a multiple of 4 long.
 * @returns The bytes.
 */
function decodeBase64(bare: string): Uint8Array {
  const binary = atob(bare + "=".repeat((4 - (bare.length % 4)) % 4));
  return Uint8Array.from(binary, (c) => c.charCodeAt(0));
}

/**
 * Encodes bytes as standard padded base64.
 *
 * @param bytes - The bytes.
 * @returns Base64 text.
 */
function encodeBase64(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary);
}

/**
 * The channel endpoint from the instance's settings.
 *
 * @param url - The `url` setting, when set.
 * @param secret - The `signing_secret` secret, when set.
 * @returns The endpoint.
 * @throws {WebhookFailure} `invalid_params` when either is missing or malformed.
 */
export function channelEndpoint(url: string | undefined, secret: string | undefined): Endpoint {
  if (url === undefined || url.trim() === "") {
    throw new WebhookFailure("invalid_params", "set the url setting to send to a channel");
  }
  if (secret === undefined || secret.trim() === "") {
    throw new WebhookFailure("invalid_params", "set the signing_secret secret");
  }
  return {
    url: parseUrl(url, "the url setting"),
    key: parseSecret(secret, "the signing_secret secret"),
  };
}

/** A patron's address, normalised. */
export interface Address {
  /** The endpoint it names. */
  endpoint: Endpoint;
  /** The document to save: `url` and `secret`, trimmed. */
  addressJson: string;
  /** A hint for the patron that holds no secret: the URL's host. */
  label: string;
}

/**
 * Reads a patron's address document.
 *
 * @param json - The document: a JSON object with string `url` and `secret`.
 * @returns The endpoint, the normalised document and its label.
 * @throws {WebhookFailure} `invalid_params` when the document is too long,
 *   not an object, misses a field, carries another key, or holds a
 *   malformed URL or secret.
 */
export function parseAddress(json: string): Address {
  if (new TextEncoder().encode(json).length > MAX_ADDRESS_BYTES) {
    throw new WebhookFailure(
      "invalid_params",
      `the address is longer than ${MAX_ADDRESS_BYTES} bytes`,
    );
  }
  let doc: unknown;
  try {
    doc = JSON.parse(json);
  } catch {
    throw new WebhookFailure("invalid_params", "the address is not JSON");
  }
  if (typeof doc !== "object" || doc === null || Array.isArray(doc)) {
    throw new WebhookFailure("invalid_params", "the address is not a JSON object");
  }
  const fields = doc as Record<string, unknown>;
  for (const key of Object.keys(fields)) {
    if (key !== "url" && key !== "secret") {
      throw new WebhookFailure(
        "invalid_params",
        "the address has a field other than url and secret",
      );
    }
  }
  const { url, secret } = fields;
  if (typeof url !== "string")
    throw new WebhookFailure("invalid_params", "the address needs a url");
  if (typeof secret !== "string") {
    throw new WebhookFailure("invalid_params", "the address needs a secret");
  }
  const endpoint = {
    url: parseUrl(url, "the webhook URL"),
    key: parseSecret(secret, "the signing secret"),
  };
  return {
    endpoint,
    addressJson: JSON.stringify({ url: url.trim(), secret: secret.trim() }),
    label: endpoint.url.host,
  };
}

/**
 * The `webhook-id` of one delivery: the same for every retry to one
 * destination and different per destination, as the ABI asks of
 * idempotency keys.
 *
 * @param notificationId - The host's notification id.
 * @param destinationId - The host's destination id.
 * @returns `msg_<notificationId>_<destinationId>`.
 */
export function webhookId(notificationId: string, destinationId: string): string {
  return `msg_${notificationId}_${destinationId}`;
}

/**
 * The JSON body posted for `message`.
 *
 * @param message - The rendered notification.
 * @param now - When it is sent.
 * @returns `{ type, timestamp, data }`, Standard Webhooks' payload shape.
 */
export function payload(message: NotificationMessage, now: Date): string {
  const data: Record<string, unknown> = {
    id: message.notificationId,
    type: message.notificationType,
    severity: message.severity,
    title: message.title,
    text: message.text,
  };
  if (message.markdown !== undefined) data["markdown"] = message.markdown;
  if (message.html !== undefined) data["html"] = message.html;
  if (message.link !== undefined) data["link"] = message.link;
  data["fields"] = message.fields.map((f) => ({ name: f.name, value: f.value }));
  if (message.groupKey !== undefined) data["groupKey"] = message.groupKey;
  data["alertAction"] = message.alertAction;
  if (message.alertKey !== undefined) data["alertKey"] = message.alertKey;
  return JSON.stringify({ type: message.notificationType, timestamp: now.toISOString(), data });
}

/**
 * The `webhook-signature` value.
 *
 * @param key - HMAC-SHA256 key.
 * @param id - `webhook-id`.
 * @param timestamp - `webhook-timestamp`, Unix seconds.
 * @param body - The exact body posted.
 * @returns `v1,<base64 signature>`.
 */
export async function sign(
  key: Uint8Array,
  id: string,
  timestamp: number,
  body: string,
): Promise<string> {
  const hmac = await crypto.subtle.importKey(
    "raw",
    Uint8Array.from(key),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const content = new TextEncoder().encode(`${id}.${timestamp}.${body}`);
  return `v1,${encodeBase64(new Uint8Array(await crypto.subtle.sign("HMAC", hmac, content)))}`;
}

/** One planned POST. */
export interface PlannedRequest {
  /** Where it goes. */
  url: string;
  /** Its headers, signature included. */
  headers: Record<string, string>;
  /** Its JSON body. */
  body: string;
}

/**
 * Plans the signed POST for one delivery.
 *
 * @param endpoint - Where it goes and the signing key.
 * @param message - The rendered notification.
 * @param destinationId - The host's destination id.
 * @param now - When it is sent.
 * @param version - Plugin version, for `user-agent`.
 * @returns The request.
 */
export async function plan(
  endpoint: Endpoint,
  message: NotificationMessage,
  destinationId: string,
  now: Date,
  version: string,
): Promise<PlannedRequest> {
  const id = webhookId(message.notificationId, destinationId);
  const timestamp = Math.floor(now.getTime() / 1000);
  const body = payload(message, now);
  return {
    url: endpoint.url.href,
    headers: {
      "content-type": "application/json",
      "user-agent": `Bookclerk-Webhook/${version}`,
      "webhook-id": id,
      "webhook-timestamp": String(timestamp),
      "webhook-signature": await sign(endpoint.key, id, timestamp, body),
    },
    body,
  };
}

/**
 * Reads a `Retry-After` value: delta seconds or an HTTP date.
 *
 * @param value - The header, when sent.
 * @param nowMs - The current time, Unix milliseconds.
 * @returns The wait in milliseconds, from 0 to `MAX_RETRY_AFTER_MS`, or
 *   `undefined` when the header is missing or malformed.
 */
export function parseRetryAfter(value: string | null, nowMs: number): number | undefined {
  if (value === null) return undefined;
  const text = value.trim();
  if (/^[0-9]{1,10}$/.test(text)) return Math.min(Number(text) * 1000, MAX_RETRY_AFTER_MS);
  // IMF-fixdate (RFC 9110 section 5.6.7), the only date form a sender may
  // generate: `Sun, 06 Nov 1994 08:49:37 GMT`.
  const date =
    /^(Mon|Tue|Wed|Thu|Fri|Sat|Sun), (\d{2}) (Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec) (\d{4}) (\d{2}):(\d{2}):(\d{2}) GMT$/.exec(
      text,
    );
  if (date === null) return undefined;
  const [, weekday, day, month, year, hour, minute, second] = date;
  const monthIndex = MONTHS.indexOf(month ?? "");
  // setUTCFullYear, unlike Date.UTC, reads years 0 to 99 as written.
  const check = new Date(0);
  check.setUTCFullYear(Number(year), monthIndex, Number(day));
  check.setUTCHours(Number(hour), Number(minute), Number(second), 0);
  const at = check.getTime();
  // An out-of-range field rolls into the next one, so every field must
  // survive the round trip.
  if (
    check.getUTCFullYear() !== Number(year) ||
    check.getUTCMonth() !== monthIndex ||
    check.getUTCDate() !== Number(day) ||
    check.getUTCHours() !== Number(hour) ||
    check.getUTCMinutes() !== Number(minute) ||
    check.getUTCSeconds() !== Number(second) ||
    WEEKDAYS[check.getUTCDay()] !== weekday
  ) {
    return undefined;
  }
  return Math.min(Math.max(at - nowMs, 0), MAX_RETRY_AFTER_MS);
}

/** Weekday abbreviations, Sunday first as `getUTCDay` counts. */
const WEEKDAYS = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/** Month abbreviations in IMF-fixdate order. */
const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/**
 * Turns the receiver's answer into delivered or a failure.
 *
 * - 2xx: delivered.
 * - 429: `rate_limited`, with the `Retry-After` wait when it gave one.
 * - 408, 425 and 5xx: `unavailable`, retried on the job's backoff.
 * - 3xx: `invalid_params`; a redirect is never followed, so a signed body
 *   never reaches a host the operator did not name.
 * - 401 and 403: `forbidden`; 404 and 410: `not_found`; any other 4xx:
 *   `invalid_params`. None of these is retried.
 *
 * @param status - HTTP status.
 * @param retryAfter - The `Retry-After` header, when sent.
 * @param nowMs - The current time, Unix milliseconds.
 * @throws {WebhookFailure} Unless `status` is 2xx.
 */
export function checkResponse(status: number, retryAfter: string | null, nowMs: number): void {
  if (status >= 200 && status < 300) return;
  if (status === 429) {
    throw new WebhookFailure(
      "rate_limited",
      "the receiver answered 429",
      parseRetryAfter(retryAfter, nowMs),
    );
  }
  if (status === 408 || status === 425 || (status >= 500 && status < 600)) {
    throw new WebhookFailure("unavailable", `the receiver answered ${status}`);
  }
  if (status >= 300 && status < 400) {
    throw new WebhookFailure(
      "invalid_params",
      `the receiver redirected (${status}); set its final URL`,
    );
  }
  if (status === 401 || status === 403) {
    throw new WebhookFailure("forbidden", `the receiver refused the request (${status})`);
  }
  if (status === 404 || status === 410) {
    throw new WebhookFailure("not_found", `the receiver has no such endpoint (${status})`);
  }
  throw new WebhookFailure("invalid_params", `the receiver answered ${status}`);
}
