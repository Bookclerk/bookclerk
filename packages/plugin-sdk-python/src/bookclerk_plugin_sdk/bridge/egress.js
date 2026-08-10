/**
 * Domain-allowlisted egress for plugin `fetch()`.
 *
 * Bound as the plugin's `globalOutbound`. Policy JSON is injected by
 * bookclerk-workerd (`mode`, `domains`, `maxRedirects`). Redirect hops after an
 * allowed initial host are followed without re-allowlisting.
 */

function hostMatches(host, pattern) {
  const h = host.toLowerCase();
  const p = pattern.toLowerCase();
  if (p.startsWith("*.")) {
    const suffix = p.slice(1); // ".example.com"
    return h.endsWith(suffix) || h === p.slice(2);
  }
  return h === p;
}

function allowsInitial(host, policy) {
  if (policy.mode !== "outbound") return false;
  return (policy.domains || []).some((d) => hostMatches(host, d));
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

      const response = await fetch(current, { redirect: "manual" });
      if (response.status >= 300 && response.status < 400) {
        const location = response.headers.get("location");
        if (!location) return response;
        hop += 1;
        const nextUrl = new URL(location, current.url);
        current = new Request(nextUrl, current);
        continue;
      }
      return response;
    }
  },
};
