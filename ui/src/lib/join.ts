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
 * - Branches of branches: a branch shares its books with its library (two
 *   settings per library in Settings → Library, which the operator or an
 *   Owner of the branch may change); the library shows them
 *   read-only per branch in Settings → Branches, with a bulk remove for
 *   titles from a branch that stopped sharing or left.
 * - An Owner of the library may instead let an **empty** install join as a
 *   **clerk** (code name: an owner node): it shares the library's database
 *   and keys and replaces whatever the install held. The choice is made on
 *   the consent page, needs a fresh second factor, and is bound to the
 *   code; the keys travel daemon to daemon, sealed to the joining node,
 *   never through this SPA.
 */

import { ApiError, type ConfigNodeView } from "@/lib/api";

/** What a library says about itself, read by this install's daemon. */
export interface LibraryInfoView {
  /** The library's owner cluster id: stable, and what a branch link is keyed by. */
  cluster_id: string;
  /** The library's public host name, from its origin (at most 253 bytes). */
  library_name: string;
  /** The library's canonical origin (`https://library.example`). */
  origin: string;
  /** Node API transport (`loopback`, `proxy`, `acme`, `self_signed`). */
  transport: string;
  /** "Let new branches join" on the library. */
  enrollment: boolean;
  /** SPKI fingerprint to confirm (self-signed libraries only). */
  fingerprint: string | null;
  /** True when the join must confirm `fingerprint` before it can start (self-signed transport). */
  needs_fingerprint: boolean;
}

/** A library this install is a branch of (no key material). */
export interface BranchLinkView {
  /** The library's owner cluster id. */
  cluster_id: string;
  /** The library's canonical origin this install dials. */
  owner_origin: string;
  /** The node id the library minted for this install when it joined. */
  node_id: string;
  /** True when this install pins the library's certificate (SPKI) instead of trusting public CAs. */
  pinned: boolean;
  /** `active`, `revoked` (by the library), or `left`. */
  state: string;
  /** When this install joined (RFC 3339, UTC). */
  enrolled_at: string;
  /** Whether this daemon's client for it is running. */
  running?: boolean;
  /** "Share this branch's books with <library>" (on by default). */
  share_books?: boolean;
  /** "Include books from my own branches" (on by default). */
  include_branches?: boolean;
}

/** `GET /api/config/join`. */
export interface JoinStatus {
  /** No Owner, no Administrator, and no library joined yet. */
  fresh: boolean;
  /** True when this install has an Owner or an Administrator. */
  has_owner: boolean;
  /** The branch name the join form suggests (this machine's name). */
  default_branch_name: string;
  /** Every library this install has joined, current or past. */
  links: BranchLinkView[];
  /** This install is empty, so it may offer to join as a clerk. */
  clerk_eligible?: boolean;
  /** Why it is not (what it already holds), when it is not. */
  clerk_problem?: string | null;
}

/** How a server joins: a branch keeps its own database; a clerk shares the library's. */
export type ConnectionType = "branch" | "clerk";

/** `POST /api/config/join/start`. */
export interface JoinStart {
  /** The library's sign-in URL to send the browser to (state and S256 challenge included). */
  authorize_url: string;
  /** The library as this daemon read it when the join started. */
  library: LibraryInfoView;
  /** The branch name the library's users will see (trimmed; this machine's name by default). */
  branch_name: string;
  /** Seconds until the pending sign-in (its verifier and node keys) expires. */
  expires_in_secs: number;
  /** Whether the sign-in offered to join as a clerk. */
  clerk_offered?: boolean;
}

/** What the consent page may offer: only an Owner gets a choice. */
export type ConnectionChoiceView =
  | { choice: false; default: "branch" }
  | {
      choice: true;
      default: "branch";
      /** Clerk may be chosen (shared database, empty joiner, second factor). */
      clerk_available: boolean;
      /** Why not, when it may not. */
      clerk_reason: string | null;
      /** Second factors the user can confirm with (`totp`, `passkey`). */
      step_up: string[];
    };

