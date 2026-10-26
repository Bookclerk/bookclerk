// What a demo session does inside its container at the start and the end,
// written against an injected `call` so the tests drive it without one.
//
// Start: wait for bookclerkd, turn the storefronts on, and with the
// per-session operator token create the visitor's account, then redeem its
// claim ticket for a portal session the visitor's browser keeps. The visitor
// acts only as that account: store accounts are connected through the
// portal, so they belong to that identity.
//
// End: sign every connected store account out through the same route the
// Accounts page uses, so Audible deregisters the device it added, before
// the container (and with it every token) is destroyed.

import { decodeClaimTicket, decodeConnections, decodeInstances, portalSessionFrom, PORTAL_COOKIE } from "./inputs.ts";

/**
 * The storefront plugins the demo image ships (their manifest ids). The host
 * discovers them but, as with any optional plugin, runs them only once the
 * operator approves their grant and deploys them on the node.
 */
export const STOREFRONTS = ["audible", "libro", "chirp", "graphicaudio"];

/**
 * Sends one request to bookclerkd in the container.
 *
 * @param path - Path and query, starting with `/`.
 * @param init - Method, headers and body.
 * @returns bookclerkd's answer.
 */
export type ContainerCall = (path: string, init?: RequestInit) => Promise<Response>;

/**
 * Lowercase hex of `n` random bytes.
 *
 * @param n - Byte count.
 * @returns Hex string of length 2n.
 */
export function randomHex(n: number): string {
  const b = new Uint8Array(n);
  crypto.getRandomValues(b);
  return Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
}

/**
 * Polls `GET /health` until bookclerkd answers 200.
 *
 * @param call - Container request function.
 * @param opts - How long to wait in
 *   all, and between attempts.
 * @returns 
 * @throws {Error} When bookclerkd has not answered 200 in time.
 */
export async function waitForHealth(call: ContainerCall, opts: { timeoutMs: number; intervalMs: number; }): Promise<void> {
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
 * Makes the visitor the host's Owner and returns the visitor's portal
 * session.
 *
 * The visitor gets the account a person who installs Bookclerk has, so the
 * demo shows everything the Owner sees. The Worker still refuses the routes
 * the demo does not allow (src/routes.ts) whatever the role. The Owner gets a
 * random password nobody sees, so the claim ticket redeems without one and
 * nobody can sign in as the visitor some other way.
 *
 * @param call - Container request function.
 * @param operatorToken - The session's operator token.
 * @returns The `bookclerk_portal_session` value.
 * @throws {Error} When bootstrap or redeem fails or answers unexpectedly.
 */
export async function signInVisitor(call: ContainerCall, operatorToken: string): Promise<string> {
  const boot = await call("/api/auth/bootstrap", {
    method: "POST",
    headers: { Authorization: `Bearer ${operatorToken}`, "Content-Type": "application/json" },
    body: JSON.stringify({ display_name: "Demo visitor", login_name: "demo", password: randomHex(24) }),
  });
  const ticket = decodeClaimTicket(await boot.text());
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
 * Approves each storefront's requested grant as the operator and deploys its
 * default instance on this node, as an operator would in Settings. Visitors
 * cannot do either: the Worker refuses those routes.
 *
 * @param call - Container request function.
 * @param operatorToken - The session's operator token.
 * @param ids - Plugin ids to enable.
 * @returns 
 * @throws {Error} When a grant or deployment is refused, or a storefront has
 *   no default instance (the image is missing it).
 */
export async function enableStorefronts(call: ContainerCall, operatorToken: string, ids: string[] = STOREFRONTS): Promise<void> {
  const json = { Authorization: `Bearer ${operatorToken}`, "Content-Type": "application/json" };
  for (const id of ids) {
    const res = await call(`/api/plugins/${encodeURIComponent(id)}/consent`, {
      method: "POST",
      headers: json,
      body: JSON.stringify({ approve: true }),
    });
    await res.body?.cancel();
    if (res.status !== 200) throw new Error(`consent for ${id} answered ${res.status}`);
  }
  const list = await call("/api/config/plugin-instances", { headers: { Authorization: json.Authorization } });
  const found = list.status === 200 ? decodeInstances(await list.text(), ids) : null;
  if (!found) throw new Error(`plugin instances answered ${list.status}`);
  for (const id of ids) {
    const instance = found.instances.get(id);
    if (!instance) throw new Error(`no default instance for ${id}`);
    const path = `/api/config/plugin-instances/${encodeURIComponent(instance)}/deployments/${encodeURIComponent(found.nodeId)}`;
    const res = await call(path, { method: "PUT", headers: json, body: JSON.stringify({ desired: "present" }) });
    await res.body?.cancel();
    if (res.status !== 200) throw new Error(`deploying ${id} answered ${res.status}`);
  }
}

/**
 * Signs every store account linked to the visitor out at its store.
 *
 * The sign-outs run side by side and each is bounded: bookclerkd itself
 * gives a store about five seconds and always deletes the tokens, so a slow
 * store cannot hold the container (or the visitor's Start over) open.
 * Failures are counted, not thrown.
 *
 * @param call - Container request function.
 * @param portalSession - The visitor's portal session.
 * @param timeoutMs - Longest wait for each request.
 * @param origin - The demo's origin, which bookclerkd's cross-site
 *   check expects on cookie-authenticated writes (its `public_origin`).
 * @returns Accounts signed out,
 *   and accounts whose sign-out failed (including when the list could not
 *   be read: then `failed` is 1).
 */
export async function signOutStores(call: ContainerCall, portalSession: string, timeoutMs: number, origin: string): Promise<{ revoked: number; failed: number; }> {
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
