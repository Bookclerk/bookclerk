/**
 * Join a library: the SPA side of the branch flow (H-4).
 *
 * The UI calls a host a **branch** and an owner cluster a **library**; the
 * API keeps the code names (`clusters`, `nodes`, `host`).
 *
 * - On the joining install, the operator's wizard calls its own daemon
 *   (`/api/config/join/*`), which reads the library, keeps the PKCE
 *   verifier and node keys server-side, and answers an authorize URL. The
 *   SPA only relays the `code` and `state` the library sends back.
 * - On the library, the consent page (`/join/approve`) asks the signed-in
 *   user "Let <branch> join <library> as yours?".
 * - Branch invites are minted in User management and shown once; they are
 *   never kept in any persisted state.
 */

import { ApiError } from "@/lib/api";

/** What a library says about itself, read by this install's daemon. */
export interface LibraryInfoView {
  cluster_id: string;
  library_name: string;
  /** The library's canonical origin (`https://library.example`). */
  origin: string;
  /** Node API transport (`loopback`, `proxy`, `acme`, `self_signed`). */
  transport: string;
  /** "Let new branches join" on the library. */
  enrollment: boolean;
  /** SPKI fingerprint to confirm (self-signed libraries only). */
  fingerprint: string | null;
  needs_fingerprint: boolean;
}

/** A library this install is a branch of (no key material). */
export interface BranchLinkView {
  cluster_id: string;
  owner_origin: string;
  node_id: string;
  pinned: boolean;
  /** `active`, `revoked` (by the library), or `left`. */
  state: string;
  enrolled_at: string;
  /** Whether this daemon's client for it is running. */
  running?: boolean;
}

/** `GET /api/config/join`. */
export interface JoinStatus {
  /** No Owner, no Administrator, and no library joined yet. */
  fresh: boolean;
  has_owner: boolean;
  default_branch_name: string;
  links: BranchLinkView[];
}

/** `POST /api/config/join/start`. */
export interface JoinStart {
  authorize_url: string;
  library: LibraryInfoView;
  branch_name: string;
  expires_in_secs: number;
}

/** `GET /api/auth/branch-consent`: what the consent page shows. */
export interface BranchConsentView {
  library_name: string;
  branch_name: string;
  /** Where the browser goes back to (the joining install). */
  redirect_origin: string;
  enrollment: boolean;
  user: { id: number; display_name: string | null };
}

/** `GET /api/config/branches`: Settings → Branches on the library. */
export interface BranchSettingsView {
  library_name: string;
  origin: string;
  transport: string;
  enrollment: boolean;
  spki_fingerprint: string | null;
}

/** A branch invite as listed (never its secret). */
export interface BranchInviteView {
  link_id: string;
  for_user_id: number;
  label: string;
  created_by: string;
  created_at: string;
  expires_at: string;
  redeemed_at: string | null;
  redeemed_node_id: string | null;
  revoked_at: string | null;
  /** `pending`, `redeemed`, `revoked`, or `expired`. */
  state: string;
}

/** A freshly minted branch invite: `url` is shown once and dropped. */
export interface MintedBranchInvite {
  link: BranchInviteView;
  url: string;
}

/**
 * Parses an error body (`{error, detail}`) into an {@link ApiError}.
 *
 * @param res - A failed response.
 * @returns The error, with `detail` (or `message`) as its message.
 */
async function joinError(res: Response): Promise<JoinApiError> {
  const text = await res.text().catch(() => "");
  let message = text || `${res.status} ${res.statusText}`;
  let code: string | undefined;
  let origin: string | undefined;
  try {
    const body = JSON.parse(text) as {
      error?: string;
      detail?: string;
      message?: string;
      origin?: string;
    };
    code = typeof body.error === "string" ? body.error : undefined;
    origin = typeof body.origin === "string" ? body.origin : undefined;
    message = body.detail?.trim() || body.message?.trim() || body.error?.trim() || message;
  } catch {
    // keep raw text
  }
  return new JoinApiError(res.status, message, code, origin);
}

