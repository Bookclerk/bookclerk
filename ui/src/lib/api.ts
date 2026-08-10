export type AcquireStatus =
  | "not_acquired"
  | "queued"
  | "downloading"
  | "acquired"
  | "error";

export type AppView = "discover" | "library" | "accounts" | "wishlist" | "settings";
export type AuthRole = "operator" | "portal";

export interface PortalInfo {
  identity_id: number;
  provider: string;
  external_user_id: string;
  label: string | null;
}

export interface AuthSession {
  authenticated: boolean;
  role?: AuthRole;
  default_view: AppView;
  can_acquire: boolean;
  portal?: PortalInfo;
}

export interface BookRecord {
  id: number;
  uuid: string;
  source: string;
  account_id: string;
  product_id: string;
  asin: string | null;
  isbn: string | null;
  marketplace: string;
  title: string;
  authors: string | null;
  narrators: string | null;
  series: string | null;
  series_index: string | null;
  /** Audible series / podcast-parent ASIN when known. */
  series_asin: string | null;
  acquire_status: AcquireStatus;
  storage_key: string | null;
  error_message: string | null;
  purchased_at: string | null;
  /** Space-separated user tags. */
  tags: string | null;
  rating_overall: number | null;
  rating_performance: number | null;
  rating_story: number | null;
  is_finished: boolean;
  pdf_status: AcquireStatus;
  pdf_storage_key: string | null;
  publisher: string | null;
  length_minutes: number | null;
  is_abridged: boolean;
  content_kind: string;
  /** Genre / category list from the store (often `;`- or `,`-separated). */
  categories: string | null;
  subtitle: string | null;
  published_at: string | null;
  description: string | null;
  language: string | null;
  cover_url: string | null;
  subjects: string | null;
  enrich_source: string | null;
  enrich_confidence: number | null;
  enrich_updated_at: string | null;
  created_at: string;
  updated_at: string;
}

export interface BooksResponse {
  books: BookRecord[];
  total: number;
  limit: number;
  offset: number;
}

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

export interface JobInfo {
  id: string;
  kind: string;
  status: string;
  detail: string | null;
}

export interface ActionResponse {
  ok: boolean;
  message: string;
  job_id: string;
}

export interface PortalBrand {
  bg: string;
  fg: string;
  accent: string;
  logo: string;
}

export interface PortalSource {
  id: string;
  name: string;
  auth: string;
  brand: PortalBrand;
}

export interface PortalConnection {
  account_id: string;
  source: string;
  label: string | null;
  connection_status: string;
  source_enabled: boolean;
  brand?: PortalBrand;
}

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

export function isApiError(error: unknown): error is ApiError {
  return error instanceof ApiError;
}

/**
 * Map API / network failures to short copy for non-technical users.
 * Keeps branded daemon messages when they already read as plain language.
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
  if (raw === "operator" || raw === "portal") return raw;
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
  portal?: PortalInfo;
}): AuthSession {
  return {
    authenticated: body.authenticated,
    role: normalizeRole(body.role),
    default_view: normalizeView(body.default_view),
    can_acquire: Boolean(body.can_acquire),
    portal: body.portal,
  };
}

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
    portal?: PortalInfo;
  }>(res);
  return toAuthSession(body);
}

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

export async function logout(): Promise<void> {
  await fetch("/api/auth/logout", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: "{}",
  });
}

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

export interface SettingsUpdate {
  key: string;
  value: string;
}

export interface PluginSettingChoice {
  value: string;
  label: string;
}

export interface PluginSettingOption {
  key: string;
  label: string;
  value: string;
  value_type: "string" | "boolean" | "number";
  choices?: PluginSettingChoice[];
}

export interface PluginSettingsGroup {
  id: string;
  kind: string;
  /** Google favicon (or portal brand) URL for Settings list rows. */
  logo?: string;
  settings: PluginSettingOption[];
}

export interface SettingsResponse {
  settings: Record<string, string>;
  plugins: PluginSettingsGroup[];
}

export interface PluginConsentResponse {
  plugin_id: string;
  request: {
    pluginId: string;
    kind: string;
    networkMode: string;
    domains: string[];
    bindings: string[];
    compatibilityFlags: string[];
    approvedAt: string;
  };
  covered: boolean;
  summary: string[];
  existing?: PluginConsentResponse["request"];
}

export async function fetchPluginConsent(id: string): Promise<PluginConsentResponse> {
  const res = await fetch(`/api/plugins/${encodeURIComponent(id)}/consent`, {
    credentials: "include",
  });
  return parseJson<PluginConsentResponse>(res);
}

