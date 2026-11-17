/**
 * The Audiobookshelf REST calls the plugin makes.
 *
 * Contract pin: `openapi/PIN.md` in this package. Routes the committed
 * OpenAPI document does not describe (`POST /login`, `POST /api/authorize`,
 * `POST /api/libraries/{id}/scan`, `GET /api/users`) follow Audiobookshelf's
 * `server/routers/ApiRouter.js` at the pinned release.
 *
 * `fetch` is passed in so the unit tests can answer it; in the isolate it is
 * the global `fetch`, which the host's egress gate limits to the hosts the
 * operator approved.
 */

import { PROVIDER } from "./settings.ts";

/** The subset of `fetch` the client calls. */
export type Fetch = (url: string, init: RequestInit) => Promise<Response>;

/** Longest a request may take before it is abandoned, in milliseconds. */
export const REQUEST_TIMEOUT_MS = 30_000;

/** Largest response body read, in bytes; a user with a long history is far smaller. */
export const MAX_BODY_BYTES = 16 * 1024 * 1024;

/** Longest error body quoted in a failure message, in characters. */
const MAX_QUOTED_BODY = 512;

/**
 * A failed Audiobookshelf call, with the `PluginError` code the entrypoints
 * report it under.
 */
export class AbsFailure extends Error {
  /** `PluginError` wire code. */
  readonly code: string;
  /** HTTP status Audiobookshelf answered with; 0 when no response came back. */
  readonly status: number;

  /**
   * Creates a failure.
   *
   * @param code - `PluginError` wire code (`unavailable`, `unauthorized`, …).
   * @param message - What went wrong; never holds a credential.
   * @param status - HTTP status, or 0.
   */
  constructor(code: string, message: string, status = 0) {
    super(message);
    this.name = "AbsFailure";
    this.code = code;
    this.status = status;
  }
}

/**
 * Thrown before a request when the invocation's request budget is spent,
 * so a long sync stops cleanly instead of failing on the host's cap.
 */
export class BudgetSpent extends Error {
  /** Creates the error. */
  constructor() {
    super("the request budget for this call is spent");
    this.name = "BudgetSpent";
  }
}

/** How many requests one invocation may still make. */
export class RequestBudget {
  /** Requests left. */
  #left: number;

  /**
   * Creates a budget.
   *
   * @param requests - Requests allowed; `Infinity` for none.
   */
  constructor(requests: number) {
    this.#left = requests;
  }

  /** Requests left. */
  get left(): number {
    return this.#left;
  }

