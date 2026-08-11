/**
 * Domain-allowlisted egress for plugin `fetch()`.
 *
 * Bound as the plugin's `globalOutbound`. Policy JSON is injected by
 * bookclerk-workerd (`mode`, `domains`, `maxRedirects`, optional `subrequests`).
 *
 * - Initial (hop 0) hosts must match `domains` (IDNA ASCII; reject `%` / non-ASCII).
 * - Redirect hops after an allowed initial host are followed without re-allowlisting
 *   (intentional — hops stay free so storefront APIs can bounce across CDNs).
 * - When `policy.subrequests` is a finite number, each network hop in **this**
 *   egress invocation (the plugin's one `fetch()` plus its redirect chain)
 *   counts toward the budget; exceeding it returns 429. The counter is
 *   intentionally local to the invocation — Cloudflare Workers also budget
 *   subrequests per invocation, not per isolate lifetime. Aggregating across
 *   multiple plugin `fetch()` calls inside one host RPC needs a defined
 *   invocation unit comparable to CF's request model (deferred).
 * - Redirect method/body and Authorization stripping follow the Fetch
 *   HTTP-redirect fetch algorithm (https://fetch.spec.whatwg.org/#http-redirect-fetch):
 *   - 301/302 + POST → GET with null body
 *   - 303 + non-GET/HEAD → GET with null body
 *   - 307/308 preserve method/body
 *   - Cross-origin redirects drop Authorization (CORS non-wildcard request-header)
 * - Defense in depth: also drop Cookie / Cookie2 / Proxy-Authorization on
 *   cross-origin hops (plugin bridges forward caller headers explicitly).
 * - AbortSignal and other RequestInit metadata survive redirects.
 */

const CREDENTIAL_HEADERS = [
  "authorization",
  "cookie",
  "cookie2",
  "proxy-authorization",
];

/** Fetch request-body-header names — removed when method becomes GET. */
const REQUEST_BODY_HEADERS = [
  "content-encoding",
  "content-language",
  "content-location",
  "content-type",
];

function hostMatches(host, pattern) {
  const h = normalizeHostToken(host);
  const p = normalizeHostToken(pattern);
  if (h == null || p == null) return false;
  if (p.startsWith("*.")) {
    const suffix = p.slice(1); // ".example.com"
    return h.endsWith(suffix) || h === p.slice(2);
  }
  return h === p;
}

/** Trim, strip trailing dots, lowercase. Reject `%` and non-ASCII (fail closed). */
function normalizeHostToken(host) {
  if (typeof host !== "string") return null;
  let h = host.trim();
  while (h.endsWith(".")) h = h.slice(0, -1);
  h = h.toLowerCase();
  if (!h || h.includes("%")) return null;
  for (let i = 0; i < h.length; i++) {
    if (h.charCodeAt(i) > 0x7f) return null;
  }
  return h;
}

function allowsInitial(host, policy) {
  if (policy.mode !== "outbound") return false;
  const normalized = normalizeHostToken(host);
  if (normalized == null) return false;
  return (policy.domains || []).some((d) => hostMatches(normalized, d));
}

function sameOrigin(a, b) {
  return (
    a.protocol === b.protocol &&
    a.hostname === b.hostname &&
    a.port === b.port
  );
}

/**
 * Build the next request for a redirect response.
 *
 * Aligns with Fetch HTTP-redirect fetch for method/body and Authorization
 * stripping, while preserving AbortSignal and other RequestInit fields that
 * `new Request(url, init)` would otherwise drop when init is rebuilt.
 */
