// What a demo session does inside its container at the start and the end,
// written against an injected `call` so the tests drive it without one.
//
// Start: wait for bookclerkd, turn the storefronts on, and with the
// per-session operator token create the visitor's account, then redeem its
// claim ticket for a portal session the visitor's browser keeps. The visitor
// acts only as that account: store accounts are connected through the
// portal, so they belong to that identity. Last, wait a little for the
// visitor's Discover shelves, the page they land on, to be built.
//
// End: sign every connected store account out through the same route the
// Accounts page uses, so Audible deregisters the device it added, before
// the container (and with it every token) is destroyed.

import {
  decodeClaimTicket,
  decodeConnections,
  decodeDiscoverReady,
  decodeInstances,
  portalSessionFrom,
  PORTAL_COOKIE,
} from "./inputs.ts";

/**
 * The storefront plugins the demo image ships (their manifest ids). The host
 * discovers them but, as with any optional plugin, runs them only once the
 * operator approves their grant and deploys them on the node.
 */
export const STOREFRONTS = ["audible", "libro", "chirp", "graphicaudio"];

/**
 * Plugins a session enables after the storefronts, without which the demo
 * still works: `covers` downloads the images the web app shows (covers,
 * store icons, avatars) so they load from the demo's own origin. When one
 * can't be enabled, the app shows its fallback images.
 */
export const OPTIONAL_PLUGINS = ["covers"];

/**
 * The demo Owner's password, shown in the banner (public/_demo/banner.js)
 * so the visitor can elevate in Settings. Only `POST /api/auth/elevate`
 * accepts it; the sign-in routes are refused (src/routes.ts).
 */
