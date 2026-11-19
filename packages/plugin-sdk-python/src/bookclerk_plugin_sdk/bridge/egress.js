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
 * - Redirect hops must also match `domains` unless `allowUndeclaredPublicRedirects`
 *   is set. That flag never grants loopback, RFC1918, link-local, or metadata.
 * - Workerd `internet` `allow = ["public", …cidrs]` is the resolved-address
 *   layer (SSRF / DNS rebinding). This script is hostname policy.
 * - Hosts are judged as the URL parser prints them (`connect()` authorities
 *   go through `canonicalSocketHost` first), and the checks mirror the Rust
 *   `EgressPolicy` in `bookclerk-plugin-manifest`: IP literals are classified
 *   by range and need a covering `addressCidrs` grant unless public, and
 *   `localhost` / `metadata.google.internal` need a grant for the address
 *   they resolve to. The `egress_bridge` fuzz target runs these functions
 *   against the Rust policy, which must allow everything this script allows.
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

/** The `domains` entry that grants every public hostname. */
const ANY_PUBLIC_HOST = "*";

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

/**
 * Dotted-decimal IPv4 as the URL parser and the host print it (four parts,
 * no leading zeros) → its 32-bit value, else null. Legacy spellings (`127.1`,
 * hex, octal) are not parsed here: the URL parser has already rewritten them
 * for fetch, `canonicalSocketHost` for `connect()`, and anything else that
 * looks numeric is refused by `classifyHost`.
 */
function parseIpv4(text) {
  if (typeof text !== "string") return null;
  const parts = text.split(".");
  if (parts.length !== 4) return null;
  let addr = 0;
  for (const part of parts) {
    if (!/^(0|[1-9][0-9]{0,2})$/.test(part)) return null;
    const n = Number(part);
    if (n > 255) return null;
    addr = addr * 256 + n;
  }
  return addr;
}

/**
 * RFC 4291 text IPv6 (one `::` at most, an optional dotted IPv4 tail, no
 * zone) → its eight 16-bit groups, else null.
 */
function parseIpv6(text) {
  if (typeof text !== "string" || !text.includes(":") || text.length > 45) return null;
  const split = text.split("::");
  if (split.length > 2) return null;
  const groups = (part) => (part === "" ? [] : part.split(":"));
  const head = groups(split[0]);
  const tail = split.length === 2 ? groups(split[1]) : [];
  const words = [];
  const read = (list) => {
    for (let i = 0; i < list.length; i++) {
      const g = list[i];
      if (i === list.length - 1 && g.includes(".") && list === lastList) {
        const v4 = parseIpv4(g);
        if (v4 == null) return false;
        words.push(Math.floor(v4 / 65536), v4 % 65536);
        continue;
      }
      if (!/^[0-9a-f]{1,4}$/i.test(g)) return false;
      words.push(parseInt(g, 16));
    }
    return true;
  };
  const lastList = split.length === 2 ? tail : head;
  if (!read(head)) return null;
  const headLen = words.length;
  if (!read(tail)) return null;
  if (split.length === 1) return words.length === 8 ? words : null;
  if (words.length > 7) return null;
  const zeros = new Array(8 - words.length).fill(0);
  return [...words.slice(0, headLen), ...zeros, ...words.slice(headLen)];
}

/**
 * An address in the one form the checks below compare: IPv4 as
 * `{ v4: number }`, IPv6 as `{ v6: number[8] }`, IPv4-mapped IPv6
 * (`::ffff:a.b.c.d`) unwrapped to the IPv4 it carries.
 */
function canonicalIp(ip) {
  if (ip.v6) {
    const w = ip.v6;
    if (w[0] === 0 && w[1] === 0 && w[2] === 0 && w[3] === 0 && w[4] === 0 && w[5] === 0xffff) {
      return { v4: w[6] * 65536 + w[7] };
    }
  }
  return ip;
}

