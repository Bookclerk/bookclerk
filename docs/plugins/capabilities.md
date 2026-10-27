# Capabilities and consent

This page explains what a plugin can declare it needs (network,
databases, host bindings) and how the operator approves it before the
plugin is enabled or spawned.

## Declaring what a plugin needs

A manifest declares network access and host bindings under
**`[capabilities]`**. It can't widen the file system allowlist.

A **workerd** plugin gets hostname-filtered `fetch()` plus a separate TCP
`connect()`:

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

A **native** plugin's sibling jail denies ambient `AF_INET`, and SDK
sockets share the same policy:

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
| `deny` | Sibling guest jail `NetPolicy::Deny`; no socket proxy grants | OS jail stays `OutboundListen` for the RPC bridge; isolate `globalOutbound` → blocked |
| `outbound` | Sibling guest jail still `Deny`; TCP via `bookclerk_plugin_sdk::connect` → multiplexed CONNECT on the inherited `fd:`/`handle:` link | Isolate `globalOutbound` → egress worker; `fetch()` and `connect()` share one `EgressPolicy` |

### Network rules

- `capabilities.network.domains` is the **fetch** allowlist (workerd
  only). Raw TCP is `capabilities.network.tcp` (`host` + `ports`).
- The operator may add extra fetch hosts, TCP targets and CIDRs. The
  guest's `describe()` can't.
- The default address-space policy is the public Internet only. Loopback,
  RFC 1918, link-local, ULA and metadata addresses stay denied unless an
  explicit CIDR is granted.
- `allow_undeclared_public_redirects` lets fetch redirects leave the
  domain allowlist for **public** destinations only. It never implies
  those special ranges.

> [!TIP]
> When you need enforceable hostname allowlists for `fetch()`, ship a
> **workerd** plugin. Native plugins that need networking must use the SDK
> socket capability.

The native path is tested end to end on Linux, macOS and Windows by
`cargo test -p bookclerk-plugin-e2e --test native_gateway --test native_gateway_isolation --test native_gateway_lifecycle --test native_gateway_authenticated_endpoint`.
It checks that:

- a granted loopback port round-trips through the proxy;
- an ungranted live port is refused with `403`;
- a direct socket from the sibling jail is blocked;
- both PIDs and the host session folder go away on drop;
- concurrent sessions can't read each other's gateway state.