/** A join API refusal; `origin` is the library's own address on a mismatch. */
export class JoinApiError extends ApiError {
  origin?: string;

  constructor(status: number, message: string, code?: string, origin?: string) {
    super(status, message, { code });
    this.name = "JoinApiError";
    this.origin = origin;
  }
}

/**
 * Sends a JSON request and parses the answer.
 *
 * @param path - API path.
 * @param init - Method and optional JSON body.
 * @returns The parsed body.
 */
async function request<T>(
  path: string,
  init: { method?: string; body?: unknown } = {},
): Promise<T> {
  const res = await fetch(path, {
    method: init.method ?? "GET",
    credentials: "include",
    // Nothing on these routes may be cached (invites, authorize URLs).
    cache: "no-store",
    headers: init.body !== undefined ? { "Content-Type": "application/json" } : undefined,
    body: init.body !== undefined ? JSON.stringify(init.body) : undefined,
  });
  if (!res.ok) throw await joinError(res);
  return res.json() as Promise<T>;
}

/**
 * Whether this is a fresh install, and the libraries it joined. Operator only.
 *
 * @returns The install's join status.
 */
export function fetchJoinStatus(): Promise<JoinStatus> {
  return request("/api/config/join");
}

/**
 * Has this install's daemon read a library's `info`.
 *
 * @param address - What the operator typed (`https://…` or `host:port`).
 * @returns What the library says about itself.
 */
export function inspectLibrary(address: string): Promise<{ library: LibraryInfoView }> {
  return request("/api/config/join/inspect", { method: "POST", body: { address } });
}

/**
 * Starts the sign-in: answers the authorize URL to send the browser to.
 *
 * @param body - Address, the confirmed fingerprint, and the branch name.
 * @returns The authorize URL and what it is for.
 */
export function startJoin(body: {
  address: string;
  fingerprint?: string | null;
  branch_name?: string;
}): Promise<JoinStart> {
  return request("/api/config/join/start", {
    method: "POST",
    body: {
      address: body.address,
      fingerprint: body.fingerprint ?? undefined,
      branch_name: body.branch_name?.trim() || undefined,
    },
  });
}

/**
 * Relays the library's answer to this install's daemon, which redeems it.
 *
 * @param callback - `code`, `state`, and `error` from the callback URL.
 * @returns The recorded link and whether its client runs.
 */
export function finishJoin(
  callback: JoinCallback,
): Promise<{ cluster: BranchLinkView; library_name: string; running: boolean }> {
  return request("/api/config/join/callback", {
    method: "POST",
    body: {
      code: callback.code,
      state: callback.state,
      error: callback.error ?? undefined,
    },
  });
}

/**
 * Joins with a pasted branch invite; the SPA keeps nothing of it.
 *
 * @param link - The whole invite (`https://<library>/enroll#bcn1...`).
 * @returns The recorded link and whether its client runs.
 */
export function joinWithInvite(
  link: string,
): Promise<{ cluster: BranchLinkView; running: boolean }> {
  return request("/api/config/join/invite", { method: "POST", body: { link: link.trim() } });
}

/**
 * Leave library: tells the library, deletes the node keys, stops the client.
 *
 * @param clusterId - The library to leave.
 * @returns The link as left, and whether the library heard.
 */
export function leaveLibrary(
  clusterId: string,
): Promise<{ cluster: BranchLinkView; owner_notified: boolean }> {
  return request("/api/config/join/leave", { method: "POST", body: { cluster_id: clusterId } });
}

/**
 * The consent page's view of a branch's request.
 *
 * @param search - The page's query string (the authorize request).
 * @returns What the consent page shows.
 */
export function fetchBranchConsent(search: string): Promise<BranchConsentView> {
  const query = search.startsWith("?") ? search.slice(1) : search;
  return request(`/api/auth/branch-consent?${query}`);
}

