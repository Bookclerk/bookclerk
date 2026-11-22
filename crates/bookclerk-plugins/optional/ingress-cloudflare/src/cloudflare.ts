/**
 * Deploys the relay Worker through the Cloudflare API.
 *
 * The operator gives an account id and an API token with the "Workers
 * Scripts: Edit" permission. {@link deployRelay} uploads the relay Worker
 * (with its Durable Object and the derived bearer token as a secret
 * binding), turns on its `workers.dev` route and returns the relay's host
 * name. Uploading again replaces the script and keeps the object's data.
 */

import { RELAY_MODULES } from "./relay-source.ts";

/** Cloudflare API base URL. */
export const API_BASE = "https://api.cloudflare.com/client/v4";

/** Durable Object migration tag the relay is created under. */
export const MIGRATION_TAG = "bookclerk-relay-v1";

/** Compatibility date the relay Worker runs under. */
export const RELAY_COMPATIBILITY_DATE = "2026-08-01";

/** A request the plugin makes; `globalThis.fetch` outside tests. */
export type Fetch = (input: string, init?: RequestInit) => Promise<Response>;

/** Why a deploy failed, for the operator. */
export class DeployError extends Error {
  /**
   * Creates the error.
   *
   * @param message - What went wrong and what to do; never a credential.
   */
  constructor(message: string) {
    super(message);
    this.name = "DeployError";
  }
}

/** Where and as what the relay is deployed. */
export interface RelayTarget {
  /** Cloudflare account id: 32 lowercase hex digits. */
  accountId: string;
  /** Worker script name: a DNS label. */
  workerName: string;
  /** API token, sent only to {@link API_BASE}. */
  apiToken: string;
}

/** A Cloudflare account id. */
const ACCOUNT_ID = /^[0-9a-f]{32}$/;

/** A DNS label: lowercase letters, digits and inner dashes, 1 to 63 long. */
const LABEL = /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/;

/**
 * Whether `id` is a Cloudflare account id.
 *
 * @param id - Candidate id.
 * @returns `true` for 32 lowercase hex digits.
 */
export function isAccountId(id: string): boolean {
  return ACCOUNT_ID.test(id);
}

/**
 * Whether `label` can be one label of a host name (a Worker name or an
 * account's `workers.dev` subdomain).
 *
 * @param label - Candidate label.
 * @returns `true` for a lowercase DNS label that does not start with `xn--`.
 */
export function isDnsLabel(label: string): boolean {
  return LABEL.test(label) && !label.startsWith("xn--");
}

/**
 * The relay's host name on `workers.dev`.
 *
 * @param workerName - The Worker's name.
 * @param subdomain - The account's `workers.dev` subdomain.
 * @returns `<worker>.<subdomain>.workers.dev`.
 * @throws {DeployError} When either part is not a DNS label.
 */
export function relayHost(workerName: string, subdomain: string): string {
  if (!isDnsLabel(workerName)) throw new DeployError("worker_name must be a lowercase DNS label");
  if (!isDnsLabel(subdomain))
    throw new DeployError("Cloudflare returned an unusable workers.dev subdomain");
  return `${workerName}.${subdomain}.workers.dev`;
}

/**
 * Derives the relay's bearer token from the API token, so the plugin finds
 * the same token after a restart without storing it, and a new API token
 * gives a new relay token.
 *
 * @param target - The account, Worker and API token.
 * @returns 43 characters of unpadded base64url (an HMAC-SHA-256).
 */
