import { normalizeThemePreference, type ThemePreference } from "@/lib/theme";
import { coversSettled } from "@/lib/coverLoads";
import { createLookupBatcher } from "@/lib/lookupBatcher";
import { clearLookupCaches, createLookupCache, lookupGeneration } from "@/lib/lookupCache";

/**
 * Pipeline acquire state for a library book row.
 */
export type AcquireStatus = "not_acquired" | "queued" | "downloading" | "acquired" | "error";

/**
 * Primary authenticated SPA views mirrored in the URL path.
 */
export type AppView = "discover" | "library" | "accounts" | "wishlist" | "status" | "settings";
/**
 * Session role returned by the daemon auth APIs.
 */
export type AuthRole = "operator" | "owner" | "administrator" | "member";

/**
 * Linked portal identity attached to a user session.
 */
export interface PortalInfo {
  identity_id: number;
  provider: string;
  external_user_id: string;
  label: string | null;
}

/**
 * Signed-in administrator or member profile from `/api/auth/me`.
 */
export interface AuthMeUser {
  id: number;
  role: "administrator" | "member" | string;
  display_name: string | null;
  login_name?: string | null;
  status?: string;
  email?: string | null;
  has_password?: boolean;
  /** True when a profile picture file is stored for this user. */
  has_avatar?: boolean;
  /** `auto`, `monogram`, `gravatar`, `upload`, or `sso:{id}`. */
  avatar_source?: string | null;
  /** SHA-256 hex of the contact email for Gravatar. */
  gravatar_hash?: string | null;
  /** IdP-supplied pictures the user can pick. */
  sso_pictures?: {
    identity_id: number;
    provider: string;
    picture_url: string;
    /** Same-origin 128 px copy of `picture_url`, when the server signed one. */
    picture_path?: string | null;
    last_used_at?: string | null;
  }[];
}

/**
 * Active impersonation target when an administrator is viewing as another user.
 */
export interface AuthMeImpersonating {
  user_id: number;
  display_name: string | null;
}

/**
 * Host MFA policy plus this user's TOTP / passkey enrollment.
 */
export interface AuthSecondFactor {
  required: boolean;
  totp: boolean;
  passkey_count: number;
  enrolled: boolean;
}

/**
 * Normalized client session used to gate SPA views and actions.
 */
export interface AuthSession {
  authenticated: boolean;
  role?: AuthRole;
  default_view: AppView;
  can_acquire: boolean;
  elevated?: boolean;
  /**
   * Whether the session reaches node administration and the Status page:
   * the operator, an Owner or Administrator, or a Member who owns a node.
   */
  administers_nodes?: boolean;
  impersonating?: AuthMeImpersonating;
  portal?: PortalInfo;
  user?: AuthMeUser;
  second_factor?: AuthSecondFactor;
  /**
   * Whether the caller may see prices and store links. When false the
   * library already drops them from its answers, and the app shows no
   * store links of its own either.
   */
  see_prices: boolean;
}

/** What a work is: a book, or a podcast (its show). */
export type WorkKind = "book" | "podcast";

/** How an edition is consumed. */
export type MediaKind = "audio" | "ebook" | "print";

/** How an expression renders its work. */
export type ExpressionKind = "text" | "narration" | "dramatisation" | "graphic" | "original_audio";

/** Whether an expression is abridged. */
export type Abridgement = "unknown" | "unabridged" | "abridged";

/** How a holder came to hold a listing. */
export type Entitlement = "purchase" | "plus" | "loan" | "free";

/** How `/api/library/works` orders a page (ignored while searching). */
export type WorkSort = "title" | "author" | "series" | "purchased" | "added" | "length";

/**
 * What `facet` and `value` filter works by: a value list of the page index,
 * the caller's own tags, or their progress (`finished` / `not_finished`).
 */
export type WorkFacet =
  "author" | "narrator" | "series" | "genre" | "publisher" | "source" | "tag" | "progress";

/** One value of a facet, from `GET /api/library/works/facets/{facet}`. */
export interface FacetValue {
  /** The value, as {@link WorksQuery.value} takes it. */
  value: string;
  /** How many held works have it. */
  works: number;
}

/** A page of a facet's values and the cursor of the next one. */
export interface FacetValuesPage {
  /** The page's values, by their lower-cased spelling. */
  values: FacetValue[];
  /** Pass as `cursor` for the next page; absent on the last. */
  next_cursor?: string | null;
}

/**
 * One work row from `GET /api/library/works`: a book or podcast once,
 * however many editions and accounts hold it.
 */
export interface WorkSummary {
  /** Work id; opens the detail at `/api/library/works/{id}`. */
  id: string;
  /** Book or podcast. */
  kind: WorkKind;
  /** Display title. */
  title: string;
  /** Authors (or hosts), comma-separated, when known. */
  authors: string | null;
  /** Series name when the work is in one. */
  series: string | null;
  /** Position within the series (text, so `1.5` keeps its spelling). */
  series_position: string | null;
  /** Edition whose cover the row shows. */
  cover_edition_id: string | null;
  /** Same-origin path of that cover's processed images; see {@link coverSrc}. */
  cover_path: string | null;
  /** Media kinds the work's editions come in. */
  media_kinds: MediaKind[];
  /** The work's acquire state, summed over its editions. */
  acquire_status: AcquireStatus;
  /** Editions of the work in the library. */
  edition_count: number;
  /** Holdings of the work's listings. */
  holding_count: number;
  /** When the newest holding was added (ISO-8601). */
  added_at: string;
  /** Runtime in minutes of the longest held edition, when known. */
  length_minutes?: number | null;
  /** When the newest purchase a store reported happened (ISO-8601). */
  purchased_at?: string | null;
}

/** A page of works and the cursor of the next one. */
export interface WorksPage {
  /** The page's works. */
  works: WorkSummary[];
  /** Pass as `cursor` for the next page; absent on the last. */
  next_cursor?: string | null;
}

/** Who holds a listing: a store account, or a branch library's node. */
export type Holder = AccountHolder | NodeHolder;

/** A listing held by a store account. */
export interface AccountHolder {
  /** Discriminant. */
  kind: "account";
  /** The account's storefront plugin (`audible`, `libro`, ...). */
  plugin_key: string;
  /** The account's id on that storefront. */
  account_id: string;
}

/** A listing held by a branch library's node. */
export interface NodeHolder {
  /** Discriminant. */
  kind: "node";
  /** The holding node's id. */
  node_id: string;
}

/** One holding of a store listing. */
export interface Holding {
  /** Holding id. */
  id: string;
  /** Who holds it. */
  holder: Holder;
  /** How it is held. */
  entitlement: Entitlement;
  /** When the store says it was bought (ISO-8601), when known. */
  purchased_at: string | null;
  /** When the library first saw it (ISO-8601). */
  added_at: string;
}

/** One store listing of an edition. */
export interface Listing {
  /** Listing id. */
  id: string;
  /** Storefront plugin (`audible`, `libro`, …). */
  plugin_key: string;
  /** Marketplace code (`us`, `uk`, …). */
  marketplace: string;
  /** The store's product id (ASIN, ISBN, …). */
  product_id: string;
  /** The listing's store page, when known. */
  url: string | null;
  /** Who holds it. */
  holdings: Holding[];
}

/** One edition of a work, with its listings. */
export interface Edition {
  /** Edition id; acquire takes it in `editions`. */
  id: string;
  /** The work it belongs to (an episode for a podcast's editions). */
  work_id: string;
  /** Its expression. */
  expression_id: string;
  /** How the expression renders the work. */
  expression_kind: ExpressionKind;
  /** Language code, when known. */
  language: string | null;
  /** Whether it is abridged. */
  abridgement: Abridgement;
  /** How it is consumed. */
  media_kind: MediaKind;
  /** Edition title. */
  title: string;
  /** Subtitle, when known. */
  subtitle: string | null;
  /** Publication date (ISO-8601), when known. */
  published_at: string | null;
  /** Runtime in minutes, when known. */
  duration_minutes: number | null;
  /** Same-origin path of its processed cover; see {@link coverSrc}. */
  cover_path: string | null;
  /** Its acquire state. */
  acquire_status: AcquireStatus;
  /** Store listings that sell it. */
  listings: Listing[];
}

/** The caller's own state for a work. */
export interface WorkState {
  /** Whether they finished it. */
  is_finished: boolean;
  /** Overall rating (0–5). */
  rating_overall: number | null;
  /** Performance rating (0–5). */
  rating_performance: number | null;
  /** Story rating (0–5). */
  rating_story: number | null;
  /** Space-separated tags. */
  tags: string | null;
}

/** One work from `GET /api/library/works/{id}`. */
export interface WorkDetail {
  /** Work id. */
  id: string;
  /** Book or podcast. */
  kind: WorkKind;
  /** Display title. */
  title: string;
  /** Description, when known (may contain light HTML). */
  description: string | null;
  /** Authors (or hosts). */
  authors: string[];
  /** Series name, when in one. */
  series: string | null;
  /** Position within the series. */
  series_position: string | null;
  /** Its editions. */
  editions: Edition[];
  /** The caller's own state; absent when they recorded none. */
  state?: WorkState | null;
  /** Whether the caller can keep their own state (false for the operator token). */
  can_edit_state?: boolean;
}

/**
 * A change to the caller's own state for a work. A field left out keeps
 * its value; `null` clears a rating or the tags.
 */
export interface WorkStateEdit {
  /** Whether they finished it. */
  is_finished?: boolean;
  /** Overall rating (0–5). */
  rating_overall?: number | null;
  /** Performance rating (0–5). */
  rating_performance?: number | null;
  /** Story rating (0–5). */
  rating_story?: number | null;
  /** Whitespace-separated tags (at most 32, each at most 64 bytes). */
  tags?: string | null;
}

/**
 * Aggregate library counters from `/api/status`.
 */
export interface StatusResponse {
  accounts: number;
  books: number;
  acquired: number;
  pending: number;
  error: number;
  in_progress: number;
  listen: string;
  storage_backend: string;
  events?: EventQueueStatus;
}

/**
 * Durable domain-event delivery-queue counters from `/api/status`.
 */
export interface EventQueueStatus {
  pending: number;
  running: number;
  suspended: number;
  dead_letter: number;
  acked: number;
  oldest_pending_age_secs: number | null;
  retries_total?: number;
  suspensions_total?: number;
  dead_letters_total?: number;
  dispatch_latency_ms_avg?: number | null;
  handler_latency_ms_avg?: number | null;
}

/**
 * Operator delivery row from `/api/events/deliveries`.
 */
export interface EventDeliveryInfo {
  id: string;
  eventId: string;
  pluginId: string;
  state: string;
  attemptCount: number;
  leaseGeneration: number;
  invocationSequence: number;
  outcome: string | null;
  errorMessage: string | null;
  runAfter: string;
  updatedAt: string;
  cancelRequested: boolean;
  resourceClass: string;
  resumePending: boolean;
  hasCheckpoint: boolean;
}

/**
 * Background job row from `/api/jobs`.
 */
export interface JobInfo {
  id: string;
  kind: string;
  status: string;
  detail: string | null;
  progress?: string | null;
  attempt_count?: number;
  max_attempts?: number;
  error_kind?: string | null;
  created_at?: string | null;
  finished_at?: string | null;
}

/**
 * Ack payload for scan / acquire job triggers.
 */
export interface ActionResponse {
  ok: boolean;
  message: string;
  job_id: string;
  error?: string | null;
}

/**
 * Storefront brand colors and logo URL for Accounts UI.
 */
export interface PortalBrand {
  bg: string;
  fg: string;
  accent: string;
  logo: string;
}

/**
 * Connectable storefront advertised by `/api/portal/sources`.
 */
export interface PortalSource {
  id: string;
  name: string;
  auth: string;
  brand: PortalBrand;
}

/**
 * Linked store account for the current portal user.
 */
export interface PortalConnection {
  account_id: string;
  /** Provenance-qualified PluginKey that owns the account. */
  plugin_key: string;
  source: string;
  label: string | null;
  connection_status: string;
  source_enabled: boolean;
  brand?: PortalBrand;
}

/**
 * Portal identity from `/api/portal/me`.
 */
export interface PortalMe {
  provider: string;
  external_user_id: string;
  label: string | null;
}

const ANON_SESSION: AuthSession = {
  authenticated: false,
  default_view: "discover",
  can_acquire: false,
  see_prices: false,
};

/**
 * HTTP failure from Bookclerk JSON APIs, including optional machine `code`.
 */
export class ApiError extends Error {
  status: number;
  /** Machine slug from JSON error bodies when present. */
  code?: string;
  pluginId?: string;
  summary?: string[];

  constructor(
    status: number,
    message: string,
    extras?: { code?: string; pluginId?: string; summary?: string[] },
  ) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.code = extras?.code;
    this.pluginId = extras?.pluginId;
    this.summary = extras?.summary;
  }
}

/**
 * Narrows `unknown` to {@link ApiError}.
 *
 * @param error - Caught value from a failed API call.
 * @returns Whether `error` is an {@link ApiError} instance.
 */
export function isApiError(error: unknown): error is ApiError {
  return error instanceof ApiError;
}

/**
 * Maps API / network failures to short copy for non-technical users.
 *
 * Keeps branded daemon messages when they already read as plain language.
 *
 * @param error - Caught failure from fetch / parse helpers.
 * @param fallback - Copy used when the error has no safe user-facing text.
 * @returns Message suitable for inline SPA error banners.
 */
export function userFacingApiError(error: unknown, fallback: string): string {
  if (isApiError(error)) {
    switch (error.status) {
      case 414:
        return "This search got too large to continue. Try narrowing with filters, or start a more specific search.";
      case 408:
      case 504:
        return error.message.trim() || "That took too long. Try again in a moment.";
      case 429:
        return "Too many requests right now — wait a moment and try again.";
      case 503:
        return "The catalog is temporarily unavailable. Try again shortly.";
      default:
        break;
    }
    const msg = error.message.trim();
    // Bare HTTP status lines ("414 URI Too Long") are not useful in the UI.
    if (!msg || /^\d{3}\b/.test(msg) || /URI Too Long/i.test(msg)) {
      return fallback;
    }
    if (msg.length <= 160) return msg;
    return fallback;
  }
  if (error instanceof Error) {
    const msg = error.message.trim();
    if (msg && !/^\d{3}\b/.test(msg) && !/URI Too Long/i.test(msg) && msg.length <= 160) {
      return msg;
    }
  }
  return fallback;
}

function normalizeView(raw: string | undefined): AppView {
  if (
    raw === "library" ||
    raw === "accounts" ||
    raw === "discover" ||
    raw === "wishlist" ||
    raw === "status" ||
    raw === "settings"
  ) {
    return raw;
  }
  return "discover";
}

function normalizeRole(raw: string | undefined): AuthRole | undefined {
  if (raw === "operator" || raw === "owner" || raw === "administrator" || raw === "member") {
    return raw;
  }
  // Legacy portal sessions map to member.
  if (raw === "portal") return "member";
  return undefined;
}

async function parseJson<T>(res: Response): Promise<T> {
  if (!res.ok) {
    const text = await res.text().catch(() => "");
    let message = text || `${res.status} ${res.statusText}`;
    let code: string | undefined;
    let pluginId: string | undefined;
    let summary: string[] | undefined;
    try {
      const body = JSON.parse(text) as {
        error?: string;
        message?: string;
        plugin_id?: string;
        summary?: string[];
      };
      // Prefer the human-readable `message` (branded errors, login throttle)
      // over the machine slug in `error`.
      message = body.message?.trim() || body.error?.trim() || message;
      code = typeof body.error === "string" ? body.error : undefined;
      pluginId = typeof body.plugin_id === "string" ? body.plugin_id : undefined;
      summary = Array.isArray(body.summary)
        ? body.summary.filter((line): line is string => typeof line === "string")
        : undefined;
    } catch {
      // keep raw text
    }
    throw new ApiError(res.status, message, { code, pluginId, summary });
  }
  return res.json() as Promise<T>;
}

/**
 * Parses scan/acquire admission: 200 created, 409 existing job, other errors throw.
 *
 * @param res - Fetch response from a job-enqueue endpoint.
 * @returns Action ack including `job_id` for both created and duplicate admits.
 */
async function parseJobAdmit(res: Response): Promise<ActionResponse> {
  if (res.status === 409) {
    const body = (await res.json()) as ActionResponse;
    return {
      ok: true,
      message: body.message,
      job_id: body.job_id,
      error: body.error ?? "conflict",
    };
  }
  return parseJson(res);
}

/** Most `Retry-After` waits {@link fetchRetryingBusy} honors, in seconds. */
const MAX_RETRY_AFTER_SECONDS = 10;

/** How many times {@link fetchRetryingBusy} asks again after a 503. */
const BUSY_RETRIES = 4;

/**
 * Fetches a read, asking again while the daemon answers 503 with
 * `Retry-After`.
 *
 * The daemon answers that way when it could not check the session yet (a
 * busy library, as on a small server that is still starting), which is not
 * the same as being signed out. Only for GET requests, which are safe to
 * repeat.
 *
 * @param input - Request URL.
 * @param init - Request options; the method must be GET.
 * @param fetcher - Performs one request (defaults to `fetch`).
 * @param sleep - Waits the given milliseconds (defaults to a timer).
 * @returns The first response that is not such a 503, or the last 503.
 */
export async function fetchRetryingBusy(
  input: string,
  init: RequestInit | undefined,
  fetcher: (input: string, init?: RequestInit) => Promise<Response> = (i, o) => fetch(i, o),
  sleep: (ms: number) => Promise<void> = (ms) =>
    new Promise((resolve) => window.setTimeout(resolve, ms)),
): Promise<Response> {
  let res = await fetcher(input, init);
  for (let attempt = 0; attempt < BUSY_RETRIES; attempt += 1) {
    const header = res.status === 503 ? res.headers.get("Retry-After") : null;
    // Only delay-seconds; an HTTP-date or a missing header is not retried.
    if (header === null || !/^\d+$/.test(header.trim())) return res;
    const wait = Number(header.trim());
    await res.body?.cancel();
    await sleep(Math.min(wait, MAX_RETRY_AFTER_SECONDS) * 1000);
    res = await fetcher(input, init);
  }
  return res;
}