  /**
   * Spends one request.
   *
   * @throws {BudgetSpent} When none is left.
   */
  spend(): void {
    if (this.#left < 1) throw new BudgetSpent();
    this.#left -= 1;
  }
}

/** An Audiobookshelf user as `GET /api/users` and `POST /login` list it. */
export interface AbsUser {
  /** User id. */
  readonly id: string;
  /** Login name; empty when Audiobookshelf sent none. */
  readonly username: string;
  /** The user's API token (`POST /login` only). */
  readonly token: string | null;
}

/** One progress row of `GET /api/users/{id}` (`mediaProgress[]`). */
export interface AbsMediaProgress {
  /** Library item the row tracks. */
  readonly libraryItemId: string;
  /** Item duration in seconds. */
  readonly duration: number | null;
  /** Fraction listened, 0 to 1. */
  readonly progress: number | null;
  /** Position in seconds. */
  readonly currentTime: number | null;
  /** Whether the user finished the item. */
  readonly isFinished: boolean;
  /** When the row last changed, UTC Unix milliseconds. */
  readonly lastUpdate: number | null;
}

/** `GET /api/users/{id}`: the user and their listening progress. */
export interface AbsUserDetail {
  /** User id. */
  readonly id: string;
  /** Progress rows; empty when Audiobookshelf sent none. */
  readonly mediaProgress: readonly AbsMediaProgress[];
}

/** Matching fields of `GET /api/items/{id}` (`media.metadata`). */
export interface AbsItemMetadata {
  /** Title. */
  readonly title: string | null;
  /** Display author line. */
  readonly authorName: string | null;
  /** Amazon ASIN, as Audiobookshelf stored it. */
  readonly asin: string | null;
  /** ISBN. */
  readonly isbn: string | null;
}

/** An Audiobookshelf library (`GET /api/libraries`). */
export interface AbsLibrary {
  /** Library id. */
  readonly id: string;
  /** Display name. */
  readonly name: string;
}

/** A user that signed in with Audiobookshelf credentials. */
export interface SignedInUser {
  /** Always `audiobookshelf`. */
  readonly provider: string;
  /** Audiobookshelf user id. */
  readonly externalUserId: string;
  /** Login name. */
  readonly displayName: string;
  /** The user's Audiobookshelf token, when the server sent one. */
  readonly accessToken: string | null;
}

/**
 * Whether `hostport` names loopback (`localhost`, `127.0.0.1`, `::1`), with
 * an optional port.
 *
 * @param hostport - Host with optional `:port`; IPv6 in brackets.
 * @returns `true` for loopback.
 */
export function isLoopbackHostport(hostport: string): boolean {
  let host = hostport;
  if (hostport.startsWith("[")) {
    host = hostport.slice(1).split("]")[0] ?? "";
  } else {
    const colon = hostport.lastIndexOf(":");
    if (colon > 0 && /^[0-9]*$/.test(hostport.slice(colon + 1))) host = hostport.slice(0, colon);
  }
  return host.toLowerCase() === "localhost" || host === "127.0.0.1" || host === "::1";
}

/**
 * Checks and normalizes the configured server address.
 *
 * `http` and `https` are accepted in any case; plain `http` only for a
 * loopback host, so an API key never crosses a network in the clear.
 *
 * @param raw - `base_url` as configured.
 * @returns The address without trailing slashes.
 * @throws {AbsFailure} `invalid_params` when it is missing, not `http(s)`,
 *   or plain `http` to a host that is not loopback.
 */
export function checkBaseUrl(raw: string): string {
  const base = raw.trim().replace(/\/+$/, "");
  if (base === "") {
    throw new AbsFailure("invalid_params", "the audiobookshelf `base_url` setting is required");
  }
  const sep = base.indexOf("://");
  const scheme = sep < 0 ? "" : base.slice(0, sep).toLowerCase();
  if (scheme !== "http" && scheme !== "https") {
    throw new AbsFailure("invalid_params", "the audiobookshelf `base_url` must be http(s)");
  }
  // The host `fetch` will contact, as the URL standard parses it: a
  // hand-rolled split disagrees on input such as `\` before `@`.
  let parsed: URL;
  try {
    parsed = new URL(base);
  } catch {
    throw new AbsFailure("invalid_params", "the audiobookshelf `base_url` is not a URL");
  }
  if (parsed.hostname === "") {
    throw new AbsFailure("invalid_params", "the audiobookshelf `base_url` has no host");
  }
  if (scheme === "https") return base;
  if (!isLoopbackHostport(parsed.hostname)) {
    throw new AbsFailure(
      "invalid_params",
      "the audiobookshelf `base_url` must use https unless the host is loopback",
    );
  }
  return base;
}

/**
 * A string field of an object, or `null`.
 *
 * @param source - Anything.
 * @param key - Field name.
 * @returns The string, or `null` when missing or not a string.
 */
function str(source: unknown, key: string): string | null {
  if (source === null || typeof source !== "object") return null;
  const value = (source as Record<string, unknown>)[key];
  return typeof value === "string" ? value : null;
}

/**
 * A finite number field of an object, or `null`.
 *
 * @param source - Anything.
 * @param key - Field name.
 * @returns The number, or `null` when missing or not a finite number.
 */
function num(source: unknown, key: string): number | null {
  if (source === null || typeof source !== "object") return null;
  const value = (source as Record<string, unknown>)[key];
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

/**
 * An array field of an object.
 *
 * @param source - Anything.
 * @param key - Field name.
 * @returns The array, or empty when missing or not an array.
 */
function list(source: unknown, key: string): readonly unknown[] {
  if (source === null || typeof source !== "object") return [];
  const value = (source as Record<string, unknown>)[key];
  return Array.isArray(value) ? value : [];
}

/**
 * Reads a user object.
 *
 * @param raw - One element of `users[]`, or `login.user`.
 * @returns The user, or `null` without a string `id`.
 */
export function userFrom(raw: unknown): AbsUser | null {
  const id = str(raw, "id");
  if (id === null) return null;
  return { id, username: str(raw, "username") ?? "", token: str(raw, "token") };
}

/**
 * Reads `GET /api/users/{id}`.
 *
 * @param raw - The decoded body.
 * @returns The user's progress rows; rows without a string
 *   `libraryItemId` are skipped.
 * @throws {AbsFailure} `unavailable` without a string `id`.
 */
export function userDetailFrom(raw: unknown): AbsUserDetail {
  const id = str(raw, "id");
  if (id === null) throw new AbsFailure("unavailable", "Audiobookshelf sent a user without an id");
  const mediaProgress: AbsMediaProgress[] = [];
  for (const row of list(raw, "mediaProgress")) {
    const libraryItemId = str(row, "libraryItemId");
    if (libraryItemId === null) continue;
    const finished = (row as Record<string, unknown>)["isFinished"];
    mediaProgress.push({
      libraryItemId,
      duration: num(row, "duration"),
      progress: num(row, "progress"),
      currentTime: num(row, "currentTime"),
      isFinished: finished === true,
      lastUpdate: num(row, "lastUpdate"),
    });
  }
  return { id, mediaProgress };
}

/**
 * Reads the matching fields of `GET /api/items/{id}`.
 *
 * @param raw - The decoded body.
 * @returns The fields; each is `null` when Audiobookshelf has none.
 */
export function itemMetadataFrom(raw: unknown): AbsItemMetadata {
  const media =
    raw !== null && typeof raw === "object" ? (raw as Record<string, unknown>)["media"] : null;
  const meta =
    media !== null && typeof media === "object"
      ? (media as Record<string, unknown>)["metadata"]
      : null;
  // The minified and expanded item carry `authorName`; the plain one
  // carries only `authors[]`.
  const authors = list(meta, "authors")
    .map((author) => str(author, "name"))
    .filter((name): name is string => name !== null && name.trim() !== "");
  return {
    title: str(meta, "title"),
    authorName: str(meta, "authorName") ?? (authors.length > 0 ? authors.join(", ") : null),
    asin: str(meta, "asin"),
    isbn: str(meta, "isbn"),
  };
}

/**
 * Reads a response body, stopping as soon as it passes `max` bytes.
 *
 * @param response - The response to read.
 * @param max - Most bytes accepted.
 * @returns The body, or `undefined` when it is larger than `max`.
 */
export async function readCapped(response: Response, max: number): Promise<Uint8Array | undefined> {
  const reader = response.body?.getReader();
  if (reader === undefined) return new Uint8Array();
  const chunks: Uint8Array[] = [];
  let total = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    if (total > max) {
      await reader.cancel();
      return undefined;
    }
    chunks.push(value);
  }
  const out = new Uint8Array(total);
  let at = 0;
  for (const chunk of chunks) {
    out.set(chunk, at);
    at += chunk.byteLength;
  }
  return out;
}

/**
 * The `PluginError` code for an error status.
 *
 * @param status - HTTP status.
 * @returns `unauthorized` for 401, `forbidden` for 403, `not_found` for
 *   404, `rate_limited` for 429, `invalid_params` for another 4xx, else
 *   `unavailable`.
 */
export function codeForStatus(status: number): string {
  if (status === 401) return "unauthorized";
  if (status === 403) return "forbidden";
  if (status === 404) return "not_found";
  if (status === 429) return "rate_limited";
  if (status >= 400 && status < 500) return "invalid_params";
  return "unavailable";
}

/** One request to send. */
interface Call {
  /** HTTP method. */
  readonly method: "GET" | "POST";
  /** Path under the base URL, with its query. */
  readonly path: string;
  /** Whether to send the API key. */
  readonly auth: boolean;
  /** JSON body. */
  readonly body?: unknown;
}

/** Thin Audiobookshelf REST client with Bearer auth. */
export class AbsClient {
  /** Scheme and host, without trailing slashes. */
  readonly baseUrl: string;
  /** API key sent as a Bearer token. */
  readonly #apiKey: string;
  /** How requests are sent. */
  readonly #fetch: Fetch;
  /** Requests this invocation may still make. */
  readonly budget: RequestBudget;