export async function approvePluginConsent(id: string): Promise<PluginConsentResponse> {
  const res = await fetch(`/api/plugins/${encodeURIComponent(id)}/consent`, {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ approve: true }),
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

export async function fetchPreferences(): Promise<UserPreferences> {
  const res = await fetchWithTimeout(
    "/api/preferences",
    { credentials: "include" },
    8_000,
    "Preferences",
  );
  return parsePreferencesBody(await parseJson(res));
}

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

export async function fetchSettings(): Promise<SettingsResponse> {
  const res = await fetch("/api/settings", { credentials: "include" });
  return parseJson<SettingsResponse>(res);
}

export async function patchSettings(body: { settings: SettingsUpdate[] }): Promise<SettingsResponse> {
  const res = await fetch("/api/settings", {
    method: "PATCH",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return parseJson<SettingsResponse>(res);
}

export async function portalRedeem(ticket: string): Promise<void> {
  const res = await fetch("/api/portal/redeem", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ ticket }),
  });
  await parseJson(res);
}

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

export async function portalLogout(): Promise<void> {
  await fetch("/api/portal/logout", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: "{}",
  });
}

export async function portalMe(): Promise<PortalMe> {
  const res = await fetch("/api/portal/me", { credentials: "include" });
  return parseJson(res);
}

export async function portalSources(): Promise<PortalSource[]> {
  const res = await fetch("/api/portal/sources", { credentials: "include" });
  const body = await parseJson<{ sources: PortalSource[] }>(res);
  return body.sources;
}

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

export async function portalConnections(): Promise<PortalConnection[]> {
  const res = await fetch("/api/portal/connections", { credentials: "include" });
  const body = await parseJson<{ connections: PortalConnection[] }>(res);
  return body.connections;
}

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

/** Sign out regardless of operator vs portal session. */
export async function signOut(role?: AuthRole): Promise<void> {
  if (role === "portal") {
    await portalLogout();
    return;
  }
  try {
    await logout();
  } catch {
    await portalLogout().catch(() => undefined);
  }
}

export async function fetchStatus(): Promise<StatusResponse> {
  const res = await fetch("/api/status", { credentials: "include" });
  return parseJson(res);
}

export async function fetchJobs(): Promise<JobInfo[]> {
  const res = await fetch("/api/jobs", { credentials: "include" });
  return parseJson(res);
}

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

export async function triggerScan(): Promise<ActionResponse> {
  const res = await fetch("/api/library/scan", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: "{}",
  });
  return parseJson(res);
}

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

export function coverUrl(uuid: string): string {
  return `/api/library/books/${encodeURIComponent(uuid)}/cover`;
}

export interface StoreEdition {
  source: string;
  product_id: string;
}

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

export interface PurchaseHintsResponse {
  hints: PurchaseHint[];
  best: PurchaseHint | null;
}

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

export interface DiscoverShelf {
  id: string;
  title: string;
  subtitle: string | null;
  items: Recommendation[];
}

export interface ShelfKindInfo {
  id: string;
  label: string;
}

export interface DiscoverFeed {
  shelves: DiscoverShelf[];
  shelf_kinds?: ShelfKindInfo[];
}

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

export type CatalogSearchSort =
  | "relevance"
  | "popularity"
  | "rating"
  | "title"
  | "author"
  | "price"
  | "length";

export type CatalogSortDir = "asc" | "desc";

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

export interface CatalogSearchPage {
  items: CatalogSearchHit[];
  page_size: number;
  has_more: boolean;
  next_cursor?: string | null;
  sort: string;
  sort_dir?: string;
}

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

/** One Audible customer review for title detail. */
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

/** Public Audnexus / Audible catalog fields for detail dialogs. */
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

export type TitleReviewsSort = "MostHelpful" | "MostRecent";

export type TitleReviewsQuery = {
  asin: string;
  region?: string;
  page?: number;
  page_size?: number;
  sort_by?: TitleReviewsSort;
};

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

/** Viewport-gated cards coalesce into a short batch window. */
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

/** Viewport-gated cards coalesce into a short batch window. */
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

/** Paginated Audible customer reviews for title detail infinite scroll. */
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

/** Batch title-meta (order preserved); uses the same client TTL cache as singles. */
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

export async function fetchWishlist(): Promise<TitleRequest[]> {
  const res = await fetchWithTimeout(
    "/api/wishlist",
    { credentials: "include" },
    8_000,
    "Wishlist",
  );
  return parseJson(res);
}

export async function fetchRequestQueue(): Promise<GlobalQueueEntry[]> {
  const res = await fetch("/api/request-queue", { credentials: "include" });
  return parseJson(res);
}

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

export async function removeWishlistItem(uuid: string): Promise<TitleRequest> {
  const res = await fetch(`/api/wishlist/${encodeURIComponent(uuid)}`, {
    method: "DELETE",
    credentials: "include",
  });
  return parseJson(res);
}