async function fetchWithTimeout(
  input: RequestInfo | URL,
  init: RequestInit | undefined,
  ms: number,
  label: string,
): Promise<Response> {
  const controller = new AbortController();
  const timer = window.setTimeout(() => controller.abort(), ms);
  try {
    return await fetch(input, { ...init, signal: controller.signal });
  } catch (err) {
    if (err instanceof DOMException && err.name === "AbortError") {
      throw new ApiError(504, `${label} timed out`);
    }
    throw err;
  } finally {
    window.clearTimeout(timer);
  }
}

function toAuthSession(body: {
  authenticated: boolean;
  role?: string;
  default_view?: string;
  can_acquire?: boolean;
  elevated?: boolean;
  /**
   * Whether the session reaches node administration and the Status page:
   * the operator, an Owner or Administrator, or a Member who owns a node.
   */
  administers_nodes?: boolean;
  impersonating?: AuthMeImpersonating;
  portal?: PortalInfo;
  user?: AuthMeUser;
  second_factor?: AuthSecondFactor;
  see_prices?: boolean;
}): AuthSession {
  return {
    authenticated: body.authenticated,
    role: normalizeRole(body.role),
    default_view: normalizeView(body.default_view),
    can_acquire: Boolean(body.can_acquire),
    elevated: Boolean(body.elevated),
    administers_nodes: body.administers_nodes,
    impersonating: body.impersonating,
    portal: body.portal,
    user: body.user,
    second_factor: body.second_factor,
    // An older server that does not send it never hid prices.
    see_prices: body.see_prices !== false,
  };
}

/** Statuses of a session check that asking again can change. */
const PASSING_SESSION_CHECK_STATUSES = new Set([502, 503, 504]);

/**
 * Whether a failed session check is worth asking again: the request never
 * got an answer (a network error), timed out, or the server or a proxy said
 * it was busy. Any other answer, such as a demo that has ended (410), is
 * final and goes to the caller.
 *
 * @param err - What the session check threw.
 * @returns True when the check should be asked again.
 */
function isPassingSessionCheckFailure(err: unknown): boolean {
  if (err instanceof ApiError) return PASSING_SESSION_CHECK_STATUSES.has(err.status);
  return err instanceof TypeError;
}

/** Longest wait between {@link authMeUntilAnswered} attempts, in ms. */
const SESSION_CHECK_MAX_BACKOFF_MS = 10_000;

/**
 * Asks {@link authMe} until it answers, backing off between attempts.
 *
 * A check that timed out, could not reach the server, or found it busy
 * (502, 503 or 504) is not a sign-out, so the app keeps its loading screen
 * and asks again rather than showing the sign-in page to someone whose
 * session is fine. Waits 1 s, 2 s, 4 s … up to 10 s. Any other failure is
 * thrown to the caller.
 *
 * @param stopped - True once the caller no longer wants the answer.
 * @param check - Performs one session check (defaults to {@link authMe}).
 * @param sleep - Waits the given milliseconds (defaults to a timer).
 * @returns The session, or null when `stopped` turned true first.
 * @throws {ApiError} When the check fails in a way asking again cannot fix.
 */
export async function authMeUntilAnswered(
  stopped: () => boolean,
  check: () => Promise<AuthSession> = authMe,
  sleep: (ms: number) => Promise<void> = (ms) =>
    new Promise((resolve) => window.setTimeout(resolve, ms)),
): Promise<AuthSession | null> {
  let backoff = 1_000;
  while (!stopped()) {
    try {
      return await check();
    } catch (err) {
      if (!isPassingSessionCheckFailure(err)) throw err;
      await sleep(backoff);
      backoff = Math.min(backoff * 2, SESSION_CHECK_MAX_BACKOFF_MS);
    }
  }
  return null;
}

/**
 * Loads the current session from `GET /api/auth/me`.
 *
 * A 503 with `Retry-After` (the session could not be checked yet) is asked
 * again a few times; only a 401 means signed out.
 *
 * @returns Anonymous session when the response is 401.
 * @throws {ApiError} When the check fails or times out, which is not a
 *   sign-out.
 */
export async function authMe(): Promise<AuthSession> {
  const res = await fetchRetryingBusy("/api/auth/me", { credentials: "include" }, (input, init) =>
    fetchWithTimeout(input, init, 8_000, "Session check"),
  );
  if (res.status === 401) return ANON_SESSION;
  const body = await parseJson<{
    authenticated: boolean;
    role?: string;
    default_view?: string;
    can_acquire?: boolean;
    elevated?: boolean;
    administers_nodes?: boolean;
    impersonating?: AuthMeImpersonating;
    portal?: PortalInfo;
    user?: AuthMeUser;
    second_factor?: AuthSecondFactor;
    see_prices?: boolean;
  }>(res);
  return toAuthSession(body);
}

/**
 * Signs in with the operator token via `POST /api/auth/login`.
 *
 * @param token - Operator bearer token from `bookclerk daemon token`.
 * @returns Authenticated operator session (acquire enabled).
 */
export async function login(token: string): Promise<AuthSession> {
  const res = await fetchWithTimeout(
    "/api/auth/login",
    {
      method: "POST",
      credentials: "include",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ token }),
    },
    8_000,
    "Login",
  );
  const body = await parseJson<{
    ok: boolean;
    role?: string;
    default_view?: string;
  }>(res);
  return {
    authenticated: true,
    role: normalizeRole(body.role) ?? "operator",
    default_view: normalizeView(body.default_view),
    can_acquire: true,
    see_prices: true,
  };
}

/**
 * Public sign-in picker (operator-token availability, SSO, integration logins).
 *
 * @returns Whether SPA operator-token login is offered, plus IdP and integration buttons.
 */
export async function fetchSigninMethods(): Promise<{
  operator_token: boolean;
  oidc: OidcProvider[];
  integrations: { id: string; name: string }[];
  require_second_factor: boolean;
}> {
  const res = await fetch("/api/auth/signin", { credentials: "include" });
  const body = await parseJson<{
    operator_token?: boolean;
    oidc?: OidcProvider[];
    integrations?: { id: string; name: string }[];
    require_second_factor?: boolean;
  }>(res);
  return {
    operator_token: Boolean(body.operator_token),
    oidc: body.oidc ?? [],
    integrations: body.integrations ?? [],
    require_second_factor: Boolean(body.require_second_factor),
  };
}

/**
 * Clears the operator cookie session via `POST /api/auth/logout`.
 *
 * @returns Resolves when the library answered that the sign-out is done.
 * @throws {ApiError} When the library answers anything else.
 */
export async function logout(): Promise<void> {
  // What a viewer pays depends on their linked store accounts. Cleared
  // again once the library answers: a lookup asked while the old session
  // cookie still worked must not reach the next viewer either.
  clearLookupCaches();
  let res: Response;
  try {
    res = await fetch("/api/auth/logout", {
      method: "POST",
      credentials: "include",
      headers: { "Content-Type": "application/json" },
      body: "{}",
    });
  } finally {
    clearLookupCaches();
  }
  if (!res.ok) {
    throw new ApiError(res.status, `Sign-out failed (${res.status})`);
  }
}

/**
 * Temporarily elevates an Owner session after password re-authentication.
 *
 * @param password - The Owner's account password.
 * @returns Resolves when elevation succeeds.
 */
export async function elevate(password: string): Promise<void> {
  const res = await fetch("/api/auth/elevate", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ password }),
  });
  await parseJson(res);
}

/**
 * Ends an elevated operator window on a user session.
 *
 * @returns Resolves when elevation is cleared.
 */
export async function endElevation(): Promise<void> {
  const res = await fetch("/api/auth/elevate", {
    method: "DELETE",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: "{}",
  });
  await parseJson(res);
}

/** Alias used by Settings RBAC controls. */
export const endElevate = endElevation;

/** Enabled upstream OIDC/OAuth login providers. */
export interface OidcProvider {
  id: string;
  name: string;
  /** Built-in social preset (`google`, `github`, `apple`, `discord`) when set. */
  preset?: string | null;
}

/** Linked portal identity (local or `oidc:{id}`). */
export interface LinkedIdentity {
  provider: string;
  external_user_id: string;
  label: string | null;
}

/** How an upstream IdP decides who may become a first-party User. */
export type OidcProvisionMode = "mapped_role" | "any" | "allowlist" | "invite_only";

/** Where a provider client secret is stored (never returned in plaintext). */
export type OidcSecretSource = "env" | "config" | "store" | "none";

/** Owner/operator view of one `[auth.oidc]` provider (secrets redacted). */
export interface OidcProviderConfigView {
  id: string;
  name: string;
  preset?: string | null;
  issuer?: string | null;
  client_id: string;
  scopes: string[];
  provision: OidcProvisionMode;
  default_role: string;
  role_claim: string;
  role_map: Record<string, string>;
  link_by_email: boolean;
  allowed_email_domains: string[];
  allowed_emails: string[];
  allowed_subjects: string[];
  apple_team_id?: string | null;
  apple_key_id?: string | null;
  has_client_secret: boolean;
  has_apple_private_key?: boolean;
  secret_source: OidcSecretSource;
}

/** Owner/operator identity-broker settings payload. */
export interface OidcBrokerConfigView {
  enabled: boolean;
  allowed_email_domains: string[];
  /** Configured `integrations.public_origin` (issuer / callback / invite base). */
  public_origin?: string | null;
  /** Effective issuer origin (`public_origin`, else bound loopback, else localhost). */
  issuer_url?: string;
  /** Origin detected from this request when it is a bound loopback listen address. */
  detected_origin?: string;
  /** Absolute OpenID discovery URL. */
  discovery_url?: string;
  authorization_endpoint?: string;
  token_endpoint?: string;
  userinfo_endpoint?: string;
  revocation_endpoint?: string;
  callback_url?: string | null;
  providers: OidcProviderConfigView[];
}

/** Provider fields accepted by `PUT /api/auth/oidc/config`. */
export interface OidcProviderConfigUpdate {
  id: string;
  name: string;
  preset?: string | null;
  issuer?: string | null;
  client_id: string;
  /** Omit to keep; empty string to clear; non-empty to store. */
  client_secret?: string;
  scopes: string[];
  provision: OidcProvisionMode;
  default_role: string;
  role_claim: string;
  role_map: Record<string, string>;
  link_by_email: boolean;
  allowed_email_domains: string[];
  allowed_emails: string[];
  allowed_subjects: string[];
  apple_team_id?: string | null;
  apple_key_id?: string | null;
  apple_private_key?: string;
}

/** Body for `PUT /api/auth/oidc/config`. */
export interface OidcBrokerConfigUpdate {
  enabled: boolean;
  allowed_email_domains: string[];
  providers: OidcProviderConfigUpdate[];
  /** Omit to leave unchanged; empty string clears `integrations.public_origin`. */
  public_origin?: string | null;
}

/** Registered passkey row. */
export interface ListedPasskey {
  id: number;
  credential_id: string;
  name?: string | null;
}

/**
 * Public list of SSO providers for the login page.
 *
 * @returns
 */
export async function listOidcProviders(): Promise<{
  enabled: boolean;
  providers: OidcProvider[];
}> {
  const res = await fetch("/api/auth/oidc/providers", { credentials: "include" });
  return parseJson(res);
}

/**
 * Owner/operator identity-broker configuration (secrets redacted).
 *
 * @returns Enabled flag, callback URL, and provider rows.
 */
export async function fetchOidcConfig(): Promise<OidcBrokerConfigView> {
  const res = await fetch("/api/auth/oidc/config", { credentials: "include" });
  return parseJson(res);
}

/**
 * Replaces `[auth.oidc]` providers. Omit `client_secret` to keep the stored value.
 *
 * @param body - Broker enablement and provider list.
 * @returns Saved configuration with redacted secrets.
 */