/** `GET /api/auth/branch-consent`: what the consent page shows. */
export interface BranchConsentView {
  /** This library's name, as the consent question shows it. */
  library_name: string;
  /** The name the joining install asked to appear as. */
  branch_name: string;
  /** Where the browser goes back to (the joining install). */
  redirect_origin: string;
  /** False when this library has "Let new branches join" off; consent then refuses. */
  enrollment: boolean;
  /** The signed-in user the branch would belong to. */
  user: { id: number; display_name: string | null };
  /** Branch or clerk (absent from an older library: branch only). */
  connection?: ConnectionChoiceView;
}

/** A fresh second factor for a clerk consent. */
export type ClerkStepUp =
  | { method: "totp"; code: string }
  | { method: "passkey"; challenge_id: string; credential: Record<string, unknown> };

/** `POST /api/config/join/callback` for a clerk: this server restarts. */
export interface ClerkJoined {
  connection: "clerk";
  cluster_id: string;
  node_id: string;
  library_name: string;
  /** `postgres` or `d1`. */
  database: string;
  /** master.key was wrapped with this server's own password. */
  master_key_wrapped: boolean;
  restarting: boolean;
}

/** `POST /api/config/join/callback` for a branch. */
export interface BranchJoined {
  connection?: "branch";
  cluster: BranchLinkView;
  library_name: string;
  running: boolean;
}

/** Counts of the titles one branch shared with this library. */
export interface BranchTitleCounts {
  /** Titles it shares now. */
  shared: number;
  /** Of those, titles whose files arrived. */
  delivered: number;
  /** Titles from it that it stopped sharing, withdrew, or left behind. */
  detached: number;
}

/** One branch of this library, as Settings → Branches lists it. */
export interface BranchRowView {
  /** The node id this library gave the branch when it joined. */
  node_id: string;
  /** When the branch joined (RFC 3339, UTC). */
  created_at: string;
  /** The user of this library who owns the branch; `null` when none is recorded. */
  owner_user_id: number | null;
  /** That owner's display name; `null` when the owner is unknown. */
  owner_name: string | null;
  /** False once it left or was removed. */
  joined: boolean;
  /** As the branch last reported it; `null` before its first sync. */
  share_books: boolean | null;
  /** As the branch last reported it; `null` before its first sync. */
  include_branches: boolean | null;
  titles: BranchTitleCounts;
}

/** `GET /api/config/branches`: Settings → Branches on the library. */
export interface BranchSettingsView {
  /** This library's name, as joining installs see it. */
  library_name: string;
  /** This library's canonical origin, which branches dial. */
  origin: string;
  /** Node API transport (`loopback`, `proxy`, `acme`, `self_signed`). */
  transport: string;
  /** "Let new branches join": whether sign-ins and invites may enroll new branches. */
  enrollment: boolean;
  /** The certificate fingerprint a joining install confirms (self-signed transport only). */
  spki_fingerprint: string | null;
  /** Each branch of this library, with its reported sharing settings and title counts. */
  branches?: BranchRowView[];
}

/** A branch invite as listed (never its secret). */
export interface BranchInviteView {
  /** The invite's id (not its secret). */
  link_id: string;
  /** The user whose branch the invite enrolls. */
  for_user_id: number;
  /** A note the minting user gave the invite. */
  label: string;
  /** Audit id of the user who minted the invite. */
  created_by: string;
  /** When it was minted (RFC 3339, UTC). */
  created_at: string;
  /** When it stops working if not redeemed (RFC 3339, UTC). */
  expires_at: string;
  /** When a branch used it (RFC 3339, UTC); an invite is single-use. */
  redeemed_at: string | null;
  /** The node id minted for the branch that used it. */
  redeemed_node_id: string | null;
  /** When it was revoked (RFC 3339, UTC), if it was. */
  revoked_at: string | null;
  /** `pending`, `redeemed`, `revoked`, or `expired`. */
  state: string;
}

/** A freshly minted branch invite: `url` is shown once and dropped. */
export interface MintedBranchInvite {
  /** The invite as it is listed from now on. */
  link: BranchInviteView;
  /** The whole invite, secret included: shown once, never stored. */
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
  /** The library's own canonical origin, when the refusal was an address mismatch. */
  origin?: string;

  /**
   * @param status - HTTP status of the refusal.
   * @param message - The server's detail, for display.
   * @param code - The server's machine-readable `error` code, when it sent one.
   * @param origin - The library's own origin, on an address mismatch.
   */
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
  /** Offer to join as a clerk (this install must be empty). */
  clerk?: boolean;
}): Promise<JoinStart> {
  return request("/api/config/join/start", {
    method: "POST",
    body: {
      address: body.address,
      fingerprint: body.fingerprint ?? undefined,
      branch_name: body.branch_name?.trim() || undefined,
      clerk: body.clerk ? true : undefined,
    },
  });
}

