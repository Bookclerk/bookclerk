/**
 * The Cloudflare API for the guided setup: the envelope, accounts, tunnels,
 * zones, DNS records and tunnel tokens.
 *
 * The answers are parsed without I/O ({@link parseEnvelope},
 * {@link parseAccounts}, {@link parseTunnel}, {@link parseTunnels},
 * {@link parseZone}, {@link parseRecords}, {@link parseTunnelToken}) so the
 * fuzz target drives the same code.
 */

/** Cloudflare API base URL. */
export const API_BASE = "https://api.cloudflare.com/client/v4";

/** A request the plugin makes; `globalThis.fetch` outside tests. */
export type Fetch = (input: string, init?: RequestInit) => Promise<Response>;

/** The plugin error code a setup failure maps to. */
export type SetupErrorCode = "invalid_params" | "forbidden" | "unavailable";

/** Why a setup step failed, for the operator. */
export class SetupError extends Error {
  /** The plugin error code to answer with. */
  readonly code: SetupErrorCode;

  /**
   * Creates the error.
   *
   * @param message - What went wrong and what to do; never a credential.
   * @param code - The plugin error code to answer with.
   */
  constructor(message: string, code: SetupErrorCode = "unavailable") {
    super(message);
    this.name = "SetupError";
    this.code = code;
  }
}

/** What one API call came to. */
export type Answer =
  | {
      /** The call succeeded. */
      ok: true;
      /** Its HTTP status. */
      status: number;
      /** Its `result` member. */
      result: unknown;
    }
  | {
      /** The call failed. */
      ok: false;
      /** Its HTTP status. */
      status: number;
      /** Cloudflare's first error message, at most 200 characters, or `""`. */
      message: string;
    };

/** Makes one API call with the operator's token. */
export type Call = (
  path: string,
  init?: {
    /** HTTP method; `GET` when unset. */
    method?: string;
    /** JSON body. */
    body?: unknown;
  },
) => Promise<Answer>;

/** Cloudflare's ids: 32 lowercase hex digits. */
const ID = /^[0-9a-f]{32}$/;

/**
 * Whether `value` is one of Cloudflare's ids.
 *
 * @param value - Candidate.
 * @returns `true` for a string of 32 lowercase hex digits.
 */
export function isId(value: unknown): value is string {
  return typeof value === "string" && ID.test(value);
}

/**
 * Reads an account list.
 *
 * @param result - The list's `result`.
 * @returns The ids of the entries with a valid id, in order.
 * @throws {SetupError} When the result is not a list.
 */
export function parseAccounts(result: unknown): string[] {
  if (!Array.isArray(result)) throw new SetupError("Cloudflare's account list is not a list");
  const ids: string[] = [];
  for (const entry of result as unknown[]) {
    if (typeof entry !== "object" || entry === null) continue;
    const id = (entry as { id?: unknown }).id;
    if (isId(id)) ids.push(id);
  }
  return ids;
}

/** Longest API answer read, in bytes. */
export const MAX_ANSWER_BYTES = 1024 * 1024;

/** What an API answer's body came to. */
export type Envelope =
  | {
      /** The call succeeded. */
      ok: true;
      /** Its `result` member. */
      result: unknown;
    }
  | {
      /** The call failed. */
      ok: false;
      /** Cloudflare's first error message, at most 200 characters, or `""`. */
      message: string;
    };

/**
 * Reads an API answer.
 *
 * @param status - Its HTTP status.
 * @param text - Its body.
 * @returns The result, or the failure's message.
 * @throws {SetupError} When the body is not a JSON API envelope.
 */
export function parseEnvelope(status: number, text: string): Envelope {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    throw new SetupError(`Cloudflare answered ${status} without JSON`);
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new SetupError(`Cloudflare answered ${status} without an API envelope`);
  }
  const envelope = value as { success?: unknown; errors?: unknown; result?: unknown };
  if (status >= 200 && status < 300 && envelope.success === true) {
    return { ok: true, result: envelope.result };
  }
  const first: unknown = Array.isArray(envelope.errors) ? envelope.errors[0] : undefined;
  const message =
    typeof first === "object" && first !== null
      ? (first as { message?: unknown }).message
      : undefined;
  return { ok: false, message: typeof message === "string" ? message.slice(0, 200) : "" };
}

/**
 * Reads a body, stopping past `limit` bytes.
 *
 * @param response - The response.
 * @param limit - Most bytes read.
 * @returns The text, or `null` when the body is longer than `limit`.
 */