export async function putOidcConfig(body: OidcBrokerConfigUpdate): Promise<OidcBrokerConfigView> {
  const res = await fetch("/api/auth/oidc/config", {
    method: "PUT",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return parseJson(res);
}

/** Bookclerk-as-IdP OAuth client (secrets never listed after create/rotate). */
export interface OidcAsClient {
  client_id: string;
  name?: string | null;
  redirect_uris: string[];
  confidential: boolean;
  issue_refresh_token: boolean;
  allowed_scopes: string[];
  has_secret: boolean;
  enabled: boolean;
  /** Set when a plugin owns this client (redirects are read-only). */
  plugin_id?: string | null;
  /** Plaintext secret, returned only on create or rotate. */
  client_secret?: string | null;
}

/** Body for creating or updating a Bookclerk-as-IdP client. */
export interface OidcAsClientWrite {
  client_id?: string;
  name?: string | null;
  redirect_uris: string[];
  confidential: boolean;
  issue_refresh_token: boolean;
  allowed_scopes: string[];
  enabled: boolean;
}

/**
 * Lists Bookclerk-as-IdP clients (no plaintext secrets).
 *
 * @returns Registered OAuth clients.
 */
export async function listOidcClients(): Promise<OidcAsClient[]> {
  const res = await fetch("/api/auth/oidc/clients", { credentials: "include" });
  const body = await parseJson<{ clients?: OidcAsClient[] }>(res);
  return body.clients ?? [];
}

/**
 * Creates an OIDC client. Confidential clients return `client_secret` once.
 *
 * @param body - Client id, redirects, token policy.
 * @returns Created client; confidential clients include `client_secret` once.
 */
export async function createOidcClient(body: OidcAsClientWrite): Promise<OidcAsClient> {
  const res = await fetch("/api/auth/oidc/clients", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return parseJson(res);
}

/**
 * Updates an OIDC client without echoing a secret unless flipping to confidential.
 *
 * @param clientId - Existing client_id.
 * @param body - Redirects and token policy.
 * @returns Updated client row (no plaintext secret unless flipping to confidential).
 */
export async function updateOidcClient(
  clientId: string,
  body: OidcAsClientWrite,
): Promise<OidcAsClient> {
  const res = await fetch(`/api/auth/oidc/clients/${encodeURIComponent(clientId)}`, {
    method: "PUT",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return parseJson(res);
}

/**
 * Deletes a registered OIDC client (operator or elevated Owner only).
 *
 * @param clientId - Client to remove.
 * @returns Resolves when the client is removed.
 */
export async function deleteOidcClient(clientId: string): Promise<void> {
  const res = await fetch(`/api/auth/oidc/clients/${encodeURIComponent(clientId)}`, {
    method: "DELETE",
    credentials: "include",
  });
  if (!res.ok) {
    await parseJson(res);
  }
}

/**
 * Rotates a confidential client's secret (operator or elevated Owner only);
 * plaintext is returned once.
 *
 * @param clientId - Confidential client to rotate.
 * @returns Client row including the new plaintext secret once.
 */
export async function rotateOidcClientSecret(clientId: string): Promise<OidcAsClient> {
  const res = await fetch(`/api/auth/oidc/clients/${encodeURIComponent(clientId)}/rotate-secret`, {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: "{}",
  });
  return parseJson(res);
}

/**
 * Linked login identities for the current User.
 *
 * @returns
 */
export async function listOidcIdentities(): Promise<LinkedIdentity[]> {
  const res = await fetch("/api/auth/oidc/identities", { credentials: "include" });
  const body = await parseJson<{ identities: LinkedIdentity[] }>(res);
  return body.identities ?? [];
}

/**
 * Passkeys registered to the current User.
 *
 * @returns
 */
export async function listPasskeys(): Promise<ListedPasskey[]> {
  const res = await fetch("/api/auth/passkeys", { credentials: "include" });
  const body = await parseJson<{ passkeys: ListedPasskey[] }>(res);
  return body.passkeys ?? [];
}

/**
 * Begin WebAuthn registration (authenticated).
 *
 * @param currentPassword
 * @returns
 */
export async function passkeyRegisterBegin(currentPassword?: string): Promise<{
  challenge_id: string;
  publicKey: Record<string, unknown>;
}> {
  const res = await fetch("/api/auth/passkeys/register/begin", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(currentPassword ? { current_password: currentPassword } : {}),
  });
  return parseJson(res);
}

/**
 * Finish WebAuthn registration.
 *
 * @param body
 * @returns
 */
export async function passkeyRegisterFinish(body: {
  challenge_id: string;
  credential: Record<string, unknown>;
  name?: string;
}): Promise<void> {
  const res = await fetch("/api/auth/passkeys/register/finish", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  await parseJson(res);
}

/**
 * Delete a passkey owned by the current User.
 *
 * @param id
 * @param currentPassword
 * @returns
 */
export async function deletePasskey(id: number, currentPassword?: string): Promise<void> {
  const res = await fetch(`/api/auth/passkeys/${id}`, {
    method: "DELETE",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(currentPassword ? { current_password: currentPassword } : {}),
  });
  await parseJson(res);
}

/**
 * Begin identifier-first passkey login.
 *
 * @param login
 * @returns
 */
export async function passkeyLoginBegin(login: string): Promise<{
  challenge_id: string;
  publicKey: Record<string, unknown>;
}> {
  const res = await fetch("/api/auth/passkeys/login/begin", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ login }),
  });
  return parseJson(res);
}

/**
 * Finish passkey login (sets the portal session cookie).
 *
 * @param body
 * @returns
 */
export async function passkeyLoginFinish(body: {
  challenge_id: string;
  credential: Record<string, unknown>;
}): Promise<void> {
  const res = await fetch("/api/auth/passkeys/login/finish", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  await parseJson(res);
}

/**
 * Begin passkey Owner elevation.
 *
 * @returns
 */
export async function passkeyElevateBegin(): Promise<{
  challenge_id: string;
  publicKey: Record<string, unknown>;
}> {
  const res = await fetch("/api/auth/passkeys/elevate/begin", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: "{}",
  });
  return parseJson(res);
}

/**
 * Finish passkey Owner elevation.
 *
 * @param body
 * @returns
 */
export async function passkeyElevateFinish(body: {
  challenge_id: string;
  credential: Record<string, unknown>;
}): Promise<void> {
  const res = await fetch("/api/auth/passkeys/elevate/finish", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  await parseJson(res);
}

/**
 * Starts operator (or elevated Owner) impersonation of another user,
 * after the work registered with {@link beforeSignOut}: what is playing
 * belongs to the identity being left.
 *
 * @param userId - Target user id.
 * @returns Resolves when impersonation is active.
 */
export async function startImpersonate(userId: number): Promise<void> {
  await changeIdentity(async () => {
    const res = await fetch("/api/auth/impersonate", {
      method: "POST",
      credentials: "include",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ user_id: userId }),
    });
    await parseJson(res);
  });
}

/**
 * Stops the current operator impersonation session, after the work
 * registered with {@link beforeSignOut}: a book the impersonated person
 * was playing is closed as them, not reported as the restored account.
 *
 * @returns Resolves when impersonation ends.
 */
export async function stopImpersonate(): Promise<void> {
  await changeIdentity(async () => {
    const res = await fetch("/api/auth/impersonate", {
      method: "DELETE",
      credentials: "include",
      headers: { "Content-Type": "application/json" },
      body: "{}",
    });
    await parseJson(res);
  });
}

/**
 * User row from `GET /api/users` (administrator settings).
 */
export interface ListedUser {
  id: number;
  role: string;
  status: string;
  display_name: string | null;
  login_name: string | null;
  email?: string | null;
  has_password: boolean;
  /** Non-expired portal session present (`true` even when idle). */
  online?: boolean;
  /** Most recent unexpired portal session activity (RFC 3339). Active vs idle is ~5 minutes. */
  last_active_at?: string | null;
  /** Durable last portal use (RFC 3339); survives logout. Null means never signed in. */
  last_seen_at?: string | null;
  /** True when a profile picture file is stored for this user. */
  has_avatar?: boolean;
  /** `auto`, `monogram`, `gravatar`, `upload`, or `sso:{id}`. */
  avatar_source?: string | null;
  /** SHA-256 hex of the contact email for Gravatar. */
  gravatar_hash?: string | null;
  /** IdP-supplied pictures the user can pick. */
  sso_pictures?: {
    identity_id: number;
    provider: string;
    picture_url: string;
    /** Same-origin 128 px copy of `picture_url`, when the server signed one. */
    picture_path?: string | null;
    last_used_at?: string | null;
  }[];
  /** Unfinished listen within ~30 minutes (waveform uses the last ~5 minutes). */
  listening?: {
    title: string | null;
    provider: string;
    last_listened_at: string;
  } | null;
  /** Linked storefront / integration accounts. */
  integrations?: {
    source: string;
    account_id: string;
    label: string | null;
  }[];
  /** What the user may do here: the member controls that apply to them. */
  controls?: MemberControls;
  /** Linked login identities (local + OIDC). */
  identities?: {
    provider: string;
    external_user_id: string;
    label: string | null;
  }[];
}

/** The book field a patron scope rule matches one value of. */
export type ScopeField = "genre" | "series" | "author" | "narrator";

/** Whether a patron scope rule lets matching books in or keeps them out. */
export type RuleEffect = "include" | "exclude";

/** One rule of a patron scope. */
export interface ScopeRule {
  /** The field the rule matches. */
  field: ScopeField;
  /** One value of the field, compared without case. */
  value: string;
  /** Include or exclude. */
  effect: RuleEffect;
}

/** Which part of the library a patron sees. */
export interface PatronScope {
  /** Visibility rules; empty means every book is visible. */
  rules: ScopeRule[];
}

/** A patron's scope as `GET`/`PUT /api/users/{id}/scope` report it. */
export interface UserScope {
  /** The user. */
  user_id: number;
  /** The scope that applies: their own, or the library default. */
  scope: PatronScope;
  /** True when the user has their own scope. */
  own: boolean;
  /** False for an Owner or Administrator, who always see everything. */
  applies: boolean;
}

/**
 * Created / patched user response from RBAC provisioning APIs.
 */
export interface UserMutationResponse {
  ok: boolean;
  user: ListedUser;
  claim_ticket?: string | null;
  /** Magic-link URL for `/invite?ticket=…` when an invite was minted. */
  invite_url?: string | null;
}

/**
 * Bootstrap response for the first owner.
 */
export interface BootstrapAdministratorResponse {
  ok: boolean;
  user_id: number;
  claim_ticket: string;
  invite_url?: string | null;
  login_name: string | null;
  has_password: boolean;
}

/**
 * Active auth session row visible to the current principal.
 */
export interface ListedSession {
  id: number;
  kind: "operator" | "portal" | string;
  created_at: string;
  expires_at: string;
  last_used_at?: string | null;
  elevated?: boolean;
  impersonating_user_id?: number | null;
  is_current?: boolean;
  client_label?: string | null;
  device_type?: string | null;
  user_agent?: string | null;
  /** For a portal session: `browser`, or `clerk` for a clerk's library card. */
  channel?: "browser" | "clerk" | null;
}

/**
 * Lists users for administrator settings.
 *
 * @returns User rows (empty array when none).
 */
export async function listUsers(): Promise<ListedUser[]> {
  const res = await fetch("/api/users", { credentials: "include" });
  const body = await parseJson<{ users: ListedUser[] }>(res);
  return body.users ?? [];
}

/**
 * Creates the first Owner account when no owners (or legacy administrators) exist.
 *
 * @param body - Optional display/login/email/password fields for the new owner.
 * @returns Bootstrap result including the one-time invite magic link.
 */
export async function bootstrapAdministrator(body: {
  display_name?: string;
  login_name?: string;
  email?: string;
  password?: string;
}): Promise<BootstrapAdministratorResponse> {
  const res = await fetch("/api/auth/bootstrap", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return parseJson(res);
}

/**
 * Creates a first-party Bookclerk user.
 *
 * @param body - Role, profile fields, optional password, and invite choice.
 * @returns Created user plus optional one-time claim ticket.
 */
export async function createUser(body: {
  role?: string;
  display_name?: string;
  login_name?: string;
  email?: string;
  password?: string;
  mint_invite?: boolean;
}): Promise<UserMutationResponse> {
  const res = await fetch("/api/users", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return parseJson(res);
}

/**
 * Reads which part of the library a patron sees.
 *
 * @param id - The user.
 * @returns The scope that applies and where it comes from.
 */
export async function getUserScope(id: number): Promise<UserScope> {
  const res = await fetch(`/api/users/${encodeURIComponent(String(id))}/scope`, {
    credentials: "include",
  });
  return parseJson(res);
}

/**
 * Gives a patron their own scope, or with `null` returns them to the
 * library's default.
 *
 * @param id - The user.
 * @param scope - Their own scope, or `null`.
 * @returns The scope after the change.
 */
export async function putUserScope(id: number, scope: PatronScope | null): Promise<UserScope> {
  const res = await fetch(`/api/users/${encodeURIComponent(String(id))}/scope`, {
    method: "PUT",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ scope }),
  });
  return parseJson(res);
}

/**
 * Reads the library's default patron scope.
 *
 * @returns The default.
 */
export async function getDefaultScope(): Promise<{ scope: PatronScope }> {
  const res = await fetch("/api/users/default-scope", { credentials: "include" });
  return parseJson(res);
}

/**
 * Replaces the library's default patron scope.
 *
 * @param scope - The new default.
 * @returns The default after the change.
 */
export async function putDefaultScope(scope: PatronScope): Promise<{ scope: PatronScope }> {
  const res = await fetch("/api/users/default-scope", {
    method: "PUT",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(scope),
  });
  return parseJson(res);
}

/** One thing an administrator may allow or refuse a patron. */
export type MemberControl =
  "link_accounts" | "download" | "edit_metadata" | "upload" | "request_books" | "see_prices";

/** A value for every member control (`true` = allowed). */
export type MemberControls = Record<MemberControl, boolean>;

/** One patron's overrides; a missing control follows the library's default. */
export type ControlOverrides = Partial<Record<MemberControl, boolean>>;

/** A patron's member controls as `GET /api/users/{id}/controls` reports them. */
export interface UserControls {
  /** The user. */
  user_id: number;
  /** The library's default for every patron. */
  defaults: MemberControls;
  /** The controls set for this patron alone. */
  overrides: ControlOverrides;
  /** What applies to the user now. */
  effective: MemberControls;
  /** False for an Owner, who may always do everything. */
  applies: boolean;
  /**
   * True for a Member, who follows the library default; false for an
   * Administrator, whom only controls set for them alone limit.
   */
  follows_default: boolean;
}

/**
 * Reads what a patron may do.
 *
 * @param id - The user.
 * @returns The default, the patron's overrides and what applies.
 */
export async function getUserControls(id: number): Promise<UserControls> {
  const res = await fetch(`/api/users/${encodeURIComponent(String(id))}/controls`, {
    credentials: "include",
  });
  return parseJson(res);
}

/**
 * Sets the controls that differ for one patron; the rest follow the
 * library's default.
 *
 * @param id - The user.
 * @param overrides - The controls set for them alone (`{}` for none).
 * @returns The controls after the change.
 */
export async function putUserControls(
  id: number,
  overrides: ControlOverrides,
): Promise<UserControls> {
  const res = await fetch(`/api/users/${encodeURIComponent(String(id))}/controls`, {
    method: "PUT",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ overrides }),
  });
  return parseJson(res);
}

/**
 * Reads the library's default member controls.
 *
 * @returns The default.
 */
export async function getDefaultControls(): Promise<{ controls: MemberControls }> {
  const res = await fetch("/api/users/default-controls", { credentials: "include" });
  return parseJson(res);
}

/**
 * Replaces the library's default member controls.
 *
 * @param controls - The new default.
 * @returns The default after the change.
 */
export async function putDefaultControls(
  controls: MemberControls,
): Promise<{ controls: MemberControls }> {
  const res = await fetch("/api/users/default-controls", {
    method: "PUT",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(controls),
  });
  return parseJson(res);
}

/**
 * Updates a first-party user role, status, or profile fields.
 *
 * @param id - User id to patch.
 * @param body - Partial user fields accepted by the daemon.
 * @returns Patched user row.
 */
export async function patchUser(
  id: number,
  body: {
    role?: string;
    status?: string;
    display_name?: string;
    login_name?: string;
    email?: string;
  },
): Promise<UserMutationResponse> {
  const res = await fetch(`/api/users/${encodeURIComponent(String(id))}`, {
    method: "PATCH",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return parseJson(res);
}

/**
 * Mints a fresh one-time claim ticket for an existing active user.
 *
 * @param id - User id.
 * @returns One-time claim ticket.
 */
export async function mintUserClaimTicket(id: number): Promise<{
  ok: boolean;
  claim_ticket: string;
  invite_url?: string | null;
}> {
  const res = await fetch(`/api/users/${encodeURIComponent(String(id))}/claim-ticket`, {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: "{}",
  });
  return parseJson(res);
}

/**
 * Mints a password-reset claim ticket and revokes the user's portal sessions.
 *
 * @param id - User id.
 * @returns Reset ticket plus count of revoked sessions.
 */
export async function resetUserPassword(id: number): Promise<{
  ok: boolean;
  claim_ticket: string;
  invite_url?: string | null;
  revoked_sessions: number;
}> {
  const res = await fetch(`/api/users/${encodeURIComponent(String(id))}/reset-password`, {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: "{}",
  });
  return parseJson(res);
}

/**
 * Deletes a first-party user (operator / administrator provisioner).
 *
 * @param id - User id.
 * @returns Ack payload.
 */
export async function deleteUser(id: number): Promise<{ ok: boolean }> {
  const res = await fetch(`/api/users/${encodeURIComponent(String(id))}`, {
    method: "DELETE",
    credentials: "include",
  });
  return parseJson(res);
}

/**
 * Lists sessions for the current principal.
 *
 * @returns Active session rows.
 */
export async function listSessions(): Promise<ListedSession[]> {
  const res = await fetch("/api/auth/sessions", { credentials: "include" });
  const body = await parseJson<{ sessions: ListedSession[] }>(res);
  return body.sessions ?? [];
}

/**
 * Revokes one session visible to the current principal.
 *
 * @param id - Session id.
 * @returns Resolves when the revoke succeeds.
 */
export async function revokeSession(id: number): Promise<void> {
  const res = await fetch(`/api/auth/sessions/${encodeURIComponent(String(id))}`, {
    method: "DELETE",
    credentials: "include",
  });
  await parseJson(res);
}

/**
 * Sets a password for self or for a managed user.
 *
 * @param body - New password and optional target user id.
 * @returns Count of revoked sessions.
 */
export async function setPassword(body: {
  password: string;
  current_password?: string;
  user_id?: number;
}): Promise<{ ok: boolean; revoked_sessions: number }> {
  const res = await fetch("/api/auth/password", {
    method: "PUT",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return parseJson(res);
}

/**
 * Updates the signed-in user's display name and/or email.
 *
 * @param body - Fields to change; omitted keys are left unchanged.
 * @returns Updated profile snapshot from the daemon.
 */
export async function patchProfile(body: {
  display_name?: string;
  email?: string;
  avatar_source?: string;
}): Promise<{ ok: boolean; user: AuthMeUser }> {
  const res = await fetch("/api/auth/profile", {
    method: "PATCH",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return parseJson(res);
}

/**
 * Authenticated URL for a user's stored profile picture.
 *
 * @param userId - First-party `users.id`.
 * @param cacheKey - Optional cache-buster after upload.
 * @returns Path used by `<img src>`.
 */
export function userAvatarUrl(userId: number, cacheKey?: number): string {
  const base = `/api/users/${userId}/avatar`;
  return cacheKey ? `${base}?v=${cacheKey}` : base;
}

/**
 * Uploads a JPEG/PNG/WebP profile picture for the signed-in user.
 *
 * @param file - Image selected from the file picker.
 */
export async function uploadProfileAvatar(file: File): Promise<void> {
  const res = await fetch("/api/auth/profile/avatar", {
    method: "PUT",
    credentials: "include",
    headers: { "Content-Type": file.type || "application/octet-stream" },
    body: file,
  });
  await parseJson(res);
}

/**
 * Removes the signed-in user's stored profile picture.
 */
export async function deleteProfileAvatar(): Promise<void> {
  const res = await fetch("/api/auth/profile/avatar", {
    method: "DELETE",
    credentials: "include",
  });
  await parseJson(res);
}

/**
 * Signs in with username/password via `POST /api/auth/password`.
 *
 * @param login - Username or email.
 * @param password - Account password.
 * @returns Session cookie on success, or an MFA challenge when TOTP is enabled.
 */
export async function passwordLogin(
  login: string,
  password: string,
): Promise<{ ok: boolean; mfa?: { method: string; challenge_id: string } }> {
  const res = await fetch("/api/auth/password", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ login, password }),
  });
  return parseJson(res);
}

/**
 * Completes password+TOTP login via `POST /api/auth/totp/login`.
 *
 * @param challengeId - Challenge from the password-login MFA payload.
 * @param code - Six-digit authenticator code.
 * @returns Resolves when the session cookie is set.
 */
export async function totpLogin(challengeId: string, code: string): Promise<void> {
  const res = await fetch("/api/auth/totp/login", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ challenge_id: challengeId, code }),
  });
  await parseJson(res);
}

/**
 * Whether TOTP is confirmed for the current user.
 *
 * @returns `{ enabled }` for the signed-in user.
 */
export async function fetchTotpStatus(): Promise<{ enabled: boolean }> {
  const res = await fetch("/api/auth/totp", { credentials: "include" });
  return parseJson(res);
}

/**
 * Starts TOTP enrollment (pending secret, otpauth URL, QR SVG).
 *
 * @param currentPassword - Required when the portal session is older than the reauth window.
 * @returns Pending secret, otpauth URL, and QR SVG.
 */
export async function totpEnrollBegin(currentPassword?: string): Promise<{
  secret: string;
  otpauth_url: string;
  qr_svg: string;
}> {
  const res = await fetch("/api/auth/totp/enroll/begin", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(currentPassword ? { current_password: currentPassword } : {}),
  });
  return parseJson(res);
}

/**
 * Confirms the pending TOTP secret with an authenticator code.
 *
 * @param code - Six-digit code from the authenticator app.
 * @returns Resolves when enrollment is confirmed.
 */
export async function totpEnrollFinish(code: string): Promise<void> {
  const res = await fetch("/api/auth/totp/enroll/finish", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ code }),
  });
  await parseJson(res);
}

/**
 * Disables TOTP for the current user.
 *
 * @param currentPassword - Required when the portal session is older than the reauth window.
 * @returns Resolves when TOTP is disabled.
 */
export async function disableTotp(currentPassword?: string): Promise<void> {
  const res = await fetch("/api/auth/totp", {
    method: "DELETE",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(currentPassword ? { current_password: currentPassword } : {}),
  });
  await parseJson(res);
}

/**
 * Host-wide password second-factor policy (`daemon.auth.require_second_factor`).
 *
 * @returns Whether password login requires a second factor.
 */
export async function fetchMfaPolicy(): Promise<{ require_second_factor: boolean }> {
  const res = await fetch("/api/auth/mfa-policy", { credentials: "include" });
  return parseJson(res);
}

/**
 * Persists whether password login requires TOTP or a passkey (operator or
 * elevated Owner only).
 *
 * @param requireSecondFactor - When true, password-only sign-in is not enough once enrolled.
 * @returns Saved `{ require_second_factor }` flag.
 */
export async function putMfaPolicy(
  requireSecondFactor: boolean,
): Promise<{ require_second_factor: boolean }> {
  const res = await fetch("/api/auth/mfa-policy", {
    method: "PUT",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ require_second_factor: requireSecondFactor }),
  });
  return parseJson(res);
}

/**
 * Per-user Discover / default-view preferences.
 */
export interface UserPreferences {
  default_view: AppView;
  disabled_shelves: string[];
  discover_sort: CatalogSearchSort;
  discover_sort_dir: "asc" | "desc";
  /** `null` / omitted = browser language; `__all__` = no hard filter. */
  discover_language: string | null;
  /** Store ids hidden in Discover. Empty = all sources including future. */
  discover_excluded_sources: string[];
  /** Appearance: `system` follows the OS (fallback light); `light` / `dark` pin it. */
  theme: ThemePreference;
}

