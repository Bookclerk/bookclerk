# Integrations

Integrations are **outbound** plugins: they react to library events, expose
health/diagnose probes, and optionally participate in the **SPA Accounts /
claim-ticket** flow (external identities for Discover personalization).

First-party today: **Audiobookshelf** (dual load — in-process `register()` for
`cargo run`, plus `crates/bookclerk-plugins/integration-audiobookshelf/`).
Third-party integrations install as external plugins ([plugins.md](plugins.md)).

The ABS plugin is **ABS-only** (scan, listening sync, credential auth, user
observation via `event_poll`). Claim tickets and the SPA Accounts portal live
in core Bookclerk; the host polls `event_poll` and kicks off those workflows.

## CLI

```bash
bookclerk integrations status
bookclerk integrations test
bookclerk integrations test --integration audiobookshelf
bookclerk integrations scan --integration audiobookshelf [--force]

bookclerk integrations tickets create \
  --provider audiobookshelf \
  --external-user-id <id> \
  [--label "Display name"]
bookclerk integrations tickets list
```

Host commands go through `IntegrationRegistry` capabilities; adapter-specific
HTTP clients stay inside the ABS plugin package
(`bookclerk-plugin-integration-audiobookshelf`).

Account linking and claim redemption live in the main SPA (`Accounts` nav /
login page). Portal HTTP APIs are at `/api/portal/*`.

## Audiobookshelf

```toml
[integrations]
# public_origin = "https://bookclerk.example.com"   # shareable SPA ?ticket= URLs

[integrations.audiobookshelf]
enabled = true
base_url = "http://audiobookshelf:80"
# Prefer BOOKCLERK_ABS_API_KEY over storing the key in TOML.
# api_key = ""
# library_id = "lib_..."
watch_users = true
notify_scan_on_acquire = true
allow_credential_login = true
```

Typical behavior when enabled:

- Health / diagnose via `integrations status` / `test`
- On `book_acquired`, optionally notify ABS to scan the library
- Optional user watch via `event_poll` — host may mint claim tickets / notify
- SPA credential return-visit login (`authenticate_user`) when allowed
- Optional **listening sync** (`supports_listening_sync`) into the shared
  `listening_progress` table — used by Discover when present, ignored when not

Listening sync is a registry capability, not an ABS-only host path:

```bash
bookclerk discover sync-listening
# POST /api/discover/sync-listening
```

Any enabled integration that advertises listening sync contributes; ranking
reads the generic table and never imports adapter clients.

OpenAPI pin / coverage notes:
[`crates/bookclerk-plugins/integration-audiobookshelf/openapi/PIN.md`](../crates/bookclerk-plugins/integration-audiobookshelf/openapi/PIN.md).

## Claim tickets & Accounts

When `bookclerkd` is running with integrations configured:

1. Operator mints a claim ticket (`integrations tickets create`).
2. User opens the ticket URL (needs `public_origin` for a shareable
   `https://…/?ticket=` link into the SPA login page).
3. Ticket redeems via `POST /api/portal/redeem` into a portal session
   (`bookclerk_portal_session`, `Path=/`) bound to that external identity.
4. Optional return visits use integration credential login
   (`POST /api/portal/login/integration`) when `allow_credential_login` is
   enabled.
5. Linked bookstore accounts are managed on the SPA **Accounts** page
   (`/api/portal/sources`, `/api/portal/connections`, …).

Keep the daemon listen address trusted — the control plane itself is
operator-authenticated; portal tickets are the user-facing claim mechanism.

## OIDC for Audiobookshelf

Bookclerk can act as an OpenID Connect authorization server so Audiobookshelf
(or another client) logs users in with Bookclerk Users — never the shared
operator token.

Discovery: `GET /.well-known/openid-configuration`

| Endpoint | Notes |
| --- | --- |
| `/oidc/authorize` | Auth code + PKCE (`S256`). Requires a **User** portal session (operator bearer is rejected). |
| `/oidc/token` | `authorization_code` / `refresh_token` |
| `/oidc/userinfo` | Bearer access token |
| `/oidc/revoke` | Refresh token revoke |

Register the ABS client (public PKCE) once after bootstrap, e.g. redirect
`http://127.0.0.1:13378/auth/openid/callback` and client id `audiobookshelf`.
Set ABS OpenID issuer to Bookclerk's `integrations.public_origin` (or the
daemon URL). Access/ID tokens are HS256 JWTs keyed from the operator token
material (rotate token → existing JWTs stop verifying).

