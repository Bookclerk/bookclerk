/**
 * Member sessions: a random cookie whose SHA-256 is a D1 row.
 *
 * The cookie is `__Host-` prefixed (Secure, Path=/, no Domain), HttpOnly and
 * SameSite=Strict, so it never reaches other bookclerk.app hosts, scripts or
 * cross-site requests. Sessions expire 8 hours after sign-in whatever the
 * activity, so a member removed from the organization loses access within 8
 * hours; logout and "sign out everyone" delete rows at once.
 */

import { first, run } from "./db.js";

/** Session cookie name. */
export const SESSION_COOKIE = "__Host-bc_session";

/** Session length, in milliseconds (absolute, from sign-in). */
export const SESSION_TTL_MS = 8 * 60 * 60 * 1000;

/** Organization roles a session can carry. */
export const ROLES = /** @type {const} */ (["admin", "member"]);

/**
 * Base64url without padding.
 *
 * @param {Uint8Array} bytes Input.
 * @returns {string} Encoded text.
 */
export function base64url(bytes) {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/**
 * Random base64url token.
 *
 * @param {number} bytes Entropy, in bytes.
 * @returns {string} The token.
 */
export function randomToken(bytes) {
  return base64url(crypto.getRandomValues(new Uint8Array(bytes)));
}

/**
 * Lowercase hex SHA-256 of a string.
 *
 * @param {string} text Input.
 * @returns {Promise<string>} 64 hex characters.
 */
export async function sha256Hex(text) {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

/**
 * Value of one cookie from a request.
 *
 * @param {Request} request Incoming request.
 * @param {string} name Cookie name.
 * @returns {string | null} The value, or null when absent.
 */
export function readCookie(request, name) {
  const header = request.headers.get("cookie") || "";
  for (const part of header.split(";")) {
    const eq = part.indexOf("=");
    if (eq < 0) continue;
    if (part.slice(0, eq).trim() === name) return part.slice(eq + 1).trim();
  }
  return null;
}

/**
 * A `Set-Cookie` value for a `__Host-` cookie.
 *
 * @param {string} name Cookie name (must start with `__Host-`).
 * @param {string} value Cookie value (base64url or empty).
 * @param {{maxAge: number, sameSite: "Strict" | "Lax"}} opts Lifetime in
 *   seconds (0 deletes) and SameSite mode.
 * @returns {string} Header value.
 */
export function hostCookie(name, value, opts) {
  return `${name}=${value}; Path=/; Secure; HttpOnly; SameSite=${opts.sameSite}; Max-Age=${opts.maxAge}`;
}

/**
 * A signed-in member.
 *
 * @typedef {{userId: number, login: string, role: "admin" | "member",
 *   expiresAt: number}} Member
 */

/**
 * Creates a session row and the cookie that names it.
 *
 * @param {D1Database} db Database.
 * @param {{userId: number, login: string, role: "admin" | "member"}} who
 *   Verified member.
 * @param {number} now Unix time in milliseconds.
 * @returns {Promise<string>} The `Set-Cookie` value.
 */
export async function createSession(db, who, now) {
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
 * @param {D1Database} db Database.
 * @param {Request} request Incoming request.
 * @param {number} now Unix time in milliseconds.
 * @returns {Promise<Member | null>} The member, or null when there is no
 *   valid, unexpired session.
 */
export async function currentMember(db, request, now) {
  const token = readCookie(request, SESSION_COOKIE);
  if (!token || !/^[A-Za-z0-9_-]{43}$/.test(token)) return null;
  const row = await first(
    db,
    "SELECT user_id, login, role, expires_at FROM sessions WHERE id_hash = ? AND expires_at > ?",
    await sha256Hex(token),
    now,
  );
  if (!row) return null;
  const role = ROLES.includes(row.role) ? row.role : "member";
  return { userId: Number(row.user_id), login: String(row.login), role, expiresAt: Number(row.expires_at) };
}

/**
 * Deletes the request's session, if any.
 *
 * @param {D1Database} db Database.
 * @param {Request} request Incoming request.
 * @returns {Promise<string>} A `Set-Cookie` value that clears the cookie.
 */
export async function endSession(db, request) {
  const token = readCookie(request, SESSION_COOKIE);
  if (token && /^[A-Za-z0-9_-]{43}$/.test(token)) {
    await run(db, "DELETE FROM sessions WHERE id_hash = ?", await sha256Hex(token));
  }
  return hostCookie(SESSION_COOKIE, "", { maxAge: 0, sameSite: "Strict" });
}

/**
 * Deletes every session (owner action).
 *
 * @param {D1Database} db Database.
 * @returns {Promise<void>}
 */
export async function endAllSessions(db) {
  await run(db, "DELETE FROM sessions");
}

/**
 * Whether a state-changing request comes from this origin's own pages.
 *
 * SameSite=Strict already keeps the session cookie off cross-site requests;
 * this also refuses a POST whose Origin is missing or foreign.
 *
 * @param {Request} request Incoming POST.
 * @returns {boolean} True when `Origin` equals the request's own origin.
 */
export function sameOrigin(request) {
  const origin = request.headers.get("origin");
  return Boolean(origin) && origin === new URL(request.url).origin;
}
