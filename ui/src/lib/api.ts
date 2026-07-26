export type AcquireStatus =
  | "not_acquired"
  | "queued"
  | "downloading"
  | "acquired"
  | "error";

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

async function parseJson<T>(res: Response): Promise<T> {
  if (!res.ok) {
    const text = await res.text().catch(() => "");
    throw new Error(text || `${res.status} ${res.statusText}`);
  }
  return res.json() as Promise<T>;
}

export async function authMe(): Promise<boolean> {
  const res = await fetch("/api/auth/me", { credentials: "include" });
  if (res.status === 401) return false;
  const body = await parseJson<{ authenticated: boolean }>(res);
  return body.authenticated;
}

export async function login(token: string): Promise<void> {
  const res = await fetch("/api/auth/login", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ token }),
  });
  await parseJson(res);
}

export async function logout(): Promise<void> {
  await fetch("/api/auth/logout", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: "{}",
  });
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
  sp.set("limit", String(params.limit ?? 200));
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
