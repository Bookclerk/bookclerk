# ADR: Workers RPC plugin ABI + per-jail embedded workerd

- **Status:** Accepted
- **Date:** 2026-08-09
- **Updated:** 2026-08-15 (`api_version = 2` freeze: workerd control-plane front door, typed errors, JobInvocation, backend-neutral bindings)

## Context

Bookclerk needs a multi-language plugin model where third-party authors ship
portable JS/TS (or Python) modules without per-OS binaries, while native Rust
guests remain available for DRM and heavy dependencies. The host↔plugin
contract must be **identical** across runtimes.

## Decision

1. **Product `api_version = 2`.** The ABI is an object-capability Workers RPC:
   role-specific classes (`BookclerkPlugin`, `Destination`, `Source`,
   `JobHandler`, `ContentSource`, `Integration`, `Database`), transferable byte
   streams, and **invocation-scoped** stub lifetime (create, invoke, dispose in
   one request). RPC carries bounded values and **stream/stub capabilities**.
   Cap'n Proto field/union ordinals are append-only; unknown members fail
   closed or return typed `unsupported`. `describe()` advertises `abiMajor` /
   `abiMinor` and `supportedRoles`; the signed manifest is the host allowlist.
2. **Workerd is the control-plane front door; native jail is a backend.**
   - **Control plane:** invocation, policy, binding, lifecycle, and outcome
     always pass through the workerd entrypoint (or a generated backend-proxy
     entrypoint the first-party wrapper owns).
   - **Media fast path:** large `ByteSource` streams may be realized by a
     trusted broker **directly to the destination** without entering JavaScript
     memory. Observable backpressure, cancellation, checksum, and error
     semantics stay identical to the isolate path.
   - **Cap'n Proto** is the broker↔native protocol. Direct native Cap'n Proto
     to the host is a **host-selected** compatibility fallback — the plugin
     cannot request it to bypass policy.
   - **Bindings:** authors see frozen `BookclerkContext.bindings` (`HTTP` /
     `STORAGE` / `SECRETS` / `OAUTH`) and optional `ctx.native`. The trusted
     adapter sees private `AdapterEnv.PLUGIN_BACKEND`. Do **not** freeze
     `env.NATIVE_PLUGIN`. The host executor owns the process tree and outer
     sandbox; it launches workerd and the trusted native broker. The broker
     launches or connects to the verified native guest. Plugin-controlled
     input cannot choose the executable or weaken the sandbox.
   - **Native:** guests serve [`plugin_v2.capnp`](../../crates/bookclerk-plugin-abi/schema/plugin_v2.capnp)
     via `capnp-rpc` (`serve`). They do **not** speak newline JSON as the
     product ABI. Native DRM plugins do not implement Cloudflare’s private
     `worker-interface.capnp`; the host maps both adapters onto the same Rust
     traits.
3. **v1 newline JSON is removed.** Guests speak Cap'n Proto `api_version = 2`
   (or workerd Workers RPC mapped onto the same contract). Scalar JSON remains
   only as a versioned escape hatch for plugin-specific config, not as a spawn
   handshake.
4. **Greenfield (no `protocol` key).** No dual-stack product ABI with legacy
   JSON-RPC stdio framing.
5. **Isolation:** instances are keyed by `(plugin_id, account_id)`. Different
   accounts / security principals **never execute concurrently in the same
   isolate**. One isolate per invocation is allowed for higher-risk work.
   Per-invocation grants are scoped, expiring, revocable, and operation-limited;
   they are revoked on completion, cancellation, fence loss, suspension, or
   disconnect. Within one plugin instance, capability delegation is allowed
   (Workers RPC stubs are transferable). The OS jail remains mandatory:
   workerd is [not a hardened sandbox](https://github.com/cloudflare/workerd#warning-workerd-is-not-a-hardened-sandbox).
   Pin advances via a daily CI job with a **7-day publish cooldown**
   (same supply-chain posture as Dependabot); see
   [packaging.md](../packaging.md#cloudflare-workerd-pin).
   Local embed only — not Cloudflare cloud execution. No JS-less stdio shim.
6. **Network:**
   - **Workerd:** operator approves `capabilities.network.domains`. Isolate
     egress enforces the allowlist. **Every redirect hop is checked**, not
     only the initial host. Resolved IPs are checked against private / local /
     metadata ranges; DNS rebinding and Host/SNI mismatch are rejected
     (full IP/SNI enforcement in the native broker is follow-up — this ADR
     must not freeze initial-host-only or “native = coarse unrestricted”).
   - **Brokered HTTP** uses the same domain grants as isolate fetch. **Raw
     TCP, UDP, and listen are distinct capabilities.** Jail default-deny
     remains. Native outbound is not permanently coarse-unrestricted.
   - The OS jail for every workerd guest is `NetPolicy::OutboundListen` so
     `bookclerk-workerd` can `bind(127.0.0.1:0)` for the host↔isolate RPC
     bridge, **including when the stored grant is `network_mode = "deny"`**.
     Linux Landlock can restrict `bind` but cannot restrict outbound
     `connect`, so that OS layer is **not** the grant denial boundary. Grant
     denial is isolate-enforced: `BOOKCLERK_WORKERD_GRANT_NETWORK_MODE` sets
     plugin `globalOutbound` to `blocked` (deny) or the egress proxy
     (outbound). The generated workerd config exposes a single listen socket
     (`rpc` → bridge); compatibility flags (`python_workers`,
     `nodejs_compat`, …) do not add sockets.
7. **Consent UX:** CLI and web UI prompt before enable; grants persisted;
   capability widening re-prompts. The same covering grant is enforced at every
   external spawn and at privileged delivery (`config` / `secrets` / `work_fs` /
   `oauth`). Native outbound shows an explicit warning.
8. **`compatibility_date` newer than bundled workerd:** warn, still load.
9. **`[workerd].limits`:** local workerd does **not** Cap'n Proto-enforce
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
- CI requires the pinned `workerd` binary for the v2 contract suite; the job
  fails if the workerd adapter cannot start (`BOOKCLERK_V2_SKIP_WORKERD=1` is
  local native-only).

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

Production native-behind-workerd executor, deny-direct-native-egress / HTTP
proxy, OAuth/listen broker, container executor, and VPS benchmarks are
**out of this ABI freeze**. Direct native Cap'n Proto stays as a host-selected
fallback.

Instances are keyed by `(plugin_id, account_id)`. Shared-isolate concurrent
principals are not a proven isolation boundary (stubs are transferable).

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