export const VISITOR_PASSWORD = "bookclerk-demo";

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
export async function waitForHealth(
  call: ContainerCall,
  opts: { timeoutMs: number; intervalMs: number },
): Promise<void> {
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
 * Waits for the visitor's Discover shelves, the page a signed-in visitor
 * lands on, to be stored.
 *
 * The first read of `GET /api/discover/status` queues the visitor's
 * storefront fetch and ranking; later reads only report. The wait is
 * best-effort: at the deadline the session opens anyway and the page fills
 * its placeholders as the shelves arrive. A failed read counts as not ready.
 *
 * @param call - Container request function.
 * @param portalSession - The visitor's portal session.
 * @param opts - How long to wait in all, and between reads.
 * @returns True when the shelves were ready in time; otherwise false.
 */
export async function waitForShelves(
  call: ContainerCall,
  portalSession: string,
  opts: { timeoutMs: number; intervalMs: number },
): Promise<boolean> {
  const deadline = Date.now() + opts.timeoutMs;
  const headers = { Cookie: `${PORTAL_COOKIE}=${portalSession}` };
  for (;;) {
    const left = deadline - Date.now();
    if (left <= 0) return false;
    try {
      const res = await call("/api/discover/status", {
        headers,
        signal: AbortSignal.timeout(left),
      });
      if (res.status !== 200) await res.body?.cancel();
      else if (decodeDiscoverReady(await res.text()) === true) return true;
    } catch {
      // Not ready yet: a slow or refused read is retried until the deadline.
    }
    const pause = Math.min(opts.intervalMs, deadline - Date.now());
    if (pause <= 0) return false;
    await new Promise((r) => setTimeout(r, pause));
  }
}

/**
 * How long the start waits for the visitor's Discover shelves before the
 * session opens anyway, and how long it pauses between reads.
 */
export const SHELVES_WAIT = { timeoutMs: 25_000, intervalMs: 1_000 } as const;

/**
 * Builds the visitor's Discover shelves before the session opens, waiting
 * at most {@link SHELVES_WAIT}. Never fails the start: past the deadline it
 * logs a warning and the page fills in as the shelves arrive.
 *
 * @param call - Container request function.
 * @param portalSession - The visitor's portal session.
 * @param sessionId - The demo session's id, for the warning.
 * @param wait - How long to wait in all, and between reads.
 * @returns True when the shelves were ready in time; otherwise false.
 */
export async function prepareShelves(
  call: ContainerCall,
  portalSession: string,
  sessionId: string,
  wait: { timeoutMs: number; intervalMs: number } = SHELVES_WAIT,
): Promise<boolean> {
  const ready = await waitForShelves(call, portalSession, wait);
  if (!ready) console.warn(`demo session ${sessionId} opened before its shelves were ready`);
  return ready;
}

/**
 * Makes the visitor the host's Owner and returns the visitor's portal
 * session.
 *
 * The visitor gets the account a person who installs Bookclerk has, so the
 * demo shows everything the Owner sees. The Worker still refuses the routes
 * the demo does not allow (src/routes.ts) whatever the role. The Owner's
 * password is {@link VISITOR_PASSWORD}, which the banner shows, so the
 * visitor can elevate to operator and impersonate the seeded users as a real
 * Owner would. Knowing it signs nobody in: every password sign-in route is
 * refused, and each visitor has a VM of their own.
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
    body: JSON.stringify({
      display_name: "Demo visitor",
      login_name: "demo",
      password: VISITOR_PASSWORD,
    }),
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
 * Reads this node's id, the wanted plugins' default instances and which of
 * them are already deployed here.
 *
 * @param call - Container request function.
 * @param authorization - The operator's `Authorization` header value.
 * @param ids - Plugin ids to look for.
 * @returns What {@link decodeInstances} read.
 * @throws {Error} When the list cannot be read.
 */
async function listInstances(
  call: ContainerCall,
  authorization: string,
  ids: string[],
): Promise<{ nodeId: string; instances: Map<string, string>; present: Set<string> }> {
  const list = await call("/api/config/plugin-instances", {
    headers: { Authorization: authorization },
  });
  const found = list.status === 200 ? decodeInstances(await list.text(), ids) : null;
  if (!found) {
    await list.body?.cancel();
    throw new Error(`plugin instances answered ${list.status}`);
  }
  return found;
}

/**
 * Approves each plugin's requested grant as the operator and deploys its
 * default instance on this node, as an operator would in Settings. Visitors
 * cannot do either: the Worker refuses those routes. A plugin whose default
 * instance is already deployed on this node (a baked image ships them so)
 * is left alone.
 *
 * @param call - Container request function.
 * @param operatorToken - The session's operator token.
 * @param ids - Plugin ids to enable.
 * @returns
 * @throws {Error} When the instance list cannot be read, a grant or
 *   deployment is refused, or a storefront has no default instance (the
 *   image is missing it).
 */
export async function enableStorefronts(
  call: ContainerCall,
  operatorToken: string,
  ids: string[] = STOREFRONTS,
): Promise<void> {
  const json = { Authorization: `Bearer ${operatorToken}`, "Content-Type": "application/json" };
  const before = await listInstances(call, json.Authorization, ids);
  const missing = ids.filter((id) => !before.present.has(id));
  if (missing.length === 0) return;
  for (const id of missing) {
    const res = await call(`/api/plugins/${encodeURIComponent(id)}/consent`, {
      method: "POST",
      headers: json,
      body: JSON.stringify({ approve: true }),
    });
    await res.body?.cancel();
    if (res.status !== 200) throw new Error(`consent for ${id} answered ${res.status}`);
  }
  const found = await listInstances(call, json.Authorization, missing);
  for (const id of missing) {
    const instance = found.instances.get(id);
    if (!instance) throw new Error(`no default instance for ${id}`);
    const path = `/api/config/plugin-instances/${encodeURIComponent(instance)}/deployments/${encodeURIComponent(found.nodeId)}`;
    const res = await call(path, {
      method: "PUT",
      headers: json,
      body: JSON.stringify({ desired: "present" }),
    });
    await res.body?.cancel();
    if (res.status !== 200) throw new Error(`deploying ${id} answered ${res.status}`);
  }
}

/**
 * Enables the storefronts, then each of [`OPTIONAL_PLUGINS`], whose failure
 * is logged instead of ending the session.
 *
 * One list read comes first: on a baked image every plugin is already
 * deployed, and nothing else is sent.
 *
 * @param call - Container request function.
 * @param operatorToken - The session's operator token.
 * @param warn - Where an optional plugin's failure is reported.
 * @returns
 * @throws {Error} When the instance list cannot be read or a storefront
 *   cannot be enabled.
 */
export async function enableDemoPlugins(
  call: ContainerCall,
  operatorToken: string,
  warn: (message: string) => void = console.warn,
): Promise<void> {
  const listed = await listInstances(call, `Bearer ${operatorToken}`, [
    ...STOREFRONTS,
    ...OPTIONAL_PLUGINS,
  ]);
  const storefronts = STOREFRONTS.filter((id) => !listed.present.has(id));
  if (storefronts.length > 0) await enableStorefronts(call, operatorToken, storefronts);
  for (const id of OPTIONAL_PLUGINS) {
    if (listed.present.has(id)) continue;
    try {
      await enableStorefronts(call, operatorToken, [id]);
    } catch (err) {
      warn(`demo: ${id} not enabled: ${err instanceof Error ? err.message : String(err)}`);
    }
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
export async function signOutStores(
  call: ContainerCall,
  portalSession: string,
  timeoutMs: number,
  origin: string,
): Promise<{ revoked: number; failed: number }> {
  const cookie = { Cookie: `${PORTAL_COOKIE}=${portalSession}` };
  let accounts;
  try {
    const res = await call("/api/portal/connections", {
      headers: cookie,
      signal: AbortSignal.timeout(timeoutMs),
    });
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
