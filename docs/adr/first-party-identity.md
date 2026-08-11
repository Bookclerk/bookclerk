# ADR: First-party identity (Operator / Administrator / Member)

- **Status:** Accepted
- **Date:** 2026-08-11
- **Tracking:** [#117](https://github.com/fritz-fritz/bookclerk/issues/117), PR [#129](https://github.com/fritz-fritz/bookclerk/pull/129)
- **Prerequisite:** [#116](https://github.com/fritz-fritz/bookclerk/issues/116) (atomic config reload; closed)

## Context

Bookclerk historically split identity into a shared **operator token** (control
plane) and **portal identities** keyed by external `(provider, external_user_id)`
from claim tickets or integration `authenticateUser`. Bookstore connect via
`bookclerk auth` could create store accounts with no portal link. The product
goal is a self-hostable first-party User as the security principal for library
and Accounts, without requiring Redis or an external IdP.

## Decision

### Principals

| Principal | Powers |
| --- | --- |
| **Operator** | Control plane via shared operator token + durable hashed sessions. **Cannot** connect bookstore sources. Can **impersonate** any User. Creates the linked **Administrator** on bootstrap. |
| **Administrator** | Normal User + admin caps (provision users, acquire). Can **elevate to Operator** with reauth. |
| **Member** | Normal User; may connect sources under policy. |

### Federation and OIDC

- Keep claim tickets and integration `authenticateUser` even when a User has no
  local password yet. External bookstore/integration accounts are **connections**,
  not login principals.
- Bookclerk is the OIDC authorization server for Audiobookshelf (core AS;
  plugins do not implement the protocol). Tokens are bound to a **User**, never
  minted from the operator token alone.

### CLI

- Remove the entire `bookclerk auth` command group. Store connect lives in the
  User SPA Accounts UI only. Operational account listing / scan toggles move
  under `library` (or equivalent) without creating unlinked store credentials.

### Small-VPS constraint

Use the existing database abstraction / SQLite platform path and bounded
cleanup. Baseline must not require Redis, an external IdP, or another always-on
service.

## Phases (single draft PR)

Implemented on branch `cursor/first-party-auth-sessions-0188` (PR #129). Phase
acceptance is gated by automated tests listed in that PR and in [#117](https://github.com/fritz-fritz/bookclerk/issues/117).

| Phase | Scope |
| --- | --- |
| **0** | Durable hashed operator sessions; portal logout revoke; atomic claim redeem; Secure cookies; `daemon.trusted_proxies` |
| **1** | `users` schema; migrate `portal_identities`; ban CLI/operator store-link; prefs `user:{id}` |
| **2** | Administrator elevate-to-Operator; Operator impersonate User |
| **3** | Bootstrap Administrator; invites; Argon2id; federation without local password |
| **4** | Core OIDC AS (code + PKCE) for ABS |
| **5** | CSRF/Origin; session list/revoke; audit log; account-link invariant; proxy TLS docs |

## Consequences

- Portal cookies and `/api/portal/*` resolve to a first-party `users` row.
- Operator prefs subject remains `operator`; user prefs use `user:{id}`.
- Docs: [gui.md](../gui.md), [operations.md](../operations.md),
  [integrations.md](../integrations.md), [database.md](../database.md).

## Non-goals

- Requiring an external IdP or Redis
- WebAuthn/passkeys in the first provisioning cut (password first; passkeys later)
- Spatial/Atmos DRM
- Reopening #116
