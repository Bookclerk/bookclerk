/**
 * Pipeline acquire state for a library book row.
 */
export type AcquireStatus =
  | "not_acquired"
  | "queued"
  | "downloading"
  | "acquired"
  | "error";

/**
 * Primary authenticated SPA views mirrored in the URL path.
 */
export type AppView = "discover" | "library" | "accounts" | "wishlist" | "settings";
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
  has_password?: boolean;
}

/**
 * Active impersonation target when an administrator is viewing as another user.
 */
export interface AuthMeImpersonating {
  user_id: number;
  display_name: string | null;
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
  impersonating?: AuthMeImpersonating;
  portal?: PortalInfo;
  user?: AuthMeUser;
}

/**
 * Library book row from `/api/library/books`.
 *
 * Field names match the daemon JSON (`snake_case`). Use `uuid` for acquire /
 * cover URLs; `asin` / `isbn` are store product ids when known.
 */
export interface BookRecord {
  /** Library row primary key. */
  id: number;
  /** Stable book uuid used by acquire / cover / detail APIs. */
  uuid: string;
  /** Storefront plugin id (`audible`, `libro`, …). */
  source: string;
  /** Sealed account id that owns this title. */
  account_id: string;
  /** Store-native product id (ASIN, ISBN, UUID, …). */
  product_id: string;
  /** Audible ASIN when the source provides one. */
  asin: string | null;
  /** ISBN-13 / ISBN-10 when the source provides one. */
  isbn: string | null;
  /** Marketplace / locale code (`us`, `uk`, …). */
  marketplace: string;
  /** Display title. */
  title: string;
  /** Semicolon- or comma-separated author list. */
  authors: string | null;
  /** Semicolon- or comma-separated narrator list. */
  narrators: string | null;
  /** Series name when known. */
  series: string | null;
  /** Position within the series (string to preserve `#1.5` styles). */
  series_index: string | null;
  /** Audible series / podcast-parent ASIN when known. */
  series_asin: string | null;
  /** Pipeline state for the main audio acquire job. */
  acquire_status: AcquireStatus;
  /** Destination object key after a successful acquire. */
  storage_key: string | null;
  /** Last acquire / scan error message when status is `error`. */
  error_message: string | null;
  /** Purchase / library-add timestamp from the store (ISO-8601). */
  purchased_at: string | null;
  /** Space-separated user tags. */
  tags: string | null;
  /** Overall community rating (0–5) when enriched. */
  rating_overall: number | null;
  /** Performance / narrator rating when enriched. */
  rating_performance: number | null;
  /** Story rating when enriched. */
  rating_story: number | null;
  /** Whether the listener marked the title finished. */
  is_finished: boolean;
  /** Pipeline state for companion PDF acquire. */
  pdf_status: AcquireStatus;
  /** Destination key for an acquired companion PDF. */
  pdf_storage_key: string | null;
  /** Publisher name when known. */
  publisher: string | null;
  /** Runtime length in minutes when known. */
  length_minutes: number | null;
  /** True when the edition is abridged. */
  is_abridged: boolean;
  /** Content kind slug (`audiobook`, `podcast`, …). */
  content_kind: string;
  /** Genre / category list from the store (often `;`- or `,`-separated). */
  categories: string | null;
  /** Subtitle when the store provides one. */
  subtitle: string | null;
  /** Original publication date (ISO-8601 date or datetime). */
  published_at: string | null;
  /** Long-form description / blurb (may contain light HTML). */
  description: string | null;
  /** BCP-47-ish language code from the store. */
  language: string | null;
  /** Remote or proxied cover image URL. */
  cover_url: string | null;
  /** Subject / BISAC-style tags when enriched. */
  subjects: string | null;
  /** Enrichment provider id that last wrote metadata. */
  enrich_source: string | null;
  /** Enrichment confidence score (0–1) when present. */
  enrich_confidence: number | null;
  /** When enrichment last ran (ISO-8601). */
  enrich_updated_at: string | null;
  /** Row insert time (ISO-8601). */
  created_at: string;
  /** Row last update time (ISO-8601). */
  updated_at: string;
}