See also the ABS plugin notes under
`crates/bookclerk-plugins/optional/integration-audiobookshelf/`.

## Optional identity broker (upstream OIDC / social)

Bookclerk can **consume** one or more upstream OIDC or OAuth providers while
remaining the authorization server for Audiobookshelf. Upstream login creates
or links a first-party User; ABS still trusts Bookclerk’s issuer (`sub` =
Bookclerk `user_id`). Upstream tokens are never forwarded.

The **Operator** token is never an OAuth subject (not JIT’d, linked, or shown
on the SSO login page).

```toml
[auth.oidc]
enabled = true

[[auth.oidc.providers]]
id = "corp"
name = "Company SSO"
issuer = "https://idp.example.com/realms/corp"
client_id = "bookclerk"
provision = "mapped_role"
role_claim = "groups"
role_map = { "bookclerk-owners" = "owner", "bookclerk-admins" = "administrator", "bookclerk-users" = "member" }
link_by_email = false

[[auth.oidc.providers]]
id = "google"
name = "Google"
preset = "google"   # google | github | apple | discord
client_id = "....apps.googleusercontent.com"
provision = "allowlist"
default_role = "member"
allowed_email_domains = ["family.example"]
```

Client secrets: `BOOKCLERK_OIDC_<ID>_CLIENT_SECRET` (hyphens → underscores),
`encrypted_secrets` (`kind=oidc_client`, `name=<provider id>`), or
`client_secret` in TOML (redacted from logs). Sign in with Apple also needs
`apple_team_id`, `apple_key_id`, and a `.p8` private key (`BOOKCLERK_OIDC_<ID>_APPLE_PRIVATE_KEY`
or the sealed store) to mint the ES256 client-secret JWT; the callback is
`POST` `form_post`. Redirect URI is
`{integrations.public_origin}/api/auth/oidc/callback`.

Owners (without elevating) and Operators can also manage this from **Settings →
Sign-in**. The UI writes `[auth.oidc]` to `config.toml` and stores client
secrets in `encrypted_secrets` rather than TOML. Administrators cannot change
IdP settings.

| `provision` | Who gets in |
| --- | --- |
| `mapped_role` | Must present a mapped group (`owner` / `administrator` / `member`). Default for enterprise IdPs. Role sync on every login (Owner > Administrator > Member). Last-Owner demote is blocked; sign-in still succeeds. |
| `any` | Any authenticated account; JIT as `default_role` (usually `member`). Social / open homelab. Does not promote to Owner/Admin unless `role_map` also matches. |
| `allowlist` | Authenticated **and** email / domain / `sub` allowlist. |
| `invite_only` | No JIT. SSO only links a pre-created User (`sub` or unique email). |

Primary key is `(provider id, sub)` stored as `portal_identities.provider = oidc:{id}`.
`link_by_email` (default **off**) attaches a new `sub` to the unique matching
active User only when the provider adapter proves a **verified** email
(`email_verified` / GitHub `/user/emails` / Discord `verified` / Apple ID token).
Mapping a role to `operator` is rejected.

Owner elevation uses IdP step-up (`prompt=login` on a linked provider), a
**passkey**, or a local password — never a stored IdP password. Passkeys are
the User-plane hatch when an IdP is down; the Operator token remains host
break-glass. Logout is local (`POST /api/auth/logout`); the upstream IdP
session is not terminated.

| Endpoint | Notes |
| --- | --- |
| `GET /api/auth/oidc/providers` | Public list of enabled login buttons |
| `GET`/`PUT /api/auth/oidc/config` | Owner or Operator: list/replace providers (secrets redacted) |
| `GET /api/auth/oidc/login?provider=` | Start SSO (PKCE + nonce; Apple uses `response_mode=form_post`) |
| `GET /api/auth/oidc/elevate?provider=` | Owner step-up (`prompt=login`) |
| `GET`/`POST /api/auth/oidc/callback` | Token exchange + JIT / link (Apple POST `form_post`) |
| `GET /api/auth/oidc/identities` | Linked IdPs for the current User |
| `GET`/`POST`/`DELETE /api/auth/passkeys…` | WebAuthn register, login, elevate |

See [ADR: first-party identity](adr/first-party-identity.md) and
[gui.md](gui.md).

## Enabling third-party integrations

External integrations **default to disabled**. Install under
`$BOOKCLERK_FILES_DIR/plugins/<name>/` and enable by id:

```toml
[integrations.echo]
enabled = true
```

See the echo example walkthrough in [plugins.md](plugins.md).
