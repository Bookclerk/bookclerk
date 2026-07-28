export type AcquireStatus =
  | "not_acquired"
  | "queued"
  | "downloading"
  | "acquired"
  | "error";

export type AppView = "discover" | "library" | "accounts" | "wishlist";
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
  acquire_status: AcquireStatus;
  storage_key: string | null;
  error_message: string | null;
  length_minutes: number | null;
  content_kind: string;
  subtitle: string | null;
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

function normalizeView(raw: string | undefined): AppView {
  if (
    raw === "library" ||
    raw === "accounts" ||
    raw === "discover" ||
    raw === "wishlist"
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
    try {
      const body = JSON.parse(text) as { error?: string; message?: string };
      message = body.error || body.message || message;
    } catch {
      // keep raw text
    }
    throw new Error(message);
  }
  return res.json() as Promise<T>;
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
  const res = await fetch("/api/auth/me", { credentials: "include" });
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
  const res = await fetch("/api/auth/login", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ token }),
  });
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
}

export async function fetchPreferences(): Promise<UserPreferences> {
  const res = await fetch("/api/preferences", { credentials: "include" });
  const body = await parseJson<{
    default_view?: string;
    disabled_shelves?: string[];
  }>(res);
  return {
    default_view: normalizeView(body.default_view),
    disabled_shelves: Array.isArray(body.disabled_shelves)
      ? body.disabled_shelves.filter((x): x is string => typeof x === "string")
      : [],
  };
}

export async function patchPreferences(body: {
  default_view?: AppView;
  disabled_shelves?: string[];
}): Promise<UserPreferences> {
  const res = await fetch("/api/preferences", {
    method: "PATCH",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  const out = await parseJson<{
    default_view?: string;
    disabled_shelves?: string[];
  }>(res);
  return {
    default_view: normalizeView(out.default_view),
    disabled_shelves: Array.isArray(out.disabled_shelves)
      ? out.disabled_shelves.filter((x): x is string => typeof x === "string")
      : [],
  };
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
  created_at: string;
  updated_at: string;
}

export interface GlobalQueueEntry {
  work_key: string;
  title: string;
  authors: string | null;
  asin: string | null;
  isbn: string | null;
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
  asin: string | null;
  isbn: string | null;
  store_editions: StoreEdition[];
  sources: string[];
}

export async function fetchDiscoverFeed(limit = 36): Promise<DiscoverFeed> {
  // Seed storefront URLs in the feed; live prices load viewport-gated per card.
  const res = await fetch(
    `/api/discover/recommendations?limit=${limit}&no_purchase_hints=true`,
    { credentials: "include" },
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
      const res = await fetch("/api/discover/purchase-hints/batch", {
        method: "POST",
        credentials: "include",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          queries: chunk.map((w) => serializePurchaseHintsQuery(w.query)),
        }),
      });
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
            const res = await fetch("/api/discover/purchase-hints", {
              method: "POST",
              credentials: "include",
              headers: { "Content-Type": "application/json" },
              body: JSON.stringify(serializePurchaseHintsQuery(waiter.query)),
            });
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

/** Viewport-gated cards coalesce into a short batch window. */
export function fetchPurchaseHints(
  body: PurchaseHintsQuery,
): Promise<PurchaseHintsResponse> {
  return new Promise((resolve, reject) => {
    purchaseHintsQueue.push({ query: body, resolve, reject });
    if (purchaseHintsTimer == null) {
      purchaseHintsTimer = setTimeout(() => {
        void flushPurchaseHintsQueue();
      }, 40);
    }
  });
}

export async function searchCatalog(
  q: string,
  limit = 12,
): Promise<CatalogSearchHit[]> {
  const trimmed = q.trim();
  if (trimmed.length < 2) return [];
  const sp = new URLSearchParams({ q: trimmed, limit: String(limit) });
  const res = await fetch(`/api/discover/search?${sp}`, {
    credentials: "include",
  });
  return parseJson(res);
}

export async function fetchWishlist(): Promise<TitleRequest[]> {
  const res = await fetch("/api/wishlist", { credentials: "include" });
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