/**
 * One config key/value pair for `PATCH /api/settings`.
 */
export interface SettingsUpdate {
  key: string;
  value: string;
}

/**
 * Select option for a plugin setting.
 */
export interface PluginSettingChoice {
  value: string;
  label: string;
}

/**
 * Editable plugin setting descriptor from the settings API.
 *
 * For a plugin instance group the key is the bare `[[settings]]` key from the
 * plugin's manifest and `value` is the effective value (document over schema
 * default). A `secret` option never carries its value.
 */
export interface PluginSettingOption {
  key: string;
  label: string;
  /** String form of the value; for a `table` setting, the rows as JSON text. */
  value: string;
  value_type: "string" | "boolean" | "number" | "secret" | "table";
  choices?: PluginSettingChoice[];
  /** True for a `secret` setting; `value` is always empty. */
  secret?: boolean;
  /** For a secret setting, whether a sealed value is stored for the instance. */
  secret_set?: boolean;
  /** Columns of a `table` setting, in display order. */
  columns?: PluginSettingColumn[];
}

/**
 * One column of a `table` setting: the record field each row fills.
 */
export interface PluginSettingColumn {
  /** Field name in each row object. */
  key: string;
  /** Column heading. */
  label: string;
  value_type: "string" | "boolean" | "number";
  /** Dropdown values of an `enum` column. */
  choices?: PluginSettingChoice[];
  /** String form of the value a new row starts with. */
  default?: string;
  /** True when every row must have a value in this column. */
  required?: boolean;
}

/** One stored cell of a `table` setting row. */
export type SettingCellValue = string | number | boolean;

/** One stored row of a `table` setting, keyed by column. */
export type SettingRow = Record<string, SettingCellValue>;

/** One value in a plugin instance document. */
export type InstanceSettingValue = SettingCellValue | SettingRow[];

/**
 * This node's deployment of a plugin instance, with its latest observation.
 */
export interface PluginDeploymentView {
  deployment_id: string;
  node_id: string;
  /**
   * `present` / `absent` when an operator manages the instance (the Enabled
   * switch), `placed` / `unplaced` when the library scheduler does. The
   * host runs it for `present` and `placed`.
   */
  desired: "present" | "absent" | "placed" | "unplaced" | string;
  revision: number;
  /** `materialized`, `running`, `healthy`, `error`, or `retired` when observed. */
  status?: string;
  detail?: string;
  applied_config_revision?: number;
  observed_at?: string;
}

/**
 * Scheduler placement of a plugin instance: the operator's intent and the
 * last scheduler pass's result.
 */
export interface PluginPlacementView {
  /**
   * Nodes that should run the instance: a count (0 stops every replica) or
   * `"all"` (every live, schedulable host with budget room).
   */
  replicas: number | "all";
  /** Label constraints (`nas`, `zone=eu`, `!slow`, `host=<id>`). */
  constraints: string[];
  /** Compare-and-swap revision of the intent. */
  revision: number;
  /** Replicas the last scheduler pass placed. */
  placed: number;
  /** Why fewer than requested are placed; empty when satisfied. */
  detail: string;
  scheduled_at: string;
  updated_at: string;
}

/**
 * Settings group for one plugin instance, or for a bootstrap database adapter.
 */
export interface PluginSettingsGroup {
  id: string;
  /** Handler family (`source`, `integration`, `output`, `database`). */
  family: string;
  /** Provenance-qualified PluginKey when this row came from a discovered install. */
  plugin_key?: string;
  /** Host-evaluated provenance (`platform_bundled`, `verified_staged`, `installed`, …). */
  provenance?: string;
  /** Same-origin logo for Settings list rows (embedded, or a copy of the remote logo). */
  logo?: string;
  settings: PluginSettingOption[];
  /** Instance this group edits; absent for a `config.toml`-backed database adapter. */
  plugin_instance_id?: string;
  /** Operator-facing instance name, when set. */
  display_name?: string;
  /** True when a bare plugin key or alias routes to this instance. */
  is_default?: boolean;
  /** Instance document revision for compare-and-swap writes. */
  config_revision?: number;
  /** This node's deployment of the instance, when one exists. */
  deployment?: PluginDeploymentView;
  /** Scheduler placement; absent when the instance is managed by hand. */
  placement?: PluginPlacementView;
  /** Nodes whose deployment of the instance is `present`. */
  present_on?: string[];
}

/**
 * Instance document as `GET /api/config/plugin-instances/{id}/config` returns it.
 */
export interface PluginInstanceConfig {
  plugin_instance_id: string;
  plugin_key: string;
  revision: number;
  /** Stored settings (operator writes only; defaults are not stored). */
  settings: Record<string, InstanceSettingValue>;
  /** Secret refs (names only). */
  secret_refs: { key: string; name: string }[];
  /** Schema defaults under the stored settings. */
  effective: Record<string, unknown>;
}

/**
 * Body for `PUT /api/config/plugin-instances/{id}/config`.
 */
export interface PluginInstanceConfigWrite {
  expected_revision: number;
  settings: Record<string, InstanceSettingValue>;
  secret_refs: { key: string; name: string }[];
  /** Secret values to seal; never echoed back. */
  secrets?: Record<string, string>;
}

/**
 * Flat config map plus per-plugin setting groups.
 */
export interface SettingsResponse {
  settings: Record<string, string>;
  effective?: Record<string, string>;
  plugins: PluginSettingsGroup[];
  /** Host max jail CPU in cores (2 d.p.; equals logical CPU count). */
  host_cpu_cores_max?: number;
  /** Optional global per-jail CPU ceiling in cores. */
  jail_cpu_cores?: number | null;
  /**
   * `core.output` document revision the `output.*` values were read from; send it
   * as `expected_output_revision`. `effective["output.revision"]` is the revision
   * this host has applied, which can lag the document. Absent when the server
   * could not read `core.output`; the `output.*` values are then missing too and
   * must not be edited.
   */
  output_revision?: number;
}

/**
 * Plugin consent grant shape serialized by the daemon.
 */
export interface PluginGrant {
  pluginKey?: string;
  pluginId: string;
  /** Exported entrypoints the operator approved (`storefront`, `cli`, …). */
  entrypoints: string[];
  /** Event types the operator approved the plugin to publish. */
  producers: string[];
  /** Approved event-consumer subscriptions (type + schemas + suspend + filter). */
  consumers?: Array<{
    eventType: string;
    schemaVersions: number[];
    supportsSuspend: boolean;
    filter?: string | null;
  }>;
  /** Approved `[triggers].jobs` types. */
  jobs?: string[];
  networkMode: string;
  domains: string[];
  manifestDomains?: string[];
  operatorAddedDomains?: string[];
  operatorDeniedDomains?: string[];
  tcp?: { host: string; ports: number[] }[];
  operatorAddedTcp?: { host: string; ports: number[] }[];
  operatorDeniedTcp?: { host: string; ports: number[] }[];
  addressCidrs?: string[];
  operatorAddedCidrs?: string[];
  operatorDeniedCidrs?: string[];
  allowUndeclaredPublicRedirects?: boolean;
  /**
   * Hosts that receive the plugin's own `User-Agent`; every other request
   * carries the host's. Absent on a grant approved before this permission
   * existed, which keeps whatever the plugin declares.
   */
  ownUserAgentHosts?: string[];
  /** The plugin's reason for `ownUserAgentHosts`, from `plugin.toml`. */
  ownUserAgentReason?: string;
  authorityRevision?: string;
  bindings: string[];
  compatibilityFlags: string[];
  approvedAt: string;
  cpuMs?: number;
  subrequests?: number;
  diskMib?: number;
  memoryMib?: number;
  /** Jail CPU in cores (2 d.p.) from the daemon. */
  cpuCores?: number;
  /** Extra process/thread budget beyond launcher overhead (native). */
  extraProcesses?: number;
}

/**
 * Branded display metadata for a plugin consent dialog.
 */
export interface PluginConsentBrand {
  name: string;
  bg?: string | null;
  fg?: string | null;
  accent?: string | null;
  logo?: string | null;
}

/**
 * Host-capped consent limit defaults (workerd budgets + shared jail/disk).
 *
 * Jail CPU fields are cores (2 d.p.) from the daemon — no percent conversion.
 */
export interface PluginConsentLimits {
  cpu_ms: number;
  subrequests: number;
  max_cpu_ms: number;
  max_subrequests: number;
  disk_mib: number;
  max_disk_mib: number;
  memory_mib: number;
  max_memory_mib: number;
  /** Manifest / default jail CPU in cores. */
  cpu_cores: number;
  /** Host max jail CPU in cores. */
  max_cpu_cores: number;
  /** Optional Settings global jail CPU ceiling in cores. */
  jail_cpu_cores?: number | null;
  /** Default extra process/thread budget (native). */
  extra_processes: number;
  /** Host hard cap for extra process budget. */
  max_extra_processes: number;
  known_bindings: string[];
}

/**
 * Network/capability consent status for a plugin.
 */
export interface PluginConsentResponse {
  plugin_id: string;
  plugin_key?: string;
  provenance?: string;
  /** Guest runtime: `native` or `workerd`. */
  runtime: string;
  request: PluginGrant;
  covered: boolean;
  summary: string[];
  existing?: PluginGrant;
  /**
   * What the request asks for beyond the stored grant (a plugin upgrade that
   * widened its capabilities); absent when no grant is stored. The plugin runs
   * under the stored grant until these are approved.
   */
  pending?: PluginConsentPending;
  brand?: PluginConsentBrand;
  limits: PluginConsentLimits;
  /** The `User-Agent` the host sends, except to the request's `ownUserAgentHosts`. */
  host_user_agent?: string;
  /**
   * The destination each network setting names on each instance and whether it
   * is approved. A `pending` row is unreachable until approved, and `covered` is
   * false while one is.
   */
  setting_destinations?: PluginSettingDestination[];
}

/** Where a setting's destination points, which decides how the grant reaches it. */
export type SettingDestinationScope = "name" | "publicAddress" | "localAddress" | "restricted";

/** One network setting's destination on one plugin instance. */
export interface PluginSettingDestination {
  /** The instance whose settings name it; absent while the plugin has no instance. */
  instanceId?: string;
  /** The setting key the grant is stored under. */
  setting: string;
  /** The setting's label. */
  label: string;
  /** `fetch` (HTTP requests) or `tcp` (a raw socket). */
  network: "fetch" | "tcp";
  /** `scheme://host:port` for a fetch, `host:port` for TCP; absent when unset or invalid. */
  destination?: string;
  /**
   * `name` may resolve to a local-network address for this name only;
   * `localAddress` is exactly that one local-network address; `restricted`
   * (loopback, link-local, `localhost`) also needs an address grant.
   */
  scope?: SettingDestinationScope;
  /** True for an `https` fetch; false trusts the network's DNS answer. */
  tls?: boolean;
  /** Approval state of the destination. */
  state: "approved" | "pending" | "unset" | "invalid";
  /** Why the value names no destination, when `state` is `invalid`. */
  error?: string;
}

/** A setting destination the operator approves, as the consent response shows it. */
export interface PluginSettingDestinationApproval {
  /** The setting key. */
  setting: string;
  /** The destination exactly as `PluginSettingDestination.destination` shows it. */
  destination: string;
}

/** The difference between a plugin's request and its stored grant. */
export interface PluginConsentPending {
  /** Operator-facing lines naming every capability awaiting consent (the CLI prints the same). */
  lines: string[];
  /** Fetch host patterns the request adds (`*` is any public host). */
  domains: string[];
  /** TCP `host:ports` the request adds, narrowed to the new ports. */
  tcp: { host: string; ports: number[] }[];
  /** CIDRs the request adds. */
  addressCidrs: string[];
  /** Whether the request newly asks to follow undeclared public redirects. */
  allowUndeclaredPublicRedirects: boolean;
}

function consentQuery(pluginKey?: string): string {
  const trimmed = pluginKey?.trim();
  if (!trimmed) return "";
  return `?plugin_key=${encodeURIComponent(trimmed)}`;
}

/**
 * Loads network/capability consent status for a plugin before enable.
 *
 * Call from Settings when an operator reviews domains/bindings that still need
 * approval; `covered` is false until approvePluginConsent succeeds.
 *
 * @param id - Display alias from Settings.
 * @param pluginKey - Provenance-qualified PluginKey when the alias is ambiguous.
 * @returns Current consent request payload and whether it already covers the ask.
 */
export async function fetchPluginConsent(
  id: string,
  pluginKey?: string,
): Promise<PluginConsentResponse> {
  const res = await fetch(
    `/api/plugins/${encodeURIComponent(id)}/consent${consentQuery(pluginKey)}`,
    {
      credentials: "include",
    },
  );
  return parseJson<PluginConsentResponse>(res);
}

/**
 * Approves network consent for a plugin.
 *
 * @param id - Display alias from Settings.
 * @param grant - Operator-approved grant overlay; omitted fields keep defaults.
 * @param pluginKey - Provenance-qualified PluginKey when the alias is ambiguous.
 * @param settingDestinations - Setting destinations to approve; one left out
 *   stays pending, and omitting the list keeps earlier approvals that a setting
 *   still names.
 * @returns Updated consent response after approval.
 */
export async function approvePluginConsent(
  id: string,
  grant?: {
    networkMode: string;
    domains: string[];
    tcp?: { host: string; ports: number[] }[];
    addressCidrs?: string[];
    allowUndeclaredPublicRedirects?: boolean;
    ownUserAgentHosts?: string[];
    bindings: string[];
    compatibilityFlags: string[];
    cpuMs?: number;
    subrequests?: number;
    diskMib?: number;
    memoryMib?: number;
    cpuCores?: number;
    extraProcesses?: number;
  },
  pluginKey?: string,
  settingDestinations?: PluginSettingDestinationApproval[],
): Promise<PluginConsentResponse> {
  const res = await fetch(
    `/api/plugins/${encodeURIComponent(id)}/consent${consentQuery(pluginKey)}`,
    {
      method: "POST",
      credentials: "include",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        approve: true,
        ...(grant ? { grant } : {}),
        ...(settingDestinations ? { setting_destinations: settingDestinations } : {}),
      }),
    },
  );
  return parseJson<PluginConsentResponse>(res);
}

function normalizeCatalogSort(raw: unknown): CatalogSearchSort {
  const s = typeof raw === "string" ? raw.trim().toLowerCase() : "";
  if (
    s === "popularity" ||
    s === "rating" ||
    s === "title" ||
    s === "author" ||
    s === "price" ||
    s === "length"
  ) {
    return s;
  }
  return "relevance";
}

function normalizeCatalogSortDir(raw: unknown): "asc" | "desc" {
  return typeof raw === "string" && raw.trim().toLowerCase() === "asc" ? "asc" : "desc";
}

function parsePreferencesBody(body: {
  default_view?: string;
  disabled_shelves?: string[];
  discover_sort?: string;
  discover_sort_dir?: string;
  discover_language?: string | null;
  discover_excluded_sources?: string[];
  theme?: string;
}): UserPreferences {
  return {
    default_view: normalizeView(body.default_view),
    disabled_shelves: Array.isArray(body.disabled_shelves)
      ? body.disabled_shelves.filter((x): x is string => typeof x === "string")
      : [],
    discover_sort: normalizeCatalogSort(body.discover_sort),
    discover_sort_dir: normalizeCatalogSortDir(body.discover_sort_dir),
    discover_language:
      typeof body.discover_language === "string" && body.discover_language.trim()
        ? body.discover_language.trim()
        : null,
    discover_excluded_sources: Array.isArray(body.discover_excluded_sources)
      ? body.discover_excluded_sources.filter((x): x is string => typeof x === "string")
      : [],
    theme: normalizeThemePreference(body.theme),
  };
}

/**
 * Loads the signed-in user's preferences.
 *
 * @returns Normalized {@link UserPreferences}.
 */
export async function fetchPreferences(): Promise<UserPreferences> {
  const res = await fetchWithTimeout(
    "/api/preferences",
    { credentials: "include" },
    8_000,
    "Preferences",
  );
  return parsePreferencesBody(await parseJson(res));
}

/**
 * Partially updates the signed-in user's preferences.
 *
 * @param body - Fields to patch (`discover_language: null` clears to browser default).
 * @returns Updated preferences after save.
 */
export async function patchPreferences(body: {
  default_view?: AppView;
  disabled_shelves?: string[];
  discover_sort?: CatalogSearchSort;
  discover_sort_dir?: "asc" | "desc";
  /** Pass `null` to clear to browser default. */
  discover_language?: string | null;
  discover_excluded_sources?: string[];
  theme?: ThemePreference;
}): Promise<UserPreferences> {
  const res = await fetchWithTimeout(
    "/api/preferences",
    {
      method: "PATCH",
      credentials: "include",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    },
    8_000,
    "Save preferences",
  );
  return parsePreferencesBody(await parseJson(res));
}

/**
 * Loads operator settings groups and plugin option schemas for the Settings page.
 *
 * @returns Core settings plus per-plugin option groups.
 */
export async function fetchSettings(): Promise<SettingsResponse> {
  const res = await fetch("/api/settings", { credentials: "include" });
  return parseJson<SettingsResponse>(res);
}

/**
 * Applies a batch of operator setting key/value updates from the Settings form.
 *
 * The batch is one authority. File-backed keys (listen, auth, plugins) are not
 * combined with `events.*` or `output.*`, which the daemon stores as the
 * `core.events` and `core.output` documents in the library database; a mixed
 * batch is rejected with 400 and a lost compare-and-swap with 409.
 *
 * @param body - List of `{ key, value }` updates (plugin knobs use dotted keys).
 * @returns Refreshed settings payload after the patch.
 */
/**
 * Reads a plugin instance's stored document.
 *
 * @param instanceId - Plugin instance id.
 * @returns Stored settings, secret refs, and the document revision.
 */
export async function fetchPluginInstanceConfig(instanceId: string): Promise<PluginInstanceConfig> {
  const res = await fetch(`/api/config/plugin-instances/${encodeURIComponent(instanceId)}/config`, {
    credentials: "include",
  });
  return parseJson<PluginInstanceConfig>(res);
}

