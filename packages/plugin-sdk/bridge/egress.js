/**
 * Domain-allowlisted egress for plugin `fetch()` and TCP `connect()`.
 *
 * Bound as the plugin's `globalOutbound`. Policy JSON is injected by
 * bookclerk-workerd (`mode`, `domains`, `maxRedirects`, `subrequests`,
 * `allowUndeclaredPublicRedirects`, `tcp`, `addressCidrs`).
 *
 * - Initial (hop 0) hosts must match `domains` (IDNA ASCII; reject `%` / non-ASCII).
 * - Redirect hops must also match `domains` unless `allowUndeclaredPublicRedirects`
 *   is set. That flag never grants loopback, RFC1918, link-local, or metadata.
 * - Workerd `internet` `allow = ["public", …cidrs]` is the resolved-address
 *   layer (SSRF / DNS rebinding). This script is hostname policy.
 * - TCP `connect` requires a matching `tcp[]` host+port grant. Fetch grants
 *   do not imply TCP. Author isolates keep using `import { connect } from
 *   "cloudflare:sockets"`; this worker is the gateway, not a polyfill.
 * - When `policy.subrequests` is a finite number, each network hop in **this**
 *   egress invocation counts toward the budget.
 * - Redirect method/body and Authorization stripping follow the Fetch
 *   HTTP-redirect fetch algorithm.
 */

import { connect } from "cloudflare:sockets";

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

function parseIpv4(host) {
  const h = normalizeHostToken(host);
  if (h == null) return null;
  const v4 = h.match(/^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/);
  if (!v4) return null;
  const o = v4.slice(1).map((n) => Number(n));
  if (o.some((n) => n > 255)) return null;
  return (o[0] << 24) | (o[1] << 16) | (o[2] << 8) | o[3];
}

function isPublicIpv4(addr) {
  const a = (addr >>> 24) & 0xff;
  const b = (addr >>> 16) & 0xff;
  if (a === 0 || a === 10 || a === 127) return false;
  if (a === 172 && b >= 16 && b <= 31) return false;
  if (a === 192 && b === 168) return false;
  if (a === 169 && b === 254) return false;
  if (a === 100 && b >= 64 && b <= 127) return false;
  if (a === 198 && (b === 18 || b === 19)) return false;
  return true;
}

function cidrContainsIpv4(cidr, addr) {
  const parts = String(cidr).split("/");
  const net = parseIpv4(parts[0]);
  if (net == null) return false;
  const prefix = parts.length > 1 ? Number(parts[1]) : 32;
  if (!Number.isInteger(prefix) || prefix < 0 || prefix > 32) return false;
  const mask = prefix === 0 ? 0 : (0xffffffff << (32 - prefix)) >>> 0;
  return ((addr >>> 0) & mask) === ((net >>> 0) & mask);
}

function ipv4Granted(addr, policy) {
  if (isPublicIpv4(addr)) return true;
  const cidrs = policy.addressCidrs || [];
  return cidrs.some((c) => cidrContainsIpv4(c, addr));
}

function isRestrictedHostname(host) {
  const h = normalizeHostToken(host);
  if (h == null) return true;
  return (
    h === "localhost" ||
    h === "localhost.localdomain" ||
    h.endsWith(".localhost") ||
    h === "metadata.google.internal"
  );
}

function hostnameAddressOk(host, policy) {
  const cidrs = policy.addressCidrs || [];
  if (isRestrictedHostname(host)) {
    const loop4 = parseIpv4("127.0.0.1");
    return cidrs.some((c) => cidrContainsIpv4(c, loop4) || String(c).startsWith("::1"));
  }
  const addr = parseIpv4(host);
  if (addr != null) {
    return ipv4Granted(addr, policy);
  }
  if (normalizeHostToken(host) === "::1") {
    return cidrs.some((c) => String(c) === "::1/128" || String(c) === "::1");
  }
  return true;
}

