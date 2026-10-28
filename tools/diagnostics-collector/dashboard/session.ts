/**
 * Member sessions: a random cookie whose SHA-256 is a D1 row.
 *
 * The cookie is `__Host-` prefixed (Secure, Path=/, no Domain), HttpOnly and
 * SameSite=Strict, so it never reaches other bookclerk.app hosts, scripts or
 * cross-site requests. Sessions expire 8 hours after sign-in whatever the
 * activity, so a member removed from the organization loses access within 8
 * hours; logout and "sign out everyone" delete rows at once.
 */

import { first, run } from "./db.ts";

/** Session cookie name. */
export const SESSION_COOKIE = "__Host-bc_session";

/** Session length, in milliseconds (absolute, from sign-in). */
export const SESSION_TTL_MS = 8 * 60 * 60 * 1000;

/** Organization roles a session can carry. */
export const ROLES = ["admin", "member"] as const;

/**
 * Base64url without padding.
 *
 * @param bytes - Input.
 * @returns Encoded text.
 */
export function base64url(bytes: Uint8Array): string {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/**
 * Random base64url token.
 *
 * @param bytes - Entropy, in bytes.
 * @returns The token.
 */
export function randomToken(bytes: number): string {
  return base64url(crypto.getRandomValues(new Uint8Array(bytes)));
}

/**
 * Lowercase hex SHA-256 of a string.
 *
 * @param text - Input.
 * @returns 64 hex characters.
 */
export async function sha256Hex(text: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

/**
 * Value of one cookie from a request.
 *
 * @param request - Incoming request.
 * @param name - Cookie name.
 * @returns The value, or null when absent.
 */
export function readCookie(request: Request, name: string): string | null {
  return cookieValue(request.headers.get("cookie") || "", name);
}

/**
 * Value of one cookie from a `Cookie` header (sans-IO, so the fuzz tests
 * drive the same code as requests). The first cookie with the name wins.
 *
 * @param header - `Cookie` header text (untrusted).
 * @param name - Cookie name.
 * @returns The value, or null when absent.
 */
export function cookieValue(header: string, name: string): string | null {
  for (const part of String(header).split(";")) {
    const eq = part.indexOf("=");
    if (eq < 0) continue;
    if (part.slice(0, eq).trim() === name) return part.slice(eq + 1).trim();
  }
  return null;
}

/**
 * A `Set-Cookie` value for a `__Host-` cookie.
 *
 * @param name - Cookie name (must start with `__Host-`).
 * @param value - Cookie value (base64url or empty).
 * @param opts - Lifetime in
 *   seconds (0 deletes) and SameSite mode.
 * @returns Header value.
 */
export function hostCookie(
  name: string,
  value: string,
  opts: { maxAge: number; sameSite: "Strict" | "Lax" },
): string {
  return `${name}=${value}; Path=/; Secure; HttpOnly; SameSite=${opts.sameSite}; Max-Age=${opts.maxAge}`;
}

/** A signed-in member. */
export type Member = {
  /** GitHub user ID. */
  userId: number;
  /** GitHub login. */
  login: string;
  /** Organization role. */
  role: (typeof ROLES)[number];
  /** When the session ends, Unix time in milliseconds. */
  expiresAt: number;
};

/**
 * Creates a session row and the cookie that names it.
 *
 * @param db - Database.
 * @param who - Verified member.
 * @param now - Unix time in milliseconds.
 * @returns The `Set-Cookie` value.
 */
export async function createSession(
  db: D1Database,
  who: { userId: number; login: string; role: "admin" | "member" },
  now: number,
): Promise<string> {
  const token = randomToken(32);
  await run(
    db,
    "INSERT INTO sessions (id_hash, user_id, login, role, created_at, expires_at) VALUES (?, ?, ?, ?, ?, ?)",
    await sha256Hex(token),
    who.userId,
    who.login,
    who.role,
    now,
    now + SESSION_TTL_MS,
  );
  return hostCookie(SESSION_COOKIE, token, { maxAge: SESSION_TTL_MS / 1000, sameSite: "Strict" });
}

/**
 * The member a request's session cookie names, if it is live.
 *
 * @param db - Database.
 * @param request - Incoming request.
 * @param now - Unix time in milliseconds.
 * @returns The member, or null when there is no
 *   valid, unexpired session.
 */
export async function currentMember(
  db: D1Database,
  request: Request,
  now: number,
): Promise<Member | null> {
  const token = sessionToken(request.headers.get("cookie") || "");
  if (!token) return null;
  const row = await first(
    db,
    "SELECT user_id, login, role, expires_at FROM sessions WHERE id_hash = ? AND expires_at > ?",
    await sha256Hex(token),
    now,
  );
  return memberFromRow(row, now);
}

/**
 * The session token a `Cookie` header carries, if well formed (sans-IO, so
 * the fuzz tests drive the same code as requests).
 *
 * @param cookieHeader - `Cookie` header (untrusted).
 * @returns The 43-character base64url token, or null.
 */
export function sessionToken(cookieHeader: string): string | null {
  const token = cookieValue(cookieHeader, SESSION_COOKIE);
  return token && /^[A-Za-z0-9_-]{43}$/.test(token) ? token : null;
}

/**
 * The authorization decision for a looked-up session row (sans-IO).
 *
 * @param row - `sessions` row, or null when
 *   no row matched.
 * @param now - Unix time in milliseconds.
 * @returns The member, or null when there is no row, it has
 *   expired, or its user id or login is malformed. An unknown role reads as
 *   "member", never "admin".
 */
export function memberFromRow(row: Record<string, unknown> | null, now: number): Member | null {
  if (!row || typeof row !== "object") return null;
  const expiresAt = Number(row.expires_at);
  const userId = Number(row.user_id);
  const login = typeof row.login === "string" ? row.login : "";
  if (!Number.isFinite(expiresAt) || expiresAt <= now) return null;
  if (!Number.isSafeInteger(userId) || userId <= 0 || !/^[A-Za-z0-9-]{1,39}$/.test(login))
    return null;
  const role = row.role === "admin" ? "admin" : "member";
  return { userId, login, role, expiresAt };
}

/**
 * Deletes the request's session, if any.
 *
 * @param db - Database.
 * @param request - Incoming request.
 * @returns A `Set-Cookie` value that clears the cookie.
 */
export async function endSession(db: D1Database, request: Request): Promise<string> {
  const token = sessionToken(request.headers.get("cookie") || "");
  if (token) {
    await run(db, "DELETE FROM sessions WHERE id_hash = ?", await sha256Hex(token));
  }
  return hostCookie(SESSION_COOKIE, "", { maxAge: 0, sameSite: "Strict" });
}

/**
 * Deletes every session (owner action).
 *
 * @param db - Database.
 */
export async function endAllSessions(db: D1Database): Promise<void> {
  await run(db, "DELETE FROM sessions");
}

/**
 * Whether a state-changing request comes from this origin's own pages.
 *
 * SameSite=Strict already keeps the session cookie off cross-site requests;
 * this also refuses a POST whose Origin is missing or foreign.
 *
 * @param request - Incoming POST.
 * @returns True when `Origin` equals the request's own origin.
 */
export function sameOrigin(request: Request): boolean {
  const origin = request.headers.get("origin");
  return Boolean(origin) && origin === new URL(request.url).origin;
}
