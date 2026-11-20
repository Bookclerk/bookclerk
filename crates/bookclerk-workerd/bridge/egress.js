/**
 * Domain-allowlisted egress for plugin `fetch()` and TCP `connect()`.
 *
 * Bound as the plugin's `globalOutbound`. Policy JSON is injected by
 * bookclerk-workerd (`mode`, `domains`, `maxRedirects`, `subrequests`,
 * `allowUndeclaredPublicRedirects`, `tcp`, `addressCidrs`).
 *
 * - Initial (hop 0) hosts must match `domains` (IDNA ASCII; reject `%` / non-ASCII).
 *   A bare `*` entry matches every hostname but never an IP literal; it is a
 *   fetch grant only and never matches a `tcp[]` grant.
 * - A `domains` entry may end in `:port` (`abs.example.com:13378`,
 *   `[2606:4700::1]:9000`); it then grants that port only. An entry with no
 *   port grants 80 and 443. The URL's effective port (its scheme default when
 *   it names none) must be granted.
 * - Redirect hops must also match `domains` unless `allowUndeclaredPublicRedirects`
 *   is set. That flag never grants loopback, RFC1918, link-local, or metadata,
 *   and an undeclared hop stays on ports 80 and 443.
 * - The host's CONNECT proxy is the resolved-address layer (SSRF / DNS
 *   rebinding): it resolves each tunnel, checks every address and dials the
 *   checked one. This script is hostname policy. A local-network IPv4
 *   literal listed in `localNames` (a setting named it) passes here.
 * - TCP `connect` requires a matching `tcp[]` host+port grant. Fetch grants
 *   do not imply TCP. Author isolates keep using `import { connect } from
 *   "cloudflare:sockets"`; this worker is the gateway, not a polyfill.
 * - When `policy.subrequests` is a finite number, each network hop in **this**
 *   egress invocation counts toward the budget.
 * - Redirect method/body and Authorization stripping follow the Fetch
 *   HTTP-redirect fetch algorithm.
 * - Every fetch hop carries the host's honest `User-Agent` (`userAgent`,
 *   `bookclerk/<version> (+https://bookclerk.app)`). The guest's own agent
 *   is kept only on hosts matching `ownUserAgentHosts`, which the operator
 *   approved on the consent form. A `*` entry never matches.
 */

import { connect } from "cloudflare:sockets";

const CREDENTIAL_HEADERS = ["authorization", "cookie", "cookie2", "proxy-authorization"];

/** Fetch request-body-header names — removed when method becomes GET. */
const REQUEST_BODY_HEADERS = [
  "content-encoding",
  "content-language",
  "content-location",
  "content-type",
];

/** The `domains` entry that grants every public hostname. */
const ANY_PUBLIC_HOST = "*";

/** Ports a `domains` entry that names no port grants. */
const FETCH_DEFAULT_PORTS = [80, 443];

/** Agent sent when the injected policy names none (SDK dev harnesses). */
const FALLBACK_USER_AGENT = "bookclerk (+https://bookclerk.app)";

function hostMatches(host, pattern) {
  const h = normalizeHostToken(host);
  const p = normalizeHostToken(pattern);
  if (h == null || p == null) return false;
  if (p === ANY_PUBLIC_HOST) return !isIpLiteral(h);
  if (p.startsWith("*.")) {
    const suffix = p.slice(1); // ".example.com"
    return h.endsWith(suffix) || h === p.slice(2);
  }
  return h === p;
}

/** Canonical decimal port (digits, no leading zero, 1-65535), else null. */
function parseCanonicalPort(text) {
  if (!/^[1-9][0-9]{0,4}$/.test(text)) return null;
  const port = Number(text);
  return port <= 65535 ? port : null;
}

/**
 * Splits a `domains` entry into `{ host, port }` (`port` null when the entry
 * names none). `[v6]` and `[v6]:port` carry IPv6 literals; a bare IPv6
 * literal has several colons and no port. Null when malformed.
 */