/** IPv4 outside every special-use range the host refuses without a CIDR grant. */
function isPublicIpv4(addr) {
  const a = Math.floor(addr / 16777216);
  const b = Math.floor(addr / 65536) % 256;
  const c = Math.floor(addr / 256) % 256;
  if (a === 0 || a === 10 || a === 127) return false;
  if (a === 172 && b >= 16 && b <= 31) return false;
  if (a === 192 && b === 168) return false;
  if (a === 169 && b === 254) return false;
  if (a === 100 && b >= 64 && b <= 127) return false;
  if (a === 198 && (b === 18 || b === 19)) return false;
  if (a >= 224 && a <= 239) return false; // multicast
  if (addr === 0xffffffff) return false; // broadcast
  if (a === 192 && b === 0 && c === 2) return false; // documentation
  if (a === 198 && b === 51 && c === 100) return false;
  if (a === 203 && b === 0 && c === 113) return false;
  return true;
}

/** IPv6 that is not unspecified, loopback, multicast, ULA, link-local or documentation. */
function isPublicIpv6(w) {
  if (w.every((g) => g === 0)) return false;
  if (w.slice(0, 7).every((g) => g === 0) && w[7] === 1) return false;
  if (w[0] >= 0xff00) return false;
  if ((w[0] & 0xfe00) === 0xfc00) return false;
  if ((w[0] & 0xffc0) === 0xfe80) return false;
  if (w[0] === 0x2001 && w[1] === 0xdb8) return false;
  return true;
}

/** `addr/prefix` or a bare address (a host route) → `{ ip, prefix }`, else null. */
function parseCidr(raw) {
  if (typeof raw !== "string") return null;
  const text = raw.trim();
  const slash = text.indexOf("/");
  const addrText = slash === -1 ? text : text.slice(0, slash);
  const v4 = parseIpv4(addrText);
  const v6 = v4 == null ? parseIpv6(addrText) : null;
  if (v4 == null && v6 == null) return null;
  const max = v4 != null ? 32 : 128;
  let prefix = max;
  if (slash !== -1) {
    const p = text.slice(slash + 1);
    if (!/^\+?[0-9]{1,3}$/.test(p)) return null;
    prefix = Number(p);
    if (prefix > max) return null;
  }
  return { ip: v4 != null ? { v4 } : { v6 }, prefix };
}

/** Whether the first `prefix` bits of two 16-bit group lists agree. */
function groupsPrefixEqual(a, b, prefix) {
  let left = prefix;
  for (let i = 0; i < a.length && left > 0; i++) {
    const bits = Math.min(16, left);
    const shift = 16 - bits;
    if (a[i] >> shift !== b[i] >> shift) return false;
    left -= bits;
  }
  return true;
}

/** Whether a CIDR grant covers an address (both canonicalized first). */
function cidrContains(grant, ip) {
  const net = canonicalIp(grant.ip);
  const addr = canonicalIp(ip);
  if (net.v4 != null && addr.v4 != null) {
    const p = Math.min(grant.prefix, 32);
    const toGroups = (n) => [Math.floor(n / 65536), n % 65536];
    return groupsPrefixEqual(toGroups(net.v4), toGroups(addr.v4), p);
  }
  if (net.v6 && addr.v6) return groupsPrefixEqual(net.v6, addr.v6, Math.min(grant.prefix, 128));
  return false;
}

