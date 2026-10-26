# Capabilities and consent

What a plugin declares it needs (filesystem, network, databases, queues), and how the operator approves it before the plugin is enabled or spawned.

## Declaring what a plugin needs

A manifest declares network and host bindings under **`[capabilities]`** — not
a free-form filesystem widen.

**Workerd** (hostname-filtered `fetch()` plus separate TCP `connect()`):

```toml
runtime = "workerd"
# …

[capabilities.network]
mode = "outbound"
domains = ["api.example.com", "www.example.com"]   # required; isolate fetch allowlist
tcp = [{ host = "db.example.com", ports = [5432] }]  # optional; fetch does not imply TCP
# allow_undeclared_public_redirects = true           # operator-visible; never grants private ranges
address_cidrs = ["10.0.60.100/32"]                   # optional; default is public Internet only
```

**Native** (sibling jail denies ambient `AF_INET`; SDK sockets share the same policy):

```toml
runtime = "native"
command = "./my-plugin"

[capabilities.network]
mode = "outbound"
tcp = [{ host = "api.example.com", ports = [443] }]
# domains are workerd-only — omit them on native plugins.
```

| Network `mode` | Native-behind-workerd | Workerd |
| --- | --- | --- |
| `deny` | sibling guest jail `NetPolicy::Deny`; no socket proxy grants | OS jail stays `OutboundListen` for the RPC bridge; isolate `globalOutbound` → blocked |
| `outbound` | sibling guest jail still `Deny`; TCP via `bookclerk_plugin_sdk::connect` → multiplexed CONNECT on the inherited `fd:`/`handle:` link | isolate `globalOutbound` → egress worker; `fetch()` and `connect()` share one `EgressPolicy` |

`capabilities.network.domains` is the **fetch** allowlist (workerd only). Raw TCP is `capabilities.network.tcp` (`host` + `ports`). The operator may add extra fetch hosts, TCP targets, and CIDRs; the guest `describe()` cannot. Default address-space policy is public Internet only — loopback, RFC1918, link-local, ULA, and metadata stay denied unless an explicit CIDR is granted. `allow_undeclared_public_redirects` lets fetch redirects leave the domain allowlist for **public** destinations only; it never implies those special ranges.

When you need enforceable hostname allowlists for `fetch()`, ship a **workerd** plugin. Native plugins that need networking must use the SDK socket capability.

The native path is exercised end to end on Linux, macOS and Windows by `cargo test -p bookclerk-plugin-e2e --test native_gateway --test native_gateway_isolation --test native_gateway_lifecycle --test native_gateway_authenticated_endpoint`: a granted loopback port round-trips through the proxy, an ungranted live port is refused with `403`, a direct socket from the sibling jail is blocked, both pids and the host session directory go away on drop, and concurrent sessions cannot read each other's gateway state (see [ci.md](../contributing/ci.md#native-behind-workerd-gateway-smoke)).

Workerd egress matching (shared `EgressPolicy` + `bridge/egress.js`):

- **Every hop.** The request URL's host must be on
  `capabilities.network.domains` (with `*.` prefix wildcards), including
  **redirect hops**, unless the operator granted undeclared public redirects.
  Matching uses **IDNA ToASCII** on both the request host and allowlist
  patterns; percent-encoded hosts and failed IDNA are **rejected** (fail
  closed). Unicode and Punycode forms of the same name match after
  normalization.
- **Resolved addresses.** Permission is enforced against the resolved IP
  (`public` plus explicit CIDRs in workerd `network.allow`). DNS rebinding
  from a public hostname to a private/loopback/metadata address is denied
  unless that CIDR is granted.