function splitEntryPort(entry) {
  if (typeof entry !== "string") return null;
  const e = entry.trim();
  if (e.startsWith("[")) {
    const end = e.indexOf("]");
    if (end < 0) return null;
    const host = e.slice(1, end);
    const after = e.slice(end + 1);
    if (after === "") return { host, port: null };
    if (!after.startsWith(":")) return null;
    const port = parseCanonicalPort(after.slice(1));
    return port == null ? null : { host, port };
  }
  const colon = e.lastIndexOf(":");
  if (colon >= 0 && !e.slice(0, colon).includes(":")) {
    const port = parseCanonicalPort(e.slice(colon + 1));
    return port == null ? null : { host: e.slice(0, colon), port };
  }
  return { host: e, port: null };
}

/** The URL's port, or its scheme default (80 for http, 443 for https). */
function effectivePort(url) {
  if (url.port) return Number(url.port);
  if (url.protocol === "http:") return 80;
  if (url.protocol === "https:") return 443;
  return null;
}

/** `[v6]` → `v6`, so a URL hostname compares with a stored IPv6 entry. */
function unbracketed(host) {
  return host.startsWith("[") && host.endsWith("]") ? host.slice(1, -1) : host;
}

/** True when some `domains` entry covers `host` and grants `port`. */
function domainGrants(host, port, policy) {
  const normalized = normalizeHostToken(host);
  if (normalized == null || port == null) return false;
  const bare = unbracketed(normalized);
  return (policy.domains || []).some((d) => {
    const entry = splitEntryPort(d);
    if (entry == null || !hostMatches(bare, entry.host)) return false;
    return entry.port == null ? FETCH_DEFAULT_PORTS.includes(port) : entry.port === port;
  });
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

/**
 * True for an IPv6 literal or a name whose last label is numeric (dotted,
 * decimal, hex or octal IPv4 spellings), which `*` never matches.
 */
function isIpLiteral(host) {
  if (host.includes(":") || host.startsWith("[")) return true;
  const last = host.slice(host.lastIndexOf(".") + 1);
  return /^(0x[0-9a-f]*|[0-9]+)$/.test(last);
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

/** RFC 1918 or RFC 6598 shared space: a LAN address a setting may name. */
function isLocalNetworkIpv4(addr) {
  const a = (addr >>> 24) & 0xff;
  const b = (addr >>> 16) & 0xff;
  return (
    a === 10 ||
    (a === 172 && b >= 16 && b <= 31) ||
    (a === 192 && b === 168) ||
    (a === 100 && b >= 64 && b <= 127)
  );
}

/**
 * True when an IPv4 literal may be requested: public, inside a CIDR grant,
 * or a local-network literal a setting named (`localNames`), which grants
 * exactly that address.
 */
function ipv4Granted(addr, policy) {
  if (isPublicIpv4(addr)) return true;
  const cidrs = policy.addressCidrs || [];
  if (cidrs.some((c) => cidrContainsIpv4(c, addr))) return true;
  return (
    isLocalNetworkIpv4(addr) && (policy.localNames || []).some((n) => parseIpv4(String(n)) === addr)
  );
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

function allowsInitial(host, port, policy) {
  if (policy.mode !== "outbound") return false;
  if (!hostnameAddressOk(host, policy)) return false;
  return domainGrants(host, port, policy);
}

function allowsRedirect(host, port, hop, policy, maxRedirects) {
  if (policy.mode !== "outbound") return false;
  if (hop >= maxRedirects) return false;
  if (!hostnameAddressOk(host, policy)) return false;
  if (domainGrants(host, port, policy)) return true;
  return Boolean(policy.allowUndeclaredPublicRedirects) && FETCH_DEFAULT_PORTS.includes(port);
}

function allowsTcp(host, port, policy) {
  if (policy.mode !== "outbound") return false;
  if (!hostnameAddressOk(host, policy)) return false;
  const normalized = normalizeHostToken(host);
  if (normalized == null) return false;
  const grants = policy.tcp || [];
  return grants.some(
    (g) =>
      g.host !== ANY_PUBLIC_HOST &&
      hostMatches(normalized, g.host) &&
      Array.isArray(g.ports) &&
      g.ports.includes(port),
  );
}

/**
 * True when the guest's own `User-Agent` may go to `host`.
 *
 * Host and patterns are normalized once and compared as they are, never
 * through `hostMatches` (which normalizes again), so a padded or dotted `*`
 * never becomes a match-everything pattern.
 */
function keepsOwnUserAgent(host, policy) {
  const h = normalizeHostToken(host);
  if (h == null) return false;
  return (policy.ownUserAgentHosts || []).some((p) => {
    const pattern = normalizeHostToken(p);
    if (pattern == null || pattern === ANY_PUBLIC_HOST) return false;
    if (pattern.startsWith("*.")) {
      const suffix = pattern.slice(2);
      return h === suffix || h.endsWith(`.${suffix}`);
    }
    return h === pattern;
  });
}

/**
 * The request to send for one hop, with the `User-Agent` the policy allows:
 * the guest's own on an approved host, otherwise the host's agent.
 */
function withUserAgent(request, hostname, policy) {
  if (request.headers.has("user-agent") && keepsOwnUserAgent(hostname, policy)) {
    return request;
  }
  const agent =
    typeof policy.userAgent === "string" && policy.userAgent
      ? policy.userAgent
      : FALLBACK_USER_AGENT;
  const headers = new Headers(request.headers);
  headers.set("user-agent", agent);
  return new Request(request, { headers });
}

function sameOrigin(a, b) {
  return a.protocol === b.protocol && a.hostname === b.hostname && a.port === b.port;
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

/**
 * The pending network setting whose destination is `host:port`, if any.
 * Pending settings grant nothing; they only let a refusal name the setting.
 */
function pendingSetting(host, port, policy) {
  const want = unbracketed(String(host)).toLowerCase().replace(/\.$/, "");
  return (policy.pendingSettings || []).find(
    (p) => p && Number(p.port) === port && String(p.host).toLowerCase() === want,
  );
}

/** `host:port` as a refusal names it (IPv6 in brackets). */
function destinationOf(host, port) {
  const bare = unbracketed(String(host));
  return bare.includes(":") ? `[${bare}]:${port}` : `${bare}:${port}`;
}

/**
 * The typed `network_not_granted` refusal: a 403 whose JSON body names the
 * destination and, when a pending network setting names it, that setting.
 * `x-bookclerk-error` repeats the code so a plugin can tell this refusal
 * from an upstream 403.
 */
function notGranted(host, port, policy, fallback) {
  const destination = destinationOf(host, port);
  const pending = pendingSetting(host, port, policy);
  const details = { destination };
  if (pending) details.setting = String(pending.setting);
  const message = pending
    ? `${destination} is named by setting \`${pending.setting}\` and awaits the operator's approval`
    : fallback;
  return new Response(
    JSON.stringify({ error: { code: "network_not_granted", message, details } }),
    {
      status: 403,
      headers: {
        "content-type": "application/json",
        "x-bookclerk-error": "network_not_granted",
      },
    },
  );
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
      const port = effectivePort(url);
      if (hop === 0) {
        if (!allowsInitial(url.hostname, port, policy)) {
          return notGranted(
            url.hostname,
            port,
            policy,
            `host \`${url.host}\` not in capabilities.network.domains`,
          );
        }
      } else if (!allowsRedirect(url.hostname, port, hop, policy, maxRedirects)) {
        if (hop >= maxRedirects) {
          return new Response("too many redirects", { status: 508 });
        }
        return notGranted(
          url.hostname,
          port,
          policy,
          `redirect host \`${url.host}\` is not permitted`,
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
      const toFetch = withUserAgent(
        mayReplayBody ? current.clone() : current,
        url.hostname,
        policy,
      );

      if (subrequestLimit != null) {
        subrequestCount += 1;
      }
      const response = await fetch(toFetch, { redirect: "manual" });
      if (response.status >= 300 && response.status < 400) {
        // `redirect: "manual"` hands the 3xx back so the plugin can vet
        // each hop itself (as on Cloudflare); this gate still checked the
        // host it was sent to.
        if (request.redirect === "manual") return response;
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
      const pending = pendingSetting(hostname, port, policy);
      throw new Error(
        pending
          ? `network_not_granted: ${destinationOf(hostname, port)} is named by setting \`${pending.setting}\` and awaits the operator's approval`
          : `network_not_granted: tcp connect \`${hostname}:${port}\` is not in capabilities.network.tcp`,
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