function allowsInitial(host, policy) {
  if (policy.mode !== "outbound") return false;
  if (!hostnameAddressOk(host, policy)) return false;
  const normalized = normalizeHostToken(host);
  if (normalized == null) return false;
  return (policy.domains || []).some((d) => hostMatches(normalized, d));
}

function allowsRedirect(host, hop, policy, maxRedirects) {
  if (policy.mode !== "outbound") return false;
  if (hop >= maxRedirects) return false;
  if (!hostnameAddressOk(host, policy)) return false;
  if (policy.allowUndeclaredPublicRedirects) {
    return true;
  }
  const normalized = normalizeHostToken(host);
  if (normalized == null) return false;
  return (policy.domains || []).some((d) => hostMatches(normalized, d));
}

function allowsTcp(host, port, policy) {
  if (policy.mode !== "outbound") return false;
  if (!hostnameAddressOk(host, policy)) return false;
  const normalized = normalizeHostToken(host);
  if (normalized == null) return false;
  const grants = policy.tcp || [];
  return grants.some(
    (g) => hostMatches(normalized, g.host) && Array.isArray(g.ports) && g.ports.includes(port),
  );
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

function parseAuthority(authority) {
  if (typeof authority !== "string" || !authority) {
    return { hostname: "", port: 0 };
  }
  if (authority.startsWith("[")) {
    const end = authority.indexOf("]");
    if (end === -1) return { hostname: authority, port: 0 };
    const hostname = authority.slice(1, end);
    const rest = authority.slice(end + 1);
    const port = rest.startsWith(":") ? Number(rest.slice(1)) : 0;
    return { hostname, port };
  }
  const idx = authority.lastIndexOf(":");
  if (idx === -1) return { hostname: authority, port: 0 };
  return {
    hostname: authority.slice(0, idx),
    port: Number(authority.slice(idx + 1)),
  };
}

/** Workerd `json` bindings arrive as objects; keep string parse for tests. */
function loadPolicy(raw) {
  if (raw == null || raw === "") return {};
  if (typeof raw === "string") {
    return JSON.parse(raw);
  }
  if (typeof raw === "object") return raw;
  throw new Error("invalid egress policy type");
}

export default {
  async fetch(request, env) {
    let policy;
    try {
      policy = loadPolicy(env.EGRESS_POLICY);
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
      } else if (!allowsRedirect(url.hostname, hop, policy, maxRedirects)) {
        return new Response(
          hop >= maxRedirects
            ? "too many redirects"
            : `redirect host \`${url.hostname}\` is not permitted`,
          { status: hop >= maxRedirects ? 508 : 403 },
        );
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

  /**
   * Inbound CONNECT from the plugin isolate's `cloudflare:sockets` `connect()`.
   *
   * `socket.opened.localAddress` is the CONNECT authority (`host:port`) the
   * peer targeted. Dial the destination through this worker's `internet`
   * service after TCP policy checks, then splice streams.
   */
  async connect(socket, env) {
    let policy;
    try {
      policy = loadPolicy(env.EGRESS_POLICY);
    } catch {
      throw new Error("invalid egress policy");
    }
    if (policy.mode === "deny") {
      throw new Error("network denied by plugin capabilities");
    }
    const opened = await socket.opened;
    const authority = (opened && opened.localAddress) || "";
    const parsed = parseAuthority(authority);
    const hostname = parsed.hostname;
    const port = parsed.port;
    if (!allowsTcp(hostname, port, policy)) {
      try {
        await socket.close();
      } catch {
        /* already closed */
      }
      throw new Error(
        `tcp connect \`${hostname}:${port}\` is not in capabilities.network.tcp`,
      );
    }
    const outbound = connect({ hostname, port });
    await outbound.opened;
    await Promise.all([
      socket.readable.pipeTo(outbound.writable),
      outbound.readable.pipeTo(socket.writable),
    ]);
  },
};