/**
 * Replaces a plugin instance's document (compare-and-swap on `expected_revision`).
 *
 * Secrets in `body.secrets` are sealed server-side and referenced by name;
 * the response never carries them.
 *
 * @param instanceId - Plugin instance id.
 * @param body - New document plus secrets to seal.
 * @returns The committed revision.
 */
export async function putPluginInstanceConfig(
  instanceId: string,
  body: PluginInstanceConfigWrite,
): Promise<{ plugin_instance_id: string; revision: number }> {
  const res = await fetch(`/api/config/plugin-instances/${encodeURIComponent(instanceId)}/config`, {
    method: "PUT",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return parseJson(res);
}

/**
 * Sets whether a plugin instance should run on a host (the enable switch).
 *
 * @param instanceId - Plugin instance id.
 * @param nodeId - Target node id.
 * @param desired - `present` to run it there, `absent` to retire it.
 * @param expectedRevision - Deployment revision last observed, for compare-and-swap.
 * @returns The deployment row after the write.
 */
export async function putPluginDeployment(
  instanceId: string,
  nodeId: string,
  desired: "present" | "absent",
  expectedRevision?: number,
): Promise<PluginDeploymentView> {
  const res = await fetch(
    `/api/config/plugin-instances/${encodeURIComponent(instanceId)}/deployments/${encodeURIComponent(nodeId)}`,
    {
      method: "PUT",
      credentials: "include",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ desired, expected_revision: expectedRevision }),
    },
  );
  return parseJson<PluginDeploymentView>(res);
}

/**
 * Hands an instance's deployments to the library scheduler, or changes its
 * placement (compare-and-swap on `expected_revision`; 0 creates).
 *
 * @param instanceId - Plugin instance id.
 * @param body - Replica count, label constraints, and the revision last seen.
 * @returns The stored placement.
 */
export async function putPluginPlacement(
  instanceId: string,
  body: { replicas: number | "all"; constraints: string[]; expected_revision: number },
): Promise<{ plugin_instance_id: string; placement: PluginPlacementView }> {
  const res = await fetch(
    `/api/config/plugin-instances/${encodeURIComponent(instanceId)}/placement`,
    {
      method: "PUT",
      credentials: "include",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    },
  );
  return parseJson(res);
}

/**
 * Drops an instance's placement: its deployments stay as they are and the
 * Enabled switch manages them again.
 *
 * @param instanceId - Plugin instance id.
 * @returns Whether a placement was removed.
 */
export async function deletePluginPlacement(
  instanceId: string,
): Promise<{ plugin_instance_id: string; deleted: boolean }> {
  const res = await fetch(
    `/api/config/plugin-instances/${encodeURIComponent(instanceId)}/placement`,
    { method: "DELETE", credentials: "include" },
  );
  return parseJson(res);
}

/**
 * A node's runtime document (`node.runtime`): its label, the share of its
 * detected capacity the scheduler may commit, and the drain switch.
 */
export interface NodeRuntimeView {
  label: string;
  /** Share of detected CPU the scheduler may commit (1..=100). */
  cpu_budget_percent: number;
  /** Share of detected memory the scheduler may commit (1..=100). */
  memory_budget_percent: number;
  /** False drains the node: nothing new is placed there. */
  schedulable: boolean;
}

/** A node's latest heartbeat (every 15 s while its daemon runs). */
export interface NodeStatusView {
  heartbeat_at: string;
  started_at: string;
  interval_ms: number;
  /** Detected CPU capacity in millicores. */
  cpu_millicores: number;
  /** Detected memory in bytes, when the node could read it. */
  memory_bytes: number | null;
  cpu_budget_millicores: number;
  memory_budget_bytes: number | null;
  schedulable: boolean;
  /** Measured CPU in use since the previous heartbeat. */
  cpu_used_millicores: number | null;
  /** Measured memory available to new work. */
  memory_available_bytes: number | null;
  files_dir_free_bytes: number | null;
  /** Sum of the running guests' jail ceilings. */
  reserved: { memory_bytes: number; cpu_millicores: number };
  running_guests: number;
  /** Recent online fraction in parts per million (decays over about a day). */
  availability_ppm: number;
  /**
   * The address other installs reach a branch at, as its heartbeat last
   * reported it; null for a local branch and for a shared node.
   */
  public_origin?: string | null;
}

/** How a node relates to the library. */
export type BranchRelationship = "shared_branch" | "remote_branch" | "local_branch";

/** One library node as `GET /api/config/nodes` returns it. */
export interface ConfigNodeView {
  node_id: string;
  target: string;
  /** Operator labels. */
  labels: string[];
  /** Labels every node carries (`node=<id>`, `target=<target>`). */
  implicit_labels: string[];
  /** True while the node's daemon heartbeats. */
  live: boolean;
  runtime: NodeRuntimeView;
  /** Latest heartbeat; null when the node's daemon never ran. */
  status: NodeStatusView | null;
  software_version?: string;
  /** True for the node serving this request. */
  this_node: boolean;
  /** `shared` (shares the database) or `branch` (its own database, enrolled to contribute). */
  kind: "shared" | "branch";
  /**
   * `shared_branch` (shares the database), `remote_branch` (a branch that
   * reports a public address) or `local_branch` (a branch with none).
   */
  relationship: BranchRelationship;
  /** User who administers this node, when one is assigned. */
  owner: { user_id: number; display_name: string | null; role: string } | null;
}

/** What the caller may do on the nodes it was sent. */
export interface NodeCapabilities {
  /** `all` for an operator; `owned` for a user who owns nodes. */
  scope: "all" | "owned";
  can_edit_runtime: boolean;
  can_edit_labels: boolean;
  can_assign_owner: boolean;
}

/** `GET /api/config/nodes`. */
export interface NodesResponse {
  node_id: string;
  nodes: ConfigNodeView[];
  capabilities: NodeCapabilities;
}

/**
 * Lists the nodes the caller may see: every node for an operator, the
 * owned ones for a node owner.
 *
 * @returns The serving node's id, the visible nodes, and what the caller may edit.
 */
export async function listConfigNodes(): Promise<NodesResponse> {
  const res = await fetch("/api/config/nodes", { credentials: "include" });
  return parseJson(res);
}

/** Library-wide health from `GET /api/library-status`. */
export interface LibrarySummary {
  nodes: number;
  live: number;
  drained: number;
  branch_nodes: number;
  cpu_budget_millicores: number;
  cpu_reserved_millicores: number;
  memory_budget_bytes: number;
  memory_reserved_bytes: number;
  running_guests: number;
  active_jobs: number;
  deliveries: {
    pending: number;
    running: number;
    dead_letter: number;
    oldest_pending_age_secs: number | null;
  };
}

/** `GET /api/library-status`: library health plus the nodes the caller may see. */
export interface LibraryStatusResponse extends NodesResponse {
  library: LibrarySummary;
}

/**
 * Library health every node administrator may see, with detail for the
 * caller's visible nodes.
 *
 * @returns Library summary, visible nodes, and the caller's capabilities.
 */
export async function fetchLibraryStatus(): Promise<LibraryStatusResponse> {
  const res = await fetch("/api/library-status", { credentials: "include" });
  return parseJson(res);
}

/**
 * Assigns a node to the user who administers it, or clears its owner.
 *
 * @param nodeId - Node to assign.
 * @param userId - Owning user, or `null` to clear.
 * @returns The node id and its owner.
 */
export async function putNodeOwner(
  nodeId: string,
  userId: number | null,
): Promise<{ node_id: string; owner_user_id: number | null }> {
  const res = await fetch(`/api/config/nodes/${encodeURIComponent(nodeId)}/owner`, {
    method: "PUT",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ user_id: userId }),
  });
  return parseJson(res);
}

/**
 * Edits a node's label, budgets, or drain switch. The node applies it at
 * its next heartbeat.
 *
 * @param nodeId - Node to edit.
 * @param body - Fields to change; omitted fields keep their value.
 * @returns The stored runtime document.
 */
export async function putNodeRuntime(
  nodeId: string,
  body: Partial<NodeRuntimeView>,
): Promise<{ node_id: string; revision: number; runtime: NodeRuntimeView }> {
  const res = await fetch(`/api/config/nodes/${encodeURIComponent(nodeId)}/runtime`, {
    method: "PUT",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return parseJson(res);
}

/**
 * Replaces a node's operator labels.
 *
 * @param nodeId - Node to edit.
 * @param labels - The complete label set (`nas`, `zone=eu`).
 * @returns The updated node.
 */
export async function putNodeLabels(nodeId: string, labels: string[]): Promise<unknown> {
  const res = await fetch(`/api/config/nodes/${encodeURIComponent(nodeId)}/labels`, {
    method: "PUT",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ labels }),
  });
  return parseJson(res);
}

/** A library this install is enrolled in as a host (no key material). */
export interface LibraryLinkView {
  /** Id of the library; one link per library. */
  library_id: string;
  /** Canonical origin (`https://host[:port]`) this host dials the owner at. */
  owner_origin: string;
  /** Node id the owner minted for this install. */
  node_id: string;
  /** True when the link pins the owner's self-signed certificate. */
  pinned: boolean;
  /** `active`, `revoked` (by the owner), or `left` (by this host). */
  state: string;
  /** RFC 3339 time of the latest enrollment (a re-enrollment resets it). */
  enrolled_at: string;
  /** "Share this branch's books with <library>" (on by default). */
  share_books?: boolean;
  /** "Include books from my own branches" (on by default). */
  include_branches?: boolean;
  /** "List my members' books" when the library asks (on by default). */
  list_members_books?: boolean;
  /** "Download for other members of <library>" (off by default). */
  work_for_members?: boolean;
  /** "Keep copies of other members' books" (off by default). */
  keep_member_copies?: boolean;
  /** "Keep copies on this computer" (on by default): take the library's copies of this server's person's books. */
  keep_copies?: boolean;
  /** The most jobs this host runs for the library at once; `null` is an equal share of the machine. */
  max_jobs?: number | null;
  /** Jobs the library may have running here now; absent while its link is not running. */
  job_share?: number;
}

/**
 * The parts of a plugin grant the consent screen shows. Every field is
 * optional because the screen renders whatever the host sent; a missing set
 * means the grant asks for none of it, and a missing or `null` limit leaves
 * that limit to the version's manifest.
 */
export interface RemoteGrantView {
  /** Display alias from `plugin.toml` (not security authoritative). */
  pluginId?: string;
  /** `deny` (no egress) or `outbound`; anything else is treated as `deny`. */
  networkMode?: string;
  /** Outbound domain patterns the guest may dial first (redirect hops aside). */
  domains?: string[];
  /** Raw TCP destinations: a host and the ports allowed on it. */
  tcp?: { host: string; ports: number[] }[];
  /** CIDR ranges beyond the public Internet (private or local addresses). */
  addressCidrs?: string[];
  /** Hosts that receive the plugin's own `User-Agent` instead of the host's. */
  ownUserAgentHosts?: string[] | null;
  /** The plugin's reason for `ownUserAgentHosts`. */
  ownUserAgentReason?: string | null;
  /** Host bindings (`config`, `secrets`, `kv`, …). */
  bindings?: string[];
  /** `[triggers].jobs` types it may handle. */
  jobs?: string[];
  /** Named entrypoints it exports (`storefront`, `storage`, `cli`, …). */
  entrypoints?: string[];
  /** Event types it may publish and queue bindings it may send to. */
  producers?: string[];
  /** Events and queues it consumes (shape owned by the host). */
  consumers?: unknown[];
  /** Follow a redirect to a public host it did not declare. */
  allowUndeclaredPublicRedirects?: boolean;
  /** workerd compatibility flags the isolate runs with. */
  compatibilityFlags?: string[];
  /** Memory ceiling in MiB. */
  memoryMib?: number | null;
  /** CPU rate as a percent of one logical CPU (above 100 asks for more than one core). */
  cpuRatePercent?: number | null;
  /** Disk budget in MiB for each of its `data/` and `tmp/` directories. */
  diskMib?: number | null;
  /** workerd CPU budget per request, in milliseconds. */
  cpuMs?: number | null;
  /** workerd outbound fetch (subrequest) budget per request. */
  subrequests?: number | null;
  /** Processes or threads allowed beyond the launcher's own (native guests). */
  extraProcesses?: number | null;
}

/** Where an owner plugin's request to run here stands. */
export type RemoteConsentState =
  "consented" | "awaiting_host_consent" | "awaiting_host_reapproval" | "invalid_grant";

/** One owner plugin's request to run on this host, and this host's answer. */
export interface RemoteConsentView {
  /** Library that asks; matches a {@link LibraryLinkView.library_id}. */
  library_id: string;
  /** Canonical plugin key the request is for. */
  plugin_key: string;
  /** Version the owner now asks to run. */
  requested_version: string;
  /** What that version asks for; `null` when the stored request does not parse. */
  requested_grant: RemoteGrantView | null;
  /** The archive this host would fetch for that version (hex SHA-256 digests). */
  requested_artifact?: {
    /** `blob:` (served by the owner) or `https:` archive URL. */
    url: string;
    /** Digest of the archive file. */
    archive_sha256: string;
    /** Digest of the unpacked payload tree. */
    payload_root_sha256: string;
  };
  /** Digest of the whole request (version, grant, archive); approve by it. */
  request_digest: string;
  /** Version this host approved, if any. */
  approved_version: string | null;
  /** What this host granted (the request, narrowed), if approved. */
  grant: RemoteGrantView | null;
  /** RFC 3339 time of the approval, if approved. */
  approved_at: string | null;
  /** Who approved: the operator's audit identity, or `platform` for an automatic approval. */
  approved_by: string | null;
  /** RFC 3339 time this host's operator revoked it; a revoke holds until approved again. */
  revoked_at?: string | null;
  /** RFC 3339 time the row last changed. */
  updated_at?: string;
  /** Whether the guest may run here now, or what it waits on. */
  state: RemoteConsentState;
  /**
   * What the current request asks for beyond the request this host approved,
   * as the lines the CLI prints; empty when nothing was approved yet.
   */
  pending?: string[];
}

/** `GET /api/config/library-consents`. */
export interface LibraryConsentsResponse {
  /** Every library link this install holds, in any state. */
  libraries: LibraryLinkView[];
  /** Every recorded owner plugin request, approved or not. */
  consents: RemoteConsentView[];
}

/**
 * Body of an approval: the reviewed request and what of it to leave out.
 * An approval can only narrow the request, never widen it.
 */
export interface RemoteApprovalBody {
  /** Library of the request. */
  library_id: string;
  /** Plugin key of the request. */
  plugin_key: string;
  /** The `request_digest` reviewed; refused when the recorded request is another one. */
  request_digest: string;
  /** Outbound domain patterns to remove from the grant. */
  deny_domains?: string[];
  /** Host bindings to remove (`secrets`, `oauth`, `kv`, …). */
  deny_bindings?: string[];
  /** Requested hosts refused the plugin's own User-Agent; they get Bookclerk's agent. */
  deny_own_user_agent_hosts?: string[];
  /** True turns outbound network off altogether (no domains, TCP or CIDRs). */
  deny_network?: boolean;
}

/**
 * Like `parseJson`, but a refusal's `detail` becomes the error message.
 *
 * @param res - Response from a consent route.
 * @returns The parsed body.
 */
async function parseConsentJson<T>(res: Response): Promise<T> {
  if (res.ok) return res.json() as Promise<T>;
  const text = await res.text().catch(() => "");
  let message = text || `${res.status} ${res.statusText}`;
  let code: string | undefined;
  try {
    const body = JSON.parse(text) as { error?: string; detail?: string; message?: string };
    code = typeof body.error === "string" ? body.error : undefined;
    message = body.detail?.trim() || body.message?.trim() || body.error?.trim() || message;
  } catch {
    // keep raw text
  }
  throw new ApiError(res.status, message, { code });
}

/**
 * Lists the libraries this host is enrolled in and what each asks to
 * run here. Operator only.
 *
 * @returns Links and consent rows.
 */
export async function listLibraryConsents(): Promise<LibraryConsentsResponse> {
  const res = await fetch("/api/config/library-consents", { credentials: "include" });
  return parseConsentJson(res);
}

/**
 * Approves an owner plugin's request, optionally narrowed. Only what the
 * request asks for can be approved.
 *
 * @param body - The request reviewed and what to leave out.
 * @returns The consent row after approval.
 */
export async function approveLibraryConsent(
  body: RemoteApprovalBody,
): Promise<{ consent: RemoteConsentView }> {
  const res = await fetch("/api/config/library-consents/approve", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return parseConsentJson(res);
}

/**
 * Withdraws an approval: held jobs of the plugin are cancelled and given
 * back, and nothing of it is claimed until approved again.
 *
 * @param libraryId - Library.
 * @param pluginKey - Canonical plugin key.
 * @returns The consent row after revoking.
 */
export async function revokeLibraryConsent(
  libraryId: string,
  pluginKey: string,
): Promise<{ consent: RemoteConsentView }> {
  const res = await fetch("/api/config/library-consents/revoke", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ library_id: libraryId, plugin_key: pluginKey }),
  });
  return parseConsentJson(res);
}

/**
 * Writes bootstrap settings (`daemon`, `jail`, `[database]` keys) through
 * `PATCH /api/settings`. Plugin settings are not accepted here: they are
 * instance documents, written with {@link putPluginInstanceConfig}.
 *
 * @param body - Settings updates to apply.
 * @returns The settings view after the write.
 */