/**
 * Relays the library's answer to this install's daemon, which redeems it.
 *
 * @param callback - `code`, `state`, `error`, and `connection` from the callback URL.
 * @returns The recorded link (branch), or what was applied (clerk).
 */
export function finishJoin(callback: JoinCallback): Promise<BranchJoined | ClerkJoined> {
  return request("/api/config/join/callback", {
    method: "POST",
    body: {
      code: callback.code,
      state: callback.state,
      error: callback.error ?? undefined,
      connection: callback.connection === "clerk" ? "clerk" : undefined,
    },
  });
}

/**
 * True when the callback answer is a clerk join.
 *
 * @param joined - What `finishJoin` answered.
 * @returns Whether this server now shares the library's database.
 */
export function isClerkJoined(joined: BranchJoined | ClerkJoined): joined is ClerkJoined {
  return joined.connection === "clerk";
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
 * True when `role` may change Settings → Library's two sharing settings:
 * the operator (not while impersonating) or an Owner. The daemon checks the
 * stored role again; anyone else is refused.
 *
 * @param role - The session's role (`operator`, `owner`, ...), if known.
 * @param impersonating - True while the operator acts as another user.
 * @returns Whether to show the sharing settings.
 */
export function canChangeBranchSharing(role: string | undefined, impersonating = false): boolean {
  return (role === "operator" && !impersonating) || role === "owner";
}

/**
 * Settings → Library for the operator or an Owner: the libraries this
 * server is a branch of, with their two sharing settings.
 *
 * @returns The libraries.
 */
export function fetchBranchSharing(): Promise<{ clusters: BranchLinkView[] }> {
  return request("/api/config/join/sharing");
}

/**
 * Settings → Library: the two sharing settings for one library this server
 * is a branch of (the operator or an Owner). The library hears them at once.
 *
 * @param clusterId - The library.
 * @param shareBooks - "Share this branch's books with <library>".
 * @param includeBranches - "Include books from my own branches".
 * @returns The link with the stored settings.
 */
export function setBranchSharing(
  clusterId: string,
  shareBooks: boolean,
  includeBranches: boolean,
): Promise<{ cluster: BranchLinkView }> {
  return request("/api/config/join/sharing", {
    method: "POST",
    body: { cluster_id: clusterId, share_books: shareBooks, include_branches: includeBranches },
  });
}

/**
 * Removes the entries of titles from branches that stopped sharing or left
 * (one branch, or all). Files already delivered stay in the outputs.
 *
 * @param nodeId - Only this branch; every branch when omitted.
 * @returns How many entries were removed.
 */
export function removeBranchTitles(nodeId?: string): Promise<{ removed: number }> {
  return request("/api/config/branches/titles/remove", {
    method: "POST",
    body: nodeId ? { node_id: nodeId } : {},
  });
}

/**
 * One line on what a branch shares with this library (read-only).
 *
 * @param row - The branch.
 * @returns For example "Shares its books and its own branches' books".
 */
export function branchSharingSummary(row: BranchRowView): string {
  if (!row.joined) return "Left this library";
  if (row.share_books === null) return "Has not shared books yet";
  if (!row.share_books) return "Does not share its books";
  return row.include_branches
    ? "Shares its books and its own branches' books"
    : "Shares its own books only";
}

/**
 * A short note on a branch's titles, or `null` when it shared none.
 *
 * @param counts - The branch's title counts.
 * @param joined - Whether the branch is still joined.
 * @returns For example "3 books shared, 2 arrived · 1 book from a branch that left".
 */
export function branchTitlesNote(counts: BranchTitleCounts, joined: boolean): string | null {
  const parts: string[] = [];
  const books = (n: number) => `${n} ${n === 1 ? "book" : "books"}`;
  if (counts.shared > 0) parts.push(`${books(counts.shared)} shared, ${counts.delivered} arrived`);
  if (counts.detached > 0) {
    parts.push(
      `${books(counts.detached)} ${joined ? "no longer shared" : "from a branch that left"}`,
    );
  }
  return parts.length ? parts.join(" · ") : null;
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
 * @param clerk - Set to join as a clerk instead, with the Owner's step-up proof.
 * @returns Where to send the browser.
 */
export function answerBranchConsent(
  search: string,
  approve: boolean,
  clerk?: { step_up: ClerkStepUp },
): Promise<{ redirect: string }> {
  return request("/api/auth/branch-consent", {
    method: "POST",
    body: {
      ...authorizeParams(search),
      approve,
      ...(clerk && approve ? { connection: "clerk", step_up: clerk.step_up } : {}),
    },
  });
}

/**
 * Begins a passkey step-up for a clerk consent (Owners only).
 *
 * @returns The WebAuthn request options and their `challenge_id`.
 */
export function beginClerkStepUp(): Promise<{
  challenge_id: string;
  publicKey: Record<string, unknown>;
}> {
  return request("/api/auth/branch-consent/step-up", { method: "POST", body: {} });
}

/** `GET /api/config/clerks`: the owner nodes for Settings → Clerks. */
export interface ClerksResponse {
  /** This server's node id. */
  node_id: string;
  /** The owner nodes the caller may see, shaped like `listConfigNodes`'s. */
  nodes: ConfigNodeView[];
}

/**
 * Lists the owner nodes (this server and its clerks) for Settings → Clerks:
 * every one for the operator or an Owner, who may remove any of them;
 * another user only the ones they own. Unlike `listConfigNodes`, an Owner
 * sees clerks owned by nobody or by other Owners.
 *
 * @returns This server's node id and the owner nodes.
 */
export function listClerks(): Promise<ClerksResponse> {
  return request("/api/config/clerks");
}

/**
 * Removes an owner node (a clerk) that no longer runs; Settings → Clerks
 * lists them from `listClerks`.
 *
 * @param nodeId - The node to remove.
 * @returns The removed id.
 */
export function removeClerk(nodeId: string): Promise<{ node_id: string; removed: boolean }> {
  return request(`/api/config/nodes/${encodeURIComponent(nodeId)}`, { method: "DELETE" });
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
 * @param mine - Only the caller's own, even for an Owner who may see all.
 * @returns The invites.
 */
export function listBranchInvites(mine = false): Promise<{ links: BranchInviteView[] }> {
  return request(mine ? "/api/config/node-invites?mine=true" : "/api/config/node-invites");
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
  /** The one-use authorization code; empty when the sign-in was refused. */
  code: string;
  /** The opaque sign-in id this daemon issued, echoed back by the library. */
  state: string;
  /** The library's OAuth error (`access_denied` when its user said no), if any. */
  error: string | null;
  /** `clerk` when the library approved a clerk join. */
  connection?: ConnectionType;
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
    connection: params.get("connection")?.trim() === "clerk" ? "clerk" : "branch",
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
      case "not_empty":
        return "This server already holds library data, so it can only join as a branch.";
      case "owner_required":
        return "Only an Owner of the library can let a server join as a clerk.";
      case "database_not_shareable":
        return "This library's database can't be shared (SQLite), so servers can only join as branches.";
      case "step_up_required":
        return "Confirm with your authenticator code or a passkey to let a clerk join.";
      case "step_up_failed":
        return "That code or passkey did not verify. Try again.";
      case "step_up_unavailable":
        return "Joining as a clerk needs an authenticator app or a passkey on your account.";
      case "step_up_locked":
        return "Too many tries. Wait a few minutes, then try again.";
      case "clerk_not_offered":
        return "The joining server did not offer to join as a clerk.";
      case "database_guest_missing":
        return "This server doesn't have the library's database plugin installed. Install it, then try again within 10 minutes.";
      case "d1_token_missing":
        return "Set this server's Cloudflare D1 API token (BOOKCLERK_D1_API_TOKEN), then try again within 10 minutes.";
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

/**
 * Only the owner nodes (`kind = owner`): the clerks of this library.
 *
 * @param nodes - Every node the caller sees.
 * @returns The ones that share this library's database, this server first.
 */
export function clerkNodes(nodes: ConfigNodeView[]): ConfigNodeView[] {
  return nodes
    .filter((node) => node.kind === "owner")
    .sort(
      (a, b) => Number(b.this_node) - Number(a.this_node) || a.node_id.localeCompare(b.node_id),
    );
}
