# ADR: Workers RPC plugin ABI + per-jail embedded workerd

- **Status:** Accepted
- **Date:** 2026-08-09

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
     are denied.
   - **Native:** `mode = "outbound"` is coarse jail internet (**no** `domains`
     key; OS jails cannot filter by hostname across HTTP + raw TCP without a
     full mediator). Prefer workerd when hostname allowlists matter.
6. **Consent UX:** CLI and web UI prompt before enable; grants persisted;
   capability widening re-prompts. Native outbound shows an explicit warning.
7. **`compatibility_date` newer than bundled workerd:** warn, still load.

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