export async function patchSettings(body: {
  settings: SettingsUpdate[];
  /** `core.output` revision the caller read; a stale value is rejected with 409. */
  expected_output_revision?: number;
}): Promise<SettingsResponse> {
  const res = await fetch("/api/settings", {
    method: "PATCH",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return parseJson<SettingsResponse>(res);
}

/** One diagnostics report this install sent (`GET /api/diagnostics/reports`). */
export interface DiagnosticsReport {
  /** The collector's ID for the report; the GitHub issue filed for it mentions this ID. */
  report_id: string;
  /** Why it was sent: `crash`, `error_burst`, `slow_burst`, `job_failed`, `cli_error`, …. */
  trigger: string;
  /** The Bookclerk version that sent it. */
  version: string;
  /** When the collector accepted it, Unix milliseconds. */
  sent_at_unix_ms: number;
  /** The code that reads it back or deletes it (`XXXX-XXXX-XXXX-XXXX`), or null when the collector issued none. */
  access_code: string | null;
  /** When this install deleted the collector's stored copy, Unix milliseconds, or null. */
  deleted_at_unix_ms: number | null;
}

/** One logged event in a stored report. */
export interface DiagnosticsReportEvent {
  /** When it happened, Unix milliseconds, or 0 when unknown. */
  ts_unix_ms: number;
  /** Severity. */
  level: "ERROR" | "WARN" | "INFO" | "DEBUG" | "TRACE";
  /** Where it was logged. */
  target: string;
  /** The redacted message. */
  message: string;
  /** Redacted fields recorded with it. */
  fields: { name: string; value: string }[];
}

/** A report read back from the collector (`GET /api/diagnostics/reports/{id}`). */
export interface DiagnosticsReportLookup {
  /** The report's ID. */
  report_id: string;
  /** When the collector received it, Unix milliseconds. */
  received_at_unix_ms: number;
  /** When its stored copy was deleted, Unix milliseconds, or null. */
  deleted_at_unix_ms: number | null;
  /** The stored report, or null once deleted. */
  report: {
    /** Version that sent it. */
    version: string;
    /** Why it was sent. */
    trigger: string;
    /** Operating system. */
    os: string;
    /** CPU architecture. */
    arch: string;
    /** Linux distribution, or empty. */
    distro: string;
    /** Events, oldest first. */
    events: DiagnosticsReportEvent[];
  } | null;
  /** GitHub issues tracking it, the tracking issue first. */
  issues: { number: number; title: string; state: "open" | "closed"; url: string }[];
  /** The tracking issue's fix, or null. */
  fix: { pr_number: number | null; pr_url: string | null; fixed_in: string | null } | null;
}

/**
 * Reads a sent report back from the collector with its access code (operator only).
 *
 * @param reportId - The report's ID.
 * @returns The report, its issues and its fix.
 */
export async function fetchDiagnosticsReport(reportId: string): Promise<DiagnosticsReportLookup> {
  const res = await fetch(`/api/diagnostics/reports/${encodeURIComponent(reportId)}`, {
    credentials: "include",
  });
  return parseJson<DiagnosticsReportLookup>(res);
}

/**
 * Deletes the collector's stored copy of a sent report; its summary and issue
 * links stay (operator only).
 *
 * @param reportId - The report's ID.
 * @returns When the copy was deleted, Unix milliseconds.
 */
export async function deleteDiagnosticsReport(reportId: string): Promise<number> {
  const res = await fetch(`/api/diagnostics/reports/${encodeURIComponent(reportId)}`, {
    method: "DELETE",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: "{}",
  });
  const body = await parseJson<{ deleted_at_unix_ms: number }>(res);
  return body.deleted_at_unix_ms;
}

/**
 * Lists the diagnostics reports this install sent (operator only).
 *
 * @returns Reports, newest first (the install keeps the last 200).
 */
export async function fetchDiagnosticsReports(): Promise<DiagnosticsReport[]> {
  const res = await fetch("/api/diagnostics/reports", { credentials: "include" });
  const body = await parseJson<{ reports: DiagnosticsReport[] }>(res);
  return body.reports;
}

/**
 * Saves the install's diagnostics sharing choices (`diagnostics.share_reports`
 * and `diagnostics.share_usage`). The daemon applies them without a restart.
 *
 * @param shareReports - Upload redacted reports after crashes and error bursts.
 * @param shareUsage - Send the weekly anonymous check-in and tag reports with the install ID.
 * @returns The settings view after the write.
 */
export async function saveDiagnosticsSharing(
  shareReports: boolean,
  shareUsage: boolean,
): Promise<SettingsResponse> {
  return patchSettings({
    settings: [
      { key: "diagnostics.share_reports", value: String(shareReports) },
      { key: "diagnostics.share_usage", value: String(shareUsage) },
    ],
  });
}

/**
 * Redeems a claim ticket into a portal user session.
 *
 * @param ticket - One-time claim ticket string.
 * @param password - Optional password when claiming invite/reset tickets.
 * @returns Resolves when the session cookie is set.
 */
export async function portalRedeem(ticket: string, password?: string): Promise<void> {
  const nonce = claimRedeemNonce(ticket);
  const body: { ticket: string; nonce: string; password?: string } = { ticket, nonce };
  if (password?.trim()) {
    body.password = password.trim();
  }
  const res = await fetch("/api/portal/redeem", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  await parseJson(res);
  clearClaimRedeemNonce(ticket);
}

const CLAIM_REDEEM_NONCE_RE = /^[0-9a-f]{64}$/i;

function claimRedeemNonceKey(ticket: string): string {
  return `bookclerk.claimRedeemNonce:${ticket}`;
}

/**
 * Persist a 32-byte redeem nonce across HTTP retries for this ticket.
 *
 * The nonce binds the committed session to the initiating browser so a used
 * magic-link URL alone cannot replay the receipt.
 *
 * @param ticket
 * @returns
 */
function claimRedeemNonce(ticket: string): string {
  const key = claimRedeemNonceKey(ticket);
  try {
    const existing = sessionStorage.getItem(key);
    if (existing && CLAIM_REDEEM_NONCE_RE.test(existing)) {
      return existing.toLowerCase();
    }
  } catch {
    // sessionStorage can be unavailable in private modes.
  }
  const bytes = new Uint8Array(32);
  crypto.getRandomValues(bytes);
  const nonce = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
  try {
    sessionStorage.setItem(key, nonce);
  } catch {
    // Keep going with an in-memory nonce for this attempt.
  }
  return nonce;
}

function clearClaimRedeemNonce(ticket: string): void {
  try {
    sessionStorage.removeItem(claimRedeemNonceKey(ticket));
  } catch {
    // ignore
  }
}

/**
 * Signs in via an integration provider username/password.
 *
 * @param body - Provider id plus credentials.
 * @returns Resolves when the portal session is established.
 */
export async function portalLoginIntegration(body: {
  provider: string;
  username: string;
  password: string;
}): Promise<void> {
  const res = await fetch("/api/portal/login/integration", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  await parseJson(res);
}

/**
 * Clears the portal / user session cookie.
 *
 * @returns Resolves when logout finishes (best-effort).
 */
export async function portalLogout(): Promise<void> {
  // What a viewer pays depends on their linked store accounts. Cleared
  // again once the library answers (see `logout`).
  clearLookupCaches();
  try {
    await fetch("/api/portal/logout", {
      method: "POST",
      credentials: "include",
      headers: { "Content-Type": "application/json" },
      body: "{}",
    });
  } finally {
    clearLookupCaches();
  }
}

/**
 * Loads the linked portal identity for the signed-in SPA session.
 *
 * @returns Portal identity metadata used by Accounts / Settings headers.
 */
export async function portalMe(): Promise<PortalMe> {
  const res = await fetch("/api/portal/me", { credentials: "include" });
  return parseJson(res);
}

/**
 * Lists connectable storefronts for Accounts.
 *
 * @returns Source descriptors including brand metadata.
 */
export async function portalSources(): Promise<PortalSource[]> {
  const res = await fetch("/api/portal/sources", { credentials: "include" });
  const body = await parseJson<{ sources: PortalSource[] }>(res);
  return body.sources;
}

/**
 * Password-connects a storefront account.
 *
 * @param id - Source plugin id.
 * @param body - Store email and password.
 * @returns Resolves when the connection is stored.
 */
export async function portalSourceLogin(
  id: string,
  body: { email: string; password: string },
): Promise<void> {
  const res = await fetch(`/api/portal/sources/${encodeURIComponent(id)}/login`, {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  await parseJson(res);
}

/**
 * Starts OAuth for a storefront and returns the authorize URL.
 *
 * @param id - Source plugin id.
 * @returns Object with browser `url` to open.
 */
export async function portalSourceOauthStart(id: string): Promise<{ url: string }> {
  const res = await fetch(`/api/portal/sources/${encodeURIComponent(id)}/oauth/start`, {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: "{}",
  });
  return parseJson(res);
}

/**
 * Lists linked store accounts for the current portal user.
 *
 * @returns Connection rows.
 */
export async function portalConnections(): Promise<PortalConnection[]> {
  const res = await fetch("/api/portal/connections", { credentials: "include" });
  const body = await parseJson<{ connections: PortalConnection[] }>(res);
  return body.connections;
}

/**
 * Disconnects a storefront account from the current portal user.
 *
 * Does not delete library rows; only revokes the sealed credentials / link.
 * Accounts belong to a plugin, so the source (alias or PluginKey) is part of
 * the address: the same account id may exist on two stores.
 *
 * @param source - Source id or PluginKey from {@link portalConnections}.
 * @param accountId - Portal connection account id from {@link portalConnections}.
 * @returns Resolves when the revoke POST succeeds.
 */
export async function portalRevokeConnection(source: string, accountId: string): Promise<void> {
  const res = await fetch(
    `/api/portal/sources/${encodeURIComponent(source)}/connections/${encodeURIComponent(accountId)}/revoke`,
    {
      method: "POST",
      credentials: "include",
      headers: { "Content-Type": "application/json" },
      body: "{}",
    },
  );
  await parseJson(res);
}

/** Work to finish while the session can still authorize it. */
const signOutTasks = new Set<() => Promise<void>>();

/** Run once the request that changes the identity has answered or failed. */
const identityChangedTasks = new Set<() => void>();

/**
 * How long a change of identity waits for the work registered with
 * {@link beforeSignOut}, in milliseconds.
 */
export const BEFORE_SIGN_OUT_WAIT_MS = 3000;

/**
 * Registers work that must finish while the signed-in identity still
 * holds, such as the player's last position report.
 *
 * @param task - Run twice at the start of every sign-out and before
 *   impersonation starts or stops, so it must do nothing when there is
 *   nothing left to finish, and awaited for up to
 *   {@link BEFORE_SIGN_OUT_WAIT_MS}. A task that fails, throws or takes
 *   longer does not stop the change.
 * @param after - Run once the request that changes the identity has
 *   answered or failed, to take back whatever `task` held off meanwhile
 *   (the player starts no book in between).
 * @returns Removes the task and `after`.
 */
export function beforeSignOut(task: () => Promise<void>, after?: () => void): () => void {
  signOutTasks.add(task);
  if (after) identityChangedTasks.add(after);
  return () => {
    signOutTasks.delete(task);
    if (after) identityChangedTasks.delete(after);
  };
}

/**
 * Changes the signed-in identity: first the work registered with
 * {@link beforeSignOut}, then the request, then the registered `after`
 * callbacks whether the request succeeded or not.
 *
 * @param change - Sends the request that changes the identity.
 * @returns What `change` resolved with.
 */
async function changeIdentity<T>(change: () => Promise<T>): Promise<T> {
  await finishBeforeIdentityChange();
  try {
    return await change();
  } finally {
    for (const after of identityChangedTasks) {
      try {
        after();
      } catch {
        // One callback that throws does not keep the others from running.
      }
    }
  }
}

/**
 * Runs the work registered with {@link beforeSignOut} and waits for it,
 * but no longer than {@link BEFORE_SIGN_OUT_WAIT_MS}: a request that never
 * answers must not keep someone signed in.
 *
 * The tasks run twice, within the one wait: the second pass ends what
 * started while the first was out.
 *
 * @returns Resolves when both passes settled or the wait ran out.
 */
async function finishBeforeIdentityChange(): Promise<void> {
  /**
   * Starts every registered task.
   *
   * @returns Settles when all of them have.
   */
  const pass = (): Promise<unknown> =>
    Promise.all(
      // Through `then`, so a task that throws before returning is caught too.
      [...signOutTasks].map((task) =>
        Promise.resolve()
          .then(task)
          .catch(() => undefined),
      ),
    );
  let timer: ReturnType<typeof setTimeout> | undefined;
  let ranOut = false;
  const waited = new Promise<void>((resolve) => {
    timer = setTimeout(() => {
      ranOut = true;
      resolve();
    }, BEFORE_SIGN_OUT_WAIT_MS);
  });
  await Promise.race([pass(), waited]);
  // Something may have started during the wait (a book chosen while a
  // close was out). It is ended too, as the identity being left: a second
  // pass, awaited for what is left of the same wait. Once the wait has run
  // out the pass still runs, so its requests are sent before the one that
  // changes the identity.
  const again = pass();
  if (!ranOut) await Promise.race([again, waited]);
  clearTimeout(timer);
}

/**
 * Signs out regardless of operator vs user/portal session, after the
 * work registered with {@link beforeSignOut}.
 *
 * @param role - Current session role; portal roles use portal logout.
 * @returns Resolves when logout completes.
 */
export async function signOut(role?: AuthRole): Promise<void> {
  await changeIdentity(async () => {
    if (role === "administrator" || role === "member") {
      await portalLogout();
      return;
    }
    try {
      await logout();
    } catch {
      await portalLogout().catch(() => undefined);
    }
  });
}

/**
 * Loads aggregate library status counters.
 *
 * @returns Status payload for the jobs strip / library header.
 */
export async function fetchStatus(): Promise<StatusResponse> {
  const res = await fetch("/api/status", { credentials: "include" });
  return parseJson(res);
}

/**
 * Lists recent background jobs for the jobs strip / status polling.
 *
 * @returns Job rows from `/api/jobs` (newest first as returned by the daemon).
 */
export async function fetchJobs(): Promise<JobInfo[]> {
  const res = await fetch("/api/jobs", { credentials: "include" });
  return parseJson(res);
}

/** Filters and paging for {@link fetchWorks}. */
export interface WorksQuery {
  /** Search text; matches come best first and `sort` is ignored. */
  q?: string;
  /** Page order without `q`. */
  sort?: WorkSort;
  /** Only works with an edition of this media kind. */
  media_kind?: MediaKind;
  /** Only books or only podcasts. */
  kind?: WorkKind;
  /** Only works in this acquire state (`all` or omitted for every state). */
  status?: AcquireStatus | "all";
  /** Only works held on this account id. */
  account?: string;
  /** Only works with {@link WorksQuery.value} under this facet. */
  facet?: WorkFacet;
  /** The facet's value; an empty series value means works in no series. */
  value?: string;
  /** The previous page's `next_cursor`. */
  cursor?: string;
  /** Page size (the daemon caps it at 200). */
  limit?: number;
}

/**
 * Fetches one page of the library's works.
 *
 * @param query - Filters, order and the cursor of the page to read.
 * @returns The page and the next page's cursor.
 */
export async function fetchWorks(query: WorksQuery): Promise<WorksPage> {
  const sp = new URLSearchParams();
  if (query.q) sp.set("q", query.q);
  if (query.sort && !query.q) sp.set("sort", query.sort);
  if (query.media_kind) sp.set("media_kind", query.media_kind);
  if (query.kind) sp.set("kind", query.kind);
  if (query.status && query.status !== "all") sp.set("status", query.status);
  if (query.account) sp.set("account", query.account);
  if (query.facet) {
    sp.set("facet", query.facet);
    sp.set("value", query.value ?? "");
  }
  if (query.cursor) sp.set("cursor", query.cursor);
  sp.set("limit", String(query.limit ?? 40));
  const res = await fetchRetryingBusy(`/api/library/works?${sp}`, {
    credentials: "include",
  });
  return parseJson(res);
}

/**
 * Fetches one page of a facet's values across the library's held works.
 *
 * @param facet - The facet (not `progress`, whose values are fixed).
 * @param cursor - The previous page's `next_cursor`; omitted for the first.
 * @returns The page and the next page's cursor.
 */
export async function fetchFacetValues(
  facet: Exclude<WorkFacet, "progress">,
  cursor?: string,
): Promise<FacetValuesPage> {
  const sp = new URLSearchParams({ limit: "500" });
  if (cursor) sp.set("cursor", cursor);
  const res = await fetchRetryingBusy(
    `/api/library/works/facets/${encodeURIComponent(facet)}?${sp}`,
    { credentials: "include" },
  );
  return parseJson(res);
}

/**
 * Fetches one work with its editions, listings, holdings and own state.
 *
 * @param id - Work id.
 * @returns The work.
 */
export async function fetchWork(id: string): Promise<WorkDetail> {
  const res = await fetchRetryingBusy(`/api/library/works/${encodeURIComponent(id)}`, {
    credentials: "include",
  });
  return parseJson(res);
}

/**
 * Changes the caller's own finished flag, ratings or tags for a work.
 *
 * @param id - The work.
 * @param edit - The fields to change.
 * @returns The caller's state after the change.
 */
export async function putWorkState(id: string, edit: WorkStateEdit): Promise<WorkState> {
  const res = await fetch(`/api/library/works/${encodeURIComponent(id)}/state`, {
    method: "PUT",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(edit),
  });
  return parseJson(res);
}

/**
 * The editions of `work` an acquire should queue: those someone holds
 * that are not acquired or on their way.
 *
 * @param work - A work's detail.
 * @returns Edition ids for {@link triggerAcquire}'s `editions`.
 */
export function acquirableEditions(work: WorkDetail): string[] {
  return work.editions
    .filter(
      (e) =>
        e.listings.some((l) => l.holdings.length > 0) &&
        e.acquire_status !== "acquired" &&
        e.acquire_status !== "queued" &&
        e.acquire_status !== "downloading",
    )
    .map((e) => e.id);
}

/**
 * Queues a full-library scan job for accounts with scan enabled.
 *
 * Prefer CLI one-shot scans with an explicit `--account` in cloud testing; this
 * SPA helper hits the authenticated daemon endpoint used by the Library UI.
 *
 * @returns Action ack including `job_id` when accepted.
 */
export async function triggerScan(): Promise<ActionResponse> {
  const res = await fetch("/api/library/scan", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: "{}",
  });
  return parseJobAdmit(res);
}

/**
 * Queues an acquire: everything pending when the body names nothing, or
 * the named editions or title.
 *
 * @param body - `editions` (ids from a work's detail), or a title by
 *   `uuid` / `asin`.
 * @returns Action ack including `job_id`.
 */
export async function triggerAcquire(body: {
  editions?: string[];
  uuid?: string;
  asin?: string;
}): Promise<ActionResponse> {
  const res = await fetch("/api/library/acquire", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return parseJobAdmit(res);
}

/**
 * Requests cancel of a pending job or cooperative stop of a running job.
 *
 * @param id - Durable job id from `/api/jobs`.
 * @returns Action ack including the job id.
 */
export async function cancelJob(id: string): Promise<ActionResponse> {
  const res = await fetch(`/api/jobs/${encodeURIComponent(id)}/cancel`, {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: "{}",
  });
  return parseJson(res);
}

/**
 * Lists recent event deliveries for the Library strip.
 *
 * @param state - Optional lifecycle filter (`dead_letter`, `running`, …).
 * @returns Delivery rows from `/api/events/deliveries`.
 */
export async function fetchEventDeliveries(state?: string): Promise<EventDeliveryInfo[]> {
  const sp = new URLSearchParams();
  if (state) sp.set("state", state);
  const res = await fetch(`/api/events/deliveries?${sp}`, {
    credentials: "include",
  });
  return parseJson(res);
}

async function postEventDeliveryAction(
  id: string,
  action: "retry" | "acknowledge" | "cancel" | "resume",
): Promise<ActionResponse> {
  const res = await fetch(`/api/events/deliveries/${encodeURIComponent(id)}/${action}`, {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: "{}",
  });
  return parseJson(res);
}

/**
 * Re-queues a dead-lettered delivery.
 *
 * @param id - Delivery id (`{eventId}:{pluginId}`).
 * @returns Action ack from the daemon.
 */
export function retryEventDelivery(id: string): Promise<ActionResponse> {
  return postEventDeliveryAction(id, "retry");
}

/**
 * Acknowledges a dead-lettered delivery (no further retry).
 *
 * @param id - Delivery id (`{eventId}:{pluginId}`).
 * @returns Action ack from the daemon.
 */
export function ackEventDelivery(id: string): Promise<ActionResponse> {
  return postEventDeliveryAction(id, "acknowledge");
}

/**
 * Cancels a pending delivery or flags a running one.
 *
 * @param id - Delivery id (`{eventId}:{pluginId}`).
 * @returns Action ack from the daemon.
 */
export function cancelEventDelivery(id: string): Promise<ActionResponse> {
  return postEventDeliveryAction(id, "cancel");
}

/**
 * Wakes a suspended delivery for the next worker claim.
 *
 * @param id - Delivery id (`{eventId}:{pluginId}`).
 * @returns Action ack from the daemon.
 */
export function resumeEventDelivery(id: string): Promise<ActionResponse> {
  return postEventDeliveryAction(id, "resume");
}

/**
 * Builds the authenticated URL of the cover an acquire wrote beside an
 * edition's file (404 when this node has none).
 *
 * @param editionId - Edition id.
 * @returns Relative URL under `/api/library/editions/.../cover`.
 */
export function editionCoverUrl(editionId: string): string {
  return `/api/library/editions/${encodeURIComponent(editionId)}/cover`;
}

/** A processed cover size the daemon serves: thumbnail, card, or full size. */
export type CoverSize = 128 | 360 | "full";

/**
 * The same-origin URL of one processed size of a remote cover.
 *
 * The daemon hands out `cover_path` (`/api/covers/<token>`) for every remote
 * `https` cover, so the web app never loads `cover_url` from another site.
 *
 * @param path - A `cover_path` from an API response.
 * @param size - The size to load.
 * @returns The image URL, or `null` when there is no usable path.
 */
export function coverSrc(path: string | null | undefined, size: CoverSize): string | null {
  const trimmed = path?.trim();
  if (!trimmed || !trimmed.startsWith("/api/covers/") || trimmed.includes("?")) return null;
  return `${trimmed}/${size}`;
}

/**
 * Storefront product identity for a catalog work.
 */
export interface StoreEdition {
  source: string;
  product_id: string;
}

/**
 * Live or seeded storefront price / buy URL for a title.
 */
export interface PurchaseHint {
  source: string;
  product_id: string;
  title: string | null;
  url: string | null;
  price_cents?: number | null;
  currency?: string | null;
  price_label?: string | null;
  list_price_cents?: number | null;
  list_price_label?: string | null;
  member_price_cents?: number | null;
  member_price_label?: string | null;
}

/**
 * Purchase hint list plus optional best pick.
 */
export interface PurchaseHintsResponse {
  hints: PurchaseHint[];
  best: PurchaseHint | null;
  /**
   * True when the server's batch deadline cut the lookup short: the hints are
   * links without prices, so the answer is not cached and should be asked
   * for again later.
   */
  incomplete?: boolean;
}

/**
 * Discover shelf recommendation item.
 */
export interface Recommendation {
  work_id: string | null;
  title: string;
  authors: string | null;
  narrators: string | null;
  series: string | null;
  series_index: string | null;
  asin: string | null;
  isbn: string | null;
  score: number;
  reasons: string[];
  /** Stable shelf-kind tags (`finish_series`, `author`, `requests`, …). */
  categories?: string[];
  purchase_hints: PurchaseHint[];
  from_request: boolean;
  request_uuid: string | null;
  candidate_source: string | null;
  candidate_product_id: string | null;
  store_editions?: StoreEdition[];
  seed_categories?: string | null;
  /** Stable bibliographic key (`isbn:` / `asin:` / `soft:`…). */
  work_key?: string;
  cover_url?: string | null;
  /** Same-origin path of this cover's processed images; see {@link coverSrc}. */
  cover_path?: string | null;
  subtitle?: string | null;
  description?: string | null;
  publisher?: string | null;
  length_minutes?: number | null;
  published_at?: string | null;
  genres?: string | null;
  language?: string | null;
  /** People who have this title on an open wishlist. */
  wishers?: QueueWisher[];
  /** Distinct wisher count (may exceed `wishers.length`). */
  wish_count?: number;
  /** Title of the book or wish the storefront was asked about to find this card. */
  seed_title?: string | null;
  /** Starter genre this card was browsed from while the taste was thin. */
  starter_genre?: string | null;
  /** Branches of this library that currently list this title. */
  available_through?: BranchAvailability[];
}

/**
 * One branch of this library that can supply a title (it answered a
 * catalog lookup for the title's work).
 */
export interface BranchAvailability {
  /** This library's work the branch supplies; what {@link requestFromBranch} asks for. */
  work_id: string;
  /** The branch node id. */
  node_id: string;
  /** The branch owner's display name, when the owner has one. */
  owner_name?: string | null;
  /** The store the branch has the book from (`audible`). */
  store: string;
  /** The store's product id of the branch's copy. */
  product_id: string;
}

/**
 * One line naming the branches a title is available through.
 *
 * @param through - The title's branch availability.
 * @returns For example "Available through Riley's branch", or `null` when
 *   no branch lists the title.
 */
export function availableThroughLabel(through: BranchAvailability[] | undefined): string | null {
  const branches: string[] = [];
  const seen = new Set<string>();
  for (const b of through ?? []) {
    if (seen.has(b.node_id)) continue;
    seen.add(b.node_id);
    const owner = b.owner_name?.trim();
    branches.push(owner ? `${owner}'s branch` : "a branch");
  }
  if (branches.length === 0) return null;
  if (branches.length === 1) return `Available through ${branches[0]}`;
  if (branches.length === 2) return `Available through ${branches[0]} and ${branches[1]}`;
  return `Available through ${branches[0]} and ${branches.length - 1} other branches`;
}

/** `POST /api/branch-requests` answer. */
export interface BranchRequestQueued {
  /** The request's job, as `/api/jobs` lists it. */
  job_id: string;
  /** The branch the request went to. */
  node_id: string;
  /** `true` when a request for this work to this branch was already waiting. */
  already_queued: boolean;
}

/**
 * Asks a branch for a book it lists. The branch takes it automatically;
 * the book arrives with the branch's next catalog sync.
 *
 * @param workId - The title's work (`BranchAvailability.work_id`).
 * @param nodeId - The branch to ask.
 * @returns The queued request.
 * @throws {ApiError} 404 when the branch no longer lists the title.
 */
export async function requestFromBranch(
  workId: string,
  nodeId: string,
): Promise<BranchRequestQueued> {
  const res = await fetchWithTimeout(
    "/api/branch-requests",
    {
      method: "POST",
      credentials: "include",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ work_id: workId, node_id: nodeId }),
    },
    8_000,
    "Branch request",
  );
  return parseJson(res);
}

/**
 * The distinct branches a title is available through, one entry per
 * branch, in the order given, with the label to ask each by.
 *
 * @param through - The title's branch availability.
 * @returns For example `[{ node_id, work_id, label: "Riley's branch" }]`.
 */
export function branchesToAsk(
  through: BranchAvailability[] | undefined,
): { node_id: string; work_id: string; label: string }[] {
  const out: { node_id: string; work_id: string; label: string }[] = [];
  const seen = new Set<string>();
  for (const b of through ?? []) {
    if (!b.work_id || seen.has(b.node_id)) continue;
    seen.add(b.node_id);
    const owner = b.owner_name?.trim();
    out.push({
      node_id: b.node_id,
      work_id: b.work_id,
      label: owner ? `${owner}'s branch` : "the branch",
    });
  }
  return out;
}

/** The signed-in user's catalog listing choice. */
export interface CatalogListingView {
  /** `true` (the default) unless the user keeps their books out of catalog answers. */
  listed: boolean;
  /** Libraries this server is currently a branch of; the choice matters only above zero. */
  libraries: number;
  /** `true` for an Owner, whose books are always listed; `listed` then has no effect. */
  always_listed: boolean;
}

/**
 * Whether the signed-in user's books are listed to the libraries this
 * server is a branch of (`GET /api/auth/profile/catalog-listing`).
 *
 * @returns The choice, and how many libraries it applies to.
 */
export async function fetchCatalogListing(): Promise<CatalogListingView> {
  const res = await fetch("/api/auth/profile/catalog-listing", { credentials: "include" });
  return parseJson(res);
}

/**
 * Sets whether the signed-in user's books are listed to the libraries this
 * server is a branch of (`PUT /api/auth/profile/catalog-listing`).
 *
 * @param listed - `false` keeps the user's books out of catalog answers.
 * @returns The stored choice.
 */
export async function setCatalogListing(listed: boolean): Promise<CatalogListingView> {
  const res = await fetch("/api/auth/profile/catalog-listing", {
    method: "PUT",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ listed }),
  });
  return parseJson(res);
}

/**
 * Query used to resolve purchase hints for a title.
 */
export interface PurchaseHintsQuery {
  title: string;
  authors?: string | null;
  asin?: string | null;
  isbn?: string | null;
  candidate_source?: string | null;
  candidate_product_id?: string | null;
  store_editions?: StoreEdition[];
  region?: string;
}

/**
 * One Discover feed shelf (id, copy, items).
 */
export interface DiscoverShelf {
  id: string;
  title: string;
  subtitle: string | null;
  items: Recommendation[];
}

/**
 * Shelf kind id/label for preference toggles.
 */
export interface ShelfKindInfo {
  id: string;
  label: string;
}

/**
 * Discover recommendations feed payload.
 */
export interface DiscoverFeed {
  shelves: DiscoverShelf[];
  shelf_kinds?: ShelfKindInfo[];
  /**
   * How current the stored shelves are: `preparing` (none stored yet),
   * `refreshing` (newer ones queued) or `ready`.
   */
  state?: DiscoverFeedState;
  /** RFC 3339 time the stored shelves were ranked. */
  computed_at?: string;
}

/** How current a stored Discover feed is. */
export type DiscoverFeedState = "ready" | "refreshing" | "preparing";

/**
 * Wishlist / title-request row from `/api/wishlist`.
 *
 * Represents a user ask for a work (and optional acquire queue linkage via
 * `resolved_edition_id`, the edition that fulfilled it).
 */
export interface TitleRequest {
  id: number;
  uuid: string;
  identity_id: number | null;
  title: string;
  authors: string | null;
  asin: string | null;
  isbn: string | null;
  notes: string | null;
  status: string;
  /** Bibliographic key derived from the wish's identifiers, for display matching. */
  work_key: string;
  /** Library work the wish is for; every wish names one. */
  work_id: string;
  /** Edition the wish named, when its identifiers reached exactly one. */
  edition_id?: string | null;
  resolved_edition_id: string | null;
  cover_url?: string | null;
  /** Same-origin path of this cover's processed images; see {@link coverSrc}. */
  cover_path?: string | null;
  description?: string | null;
  subtitle?: string | null;
  narrators?: string | null;
  series?: string | null;
  series_index?: string | null;
  publisher?: string | null;
  length_minutes?: number | null;
  published_at?: string | null;
  genres?: string | null;
  language?: string | null;
  store_editions?: StoreEdition[];
  purchase_hints?: PurchaseHint[];
  created_at: string;
  updated_at: string;
}

/**
 * Person who has a title on an open wishlist (global queue avatars).
 */
export interface QueueWisher {
  /** First-party user id when the portal identity is linked. */
  user_id?: number | null;
  /** Portal identity that created the wish. */
  identity_id?: number | null;
  /** Display name, identity label, or Operator. */
  display_name?: string | null;
  /** Local login name when set. */
  login_name?: string | null;
  /** True when the wish came from the operator token. */
  operator?: boolean;
  /** True when an uploaded avatar file exists. */
  has_avatar?: boolean;
  /** Explicit picture choice (`auto` / `monogram` / `gravatar` / `upload` / `sso:{id}`). */
  avatar_source?: string | null;
  /** SHA-256 hex of the contact email for Gravatar. */
  gravatar_hash?: string | null;
  /** HTTPS picture from the wishing identity, when any; never loaded directly. */
  picture_url?: string | null;
  /** Same-origin 128 px copy of `picture_url`, when the server signed one. */
  picture_path?: string | null;
}

/**
 * Aggregated global request-queue entry (shared wishlist demand).
 */
export interface GlobalQueueEntry {
  work_key: string;
  /**
   * Library work the entry's wishes are for. Wishes for one work share an
   * entry even when their `work_key`s differ, so match on this first.
   */
  work_id: string;
  title: string;
  authors: string | null;
  asin: string | null;
  isbn: string | null;
  cover_url?: string | null;
  /** Same-origin path of this cover's processed images; see {@link coverSrc}. */
  cover_path?: string | null;
  description?: string | null;
  subtitle?: string | null;
  narrators?: string | null;
  series?: string | null;
  series_index?: string | null;
  publisher?: string | null;
  length_minutes?: number | null;
  published_at?: string | null;
  genres?: string | null;
  language?: string | null;
  store_editions?: StoreEdition[];
  purchase_hints?: PurchaseHint[];
  wish_count: number;
  /** Distinct people who have this title on an open wishlist. */
  wishers?: QueueWisher[];
  sample_uuids: string[];
  first_requested_at: string;
  last_requested_at: string;
  /** Final rank (`taste_score + wish_count` boost). */
  score?: number;
  taste_score?: number;
  reasons?: string[];
}

/**
 * One hit from `/api/discover/search`.
 */
export interface CatalogSearchHit {
  work_key: string;
  title: string;
  authors: string | null;
  narrators: string | null;
  series: string | null;
  series_index?: string | null;
  asin: string | null;
  isbn: string | null;
  cover_url?: string | null;
  /** Same-origin path of this cover's processed images; see {@link coverSrc}. */
  cover_path?: string | null;
  store_editions: StoreEdition[];
  sources: string[];
  subtitle?: string | null;
  description?: string | null;
  publisher?: string | null;
  length_minutes?: number | null;
  published_at?: string | null;
  genres?: string | null;
  language?: string | null;
  is_abridged?: boolean | null;
  rating_overall?: number | null;
  rating_count?: number | null;
  price_cents?: number | null;
  purchase_hints?: PurchaseHint[];
  /** Branches of this library that currently list this title. */
  available_through?: BranchAvailability[];
}

/**
 * Server-supported Discover catalog sort keys.
 */
export type CatalogSearchSort =
  "relevance" | "popularity" | "rating" | "title" | "author" | "price" | "length";

/**
 * Ascending or descending catalog sort direction.
 */
export type CatalogSortDir = "asc" | "desc";

/**
 * Server-side include/exclude filters for catalog search.
 */
export type CatalogSearchFilters = {
  authors?: string[];
  narrators?: string[];
  series?: string[];
  genres?: string[];
  sources?: string[];
  exclude_sources?: string[];
  languages?: string[];
  exclude_narrators?: string[];
  min_rating?: number;
  min_length_minutes?: number;
  max_length_minutes?: number;
};

/**
 * Cursor-paginated catalog search page.
 */
export interface CatalogSearchPage {
  items: CatalogSearchHit[];
  page_size: number;
  has_more: boolean;
  next_cursor?: string | null;
  sort: string;
  sort_dir?: string;
}

/** Stored state of the caller's Discover shelves (`GET /api/discover/status`). */
export interface DiscoverStatus {
  subject: string;
  ready: boolean;
  scope?: string | null;
  feed_computed_at?: string | null;
  candidates_fetched_at?: string | null;
  /** True while a fetch or ranking for the caller is waiting or running. */
  updating: boolean;
}

/**
 * Queues a storefront fetch and ranking for the caller's shelves now, as
 * the Refresh button asks. A fetch already waiting absorbs the request.
 *
 * @returns The caller's Discover status once the fetch is queued.
 */
export async function requestDiscoverRefresh(): Promise<DiscoverStatus> {
  const res = await fetchWithTimeout(
    "/api/discover/refresh",
    {
      method: "POST",
      credentials: "include",
      headers: { "Content-Type": "application/json" },
      body: "{}",
    },
    8_000,
    "Discover refresh",
  );
  return parseJson(res);
}

/**
 * Loads the caller's stored Discover shelves.
 *
 * @returns Discover feed payload; `state` says whether shelves are still being prepared.
 */
export async function fetchDiscoverFeed(): Promise<DiscoverFeed> {
  // The daemon only reads stored shelves (queued jobs build them), so this
  // answers quickly; live prices load per shelf, in batches (see lookupBatcher.ts).
  const res = await fetchWithTimeout(
    "/api/discover/recommendations",
    { credentials: "include" },
    8_000,
    "Discover feed",
  );
  return parseJson(res);
}

/** Most queries the lookup batch routes take in one request. */
const LOOKUP_BATCH_MAX = 24;
/** How long the first card's lookup waits for other cards to join its batch. */
const LOOKUP_BATCH_WINDOW_MS = 60;
/**
 * Longest a lookup batch waits for the covers on screen to load first. Covers
 * are what the viewer sees first; prices and details follow.
 */
const COVERS_FIRST_MAX_WAIT_MS = 2_000;

/**
 * Lets the covers on screen load before a lookup batch goes out.
 *
 * @returns A promise that resolves once they have, or after the cap.
 */
function coversFirst(): Promise<void> {
  return coversSettled(COVERS_FIRST_MAX_WAIT_MS);
}

function serializePurchaseHintsQuery(q: PurchaseHintsQuery) {
  return {
    title: q.title,
    authors: q.authors ?? undefined,
    asin: q.asin ?? undefined,
    isbn: q.isbn ?? undefined,
    candidate_source: q.candidate_source ?? undefined,
    candidate_product_id: q.candidate_product_id ?? undefined,
    store_editions: q.store_editions ?? [],
    region: q.region ?? "us",
  };
}

/**
 * Sends one purchase-hints batch; when the batch route fails, asks for each
 * query on its own instead.
 *
 * @param queries - At most {@link LOOKUP_BATCH_MAX} queries.
 * @returns One answer per query, in order.
 */
async function sendPurchaseHintsBatch(
  queries: PurchaseHintsQuery[],
): Promise<PurchaseHintsResponse[]> {
  try {
    const res = await fetchWithTimeout(
      "/api/discover/purchase-hints/batch",
      {
        method: "POST",
        credentials: "include",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ queries: queries.map(serializePurchaseHintsQuery) }),
      },
      25_000,
      "Purchase hints batch",
    );
    const data = (await parseJson(res)) as { results: PurchaseHintsResponse[] };
    return queries.map((_, idx) => data.results[idx] ?? { hints: [], best: null });
  } catch {
    // Fall back to single-card requests if the batch endpoint fails.
    return Promise.all(
      queries.map(async (query) => {
        const res = await fetchWithTimeout(
          "/api/discover/purchase-hints",
          {
            method: "POST",
            credentials: "include",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify(serializePurchaseHintsQuery(query)),
          },
          25_000,
          "Purchase hints",
        );
        return (await parseJson(res)) as PurchaseHintsResponse;
      }),
    );
  }
}