export async function readCapped(response: Response, limit: number): Promise<string | null> {
  if (response.body === null) return "";
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    size += value.length;
    if (size > limit) {
      await reader.cancel().catch(() => {});
      return null;
    }
    chunks.push(value);
  }
  const joined = new Uint8Array(size);
  let at = 0;
  for (const chunk of chunks) {
    joined.set(chunk, at);
    at += chunk.length;
  }
  return new TextDecoder().decode(joined);
}

/**
 * Calls the API with one token and reads its answer, whatever the status.
 *
 * @param fetch - The request function.
 * @param token - The API token, sent only to {@link API_BASE}.
 * @param path - Path and query under {@link API_BASE}.
 * @param init - Method and JSON body.
 * @returns The answer.
 * @throws {SetupError} When the API cannot be reached or the body is not an
 *   API envelope.
 */
export async function answer(
  fetch: Fetch,
  token: string,
  path: string,
  init: { method?: string; body?: unknown } = {},
): Promise<Answer> {
  const headers = new Headers({ authorization: `Bearer ${token}` });
  const request: RequestInit = { method: init.method ?? "GET", headers, redirect: "manual" };
  if (init.body !== undefined) {
    headers.set("content-type", "application/json");
    request.body = JSON.stringify(init.body);
  }
  let response: Response;
  try {
    response = await fetch(`${API_BASE}${path}`, request);
  } catch {
    throw new SetupError("could not reach the Cloudflare API");
  }
  const text = await readCapped(response, MAX_ANSWER_BYTES);
  if (text === null) throw new SetupError(`Cloudflare answered ${response.status} at length`);
  return { status: response.status, ...parseEnvelope(response.status, text) };
}

/**
 * The text of a failed answer, for the operator.
 *
 * @param step - What the setup was doing, such as `create the token`.
 * @param answer - The failed answer.
 * @returns `could not <step>: Cloudflare answered <status>[: <message>]`.
 */
export function refusal(step: string, answer: Answer & { ok: false }): string {
  const said = answer.message
    ? `Cloudflare answered ${answer.status}: ${answer.message}`
    : `Cloudflare answered ${answer.status}`;
  return `could not ${step}: ${said}`;
}

/**
 * The plugin error code for a refused call.
 *
 * @param status - The answer's HTTP status.
 * @returns `forbidden` for 401 and 403, else `unavailable`.
 */
export function codeOf(status: number): SetupErrorCode {
  return status === 401 || status === 403 ? "forbidden" : "unavailable";
}

/**
 * The result of an answer that must have succeeded.
 *
 * @param step - What the setup was doing.
 * @param answer - The answer.
 * @returns Its `result`.
 * @throws {SetupError} When it failed.
 */
export function resultOf(step: string, answer: Answer): unknown {
  if (!answer.ok) throw new SetupError(refusal(step, answer), codeOf(answer.status));
  return answer.result;
}

/**
 * Finds the account the setup works in.
 *
 * @param call - Calls the API with the operator's token.
 * @param given - The account id the operator chose, or `""`.
 * @returns `given` when set, else the only account the token reaches.
 * @throws {SetupError} When `given` is malformed, or the token reaches no
 *   account or more than one.
 */
export async function findAccount(call: Call, given: string): Promise<string> {
  if (given !== "") {
    if (!isId(given)) {
      throw new SetupError("account_id must be 32 lowercase hex digits", "invalid_params");
    }
    return given;
  }
  const ids = parseAccounts(resultOf("list your accounts", await call("/accounts?per_page=50")));
  if (ids.length === 1) return ids[0] as string;
  throw new SetupError(
    ids.length === 0
      ? "the token reaches no Cloudflare account"
      : "the token reaches more than one account: set account_id",
    ids.length === 0 ? "forbidden" : "invalid_params",
  );
}

/** A tunnel id: a UUID, lowercase. */
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

/** A token value the host can seal: printable ASCII without spaces. */
const TOKEN_VALUE = /^[\x21-\x7e]{1,1024}$/;

/** A tunnel in the account. */
export interface Tunnel {
  /** Its id: a lowercase UUID. */
  id: string;
  /** Its name. */
  name: string;
}

/** A DNS record at the host name. */
export interface DnsRecord {
  /** Cloudflare's id: 32 lowercase hex digits. */
  id: string;
  /** Record type, as Cloudflare names it (`A`, `CNAME`, ...). */
  type: string;
  /** The record's value. */
  content: string;
  /** Whether Cloudflare proxies it. */
  proxied: boolean;
  /** Its comment, or `""`. */
  comment: string;
}