/** The public Internet, or an address an `addressCidrs` grant covers. */
function addressAllowed(ip, policy) {
  const addr = canonicalIp(ip);
  if (addr.v4 != null ? isPublicIpv4(addr.v4) : isPublicIpv6(addr.v6)) return true;
  return (policy.addressCidrs || []).some((raw) => {
    const grant = parseCidr(raw);
    return grant != null && cidrContains(grant, addr);
  });
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

/**
 * A restricted name passes only with a grant for the address it resolves
 * to: the metadata name needs `169.254.169.254`, the loopback names need
 * `127.0.0.1` or `::1`. Any other name is the resolved-address layer's call.
 */
function restrictedNameOk(name, policy) {
  if (name === "metadata.google.internal") {
    return addressAllowed({ v4: 169 * 16777216 + 254 * 65536 + 169 * 256 + 254 }, policy);
  }
  if (name === "localhost" || name === "localhost.localdomain" || name.endsWith(".localhost")) {
    return (
      addressAllowed({ v4: 127 * 16777216 + 1 }, policy) ||
      addressAllowed({ v6: [0, 0, 0, 0, 0, 0, 0, 1] }, policy)
    );
  }
  return true;
}

/**
 * The host a gate judges: `{ ip, text }` for an address (IPv6 without its
 * brackets), `{ name }` for a hostname, or null (refused). Hosts arrive as
 * the URL parser or `canonicalSocketHost` printed them; any other numeric
 * spelling, and anything `normalizeHostToken` refuses, fails closed.
 */
function classifyHost(host) {
  const token = normalizeHostToken(host);
  if (token == null) return null;
  const bare = token.startsWith("[") && token.endsWith("]") ? token.slice(1, -1) : token;
  const v4 = parseIpv4(bare);
  if (v4 != null) return { ip: { v4 }, text: bare };
  const v6 = parseIpv6(bare);
  if (v6 != null) return { ip: { v6 }, text: bare };
  if (isIpLiteral(bare)) return null;
  return { name: bare };
}

function allowsInitial(host, policy) {
  if (policy.mode !== "outbound") return false;
  const target = classifyHost(host);
  if (target == null) return false;
  if (target.ip) {
    if (!addressAllowed(target.ip, policy)) return false;
  } else if (isRestrictedHostname(target.name) && !restrictedNameOk(target.name, policy)) {
    return false;
  }
  const text = target.ip ? target.text : target.name;
  return (policy.domains || []).some((d) => hostMatches(text, d));
}

function allowsRedirect(host, hop, policy, maxRedirects) {
  if (policy.mode !== "outbound") return false;
  if (hop >= maxRedirects) return false;
  const target = classifyHost(host);
  if (target == null) return false;
  if (target.ip) {
    return (
      addressAllowed(target.ip, policy) &&
      (policy.allowUndeclaredPublicRedirects === true ||
        (policy.domains || []).some((d) => hostMatches(target.text, d)))
    );
  }
  const restricted = isRestrictedHostname(target.name);
  if (restricted && !restrictedNameOk(target.name, policy)) return false;
  if (policy.allowUndeclaredPublicRedirects === true) {
    // A restricted name is never an undeclared public destination.
    return !restricted;
  }
  return (policy.domains || []).some((d) => hostMatches(target.name, d));
}

function allowsTcp(host, port, policy) {
  if (policy.mode !== "outbound") return false;
  const target = classifyHost(host);
  if (target == null) return false;
  if (target.ip) {
    if (!addressAllowed(target.ip, policy)) return false;
  } else if (isRestrictedHostname(target.name) && !restrictedNameOk(target.name, policy)) {
    return false;
  }
  const text = target.ip ? target.text : target.name;
  const grants = policy.tcp || [];
  return grants.some(
    (g) =>
      g.host !== ANY_PUBLIC_HOST &&
      hostMatches(text, g.host) &&
      Array.isArray(g.ports) &&
      g.ports.includes(port),
  );
}

/**
 * The hostname a `connect()` authority names, printed the way the URL parser
 * prints a fetch hostname (legacy IPv4 spellings rewritten, IPv6 compressed
 * and bracketed, names IDNA-mapped and lowercased), so one set of checks
 * judges both. Only ASCII letters, digits, `.`, `-`, `_` and `:` are
 * accepted first: the URL parser would percent-decode `%` and read `@`, `/`
 * or `\` as structure, which would judge a different host than the one
 * dialled. Null when the authority names no valid host.
 */
function canonicalSocketHost(hostname) {
  if (typeof hostname !== "string" || !/^[A-Za-z0-9._:-]+$/.test(hostname)) return null;
  const literal = hostname.includes(":") ? `[${hostname}]` : hostname;
  try {
    return new URL(`http://${literal}/`).hostname;
  } catch {
    return null;
  }
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
    const port = parsed.port;
    // Judge and dial the canonical host, so the check and the socket agree
    // on which host it is.
    const hostname = canonicalSocketHost(parsed.hostname);
    if (hostname == null || !allowsTcp(hostname, port, policy)) {
      try {
        await socket.close();
      } catch {
        /* already closed */
      }
      throw new Error(
        `tcp connect \`${parsed.hostname}:${port}\` is not in capabilities.network.tcp`,
      );
    }
    // `connect()` takes IPv6 without the URL form's brackets.
    const dial = hostname.startsWith("[") ? hostname.slice(1, -1) : hostname;
    const outbound = connect({ hostname: dial, port });
    await outbound.opened;
    await Promise.all([
      socket.readable.pipeTo(outbound.writable),
      outbound.readable.pipeTo(socket.writable),
    ]);
  },
};
