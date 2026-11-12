// The collector's cross-origin policy: every answer is closed to other
// origins, stated with explicit headers rather than left to how browsers
// treat a missing one.
//
// Nothing here is meant for another site's pages. The daemon posts /submit
// from Rust, the ingest and triage Actions call the key-protected routes,
// and the dashboard is this origin's own pages behind a session cookie, so
// no answer sends `Access-Control-Allow-Origin`. public/_headers gives the
// dashboard's static files the same set.

/**
 * Headers every answer carries: no other origin may embed it (CORP), share
 * a browsing context group with it (COOP) or frame it, and the dashboard's
 * pages load only resources that opt in (COEP; they load only their own).
 * `X-XSS-Protection: 0` turns off old browsers' XSS filter, which could leak
 * page content; the dashboard's CSP is what stops injected script.
 */
export const CLOSED_HEADERS: Readonly<Record<string, string>> = {
  "Cross-Origin-Resource-Policy": "same-origin",
  "Cross-Origin-Opener-Policy": "same-origin",
  "Cross-Origin-Embedder-Policy": "require-corp",
  "X-Frame-Options": "DENY",
  "X-Content-Type-Options": "nosniff",
  "Origin-Agent-Cluster": "?1",
  "X-Permitted-Cross-Domain-Policies": "none",
  "X-XSS-Protection": "0",
};

/** CORS grants no answer may carry, removed if a handler ever set one. */
const CORS_GRANTS = [
  "Access-Control-Allow-Origin",
  "Access-Control-Allow-Credentials",
  "Access-Control-Allow-Methods",
  "Access-Control-Allow-Headers",
  "Access-Control-Expose-Headers",
] as const;

/**
 * Gives an answer the closed cross-origin headers and strips any CORS grant.
 * A WebSocket upgrade (101) passes through untouched.
 *
 * @param response - The answer from a handler.
 * @returns The same answer with {@link CLOSED_HEADERS} set.
 */
export function withCrossOriginPolicy(response: Response): Response {
  if (response.status === 101) return response;
  const out = new Response(response.body, response);
  for (const [name, value] of Object.entries(CLOSED_HEADERS)) out.headers.set(name, value);
  for (const name of CORS_GRANTS) out.headers.delete(name);
  return out;
}