/**
 * Approves or denies a branch; answers where to send the browser.
 *
 * @param search - The authorize request (query string).
 * @param approve - True to let the branch join.
 * @returns Where to send the browser.
 */
export function answerBranchConsent(
  search: string,
  approve: boolean,
): Promise<{ redirect: string }> {
  return request("/api/auth/branch-consent", {
    method: "POST",
    body: { ...authorizeParams(search), approve },
  });
}

/**
 * Settings → Branches on the library.
 *
 * @returns The join settings.
 */
export function fetchBranchSettings(): Promise<BranchSettingsView> {
  return request("/api/config/branches");
}

/**
 * Turns "Let new branches join" on or off.
 *
 * @param enrollment - The new value.
 * @returns The stored value.
 */
export function putBranchSettings(enrollment: boolean): Promise<{ enrollment: boolean }> {
  return request("/api/config/branches", { method: "PUT", body: { enrollment } });
}

/**
 * Mints a branch invite (shown once).
 *
 * @param body - Who will own the branch (default: the caller) and a label.
 * @returns The invite record and its URL (show once).
 */
export function mintBranchInvite(body: {
  user_id?: number;
  label?: string;
}): Promise<MintedBranchInvite> {
  return request("/api/config/node-invites", { method: "POST", body });
}

/**
 * Outstanding and past branch invites the caller may see (no secrets).
 *
 * @returns The invites.
 */
export function listBranchInvites(): Promise<{ links: BranchInviteView[] }> {
  return request("/api/config/node-invites");
}

/**
 * Revokes an unredeemed branch invite.
 *
 * @param linkId - The invite's id.
 * @returns The revoked invite.
 */
export function revokeBranchInvite(linkId: string): Promise<{ link: BranchInviteView }> {
  return request(`/api/config/node-invites/${encodeURIComponent(linkId)}`, {
    method: "DELETE",
  });
}

/** What the library sent back to `/join/callback`. */
export interface JoinCallback {
  code: string;
  state: string;
  error: string | null;
}

/**
 * Reads the callback query.
 *
 * @param search - `window.location.search` on `/join/callback`.
 * @returns The answer, or `null` when it has no `state`.
 */
export function parseJoinCallback(search: string): JoinCallback | null {
  const params = new URLSearchParams(search);
  const state = params.get("state")?.trim() ?? "";
  if (!state) return null;
  return {
    code: params.get("code")?.trim() ?? "",
    state,
    error: params.get("error")?.trim() || null,
  };
}

/**
 * The authorize request's parameters as one object (a repeated name keeps
 * the first value; the daemon refuses a malformed request anyway).
 *
 * @param search - Query string.
 * @returns Name to value.
 */
export function authorizeParams(search: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [name, value] of new URLSearchParams(search)) {
    if (!(name in out)) out[name] = value;
  }
  return out;
}

/**
 * True when `redirect` goes back to the authorize request's own
 * `redirect_uri` (same origin and path), so the consent page never sends
 * the browser anywhere else.
 *
 * @param redirect - Where the daemon said to go.
 * @param search - The authorize request.
 * @returns Whether the consent page may go there.
 */
export function isConsentRedirect(redirect: string, search: string): boolean {
  const expected = authorizeParams(search).redirect_uri;
  if (!expected) return false;
  try {
    const to = new URL(redirect);
    const want = new URL(expected);
    return to.origin === want.origin && to.pathname === want.pathname;
  } catch {
    return false;
  }
}

/**
 * True for a branch invite (`https://<library>/enroll#bcn1.…`).
 *
 * @param text - Pasted text.
 * @returns Whether it has the invite's shape.
 */
