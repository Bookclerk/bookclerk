// What a demo session does inside its container at the start and the end,
// written against an injected `call` so the tests drive it without one.
//
// Start: wait for bookclerkd, create the visitor as the first Owner with the
// per-session operator token, and redeem the Owner's claim ticket for a
// portal session the visitor's browser keeps. Store accounts are connected
// through the portal, so they belong to that identity.
//
// End: sign every connected store account out through the same route the
// Accounts page uses, so Audible deregisters the device it added, before
// the container (and with it every token) is destroyed.

import { decodeBootstrap, decodeConnections, portalSessionFrom, PORTAL_COOKIE } from "./inputs.js";

/**
 * Sends one request to bookclerkd in the container.
 *
 * @callback ContainerCall
 * @param {string} path Path and query, starting with `/`.
 * @param {RequestInit} [init] Method, headers and body.
 * @returns {Promise<Response>} bookclerkd's answer.
 */

/**
 * Lowercase hex of `n` random bytes.
 *
 * @param {number} n Byte count.
 * @returns {string} Hex string of length 2n.
 */
export function randomHex(n) {
  const b = new Uint8Array(n);
  crypto.getRandomValues(b);
  return Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
}

/**
 * Polls `GET /health` until bookclerkd answers 200.
 *
 * @param {ContainerCall} call Container request function.
 * @param {{timeoutMs: number, intervalMs: number}} opts How long to wait in
 *   all, and between attempts.
 * @returns {Promise<void>}
 * @throws {Error} When bookclerkd has not answered 200 in time.
 */
export async function waitForHealth(call, opts) {
  const deadline = Date.now() + opts.timeoutMs;
  let last = "no answer";
  while (Date.now() < deadline) {
    try {
      const res = await call("/health");
      await res.body?.cancel();
      if (res.status === 200) return;
      last = `status ${res.status}`;
    } catch (err) {
      last = String(err);
    }
    await new Promise((r) => setTimeout(r, opts.intervalMs));
  }
  throw new Error(`bookclerkd did not become healthy: ${last}`);
}

/**
 * Creates the visitor's Owner account and returns its portal session.
 *
 * The Owner gets a random password nobody sees, so the claim ticket redeems
 * without one and nobody can sign in as the visitor some other way.
 *
 * @param {ContainerCall} call Container request function.
 * @param {string} operatorToken The session's operator token.
 * @returns {Promise<string>} The `bookclerk_portal_session` value.
 * @throws {Error} When bootstrap or redeem fails or answers unexpectedly.
 */
export async function signInVisitor(call, operatorToken) {
  const boot = await call("/api/auth/bootstrap", {
    method: "POST",
    headers: { Authorization: `Bearer ${operatorToken}`, "Content-Type": "application/json" },
    body: JSON.stringify({ display_name: "Demo visitor", login_name: "demo", password: randomHex(24) }),
  });
  const ticket = decodeBootstrap(await boot.text());
  if (boot.status !== 200 || !ticket) throw new Error(`bootstrap answered ${boot.status}`);
  const redeem = await call("/api/portal/redeem", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ ticket, nonce: randomHex(32) }),
  });
  await redeem.body?.cancel();
  const session = portalSessionFrom(redeem.headers.getSetCookie());
  if (redeem.status !== 200 || !session) throw new Error(`redeem answered ${redeem.status}`);
  return session;
}

/**
 * Signs every store account linked to the visitor out at its store.
 *
 * The sign-outs run side by side and each is bounded: bookclerkd itself
 * gives a store about five seconds and always deletes the tokens, so a slow
 * store cannot hold the container (or the visitor's Start over) open.
 * Failures are counted, not thrown.
 *
 * @param {ContainerCall} call Container request function.
 * @param {string} portalSession The visitor's portal session.
 * @param {number} timeoutMs Longest wait for each request.
 * @param {string} origin The demo's origin, which bookclerkd's cross-site
 *   check expects on cookie-authenticated writes (its `public_origin`).
 * @returns {Promise<{revoked: number, failed: number}>} Accounts signed out,
 *   and accounts whose sign-out failed (including when the list could not
 *   be read: then `failed` is 1).
 */
export async function signOutStores(call, portalSession, timeoutMs, origin) {
  const cookie = { Cookie: `${PORTAL_COOKIE}=${portalSession}` };
  let accounts;
  try {
    const res = await call("/api/portal/connections", { headers: cookie, signal: AbortSignal.timeout(timeoutMs) });
    accounts = res.status === 200 ? decodeConnections(await res.text()) : null;
  } catch {
    accounts = null;
  }
  if (accounts === null) return { revoked: 0, failed: 1 };
  const results = await Promise.allSettled(
    accounts.map(async (a) => {
      const path = `/api/portal/sources/${encodeURIComponent(a.source)}/connections/${encodeURIComponent(a.accountId)}/revoke`;
      const res = await call(path, {
        method: "POST",
        headers: { ...cookie, Origin: origin, "Content-Type": "application/json" },
        body: "{}",
        signal: AbortSignal.timeout(timeoutMs),
      });
      await res.body?.cancel();
      return res.status === 200;
    }),
  );
  const revoked = results.filter((r) => r.status === "fulfilled" && r.value).length;
  return { revoked, failed: results.length - revoked };
}