- **TCP.** `import { connect } from "cloudflare:sockets"` is forwarded to the
  egress worker's inbound CONNECT handler (`compatibilityFlags =
  ["experimental"]` plus `workerd serve --experimental`; workerd#6059). Fetch
  grants never imply TCP. Prefer `secureTransport: "starttls"` then
  `socket.startTls()`; incoming CONNECT with TLS is not supported by the
  pinned workerd.
- Cross-origin redirects drop `Authorization`
  (Fetch CORS non-wildcard request-header) plus `Cookie` / `Cookie2` /
  `Proxy-Authorization` as defense in depth. Method/body follow Fetch
  HTTP-redirect fetch: 301/302 convert **POST→GET** only; 303 converts
  non-GET/HEAD→GET; 307/308 preserve method/body. `AbortSignal` and other
  RequestInit metadata survive hops.
- **Python + outbound.** Workerd Python guests also require the Pyodide/CDN hosts
  (`cdn.jsdelivr.net`, `pypi.org`, `files.pythonhosted.org`) in the consent grant;
  materialize uses the same set and does not silently widen beyond it.
- **`[workerd].limits`.** Optional `cpu_ms` / `subrequests` are **clamped by the
  host** (defaults `30000` / `50` when unset or `0`; hard caps `120000` /
  `1000`). Local workerd **cannot** Cap'n Proto-emit Cloudflare-style
  `cpuMs` / `subRequests` — Bookclerk injects the clamped `subrequests` budget
  into `EGRESS_POLICY`. The egress bridge enforces it **per egress invocation**
  (one plugin `fetch()` plus that call's redirect hops → **429** when
  exceeded), matching Cloudflare's *per-invocation* subrequest budgeting rather
  than an isolate-lifetime / module-scope counter. Bookclerk plugins are
  long-lived across many host RPCs; aggregating subrequests across multiple
  plugin `fetch()` calls inside one RPC would need a CF-comparable invocation
  unit and is deferred. `cpu_ms` is clamped and logged at isolate start;
  OS-jail CPU enforcement is a follow-up
  (see [#143](https://github.com/Bookclerk/bookclerk/issues/143)).

`bindings.oauth = true` (with outbound network) is how storefronts declare an
OAuth-style callback need. The **host** owns the browser-facing TCP listener and
forwards connections over IPC (see [Interactive listeners](../internals/guest-jail.md#interactive-listeners-oauth-and-similar));
native guests with oauth also receive jail listen rights for that tunnel.

Unrestricted network access is deliberately not expressible, and **the filesystem
allowlist cannot be widened from a manifest**. A manifest ships with the plugin
it describes, so anything it can ask for is something a hostile plugin can ask
for too — consent and review are the operator controls.

Consent is keyed by `entrypoints`, the granted bindings (`[vars]`, `[secrets]`,
`[work_fs]`, `[oauth]`, `[[kv_namespaces]]`, `[[databases]]`), and every
`[[events.producers]]` type; there is no separate method list.

## Consent before enable (and every spawn)

Third-party (and newly installed) plugins require an explicit permission grant
before **enable** and again before **every external spawn**. Privileged delivery
also checks individual bindings: spawn `config`, host-injected secrets,
`work_fs` (jail `tmp` / streams), and OAuth callback proxy setup.

**Operator Settings** shows a branded consent dialog when enabling a plugin that
is not yet covered. The dialog starts from the manifest baseline and lets the
operator **widen or narrow** domains/bindings/flags, network mode, workerd
`cpuMs`/`subrequests`, and shared `diskMib` (host-capped). Daemon APIs:

| Method | Path | Notes |
| --- | --- | --- |
| `GET` | `/api/plugins/{id}/consent` | Request surface, `covered`, optional `brand` / `limits` |
| `POST` | `/api/plugins/{id}/consent` | `{ "approve": true, "grant"?: { … } }` — omit `grant` for full request |

CLI remains available (always approves the full request):

```bash
bookclerk plugins approve echo          # interactive summary of network/bindings
bookclerk plugins approve echo --yes    # non-interactive
bookclerk plugins deploy echo
```

Platform guests (`sqlite`, `local`) skip the consent UX and are enabled by
default **only when host-controlled provenance says they are verified
Bookclerk platform artifacts** (not because the manifest id is `sqlite` /
`local`). The host auto-persists a covering grant on first spawn when their
manifest stays within the installer envelope (deny network, `config` /
`work_fs` only). A third-party package that reuses those ids gets no
privilege.

An installed plugin's grant is stored on its version row, so one approval
covers every node that runs that version (`bookclerk plugins approve`,
`plugins install --approve-capabilities`, or
`POST /api/config/plugins/{key}/versions/{version}/approve`); see
[Installing plugins](../server/plugins.md#installing-plugins). A host-local tree's grant is persisted under
`$BOOKCLERK_FILES_DIR/plugin-grants.json`, keyed by provenance-qualified
PluginKey. Saving that file is the operator mechanism for those trees
(`bookclerk plugins approve` and `POST /api/plugins/{id}/consent`).
`bookclerkd` watches the file (and same-process saves notify immediately)
and **proactively** fences any live session whose persisted
`grant_revision` no longer matches. Malformed grant files keep
last-known-good authority and are retried on later ticks; the watcher
does not advance its fingerprint until a grant file loads successfully.
The vat receives `Work::Shutdown`, the jailed
`bookclerk-workerd` child is killed, the native guest and mediated TCP
proxy drop, and EVENTS / granted database channels go with the vat. This
does **not** wait for the next plugin RPC.

`grant_revision` is the digest of **persisted** operator consent.
`authority_revision` is the digest of **effective** runtime authority
(manifest ∩ grant ∩ host policy, plus host-controlled overlays such as a
configured PostgreSQL TCP URL, and clamped resource budgets). The watcher
never compares one digest type against the other. Host-config overlay
changes fence/restart through `configuration_revision` on the executor
identity. `authority_revision` is also distinct from
`configuration_revision` (installed `plugin.toml` hash).

Structural capabilities originate in the
manifest; the operator may **narrow** them but cannot invent entrypoints,
producers, or host bindings. Network destinations are operator-extensible:
operators may add fetch hosts, TCP `host:ports`, and CIDRs beyond the
manifest, **and may deny** individual fetch hosts, TCP grants, or CIDRs so a later
package upgrade of the same PluginKey does not silently restore them. Host
hard caps still
apply (`WorkerdLimits` maxes, disk/memory max 4096 MiB, CPU rate
up to `logical_cpus × 100` one-core percent, extra processes 62 / absolute PIDs
64, known bindings). Workerd plugins use isolate `cpuMs` instead of per-plugin
`cpuRatePercent`; jail CPU for workerd follows the host default /
`[plugins.jail]` per-jail ceiling (default 80). Process headroom for workerd is
host-managed (overhead 2 + default extra 2). Bookclerk does **not** guarantee
plugin behaviour if overrides remove capabilities the guest needs.

Domain allowlists and TCP grants are enforced for **both** workerd and
native-behind-workerd guests through one canonical `EgressPolicy` (workerd
`EGRESS_POLICY` / `BOOKCLERK_WORKERD_GRANT_POLICY`; native CONNECT mux on the
inherited `fd:` / `handle:` link). Direct-native diagnostic transport is
test-only and still uses the OS jail mapping. Redirect hops stay on the
fetch allowlist unless the operator grants undeclared public redirects;
address-space policy still applies.

Guest filesystem access remains install read-only plus host-managed
`plugin-state/<PluginKey fs-id>/data` and `plugin-state/<PluginKey fs-id>/tmp`
— not a free-form widen.

Global confinement knobs (Settings → Confinement, or `config.toml`):

| Key | Purpose |
| --- | --- |
| `plugins.isolation` | `required` / `best-effort` / `off` |
| `media.isolation` | same tiers for media-worker |
| `plugins.jail.memory_mib` | optional Spec memory ceiling |
| `plugins.jail.cpu_rate_percent` | per-jail CPU ceiling as integer percent of one core (default **80**; 100 = 1.00 core; UI edits cores to two decimals; max = cores×100; OS shares if oversubscribed) |
| `plugins.jail.extra_processes` | ceiling on extra processes/threads beyond launcher overhead (default **2**; Spec `active_processes` = overhead + extra) |

**Deferred (discovery/install):** a content hash bound to the grant so a
different binary under the same id cannot keep an old grant forever. End goal
once install/upgrade exists: on upgrade, **refresh the registered hash without
re-prompting** when capabilities did not widen; re-prompt only when the
capability scope expands (new structural caps stay pending; `grant_covers`
is identity-only so the previous approval remains usable). Do not
expect spawn-time hash checks in this release.

On upgrade of the **same PluginKey**, operator-added network destinations
and operator **denials** (fetch hosts, TCP grants, and CIDRs) survive; new
structural capabilities stay pending until the operator consents
(`grant_covers` is identity-only). A different provenance (even with the
same manifest alias) does not inherit grants.