/** Purchase-hint lookups, batched (see `lookupBatcher.ts`). */
const purchaseHintsBatcher = createLookupBatcher<PurchaseHintsQuery, PurchaseHintsResponse>({
  maxBatch: LOOKUP_BATCH_MAX,
  windowMs: LOOKUP_BATCH_WINDOW_MS,
  keyOf: (q) => purchaseHintsClientKey(q),
  send: sendPurchaseHintsBatch,
  before: coversFirst,
  generation: lookupGeneration,
});

/**
 * One Audible customer review for title detail.
 */
export interface TitleReview {
  id?: string | null;
  title?: string | null;
  body: string;
  author_name?: string | null;
  overall_rating?: number | null;
  performance_rating?: number | null;
  story_rating?: number | null;
  submitted_at?: string | null;
}

/**
 * Public Audnexus / Audible catalog fields for detail dialogs.
 */
export interface TitleMeta {
  asin?: string | null;
  title?: string | null;
  subtitle?: string | null;
  authors?: string | null;
  narrators?: string | null;
  series?: string | null;
  series_index?: string | null;
  isbn?: string | null;
  cover_url?: string | null;
  /** Same-origin path of this cover's processed images; see {@link coverSrc}. */
  cover_path?: string | null;
  description?: string | null;
  publisher?: string | null;
  length_minutes?: number | null;
  published_at?: string | null;
  categories?: string | null;
  language?: string | null;
  /** `true` abridged / `false` unabridged when the storefront said so. */
  is_abridged?: boolean | null;
  /** Audible community ratings (detail fetch only). */
  rating_overall?: number | null;
  rating_performance?: number | null;
  rating_story?: number | null;
  rating_count?: number | null;
  review_count?: number | null;
  /** @deprecated Prefer [`fetchTitleReviews`]. Title-meta no longer embeds reviews. */
  reviews?: TitleReview[];
}