export async function relayToken(target: RelayTarget): Promise<string> {
  const key = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(target.apiToken),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const message = new TextEncoder().encode(
    `bookclerk-relay-v1\n${target.accountId}\n${target.workerName}`,
  );
  const mac = new Uint8Array(await crypto.subtle.sign("HMAC", key, message));
  let binary = "";
  for (const byte of mac) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/** The envelope every Cloudflare API answer has. */
interface Envelope {
  /** Whether the call succeeded. */
  success?: unknown;
  /** Errors, each with a code and message. */
  errors?: unknown;
  /** The call's result. */
  result?: unknown;
}

/**
 * The first error message of a failed API answer, for the operator.
 *
 * @param envelope - The parsed answer.
 * @param status - Its HTTP status.
 * @returns A short message (Cloudflare's text, at most 200 characters).
 */
function apiError(envelope: Envelope, status: number): string {
  const first = Array.isArray(envelope.errors)
    ? (envelope.errors[0] as { message?: unknown } | undefined)
    : undefined;
  const text = typeof first?.message === "string" ? first.message.slice(0, 200) : "";
  return text ? `Cloudflare answered ${status}: ${text}` : `Cloudflare answered ${status}`;
}

/**
 * Calls the Cloudflare API.
 *
 * @param fetch - The request function.
 * @param target - Supplies the token.
 * @param path - Path under {@link API_BASE}.
 * @param init - Method and body.
 * @returns The `result` member.
 * @throws {DeployError} When the call fails or the answer is not an API
 *   envelope.
 */
async function call(
  fetch: Fetch,
  target: RelayTarget,
  path: string,
  init: RequestInit = {},
): Promise<unknown> {
  const headers = new Headers(init.headers);
  headers.set("authorization", `Bearer ${target.apiToken}`);
  let response: Response;
  try {
    response = await fetch(`${API_BASE}${path}`, { ...init, headers, redirect: "manual" });
  } catch {
    throw new DeployError("could not reach the Cloudflare API");
  }
  let envelope: Envelope;
  try {
    envelope = (await response.json()) as Envelope;
  } catch {
    throw new DeployError(`Cloudflare answered ${response.status} without JSON`);
  }
  if (typeof envelope !== "object" || envelope === null) {
    throw new DeployError(`Cloudflare answered ${response.status} without an API envelope`);
  }
  if (!response.ok || envelope.success !== true) {
    if (response.status === 401 || response.status === 403) {
      throw new DeployError(
        `${apiError(envelope, response.status)}. The API token needs the "Workers Scripts: Edit" permission on this account.`,
      );
    }
    throw new DeployError(apiError(envelope, response.status));
  }
  return envelope.result;
}

/**
 * The migration tag of the account's Worker named `workerName`.
 *
 * @param fetch - The request function.
 * @param target - The account and token.
 * @returns `null` when no such Worker exists; its tag (`""` for none)
 *   otherwise.
 * @throws {DeployError} When the list cannot be read.
 */
async function existingTag(fetch: Fetch, target: RelayTarget): Promise<string | null> {
  const result = await call(fetch, target, `/accounts/${target.accountId}/workers/scripts`);
  if (!Array.isArray(result)) throw new DeployError("Cloudflare's Worker list is not a list");
  for (const entry of result) {
    const script = (entry ?? {}) as { id?: unknown; migration_tag?: unknown };
    if (script.id === target.workerName) {
      return typeof script.migration_tag === "string" ? script.migration_tag : "";
    }
  }
  return null;
}

/**
 * The account's `workers.dev` subdomain.
 *
 * @param fetch - The request function.
 * @param target - The account and token.
 * @returns The subdomain label.
 * @throws {DeployError} When the account has none yet.
 */
async function accountSubdomain(fetch: Fetch, target: RelayTarget): Promise<string> {
  const result = (await call(fetch, target, `/accounts/${target.accountId}/workers/subdomain`)) as {
    subdomain?: unknown;
  } | null;
  const subdomain = result?.subdomain;
  if (typeof subdomain !== "string" || subdomain === "") {
    throw new DeployError(
      "This Cloudflare account has no workers.dev subdomain yet. Open Workers & Pages in the Cloudflare dashboard once to pick one.",
    );
  }
  return subdomain;
}

/**
 * The upload's metadata part.
 *
 * @param token - The relay's bearer token.
 * @param migrate - Whether to create the Durable Object class.
 * @returns The metadata object.
 */
export function uploadMetadata(token: string, migrate: boolean): Record<string, unknown> {
  return {
    main_module: "relay-worker.js",
    compatibility_date: RELAY_COMPATIBILITY_DATE,
    bindings: [
      { type: "durable_object_namespace", name: "RELAY", class_name: "Relay" },
      { type: "secret_text", name: "RELAY_TOKEN", text: token },
    ],
    ...(migrate ? { migrations: { new_tag: MIGRATION_TAG, new_sqlite_classes: ["Relay"] } } : {}),
    observability: { enabled: false },
  };
}

/**
 * Uploads the relay Worker, turns on its `workers.dev` route and returns
 * its host name.
 *
 * @param fetch - The request function.
 * @param target - The account, Worker name and API token.
 * @param token - The relay's bearer token (see {@link relayToken}).
 * @returns The relay's host name.
 * @throws {DeployError} When a call fails, or a Worker of that name exists
 *   that Bookclerk did not create.
 */
export async function deployRelay(
  fetch: Fetch,
  target: RelayTarget,
  token: string,
): Promise<string> {
  checkTarget(target);
  const tag = await existingTag(fetch, target);
  if (tag !== null && tag !== MIGRATION_TAG) {
    throw new DeployError(
      `A Worker named "${target.workerName}" already exists and is not a Bookclerk relay. Set worker_name to another name.`,
    );
  }
  const form = new FormData();
  form.append(
    "metadata",
    new Blob([JSON.stringify(uploadMetadata(token, tag === null))], { type: "application/json" }),
  );
  for (const module of RELAY_MODULES) {
    form.append(
      module.name,
      new Blob([module.source], { type: "application/javascript+module" }),
      module.name,
    );
  }
  const script = `/accounts/${target.accountId}/workers/scripts/${target.workerName}`;
  await call(fetch, target, script, { method: "PUT", body: form });
  await call(fetch, target, `${script}/subdomain`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ enabled: true, previews_enabled: false }),
  });
  return relayHost(target.workerName, await accountSubdomain(fetch, target));
}

/**
 * Refuses settings that cannot name a Worker before any call.
 *
 * @param target - The account, Worker name and API token.
 * @throws {DeployError} When the account id or Worker name is malformed.
 */
function checkTarget(target: RelayTarget): void {
  if (!isAccountId(target.accountId))
    throw new DeployError("account_id must be 32 lowercase hex digits");
  if (!isDnsLabel(target.workerName))
    throw new DeployError("worker_name must be a lowercase DNS label");
}

/**
 * The host an already deployed relay answers on, without deploying it.
 *
 * @param fetch - The request function.
 * @param target - The account, Worker name and API token.
 * @returns The relay's host name.
 * @throws {DeployError} When the settings are malformed or the call fails.
 */
export async function findRelayHost(fetch: Fetch, target: RelayTarget): Promise<string> {
  checkTarget(target);
  return relayHost(target.workerName, await accountSubdomain(fetch, target));
}