  /**
   * Creates a client.
   *
   * @param baseUrl - `base_url` as configured.
   * @param apiKey - API key.
   * @param fetcher - `fetch` to call.
   * @param budget - Requests the invocation may make.
   * @throws {AbsFailure} `invalid_params` for a base URL
   *   {@link checkBaseUrl} refuses.
   */
  constructor(
    baseUrl: string,
    apiKey: string,
    fetcher: Fetch,
    budget = new RequestBudget(Infinity),
  ) {
    this.baseUrl = checkBaseUrl(baseUrl);
    this.#apiKey = apiKey;
    this.#fetch = fetcher;
    this.budget = budget;
  }

  /**
   * Sends one request and returns its response.
   *
   * Redirects are not followed: a 307 or 308 would resend the body
   * (`/login` passwords included) and could leave HTTPS.
   *
   * @param call - What to send.
   * @returns The response, whatever its status.
   * @throws {BudgetSpent} When the invocation's budget is spent.
   * @throws {AbsFailure} `unavailable` when the request fails or times out.
   */
  async #send(call: Call): Promise<Response> {
    this.budget.spend();
    const headers: Record<string, string> = { accept: "application/json" };
    if (call.auth) headers["authorization"] = `Bearer ${this.#apiKey}`;
    const init: RequestInit = {
      method: call.method,
      headers,
      redirect: "manual",
      signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    };
    if (call.body !== undefined) {
      headers["content-type"] = "application/json";
      init.body = JSON.stringify(call.body);
    }
    try {
      return await this.#fetch(`${this.baseUrl}${call.path}`, init);
    } catch (err) {
      const reason = err instanceof Error ? err.message : String(err);
      throw new AbsFailure("unavailable", `Audiobookshelf request failed: ${reason}`);
    }
  }