function redirectRequest(current, nextUrl, status) {
  const currentUrl = new URL(current.url);
  const crossOrigin = !sameOrigin(currentUrl, nextUrl);
  const method = (current.method || "GET").toUpperCase();

  // Fetch: 301/302 switch only POST → GET; 303 switches any non-GET/HEAD → GET.
  const switchToGet =
    (status === 301 || status === 302) && method === "POST"
      ? true
      : status === 303 && method !== "GET" && method !== "HEAD";

  let nextMethod = method;
  let body = null;
  if (switchToGet) {
    nextMethod = "GET";
    body = null;
  } else if (status === 307 || status === 308) {
    nextMethod = method;
    body = method === "GET" || method === "HEAD" ? null : current.body;
  } else {
    // 301/302 with non-POST (e.g. PUT) — Fetch preserves method; no body replay
    // unless 307/308. Body source for non-POST on 301/302 is not replayed here.
    nextMethod = method;
    body = null;
  }

  const headers = new Headers(current.headers);
  if (switchToGet) {
    for (const name of REQUEST_BODY_HEADERS) {
      headers.delete(name);
    }
  }
  if (crossOrigin) {
    for (const name of CREDENTIAL_HEADERS) {
      headers.delete(name);
    }
  }

  const init = {
    method: nextMethod,
    headers,
    redirect: "manual",
    // Preserve caller metadata across hops (Fetch keeps these on the request).
    signal: current.signal,
    mode: current.mode,
    credentials: current.credentials,
    cache: current.cache,
    referrer: current.referrer,
    referrerPolicy: current.referrerPolicy,
    integrity: current.integrity,
    keepalive: current.keepalive,
  };
  if (current.cf !== undefined) {
    init.cf = current.cf;
  }
  if (body != null) {
    init.body = body;
    // Required when body is a ReadableStream in some runtimes.
    if (typeof body === "object" && body !== null && "getReader" in body) {
      init.duplex = "half";
    }
  }
  return new Request(nextUrl, init);
}

/** Finite non-negative number → enforce; otherwise treat as unlimited. */
function subrequestBudget(policy) {
  const n = policy.subrequests;
  if (typeof n !== "number" || !Number.isFinite(n) || n < 0) return null;
  return n;
}

export default {
  async fetch(request, env) {
    let policy;
    try {
      policy = JSON.parse(env.EGRESS_POLICY || "{}");
    } catch {
      return new Response("invalid egress policy", { status: 500 });
    }

    if (policy.mode === "deny") {
      return new Response("network denied by plugin capabilities", {
        status: 403,
      });
    }

    const maxRedirects = Number(policy.maxRedirects ?? 10);
    const subrequestLimit = subrequestBudget(policy);
    // Per egress invocation only (see file header). Do not hoist to module
    // scope: that would be isolate-lifetime budgeting, unlike Cloudflare.
    let subrequestCount = 0;
    let current = request;
    let hop = 0;

    while (true) {
      const url = new URL(current.url);
      if (hop === 0) {
        if (!allowsInitial(url.hostname, policy)) {
          return new Response(
            `host \`${url.hostname}\` not in capabilities.network.domains`,
            { status: 403 },
          );
        }
      } else if (hop >= maxRedirects) {
        return new Response("too many redirects", { status: 508 });
      }

      if (subrequestLimit != null && subrequestCount >= subrequestLimit) {
        return new Response(
          `subrequest limit exceeded (${subrequestLimit}); counted per egress invocation (one plugin fetch + redirect hops)`,
          { status: 429 },
        );
      }

      // Clone when the body may need replaying on 307/308.
      const method = (current.method || "GET").toUpperCase();
      const mayReplayBody = method !== "GET" && method !== "HEAD";
      const toFetch = mayReplayBody ? current.clone() : current;

      if (subrequestLimit != null) {
        subrequestCount += 1;
      }
      const response = await fetch(toFetch, { redirect: "manual" });
      if (response.status >= 300 && response.status < 400) {
        const location = response.headers.get("location");
        if (!location) return response;
        hop += 1;
        const nextUrl = new URL(location, current.url);
        current = redirectRequest(current, nextUrl, response.status);
        continue;
      }
      return response;
    }
  },
};