/**
 * Paginated library books list payload.
 */
export interface BooksResponse {
  books: BookRecord[];
  total: number;
  limit: number;
  offset: number;
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
}

/**
 * Background job row from `/api/jobs`.
 */
export interface JobInfo {
  id: string;
  kind: string;
  status: string;
  detail: string | null;
}

/**
 * Ack payload for scan / acquire job triggers.
 */
export interface ActionResponse {
  ok: boolean;
  message: string;
  job_id: string;
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
    raw === "settings"
  ) {
    return raw;
  }
  return "discover";
}

function normalizeRole(raw: string | undefined): AuthRole | undefined {
  if (
    raw === "operator" ||
    raw === "owner" ||
    raw === "administrator" ||
    raw === "member"
  ) {
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
  impersonating?: AuthMeImpersonating;
  portal?: PortalInfo;
  user?: AuthMeUser;
}): AuthSession {
  return {
    authenticated: body.authenticated,
    role: normalizeRole(body.role),
    default_view: normalizeView(body.default_view),
    can_acquire: Boolean(body.can_acquire),
    elevated: Boolean(body.elevated),
    impersonating: body.impersonating,
    portal: body.portal,
    user: body.user,
  };
}

/**
 * Loads the current session from `GET /api/auth/me`.
 *
 * @returns Anonymous session when the response is 401.
 */
export async function authMe(): Promise<AuthSession> {
  const res = await fetchWithTimeout(
    "/api/auth/me",
    { credentials: "include" },
    8_000,
    "Session check",
  );
  if (res.status === 401) return ANON_SESSION;
  const body = await parseJson<{
    authenticated: boolean;
    role?: string;
    default_view?: string;
    can_acquire?: boolean;
    elevated?: boolean;
    impersonating?: AuthMeImpersonating;
    portal?: PortalInfo;
    user?: AuthMeUser;
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
  };
}

/**
 * Clears the operator cookie session via `POST /api/auth/logout`.
 *
 * @returns Resolves when the logout request finishes.
 */
export async function logout(): Promise<void> {
  await fetch("/api/auth/logout", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: "{}",
  });
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

/**
 * Starts operator (or elevated Owner) impersonation of another user.
 *
 * @param userId - Target user id.
 * @returns Resolves when impersonation is active.
 */
export async function startImpersonate(userId: number): Promise<void> {
  const res = await fetch("/api/auth/impersonate", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ user_id: userId }),
  });
  await parseJson(res);
}

/**
 * Stops the current operator impersonation session.
 *
 * @returns Resolves when impersonation ends.
 */
export async function stopImpersonate(): Promise<void> {
  const res = await fetch("/api/auth/impersonate", {
    method: "DELETE",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: "{}",
  });
  await parseJson(res);
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
  /** Non-expired portal session present. */
  online?: boolean;
  /** Most recent portal session activity (RFC 3339). */
  last_active_at?: string | null;
  /** Unfinished listen within the recent window. */
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
export async function resetUserPassword(
  id: number,
): Promise<{
  ok: boolean;
  claim_ticket: string;
  invite_url?: string | null;
  revoked_sessions: number;
}> {
  const res = await fetch(
    `/api/users/${encodeURIComponent(String(id))}/reset-password`,
    {
      method: "POST",
      credentials: "include",
      headers: { "Content-Type": "application/json" },
      body: "{}",
    },
  );
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
 * Signs in with username/password via `POST /api/auth/password`.
 *
 * @param login - Username or email.
 * @param password - Account password.
 * @returns Resolves when the session cookie is set.
 */
export async function passwordLogin(
  login: string,
  password: string,
): Promise<void> {
  const res = await fetch("/api/auth/password", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ login, password }),
  });
  await parseJson(res);
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
 */