  /**
   * Reads a body as text within the size cap.
   *
   * @param response - The response.
   * @returns The body.
   * @throws {AbsFailure} `unavailable` when it is too large or the
   *   connection drops.
   */
  static async #text(response: Response): Promise<string> {
    let bytes: Uint8Array | undefined;
    try {
      bytes = await readCapped(response, MAX_BODY_BYTES);
    } catch (err) {
      const reason = err instanceof Error ? err.message : String(err);
      throw new AbsFailure("unavailable", `Audiobookshelf response failed: ${reason}`);
    }
    if (bytes === undefined) {
      throw new AbsFailure("unavailable", "Audiobookshelf sent a response that is too large");
    }
    return new TextDecoder().decode(bytes);
  }

  /**
   * Turns an error status into a failure that quotes the start of the body.
   *
   * @param response - A response that is not a success.
   * @returns The failure.
   */
  static async #failure(response: Response): Promise<AbsFailure> {
    let body = "";
    try {
      body = (await AbsClient.#text(response)).trim().slice(0, MAX_QUOTED_BODY);
    } catch {
      // The status alone still says what happened.
    }
    const detail = body === "" ? `HTTP ${response.status}` : `HTTP ${response.status}: ${body}`;
    return new AbsFailure(
      codeForStatus(response.status),
      `Audiobookshelf answered ${detail}`,
      response.status,
    );
  }

  /**
   * Sends a request and decodes its JSON body.
   *
   * @param call - What to send.
   * @returns The decoded body.
   * @throws {AbsFailure} For a failed request, an error status, or a body
   *   that is too large or not JSON.
   */
  async #json(call: Call): Promise<unknown> {
    const response = await this.#send(call);
    if (response.status < 200 || response.status > 299) throw await AbsClient.#failure(response);
    const text = await AbsClient.#text(response);
    try {
      return JSON.parse(text) as unknown;
    } catch {
      throw new AbsFailure("unavailable", "Audiobookshelf sent a response that is not JSON");
    }
  }

  /**
   * `POST /api/authorize`: checks the API key.
   *
   * @returns The user the key belongs to, or `null` when the reply names none.
   * @throws {AbsFailure} When the call fails.
   */
  async authorize(): Promise<AbsUser | null> {
    const body = await this.#json({ method: "POST", path: "/api/authorize", auth: true });
    return userFrom(
      body !== null && typeof body === "object" ? (body as Record<string, unknown>)["user"] : null,
    );
  }

  /**
   * `POST /login`: signs a person in with their Audiobookshelf credentials.
   * The password is sent only in the body and never logged.
   *
   * @param username - Login name.
   * @param password - Password.
   * @returns The signed-in user.
   * @throws {AbsFailure} When the call fails, or `unauthorized` when the
   *   reply names no user.
   */
  async authenticateUser(username: string, password: string): Promise<SignedInUser> {
    const body = await this.#json({
      method: "POST",
      path: "/login",
      auth: false,
      body: { username, password },
    });
    const user = userFrom(
      body !== null && typeof body === "object" ? (body as Record<string, unknown>)["user"] : null,
    );
    if (user === null) {
      throw new AbsFailure("unauthorized", "the Audiobookshelf login reply has no user");
    }
    return {
      provider: PROVIDER,
      externalUserId: user.id,
      displayName: user.username,
      accessToken: user.token,
    };
  }

  /**
   * `GET /api/users` (admin): every user on the server.
   *
   * @returns The users; entries without an id are skipped.
   * @throws {AbsFailure} When the call fails.
   */
  async listUsers(): Promise<AbsUser[]> {
    const body = await this.#json({ method: "GET", path: "/api/users", auth: true });
    return list(body, "users")
      .map(userFrom)
      .filter((user): user is AbsUser => user !== null);
  }

  /**
   * `GET /api/users/{id}`: one user with their listening progress.
   *
   * @param userId - User id.
   * @returns The user's progress.
   * @throws {AbsFailure} When the call fails.
   */
  async getUser(userId: string): Promise<AbsUserDetail> {
    return userDetailFrom(
      await this.#json({
        method: "GET",
        path: `/api/users/${encodeURIComponent(userId)}`,
        auth: true,
      }),
    );
  }

  /**
   * `GET /api/items/{id}`: the matching fields of one library item.
   *
   * @param itemId - Library item id.
   * @returns Title, author line, ASIN and ISBN.
   * @throws {AbsFailure} When the call fails.
   */
  async getItemMetadata(itemId: string): Promise<AbsItemMetadata> {
    return itemMetadataFrom(
      await this.#json({
        method: "GET",
        path: `/api/items/${encodeURIComponent(itemId)}`,
        auth: true,
      }),
    );
  }

  /**
   * `GET /api/libraries`: the libraries the key can see.
   *
   * @returns The libraries; entries without a string id and name are skipped.
   * @throws {AbsFailure} When the call fails.
   */
  async listLibraries(): Promise<AbsLibrary[]> {
    const body = await this.#json({ method: "GET", path: "/api/libraries", auth: true });
    const out: AbsLibrary[] = [];
    for (const raw of list(body, "libraries")) {
      const id = str(raw, "id");
      const name = str(raw, "name");
      if (id !== null && name !== null) out.push({ id, name });
    }
    return out;
  }

  /**
   * `POST /api/libraries/{id}/scan`: asks the server to rescan a library.
   *
   * @param libraryId - Library id.
   * @param force - Whether to ask for a forced rescan (`?force=1`).
   * @throws {AbsFailure} When the call fails or answers anything but 200
   *   or 204.
   */
  async scanLibrary(libraryId: string, force: boolean): Promise<void> {
    const query = force ? "?force=1" : "";
    const response = await this.#send({
      method: "POST",
      path: `/api/libraries/${encodeURIComponent(libraryId)}/scan${query}`,
      auth: true,
    });
    if (response.status !== 200 && response.status !== 204)
      throw await AbsClient.#failure(response);
    await response.body?.cancel();
  }
}