/**
 * Whether `value` is a tunnel id.
 *
 * @param value - Candidate.
 * @returns `true` for a lowercase UUID.
 */
export function isTunnelId(value: unknown): value is string {
  return typeof value === "string" && UUID.test(value);
}

/**
 * Reads one tunnel.
 *
 * @param result - A tunnel answer's `result`.
 * @returns The tunnel, or `null` when it is not a live tunnel (no usable
 *   id or name, or deleted).
 */
export function parseTunnel(result: unknown): Tunnel | null {
  if (typeof result !== "object" || result === null || Array.isArray(result)) return null;
  const tunnel = result as { id?: unknown; name?: unknown; deleted_at?: unknown };
  if (!isTunnelId(tunnel.id) || typeof tunnel.name !== "string") return null;
  if (tunnel.deleted_at !== undefined && tunnel.deleted_at !== null) return null;
  return { id: tunnel.id, name: tunnel.name };
}

/**
 * Finds a live tunnel by name in a tunnel list.
 *
 * @param result - The list's `result`.
 * @param name - The tunnel's name.
 * @returns The first live tunnel of that name, or `null`.
 * @throws {SetupError} When the result is not a list.
 */
export function parseTunnels(result: unknown, name: string): Tunnel | null {
  if (!Array.isArray(result)) throw new SetupError("Cloudflare's tunnel list is not a list");
  for (const entry of result as unknown[]) {
    const tunnel = parseTunnel(entry);
    if (tunnel !== null && tunnel.name === name) return tunnel;
  }
  return null;
}

/**
 * Finds the zone named `name` in a zone list.
 *
 * @param result - The list's `result`.
 * @param name - The zone name asked for.
 * @returns The zone's id, or `null` when the list holds no active zone of
 *   that name with a valid id.
 * @throws {SetupError} When the result is not a list.
 */
export function parseZone(result: unknown, name: string): string | null {
  if (!Array.isArray(result)) throw new SetupError("Cloudflare's zone list is not a list");
  for (const entry of result as unknown[]) {
    if (typeof entry !== "object" || entry === null) continue;
    const zone = entry as { id?: unknown; name?: unknown; status?: unknown };
    if (isId(zone.id) && zone.name === name && zone.status === "active") return zone.id;
  }
  return null;
}

/**
 * Reads a record list, keeping the records at `name`.
 *
 * @param result - The list's `result`.
 * @param name - The host name.
 * @returns The records named exactly `name` with a valid id, a type and
 *   text content; others are dropped.
 * @throws {SetupError} When the result is not a list.
 */
export function parseRecords(result: unknown, name: string): DnsRecord[] {
  if (!Array.isArray(result)) throw new SetupError("Cloudflare's record list is not a list");
  const out: DnsRecord[] = [];
  for (const entry of result as unknown[]) {
    if (typeof entry !== "object" || entry === null) continue;
    const record = entry as {
      id?: unknown;
      type?: unknown;
      name?: unknown;
      content?: unknown;
      proxied?: unknown;
      comment?: unknown;
    };
    if (
      !isId(record.id) ||
      typeof record.type !== "string" ||
      record.name !== name ||
      typeof record.content !== "string"
    ) {
      continue;
    }
    out.push({
      id: record.id,
      type: record.type,
      content: record.content,
      proxied: record.proxied === true,
      comment: typeof record.comment === "string" ? record.comment : "",
    });
  }
  return out;
}

/**
 * Reads a tunnel token answer.
 *
 * @param result - The answer's `result`: the token itself.
 * @returns The token.
 * @throws {SetupError} When it is not printable ASCII of 1 to 1,024 bytes.
 */
export function parseTunnelToken(result: unknown): string {
  if (typeof result !== "string" || !TOKEN_VALUE.test(result)) {
    throw new SetupError("Cloudflare's tunnel token is not usable");
  }
  return result;
}

/**
 * The zones a host name may be in, longest first: the name, then each
 * parent with at least two labels.
 *
 * @param hostname - The host name.
 * @returns The candidates.
 */
export function zoneCandidates(hostname: string): string[] {
  const labels = hostname.split(".");
  const out: string[] = [];
  for (let i = 0; i + 2 <= labels.length; i++) out.push(labels.slice(i).join("."));
  return out;
}