export interface PluginSettingOption {
  key: string;
  label: string;
  value: string;
  value_type: "string" | "boolean" | "number";
  choices?: PluginSettingChoice[];
}

/**
 * Settings group for one installed plugin.
 */
export interface PluginSettingsGroup {
  id: string;
  kind: string;
  /** Google favicon (or portal brand) URL for Settings list rows. */
  logo?: string;
  settings: PluginSettingOption[];
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
}

/**
 * Plugin consent grant shape serialized by the daemon.
 */
export interface PluginGrant {
  pluginId: string;
  kind: string;
  networkMode: string;
  domains: string[];
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
  /** Guest runtime: `native` or `workerd`. */
  runtime: string;
  request: PluginGrant;
  covered: boolean;
  summary: string[];
  existing?: PluginGrant;
  brand?: PluginConsentBrand;
  limits: PluginConsentLimits;
}

/**
 * Loads network/capability consent status for a plugin before enable.
 *
 * Call from Settings when an operator reviews domains/bindings that still need
 * approval; `covered` is false until approvePluginConsent succeeds.
 *
 * @param id - Plugin id from handshake / Settings list.
 * @returns Current consent request payload and whether it already covers the ask.
 */
export async function fetchPluginConsent(id: string): Promise<PluginConsentResponse> {
  const res = await fetch(`/api/plugins/${encodeURIComponent(id)}/consent`, {
    credentials: "include",
  });
  return parseJson<PluginConsentResponse>(res);
}

/**
 * Approves network consent for a plugin.
 *
 * @param id - Plugin id.
 * @returns Updated consent response after approval.
 */
export async function approvePluginConsent(
  id: string,
  grant?: {
    networkMode: string;
    domains: string[];
    bindings: string[];
    compatibilityFlags: string[];
    cpuMs?: number;
    subrequests?: number;
    diskMib?: number;
    memoryMib?: number;
    cpuCores?: number;
    extraProcesses?: number;
  },
): Promise<PluginConsentResponse> {
  const res = await fetch(`/api/plugins/${encodeURIComponent(id)}/consent`, {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ approve: true, ...(grant ? { grant } : {}) }),
  });
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
  return typeof raw === "string" && raw.trim().toLowerCase() === "asc"
    ? "asc"
    : "desc";
}

