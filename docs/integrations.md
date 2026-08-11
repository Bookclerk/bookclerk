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

## Enabling third-party integrations

External integrations **default to disabled**. Install under
`$BOOKCLERK_FILES_DIR/plugins/<name>/` and enable by id:

```toml
[integrations.echo]
enabled = true
```

See the echo example walkthrough in [plugins.md](plugins.md).