/**
 * Audible review sort modes for title detail.
 */
export type TitleReviewsSort = "MostHelpful" | "MostRecent";

/**
 * Paginated title-reviews request body.
 */
export type TitleReviewsQuery = {
  asin: string;
  region?: string;
  page?: number;
  page_size?: number;
  sort_by?: TitleReviewsSort;
};

/**
 * One page of Audible customer reviews.
 */
export type TitleReviewsPage = {
  asin: string;
  page: number;
  page_size: number;
  has_more: boolean;
  sort_by?: string;
  reviews: TitleReview[];
};

const PURCHASE_HINTS_CLIENT_TTL_MS = 10 * 60_000;
const TITLE_META_CLIENT_TTL_MS = 6 * 60 * 60_000;
/** Most titles each Discover lookup cache keeps for the tab. */
const LOOKUP_CACHE_ENTRIES = 600;

/** Finished price answers per title (see `lookupCache.ts`). */
const purchaseHintsClientCache = createLookupCache<PurchaseHintsResponse>(
  "purchase-hints",
  PURCHASE_HINTS_CLIENT_TTL_MS,
  LOOKUP_CACHE_ENTRIES,
);
/** Details answers per title, `null` for none (see `lookupCache.ts`). */
const titleMetaClientCache = createLookupCache<TitleMeta | null>(
  "title-meta",
  TITLE_META_CLIENT_TTL_MS,
  LOOKUP_CACHE_ENTRIES,
);

function purchaseHintsClientKey(q: PurchaseHintsQuery): string {
  const editions = (q.store_editions ?? [])
    .map((e) => `${e.source}:${e.product_id}`)
    .sort()
    .join(",");
  return [
    q.title,
    q.authors ?? "",
    q.asin ?? "",
    q.isbn ?? "",
    q.candidate_source ?? "",
    q.candidate_product_id ?? "",
    editions,
    q.region ?? "us",
  ].join("|");
}

/**
 * Resolves purchase hints through a batch shared with every other card.
 *
 * Lookups asked close together go out as one batch request, one batch at a
 * time, after the covers on screen have loaded (see `lookupBatcher.ts`).
 *
 * @param body - Title identity and optional candidate edition.
 * @returns Hints list plus optional best pick (client-cached).
 */
export function fetchPurchaseHints(body: PurchaseHintsQuery): Promise<PurchaseHintsResponse> {
  const key = purchaseHintsClientKey(body);
  const cached = purchaseHintsClientCache.get(key);
  if (cached) return Promise.resolve(cached);
  const asked = lookupGeneration();
  return purchaseHintsBatcher(body).then((value) => {
    // An unfinished lookup is not kept, so the next ask reaches the server;
    // nor is one asked for a viewer who has since signed out.
    if (!value.incomplete && lookupGeneration() === asked) {
      purchaseHintsClientCache.set(key, value);
    }
    return value;
  });
}

/**
 * Query for public title metadata enrichment.
 */
export type TitleMetaQuery = {
  title: string;
  authors?: string | null;
  asin?: string | null;
  isbn?: string | null;
  narrators?: string | null;
  length_minutes?: number | null;
  region?: string;
  /**
   * Store editions that listed the title. The server fills a sparse record
   * from the stores that answer their own product ids before asking ISBN
   * stores.
   */
  store_editions?: StoreEdition[] | null;
};

/** Most store editions the server accepts on one title-meta query. */
const TITLE_META_MAX_STORE_EDITIONS = 16;

/**
 * The store editions a title-meta query sends: the first
 * {@link TITLE_META_MAX_STORE_EDITIONS} with a source and product id.
 *
 * @param body - The title-meta query.
 * @returns The editions to send, possibly empty.
 */
function titleMetaEditions(body: TitleMetaQuery): StoreEdition[] {
  return (body.store_editions ?? [])
    .filter((edition) => edition.source.trim() !== "" && edition.product_id.trim() !== "")
    .slice(0, TITLE_META_MAX_STORE_EDITIONS);
}

function titleMetaClientKey(body: TitleMetaQuery): string {
  return [
    body.title,
    body.authors ?? "",
    body.asin ?? "",
    body.isbn ?? "",
    body.region ?? "us",
    titleMetaEditions(body)
      .map((edition) => `${edition.source}:${edition.product_id}`)
      .join(","),
  ].join("|");
}

function serializeTitleMetaQuery(body: TitleMetaQuery) {
  const storeEditions = titleMetaEditions(body);
  return {
    title: body.title,
    authors: body.authors ?? undefined,
    asin: body.asin ?? undefined,
    isbn: body.isbn ?? undefined,
    narrators: body.narrators ?? undefined,
    length_minutes: body.length_minutes ?? undefined,
    region: body.region ?? "us",
    store_editions: storeEditions.length > 0 ? storeEditions : undefined,
  };
}

/**
 * Sends one title-metadata batch and keeps each answer in the client cache.
 *
 * @param queries - At most {@link LOOKUP_BATCH_MAX} queries.
 * @returns One answer (or `null`) per query, in order.
 */
async function sendTitleMetaBatch(queries: TitleMetaQuery[]): Promise<(TitleMeta | null)[]> {
  const asked = lookupGeneration();
  const res = await fetchWithTimeout(
    "/api/discover/title-meta/batch",
    {
      method: "POST",
      credentials: "include",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ queries: queries.map(serializeTitleMetaQuery) }),
    },
    25_000,
    "Title metadata batch",
  );
  const data = (await parseJson(res)) as { results: (TitleMeta | null)[] };
  return queries.map((query, idx) => {
    const meta = data.results[idx] ?? null;
    if (lookupGeneration() === asked) titleMetaClientCache.set(titleMetaClientKey(query), meta);
    return meta;
  });
}

/** Title-metadata lookups, batched (see `lookupBatcher.ts`). */
const titleMetaBatcher = createLookupBatcher<TitleMetaQuery, TitleMeta | null>({
  maxBatch: LOOKUP_BATCH_MAX,
  windowMs: LOOKUP_BATCH_WINDOW_MS,
  keyOf: titleMetaClientKey,
  send: sendTitleMetaBatch,
  before: coversFirst,
  generation: lookupGeneration,
});

/**
 * Fetches title metadata through a batch shared with every other card (see
 * {@link fetchPurchaseHints}).
 *
 * @param body - Title identity fields.
 * @returns Metadata or `null` when none matched (client-cached).
 */
export function fetchTitleMeta(body: TitleMetaQuery): Promise<TitleMeta | null> {
  const cached = titleMetaClientCache.get(titleMetaClientKey(body));
  if (cached !== undefined) return Promise.resolve(cached);
  return titleMetaBatcher(body);
}

/**
 * Loads a page of Audible customer reviews for title detail infinite scroll.
 *
 * @param body - ASIN, region, page, and sort.
 * @returns Reviews page payload.
 */
export async function fetchTitleReviews(body: TitleReviewsQuery): Promise<TitleReviewsPage> {
  const asin = body.asin.trim();
  const page = Math.max(1, body.page ?? 1);
  const page_size = Math.min(20, Math.max(1, body.page_size ?? 5));
  const sort_by = body.sort_by ?? "MostHelpful";
  const res = await fetchWithTimeout(
    "/api/discover/title-reviews",
    {
      method: "POST",
      credentials: "include",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        asin,
        region: body.region?.trim() || "us",
        page,
        page_size,
        sort_by,
      }),
    },
    25_000,
    "Title reviews",
  );
  return (await parseJson(res)) as TitleReviewsPage;
}

/**
 * Batch title-meta fetch (order preserved); shares the client TTL cache with singles.
 *
 * @param queries - Parallel title-meta queries.
 * @returns Metadata (or `null`) aligned with `queries`.
 */
export async function fetchTitleMetaBatch(
  queries: TitleMetaQuery[],
): Promise<(TitleMeta | null)[]> {
  if (queries.length === 0) return [];
  const results: (TitleMeta | null)[] = new Array(queries.length).fill(null);
  const pending: { index: number; query: TitleMetaQuery }[] = [];
  queries.forEach((query, index) => {
    const cached = titleMetaClientCache.get(titleMetaClientKey(query));
    if (cached !== undefined) {
      results[index] = cached;
    } else {
      pending.push({ index, query });
    }
  });
  // Through the shared batcher: same scheduling, covers first, no duplicates.
  const metas = await Promise.all(pending.map((item) => titleMetaBatcher(item.query)));
  pending.forEach((item, idx) => {
    results[item.index] = metas[idx] ?? null;
  });
  return results;
}

function browserCatalogLanguage(): string {
  try {
    const raw =
      (typeof navigator !== "undefined" && (navigator.languages?.[0] || navigator.language)) ||
      "en";
    const primary = String(raw).trim().toLowerCase().split(/[-_]/)[0];
    if (primary && /^[a-z]{2,3}$/.test(primary)) return primary;
  } catch {
    /* ignore */
  }
  return "en";
}

/**
 * Searches the Discover catalog across enabled storefronts.
 *
 * Returns an empty page (without calling the network) when `q` trims to fewer
 * than 2 characters. Language defaults to the browser locale unless
 * `allLanguages` is set.
 *
 * @param q - Free-text query (minimum 2 characters after trim).
 * @param opts - Pagination cursor, sort, language, and server-side facet filters.
 * @returns Cursor-paginated search page (or an empty stub when `q` is too short).
 */
export async function searchCatalog(
  q: string,
  opts?: {
    page_size?: number;
    /** @deprecated use page_size */
    limit?: number;
    cursor?: string | null;
    sort?: CatalogSearchSort;
    sort_dir?: CatalogSortDir;
    field?: "author" | "narrator" | "series" | "genre";
    lang?: string;
    /** When true, do not hard-filter by language. */
    allLanguages?: boolean;
    filters?: CatalogSearchFilters;
  },
): Promise<CatalogSearchPage> {
  const trimmed = q.trim();
  if (trimmed.length < 2) {
    return {
      items: [],
      page_size: 0,
      has_more: false,
      next_cursor: null,
      sort: opts?.sort ?? "relevance",
      sort_dir: opts?.sort_dir ?? "desc",
    };
  }
  const pageSize = opts?.page_size ?? opts?.limit ?? 24;
  const sp = new URLSearchParams({
    q: trimmed,
    page_size: String(pageSize),
  });
  if (opts?.cursor) sp.set("cursor", opts.cursor);
  if (opts?.sort) sp.set("sort", opts.sort);
  if (opts?.sort_dir) sp.set("sort_dir", opts.sort_dir);
  if (opts?.field) sp.set("field", opts.field);
  sp.set("lang", opts?.lang?.trim() || browserCatalogLanguage());
  if (opts?.allLanguages) sp.set("all_languages", "true");
  const f = opts?.filters;
  if (f?.authors?.length) sp.set("author", f.authors.join(","));
  if (f?.narrators?.length) sp.set("narrator", f.narrators.join(","));
  if (f?.series?.length) sp.set("series", f.series.join(","));
  if (f?.genres?.length) sp.set("genre", f.genres.join(","));
  if (f?.sources?.length) sp.set("source", f.sources.join(","));
  if (f?.exclude_sources?.length) {
    sp.set("exclude_source", f.exclude_sources.join(","));
  }
  if (f?.languages?.length) sp.set("language", f.languages.join(","));
  if (f?.exclude_narrators?.length) {
    sp.set("exclude_narrator", f.exclude_narrators.join(","));
  }
  if (f?.min_rating != null && f.min_rating > 0) {
    sp.set("min_rating", String(f.min_rating));
  }
  if (f?.min_length_minutes != null && f.min_length_minutes > 0) {
    sp.set("min_length_minutes", String(f.min_length_minutes));
  }
  if (f?.max_length_minutes != null && f.max_length_minutes > 0) {
    sp.set("max_length_minutes", String(f.max_length_minutes));
  }
  const res = await fetchWithTimeout(
    `/api/discover/search?${sp}`,
    { credentials: "include" },
    14_000,
    "Catalog search",
  );
  return parseJson(res);
}

/**
 * Loads the current user's wishlist items.
 *
 * @returns Title-request rows.
 */
export async function fetchWishlist(): Promise<TitleRequest[]> {
  const res = await fetchWithTimeout(
    "/api/wishlist",
    { credentials: "include" },
    8_000,
    "Wishlist",
  );
  return parseJson(res);
}

/**
 * Loads the global request queue (shared demand).
 *
 * @returns Aggregated queue entries.
 */
export async function fetchRequestQueue(): Promise<GlobalQueueEntry[]> {
  const res = await fetch("/api/request-queue", { credentials: "include" });
  return parseJson(res);
}

/**
 * Adds a title to the current user's wishlist.
 *
 * @param body - Title fields and optional catalog enrichment.
 * @returns Created title-request row.
 */
export async function createWishlistItem(body: {
  title: string;
  authors?: string;
  asin?: string;
  isbn?: string;
  notes?: string;
  store_editions?: StoreEdition[];
  purchase_hints?: PurchaseHint[];
  cover_url?: string | null;
  description?: string | null;
  subtitle?: string | null;
  narrators?: string | null;
  series?: string | null;
  series_index?: string | null;
  publisher?: string | null;
  length_minutes?: number | null;
  published_at?: string | null;
  genres?: string | null;
  language?: string | null;
}): Promise<TitleRequest> {
  const res = await fetch("/api/wishlist", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return parseJson(res);
}

/**
 * Deletes a wishlist title-request owned by the current user.
 *
 * @param uuid - Wishlist title-request uuid from {@link fetchWishlist}.
 * @returns The removed title-request row as echoed by the API.
 */
export async function removeWishlistItem(uuid: string): Promise<TitleRequest> {
  const res = await fetch(`/api/wishlist/${encodeURIComponent(uuid)}`, {
    method: "DELETE",
    credentials: "include",
  });
  return parseJson(res);
}