function parsePreferencesBody(body: {
  default_view?: string;
  disabled_shelves?: string[];
  discover_sort?: string;
  discover_sort_dir?: string;
  discover_language?: string | null;
  discover_excluded_sources?: string[];
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
      ? body.discover_excluded_sources.filter(
          (x): x is string => typeof x === "string",
        )
      : [],
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
 * @param body - List of `{ key, value }` updates (plugin knobs use dotted keys).
 * @returns Refreshed settings payload after the patch.
 */
export async function patchSettings(body: { settings: SettingsUpdate[] }): Promise<SettingsResponse> {
  const res = await fetch("/api/settings", {
    method: "PATCH",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return parseJson<SettingsResponse>(res);
}

/**
 * Redeems a claim ticket into a portal user session.
 *
 * @param ticket - One-time claim ticket string.
 * @param password - Optional password when claiming invite/reset tickets.
 * @returns Resolves when the session cookie is set.
 */
export async function portalRedeem(ticket: string, password?: string): Promise<void> {
  const body: { ticket: string; password?: string } = { ticket };
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
  await fetch("/api/portal/logout", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: "{}",
  });
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
  const res = await fetch(
    `/api/portal/sources/${encodeURIComponent(id)}/oauth/start`,
    {
      method: "POST",
      credentials: "include",
      headers: { "Content-Type": "application/json" },
      body: "{}",
    },
  );
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
 *
 * @param accountId - Portal connection account id from {@link portalConnections}.
 * @returns Resolves when the revoke POST succeeds.
 */
export async function portalRevokeConnection(accountId: string): Promise<void> {
  const res = await fetch(
    `/api/portal/connections/${encodeURIComponent(accountId)}/revoke`,
    {
      method: "POST",
      credentials: "include",
      headers: { "Content-Type": "application/json" },
      body: "{}",
    },
  );
  await parseJson(res);
}

/**
 * Signs out regardless of operator vs user/portal session.
 *
 * @param role - Current session role; portal roles use portal logout.
 * @returns Resolves when logout completes.
 */
export async function signOut(role?: AuthRole): Promise<void> {
  if (role === "administrator" || role === "member") {
    await portalLogout();
    return;
  }
  try {
    await logout();
  } catch {
    await portalLogout().catch(() => undefined);
  }
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

/**
 * Fetches one page of library books for the Library table.
 *
 * Omits `status` (or passes `all`) to include every acquire state. Default page
 * size is 40 when `limit` is omitted.
 *
 * @param params - Optional search string, acquire-status filter, and pagination.
 * @returns Paginated books plus total count for the pager.
 */
export async function fetchBooks(params: {
  q?: string;
  status?: string;
  limit?: number;
  offset?: number;
}): Promise<BooksResponse> {
  const sp = new URLSearchParams();
  if (params.q) sp.set("q", params.q);
  if (params.status && params.status !== "all") sp.set("status", params.status);
  sp.set("limit", String(params.limit ?? 40));
  sp.set("offset", String(params.offset ?? 0));
  const res = await fetch(`/api/library/books?${sp}`, {
    credentials: "include",
  });
  return parseJson(res);
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
  return parseJson(res);
}

/**
 * Queues acquire for a library book by uuid and/or ASIN.
 *
 * @param body - Target identifiers (`uuid` and/or `asin`).
 * @returns Action ack including `job_id`.
 */
export async function triggerAcquire(body: {
  uuid?: string;
  asin?: string;
}): Promise<ActionResponse> {
  const res = await fetch("/api/library/acquire", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return parseJson(res);
}

/**
 * Builds the authenticated cover image URL for a library book.
 *
 * @param uuid - Book uuid.
 * @returns Relative URL under `/api/library/books/.../cover`.
 */
export function coverUrl(uuid: string): string {
  return `/api/library/books/${encodeURIComponent(uuid)}/cover`;
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
  subtitle?: string | null;
  description?: string | null;
  publisher?: string | null;
  length_minutes?: number | null;
  published_at?: string | null;
  genres?: string | null;
  language?: string | null;
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
}

/**
 * Wishlist / title-request row from `/api/wishlist`.
 *
 * Represents a user ask for a title (and optional acquire queue linkage via
 * `resolved_book_uuid` / `work_key`).
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
  work_key: string;
  work_id: string | null;
  resolved_book_uuid: string | null;
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
  store_editions?: StoreEdition[];
  purchase_hints?: PurchaseHint[];
  created_at: string;
  updated_at: string;
}

/**
 * Aggregated global request-queue entry (shared wishlist demand).
 */
export interface GlobalQueueEntry {
  work_key: string;
  title: string;
  authors: string | null;
  asin: string | null;
  isbn: string | null;
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
  store_editions?: StoreEdition[];
  purchase_hints?: PurchaseHint[];
  wish_count: number;
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
}

/**
 * Server-supported Discover catalog sort keys.
 */
export type CatalogSearchSort =
  | "relevance"
  | "popularity"
  | "rating"
  | "title"
  | "author"
  | "price"
  | "length";

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

/**
 * Loads Discover recommendation shelves.
 *
 * @param limit - Max items per shelf (default 36). Seed URLs only; live prices load later.
 * @returns Discover feed payload.
 */
export async function fetchDiscoverFeed(limit = 36): Promise<DiscoverFeed> {
  // Seed storefront URLs in the feed; live prices load viewport-gated per card.
  const res = await fetchWithTimeout(
    `/api/discover/recommendations?limit=${limit}&no_purchase_hints=true`,
    { credentials: "include" },
    20_000,
    "Discover feed",
  );
  return parseJson(res);
}

type PurchaseHintsWaiter = {
  query: PurchaseHintsQuery;
  resolve: (value: PurchaseHintsResponse) => void;
  reject: (err: unknown) => void;
};

let purchaseHintsQueue: PurchaseHintsWaiter[] = [];
let purchaseHintsTimer: ReturnType<typeof setTimeout> | null = null;

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

async function flushPurchaseHintsQueue() {
  const batch = purchaseHintsQueue.splice(0, purchaseHintsQueue.length);
  purchaseHintsTimer = null;
  if (batch.length === 0) return;

  // Chunk to the server's max of 24.
  for (let i = 0; i < batch.length; i += 24) {
    const chunk = batch.slice(i, i + 24);
    try {
      const res = await fetchWithTimeout(
        "/api/discover/purchase-hints/batch",
        {
          method: "POST",
          credentials: "include",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            queries: chunk.map((w) => serializePurchaseHintsQuery(w.query)),
          }),
        },
        25_000,
        "Purchase hints batch",
      );
      const data = (await parseJson(res)) as { results: PurchaseHintsResponse[] };
      chunk.forEach((waiter, idx) => {
        const result = data.results[idx] ?? { hints: [], best: null };
        waiter.resolve(result);
      });
    } catch (err) {
      // Fall back to single-card requests if the batch endpoint fails.
      await Promise.all(
        chunk.map(async (waiter) => {
          try {
            const res = await fetchWithTimeout(
              "/api/discover/purchase-hints",
              {
                method: "POST",
                credentials: "include",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify(serializePurchaseHintsQuery(waiter.query)),
              },
              25_000,
              "Purchase hints",
            );
            waiter.resolve(await parseJson(res));
          } catch (singleErr) {
            waiter.reject(singleErr);
          }
        }),
      );
      void err;
    }
  }
}

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

type CacheEntry<T> = { value: T; expires: number };

const purchaseHintsClientCache = new Map<string, CacheEntry<PurchaseHintsResponse>>();
const titleMetaClientCache = new Map<string, CacheEntry<TitleMeta | null>>();
const PURCHASE_HINTS_CLIENT_TTL_MS = 10 * 60_000;
const TITLE_META_CLIENT_TTL_MS = 6 * 60 * 60_000;

function clientCacheGet<T>(map: Map<string, CacheEntry<T>>, key: string): T | undefined {
  const hit = map.get(key);
  if (!hit) return undefined;
  if (hit.expires <= Date.now()) {
    map.delete(key);
    return undefined;
  }
  return hit.value;
}

function clientCacheSet<T>(
  map: Map<string, CacheEntry<T>>,
  key: string,
  value: T,
  ttlMs: number,
) {
  map.set(key, { value, expires: Date.now() + ttlMs });
}

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
 * Resolves purchase hints, coalescing viewport-gated cards into a short batch window.
 *
 * @param body - Title identity and optional candidate edition.
 * @returns Hints list plus optional best pick (client-cached).
 */
export function fetchPurchaseHints(
  body: PurchaseHintsQuery,
): Promise<PurchaseHintsResponse> {
  const key = purchaseHintsClientKey(body);
  const cached = clientCacheGet(purchaseHintsClientCache, key);
  if (cached) return Promise.resolve(cached);
  return new Promise((resolve, reject) => {
    purchaseHintsQueue.push({
      query: body,
      resolve: (value) => {
        clientCacheSet(purchaseHintsClientCache, key, value, PURCHASE_HINTS_CLIENT_TTL_MS);
        resolve(value);
      },
      reject,
    });
    if (purchaseHintsTimer == null) {
      purchaseHintsTimer = setTimeout(() => {
        void flushPurchaseHintsQueue();
      }, 40);
    }
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
};

function titleMetaClientKey(body: TitleMetaQuery): string {
  return [
    body.title,
    body.authors ?? "",
    body.asin ?? "",
    body.isbn ?? "",
    body.region ?? "us",
  ].join("|");
}

function serializeTitleMetaQuery(body: TitleMetaQuery) {
  return {
    title: body.title,
    authors: body.authors ?? undefined,
    asin: body.asin ?? undefined,
    isbn: body.isbn ?? undefined,
    narrators: body.narrators ?? undefined,
    length_minutes: body.length_minutes ?? undefined,
    region: body.region ?? "us",
  };
}

type TitleMetaWaiter = {
  query: TitleMetaQuery;
  resolve: (value: TitleMeta | null) => void;
  reject: (err: unknown) => void;
};

let titleMetaQueue: TitleMetaWaiter[] = [];
let titleMetaTimer: ReturnType<typeof setTimeout> | null = null;

async function flushTitleMetaQueue() {
  const batch = titleMetaQueue.splice(0, titleMetaQueue.length);
  titleMetaTimer = null;
  if (batch.length === 0) return;
  for (let i = 0; i < batch.length; i += 24) {
    const chunk = batch.slice(i, i + 24);
    try {
      const res = await fetchWithTimeout(
        "/api/discover/title-meta/batch",
        {
          method: "POST",
          credentials: "include",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            queries: chunk.map((w) => serializeTitleMetaQuery(w.query)),
          }),
        },
        25_000,
        "Title metadata batch",
      );
      const data = (await parseJson(res)) as { results: (TitleMeta | null)[] };
      chunk.forEach((waiter, idx) => {
        const meta = data.results[idx] ?? null;
        clientCacheSet(
          titleMetaClientCache,
          titleMetaClientKey(waiter.query),
          meta,
          TITLE_META_CLIENT_TTL_MS,
        );
        waiter.resolve(meta);
      });
    } catch (err) {
      chunk.forEach((waiter) => waiter.reject(err));
    }
  }
}

/**
 * Fetches title metadata, coalescing viewport-gated cards into a short batch window.
 *
 * @param body - Title identity fields.
 * @returns Metadata or `null` when none matched (client-cached).
 */
export function fetchTitleMeta(body: TitleMetaQuery): Promise<TitleMeta | null> {
  const key = titleMetaClientKey(body);
  const cached = clientCacheGet(titleMetaClientCache, key);
  if (cached !== undefined) return Promise.resolve(cached);
  return new Promise((resolve, reject) => {
    titleMetaQueue.push({ query: body, resolve, reject });
    if (titleMetaTimer == null) {
      titleMetaTimer = setTimeout(() => {
        void flushTitleMetaQueue();
      }, 40);
    }
  });
}

/**
 * Loads a page of Audible customer reviews for title detail infinite scroll.
 *
 * @param body - ASIN, region, page, and sort.
 * @returns Reviews page payload.
 */
export async function fetchTitleReviews(
  body: TitleReviewsQuery,
): Promise<TitleReviewsPage> {
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
    const cached = clientCacheGet(titleMetaClientCache, titleMetaClientKey(query));
    if (cached !== undefined) {
      results[index] = cached;
    } else {
      pending.push({ index, query });
    }
  });
  for (let i = 0; i < pending.length; i += 24) {
    const chunk = pending.slice(i, i + 24);
    const res = await fetchWithTimeout(
      "/api/discover/title-meta/batch",
      {
        method: "POST",
        credentials: "include",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          queries: chunk.map((c) => serializeTitleMetaQuery(c.query)),
        }),
      },
      25_000,
      "Title metadata batch",
    );
    const data = (await parseJson(res)) as { results: (TitleMeta | null)[] };
    chunk.forEach((item, idx) => {
      const meta = data.results[idx] ?? null;
      clientCacheSet(
        titleMetaClientCache,
        titleMetaClientKey(item.query),
        meta,
        TITLE_META_CLIENT_TTL_MS,
      );
      results[item.index] = meta;
    });
  }
  return results;
}

function browserCatalogLanguage(): string {
  try {
    const raw =
      (typeof navigator !== "undefined" &&
        (navigator.languages?.[0] || navigator.language)) ||
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
  work_key?: string;
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

