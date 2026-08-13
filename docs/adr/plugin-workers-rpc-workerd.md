# ADR: Workers RPC plugin ABI + per-jail embedded workerd

- **Status:** Accepted
- **Date:** 2026-08-09
- **Updated:** 2026-08-13 (workerd OS-jail `OutboundListen` under a deny grant)

## Context

Bookclerk needs a multi-language plugin model where third-party authors ship
portable JS/TS (or Python) modules without per-OS binaries, while native Rust
guests remain available for DRM and heavy dependencies. The host↔plugin
contract must be **identical** across runtimes.

## Decision

1. **Greenfield `api_version = 1`.** No `protocol` key. No dual-stack with
   legacy JSON-RPC stdio framing as a product ABI.
2. **Shared ABI = Workers RPC semantics.** Method calls + structured payloads,
   authored once in [`bookclerk-plugin-abi`](../../crates/bookclerk-plugin-abi)
   (`schema/abi.json`) and generated into Rust + TypeScript SDKs.
3. **Branded guest base `BookclerkPlugin`.** Authors extend/implement this type
   (TS class extends `WorkerEntrypoint`; Rust trait), never bare platform types
   in app code.
4. **Isolation:** one `bookclerk-jail` + one embedded workerd isolate per
   plugin via first-party `bookclerk-workerd` plus a **pinned Cloudflare
   `workerd` binary** (fetched by `cargo ensure-workerd` / platform packaging).
   The pin advances via a daily CI job with a **7-day publish cooldown**
   (same supply-chain posture as Dependabot); see
   [packaging.md](../packaging.md#cloudflare-workerd-pin).
   Local embed only — not Cloudflare cloud execution. No JS-less stdio shim.
5. **Network:**
   - **Workerd:** operator approves `capabilities.network.domains` (initial
     request hosts). Isolate egress enforces the allowlist; **redirect hops do
     not require allowlist membership**. Direct requests to non-listed hosts
     are denied. The OS jail for every workerd guest is
     `NetPolicy::OutboundListen` so `bookclerk-workerd` can `bind(127.0.0.1:0)`
     for the host↔isolate RPC bridge, **including when the stored grant is
     `network_mode = "deny"`**. Linux Landlock can restrict `bind` but cannot
     restrict outbound `connect`, so that OS layer is **not** the grant
     denial boundary. Grant denial is isolate-enforced:
     `BOOKCLERK_WORKERD_GRANT_NETWORK_MODE` sets plugin `globalOutbound` to
     `blocked` (deny) or the egress proxy (outbound). The generated workerd
     config exposes a single listen socket (`rpc` → bridge); compatibility
     flags (`python_workers`, `nodejs_compat`, …) do not add sockets.
     Host-prebound / Unix-socket bridging that would keep the OS jail at
     `Deny` is a follow-up, not the current spawn model.
   - **Native:** `mode = "outbound"` is coarse jail internet (**no** `domains`
     key; OS jails cannot filter by hostname across HTTP + raw TCP without a
     full mediator). Prefer workerd when hostname allowlists matter. A stored
     `deny` grant maps to `NetPolicy::Deny` even for OAuth (`Listen`) native
     guests — they do not inherit the workerd bridge exception.
6. **Consent UX:** CLI and web UI prompt before enable; grants persisted;
   capability widening re-prompts. The same covering grant is enforced at every
   external spawn and at privileged delivery (`config` / `secrets` / `work_fs` /
   `oauth`). Native outbound shows an explicit warning.
7. **`compatibility_date` newer than bundled workerd:** warn, still load.
8. **`[workerd].limits`:** local workerd does **not** Cap'n Proto-enforce
   `cpuMs` / `subRequests`. Bookclerk clamps `cpu_ms` / `subrequests` (defaults
   and hard caps), injects `subrequests` into egress policy JSON, and the
   egress bridge counts hops **per egress invocation** (one plugin `fetch()` +
   redirect chain → 429) — Cloudflare-style per-invocation budgeting, not
   isolate-lifetime module state. Cross-`fetch` aggregation within a host RPC
   is deferred until that invocation unit is defined. Effective `cpu_ms` is
   logged at isolate start; OS-jail CPU enforcement is tracked separately.

## Consequences

- Manifests declare `runtime = "workerd" | "native"` and capability sections.
- Catalog script archives ship `plugin.toml` + `modules/` only.
- Native publishers still build per-arch when they need native capabilities.
- CI must fail on ABI schema drift vs generated SDK outputs.

### Distribution tiers

| Tier | Contents |
| --- | --- |
| **Platform** | Hosts + jail + workerd + media-worker + `sqlite` + `local` |
| **Product** | Optional first-party guests (storefronts, ABS, s3, d1, postgres) |
| **Reference examples** | Echo under `examples/` — CI / `cargo dev --examples` only; never packaged |

Every guest is an **external** (jailed) subprocess, including platform sqlite/local.

## Non-goals

- Operator-side `cargo` / `rustc` / `npm` to install plugins.
- In-process Wasm/`cdylib` loading in the host.
- Marketing workerd alone as the security boundary (jail remains mandatory).
- Cloudflare-hosted Dynamic Workers for operator plugin execution.
- HMR for `cargo dev` (restart-based only).

## Follow-ups (deferred)

Today’s spawn model remains **one long-lived jail (+ isolate) per plugin id**,
shared across accounts that use that storefront. That matches warm DRM/session
state for first-party sources but is **not** multi-account isolation inside the
guest (host DB still scopes secrets; credentials are injected per RPC).

When Bookclerk defines a Cloudflare-comparable **invocation unit** (host RPC ≈
one Worker invocation) for `cpu_ms` / subrequest aggregation, plan to evolve
lifecycle and tenancy as follows — **no implementation in this ADR revision**:

1. **Invocation-style budgeting** — treat a host RPC (or an explicit job
   envelope) as the CF invocation boundary so isolate `cpu_ms` and subrequests
   aggregate like Workers, not as isolate-lifetime module state.
2. **Per-account instantiation** — for account-bearing kinds (especially
   `source` / OAuth storefronts), prefer a jail (and workerd isolate) keyed by
   `(plugin_id, account_id)` so linked users do not share process memory or
   `plugins/<id>/data`. Resource caps (`extraProcesses`, memory, CPU) stay
   **per instance**.
3. **Lifecycle by kind** — split long-lived vs one-shot:
   - **Long-lived:** platform DB/output and other shared infrastructure guests
     that amortize connection state.
   - **One-shot / short-lived:** high-trust-boundary or bursty work (and,
     eventually, many storefront account sessions) closer to Workers
     request/isolate churn.
4. Until then, keep host hygiene (prefer one account per privileged RPC) and
   document that enabling a shared storefront guest trusts it with every
   account the host sends it.
