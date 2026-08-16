# ADR: First-party identity (Operator / Owner / Administrator / Member)

- **Status:** Accepted (Phases 0–6 landed on `main`)
- **Date:** 2026-08-11
- **Updated:** 2026-08-15 (plugin-provided OIDC clients; see [plugin-oidc-clients.md](plugin-oidc-clients.md))
- **Tracking:** [#117](https://github.com/fritz-fritz/bookclerk/issues/117) (closed via [#129](https://github.com/fritz-fritz/bookclerk/pull/129)); follow-ups [#150](https://github.com/fritz-fritz/bookclerk/pull/150), [#152](https://github.com/fritz-fritz/bookclerk/pull/152)
- **Prerequisite:** [#116](https://github.com/fritz-fritz/bookclerk/issues/116) (atomic config reload; closed)

## Context

Bookclerk historically split identity into a shared **operator token** (control
plane) and **portal identities** keyed by external `(provider, external_user_id)`
from claim tickets or integration `authenticateUser`. Bookstore connect via
`bookclerk auth` could create store accounts with no portal link. The product
goal is a self-hostable first-party User as the security principal for library
and Accounts, without requiring Redis or an external IdP.

Operators who already run a directory (Authentik, Keycloak, Entra) or want
social sign-on need Bookclerk to **consume** upstream OIDC/OAuth while still
**issuing** tokens to Audiobookshelf. True token passthrough (ABS trusts the
upstream issuer) is not supportable.

## Decision

### Principals

| Principal | Powers |
| --- | --- |
| **Operator** | Control plane via shared operator token + durable hashed sessions. **Cannot** connect bookstore sources. Can **impersonate** any User. Creates the linked **Owner** on bootstrap. **Never** an OAuth/OIDC subject — not JIT’d, linked, or offered on the SSO login page. SPA operator-token paste is offered only before an active Owner exists; afterwards use tray handoff or Owner elevate. |
| **Owner** | Super-user (multiple allowed). Administrator caps plus **elevate to Operator** after step-up (IdP `prompt=login`, passkey, or local password). Last active Owner cannot be demoted/disabled/deleted. Non-elevated Owners may provision Members and Administrators, not other Owners. May be mapped from an upstream IdP role. |
| **Administrator** | Normal User + admin caps (provision Members only; cannot create, assign, or manage Administrator or Owner). **Cannot** elevate. May be JIT / role-synced from an IdP. |
| **Member** | Normal User; may connect sources under policy. May be JIT from IdP or social provision policies. |

### Federation and OIDC

- Keep claim tickets / invite magic links (`/invite?ticket=…`) and integration
  `authenticateUser` even when a User has no local password yet. External
  bookstore/integration accounts are **connections**, not login principals.
- Optional contact `email` on users supports invites and SSO `link_by_email`.
- Bookclerk is the OIDC authorization server for Audiobookshelf (core AS;
  plugins do not implement the protocol). Tokens are bound to a **User**, never
  minted from the operator token alone. Plugin-provided relying-party clients
  (ABS and later guests) are declared by the plugin and gated with an enable
  toggle; see [plugin-provided OIDC clients](plugin-oidc-clients.md).
- Bookclerk may optionally act as an OIDC/OAuth **relying party** (identity
  broker) for one or more upstream providers. Upstream login creates or links a
  first-party User; ABS still trusts Bookclerk’s issuer. IdP passwords are never
  stored. Requiring an external IdP remains a non-goal.
- Upstream roles map only to `owner` / `administrator` / `member` (never
  `operator`). Per-provider provision modes: `mapped_role`, `any`, `allowlist`,
  `invite_only`.
- Owner elevation uses IdP step-up, a local passkey, or a Bookclerk password —
  not a copied SSO password.
- **Passkeys (WebAuthn)** are the User-plane hatch when an IdP is down. The
  Operator token remains host break-glass. The relying party ID is a hostname
  (`integrations.public_origin`, or `localhost` on loopback) — never a raw IP.
  Passkeys are named at registration. Optional TOTP (authenticator app) is a
  second factor for **password** login only; passkey sign-in does not also
  require a TOTP code. Owners/operators may set `daemon.auth.require_second_factor`
  so password login must use TOTP or the user must sign in with a passkey.

### CLI

- Remove the entire `bookclerk auth` command group. Store connect lives in the
  User SPA Accounts UI only. Operational account listing / scan toggles move
  under `library` (or equivalent) without creating unlinked store credentials.

### Small-VPS constraint

Use the existing database abstraction / SQLite platform path and bounded
cleanup. Baseline must not require Redis, an external IdP, or another always-on
service.

## Phases

Core identity (Phases 0–5) shipped in [#129](https://github.com/fritz-fritz/bookclerk/pull/129)
closing [#117](https://github.com/fritz-fritz/bookclerk/issues/117). Owner/RBAC Settings UI
and provisioner matrix landed in [#150](https://github.com/fritz-fritz/bookclerk/pull/150).
Optional multi-IdP broker + WebAuthn (Phase 6) landed in
[#152](https://github.com/fritz-fritz/bookclerk/pull/152).

| Phase | Scope | Status |
| --- | --- | --- |
| **0** | Durable hashed operator sessions; portal logout revoke; atomic claim redeem; Secure cookies; `daemon.trusted_proxies` | Done (#129) |
| **1** | `users` schema; migrate `portal_identities`; ban CLI/operator store-link; prefs `user:{id}` | Done (#129) |
| **2** | Owner elevate-to-Operator (password / IdP step-up / passkey); Operator / elevated impersonate User | Done (#129, #150, #152) |
| **3** | Bootstrap Owner; invite magic links; Argon2id; federation without local password | Done (#129, #150) |
| **4** | Core OIDC AS (code + PKCE) for ABS | Done (#129) |
| **5** | CSRF/Origin; session list/revoke; audit log; account-link invariant; proxy TLS docs | Done (#129) |
| **6** | Optional multi-IdP OIDC RP (broker); WebAuthn passkeys | Done (#152) |

## Consequences

- Portal cookies and `/api/portal/*` resolve to a first-party `users` row.
- Operator prefs subject remains `operator`; user prefs use `user:{id}`.
- Docs: [gui.md](../gui.md), [operations.md](../operations.md),
  [integrations.md](../integrations.md), [database.md](../database.md),
  [plugin-provided OIDC clients](plugin-oidc-clients.md).
- Operator tray login never puts the durable token in a GET URL. Open Bookclerk
  mints a 60-second hashed one-time loopback code (`POST /api/auth/tray-handoff/prepare`
  + `GET /api/auth/tray-handoff?code=`). A same-host reverse proxy cannot complete
  that handoff; the operator session cookie is set on localhost only.
- The Owner / Administrator / Member role split is **greenfield**. There is no
  in-place upgrade that promotes existing Administrators to Owner. Testing and
  development deployments that already have a `library.db` from before this
  change must recreate it (`cargo reset --yes`, or delete
  `$BOOKCLERK_FILES_DIR` and re-bootstrap). The OIDC RP / passkey tables in this
  cut are likewise greenfield: testers should reset the database before
  validating SSO and passkey enrollment. Production hosts should plan a
  fresh files directory / restore from a compatible backup rather than expecting
  a silent role migration.

## Non-goals

- Requiring an external IdP or Redis
- Configuring ABS to trust an upstream issuer (token passthrough)
- OAuth-linking or JIT of the Operator account
- Storing IdP passwords, or requiring a Bookclerk password for SSO-only Users
- Using bookstore OAuth (Audible, etc.) as enterprise SSO
- SCIM/push provisioning in this cut (login-time JIT/sync is enough)
- Spatial/Atmos DRM
- Reopening #116