See [CI → native-behind-workerd gateway smoke](../contributing/ci.md#native-behind-workerd-gateway-smoke).

### Workerd egress matching

Workerd egress is matched by the shared `EgressPolicy` and
`bridge/egress.js`:

- **Every hop.** The request URL's host must be on
  `capabilities.network.domains` (with `*.` prefix wildcards), including
  **redirect hops**, unless the operator granted undeclared public
  redirects. Matching uses **IDNA ToASCII** on both the request host and
  the allowlist patterns. Percent-encoded hosts and failed IDNA are
  **rejected** (fail closed). Unicode and Punycode forms of the same name
  match after normalization.
- **Resolved addresses.** Permission is enforced against the resolved IP
  (`public` plus explicit CIDRs in workerd `network.allow`). DNS
  rebinding from a public hostname to a private, loopback or metadata
  address is denied unless that CIDR is granted.
- **TCP.** `import { connect } from "cloudflare:sockets"` is forwarded to
  the egress worker's inbound CONNECT handler
  (`compatibilityFlags = ["experimental"]` plus
  `workerd serve --experimental`; workerd#6059). Fetch grants never imply
  TCP. Prefer `secureTransport: "starttls"` then `socket.startTls()`:
  incoming CONNECT with TLS isn't supported by the pinned workerd.
- **Redirect headers.** Cross-origin redirects drop `Authorization`
  (Fetch CORS non-wildcard request-header), plus `Cookie`, `Cookie2` and
  `Proxy-Authorization` as defense in depth.
- **Redirect methods.** Method and body follow Fetch HTTP-redirect fetch:
  301 and 302 convert **POST to GET** only; 303 converts non-GET/HEAD to
  GET; 307 and 308 keep the method and body. `AbortSignal` and other
  RequestInit metadata survive hops.
- **Python and outbound.** Workerd Python guests also need the Pyodide and
  CDN hosts (`cdn.jsdelivr.net`, `pypi.org`, `files.pythonhosted.org`) in
  the consent grant. Materializing uses the same set and doesn't widen
  beyond it.

### Workerd limits

`[workerd].limits` takes optional `cpu_ms` and `subrequests`, **clamped by
the host**: the defaults are `30000` and `50` when unset or `0`, and the
hard caps are `120000` and `1000`.

- Local workerd **can't** Cap'n Proto-emit Cloudflare-style `cpuMs` and
  `subRequests`, so Bookclerk injects the clamped `subrequests` budget
  into `EGRESS_POLICY`.
- The egress bridge enforces it **per egress invocation**: one plugin
  `fetch()` plus that call's redirect hops, answering **429** when it is
  exceeded. That matches Cloudflare's per-invocation subrequest budget,
  not an isolate-lifetime or module-scope counter.
- Bookclerk plugins live across many host RPCs. Counting subrequests
  across several plugin `fetch()` calls inside one RPC would need a
  Cloudflare-comparable invocation unit, and is deferred.
- `cpu_ms` is clamped and logged at isolate start. OS-jail CPU enforcement
  is a follow-up
  (see [#143](https://github.com/Bookclerk/bookclerk/issues/143)).

### OAuth callbacks

`bindings.oauth = true` (with outbound network) is how storefronts
declare an OAuth-style callback need. The **host** owns the
browser-facing TCP listener and forwards connections over IPC (see
[Interactive listeners](../internals/guest-jail.md#interactive-listeners-oauth-and-similar)).
Native guests with `oauth` also receive jail listen rights for that
tunnel.

### What a manifest can't ask for

> [!IMPORTANT]
> Unrestricted network access can't be expressed, and **the file system
> allowlist can't be widened from a manifest**. A manifest ships with the
> plugin it describes, so anything it can ask for, a hostile plugin can
> ask for too. Consent and review are the operator's controls.

Guest file system access stays the install folder (read-only) plus the
host-managed `plugin-state/<PluginKey fs-id>/data` and
`plugin-state/<PluginKey fs-id>/tmp`.

Consent is keyed by `entrypoints`, the granted bindings (`[vars]`,
`[secrets]`, `[work_fs]`, `[oauth]`, `[[kv_namespaces]]`,
`[[databases]]`) and every `[[events.producers]]` type. There is no
separate method list.

## Consent before enable (and every spawn)

Third-party (and newly installed) plugins need an explicit permission
grant before **enable**, and again before **every external spawn**.
Privileged delivery also checks individual bindings: spawn `config`,
host-injected secrets, `work_fs` (jail `tmp` and streams), and OAuth
callback proxy setup.

### Approving in Settings

**Operator Settings** shows a branded consent dialog when enabling a
plugin that isn't covered yet. The dialog starts from the manifest
baseline, and lets the operator **widen or narrow**:

- domains, bindings and flags;
- the network mode;
- workerd `cpuMs` and `subrequests`;
- the shared `diskMib` (capped by the host).

The daemon APIs behind it:

| Method | Path | Notes |
| --- | --- | --- |
| `GET` | `/api/plugins/{id}/consent` | Request surface, `covered`, optional `brand` / `limits` |
| `POST` | `/api/plugins/{id}/consent` | `{ "approve": true, "grant"?: { … } }`; omit `grant` for the full request |

### Approving on the command line

The CLI always approves the full request:

```sh
bookclerk plugins approve echo          # interactive summary of network/bindings
bookclerk plugins approve echo --yes    # non-interactive
bookclerk plugins deploy echo
```

### Platform guests

Platform guests (`sqlite`, `local`) skip the consent screens and are
enabled by default, **only when host-controlled provenance says they are
verified Bookclerk platform artifacts**. The manifest id being `sqlite` or
`local` isn't enough.

The host saves a covering grant on first spawn when their manifest stays
within the installer envelope (network denied, `config` and `work_fs`
only). A third-party package that reuses those ids gets no privilege.

### Where grants are stored

An installed plugin's grant is stored on its version row, so one approval
covers every node that runs that version. Approve it with
`bookclerk plugins approve`, `plugins install --approve-capabilities`, or
`POST /api/config/plugins/{key}/versions/{version}/approve` (see
[Installing plugins](../server/plugins.md#installing-plugins)).

A host-local tree's grant is saved in
`$BOOKCLERK_FILES_DIR/plugin-grants.json`, keyed by provenance-qualified
PluginKey. Saving that file is how the operator approves those trees
(`bookclerk plugins approve` and `POST /api/plugins/{id}/consent` both do
it).

### A changed grant takes effect at once

`bookclerkd` watches `plugin-grants.json` (and saves from the same process
notify it immediately). It **proactively** fences any live session whose
saved `grant_revision` no longer matches. Fencing a session means:

- the vat receives `Work::Shutdown`;
- the jailed `bookclerk-workerd` child is killed;
- the native guest and the mediated TCP proxy drop;
- EVENTS and granted database channels go with the vat.

This does **not** wait for the next plugin RPC.

Malformed grant files keep the last-known-good authority and are retried
on later ticks. The watcher doesn't advance its fingerprint until a grant
file loads successfully.

### Revisions

| Revision | Digest of |
| --- | --- |
| `grant_revision` | **Saved** operator consent |
| `authority_revision` | **Effective** runtime authority: manifest ∩ grant ∩ host policy, plus host-controlled overlays (such as a configured PostgreSQL TCP URL) and clamped resource budgets |
| `configuration_revision` | The installed `plugin.toml` hash |

The watcher never compares one digest type against another. Host-config
overlay changes fence and restart through `configuration_revision` on the
executor identity.

### What the operator may change

Structural capabilities come from the manifest. The operator may
**narrow** them, but can't invent entrypoints, producers or host bindings.

Network destinations can be extended by the operator:

- Operators may add fetch hosts, TCP `host:ports` and CIDRs beyond the
  manifest.
- Operators **may deny** individual fetch hosts, TCP grants or CIDRs, so a
  later package upgrade of the same PluginKey doesn't quietly restore
  them.

Host hard caps still apply:

- `WorkerdLimits` maximums;
- disk and memory at most 4096 MiB;
- a CPU rate up to `logical_cpus × 100` one-core percent;
- 62 extra processes, 64 PIDs in all;
- known bindings only.

Workerd plugins use isolate `cpuMs` instead of a per-plugin
`cpuRatePercent`. Jail CPU for workerd follows the host default or the
`[plugins.jail]` per-jail ceiling (default 80). Process headroom for
workerd is managed by the host (an overhead of 2 plus a default extra 2).

> [!WARNING]
> Bookclerk does **not** guarantee plugin behavior if overrides remove
> capabilities the guest needs.

### One egress policy for both runtimes

Domain allowlists and TCP grants are enforced for **both** workerd and
native-behind-workerd guests, through one canonical `EgressPolicy`:
workerd `EGRESS_POLICY` and `BOOKCLERK_WORKERD_GRANT_POLICY`, and the
native CONNECT mux on the inherited `fd:` or `handle:` link.

- Direct-native diagnostic transport is test-only, and still uses the OS
  jail mapping.
- Redirect hops stay on the fetch allowlist unless the operator grants
  undeclared public redirects. The address-space policy still applies.

### Upgrades

On an upgrade of the **same PluginKey**:

- operator-added network destinations and operator **denials** (fetch
  hosts, TCP grants and CIDRs) survive;
- new structural capabilities stay pending until the operator consents.
  `grant_covers` is identity-only, so the previous approval stays usable
  meanwhile.

A different provenance, even with the same manifest alias, doesn't
inherit grants.

> [!NOTE]
> **Deferred:** a content hash bound to the grant, so a different binary
> under the same id can't keep an old grant forever. The end goal, once
> install and upgrade exist: on upgrade, refresh the registered hash without asking again when
> capabilities didn't widen, and ask again only when the capability scope
> grows. Don't expect spawn-time hash checks in this release.

## Global confinement settings

These are in **Settings → Confinement**, or `config.toml`:

| Key | Purpose |
| --- | --- |
| `plugins.isolation` | `required` / `best-effort` / `off` |
| `media.isolation` | The same tiers for the media worker |
| `plugins.jail.memory_mib` | Optional spec memory ceiling |
| `plugins.jail.cpu_rate_percent` | Per-jail CPU ceiling, as an integer percent of one core (default **80**; 100 = 1.00 core; the UI edits cores to two decimals; max = cores × 100; the OS shares if oversubscribed) |
| `plugins.jail.extra_processes` | Ceiling on extra processes and threads beyond the launcher overhead (default **2**; spec `active_processes` = overhead + extra) |

## Next steps

- [plugin.toml](manifest.md): where these sections go.
- [Managing plugins](../server/plugins.md): how operators install and
  approve plugins.
- [The guest jail](../internals/guest-jail.md): how the jail enforces
  this.