export function looksLikeBranchInvite(text: string): boolean {
  const trimmed = text.trim();
  try {
    const url = new URL(trimmed);
    return (
      (url.protocol === "https:" || url.protocol === "http:") &&
      url.pathname.replace(/\/$/, "") === "/enroll" &&
      url.hash.startsWith("#bcn1.")
    );
  } catch {
    return false;
  }
}

/**
 * `mm:ss` (or `h:mm:ss`) until `expiresAt`, `0:00` once past.
 *
 * @param expiresAt - Expiry (RFC 3339 or epoch ms).
 * @param now - Current time in epoch ms.
 * @returns The countdown text, or `""` for a bad date.
 */
export function formatCountdown(expiresAt: string | number, now: number): string {
  const end = typeof expiresAt === "number" ? expiresAt : Date.parse(expiresAt);
  if (!Number.isFinite(end)) return "";
  const total = Math.max(0, Math.floor((end - now) / 1000));
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const seconds = total % 60;
  const ss = String(seconds).padStart(2, "0");
  if (hours > 0) return `${hours}:${String(minutes).padStart(2, "0")}:${ss}`;
  return `${minutes}:${ss}`;
}

/**
 * Splits a `sha256:…` fingerprint into short groups for reading aloud.
 *
 * @param fingerprint - The fingerprint.
 * @param size - Characters per group.
 * @returns The groups.
 */
export function fingerprintGroups(fingerprint: string, size = 4): string[] {
  const body = fingerprint.replace(/^sha256:/i, "");
  const groups: string[] = [];
  for (let i = 0; i < body.length; i += size) groups.push(body.slice(i, i + size));
  return groups;
}

/**
 * Friendlier copy for the join API's refusals.
 *
 * @param error - The caught failure.
 * @param fallback - Copy when it has none.
 *
 * @returns Copy for an inline error.
 */
export function joinErrorMessage(error: unknown, fallback: string): string {
  if (error instanceof ApiError) {
    switch (error.code) {
      case "enrollment_disabled":
        return "This library isn't accepting new branches.";
      case "join_expired":
        return "This sign-in took too long or was already used. Start again.";
      case "denied":
        return "The library's user did not let this branch join.";
      case "transport_insecure":
        return "That library can only be joined over HTTPS (plain HTTP works only on this machine).";
      default:
        return error.message || fallback;
    }
  }
  return error instanceof Error && error.message ? error.message : fallback;
}

/** The consent page to come back to after a sign-in that leaves the SPA (SSO). */
const APPROVE_RETURN = "bookclerk.joinApproveReturn";

/**
 * Keeps the consent page's address (path and authorize query: no secret)
 * for the length of this tab, so an SSO sign-in that comes back to `/`
 * can return to it.
 *
 * @param location - `pathname + search` of `/join/approve`.
 */
export function rememberApproveReturn(location: string): void {
  try {
    sessionStorage.setItem(APPROVE_RETURN, location);
  } catch {
    // private mode: the user starts the join again
  }
}

/**
 * Takes the remembered consent page address once, if it is one.
 *
 * @returns `/join/approve?…`, or `null`.
 */
export function takeApproveReturn(): string | null {
  try {
    const value = sessionStorage.getItem(APPROVE_RETURN);
    sessionStorage.removeItem(APPROVE_RETURN);
    return value && value.startsWith("/join/approve?") ? value : null;
  } catch {
    return null;
  }
}

/** Remembers "Start a new library" so the welcome screen stays closed. */
const WELCOME_DISMISSED = "bookclerk.welcomeDismissed";

/**
 * Whether the operator already chose "Start a new library" here.
 *
 * @returns True once dismissed.
 */
export function welcomeDismissed(): boolean {
  try {
    return localStorage.getItem(WELCOME_DISMISSED) === "1";
  } catch {
    return false;
  }
}

/** Keeps the welcome screen closed in this browser. */
export function dismissWelcome(): void {
  try {
    localStorage.setItem(WELCOME_DISMISSED, "1");
  } catch {
    // private mode: the screen shows again next time
  }
}
