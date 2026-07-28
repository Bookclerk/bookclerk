# Integrations

Integrations are **outbound** plugins: they react to library events, expose
health/diagnose probes, and optionally participate in the **SPA Accounts /
claim-ticket** flow (external identities for Discover personalization).

First-party today: **Audiobookshelf**. Third-party integrations install as
external plugins ([plugins.md](plugins.md)).

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
HTTP clients stay inside `bookclerk-integrations`.

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
- Optional user watch + SPA credential return-visit login
- Claim tickets bind an external ABS user to a Bookclerk portal identity
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
[`crates/bookclerk-integrations/openapi/PIN.md`](../crates/bookclerk-integrations/openapi/PIN.md).

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

## Enabling third-party integrations

External integrations **default to disabled**. Install under
`$BOOKCLERK_FILES_DIR/plugins/<name>/` and enable by id:

```toml
[integrations.echo]
enabled = true
```

See the echo example walkthrough in [plugins.md](plugins.md).
