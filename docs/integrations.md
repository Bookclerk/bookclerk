# Integrations

Integrations are **outbound** plugins: they react to library events, expose
health/diagnose probes, and optionally participate in the **Connect portal**
(claim tickets + credential login for external identities).

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

## Audiobookshelf

```toml
[integrations]
portal_base_path = "/connect"
# public_origin = "https://abs.example.com"   # for shareable ticket URLs

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
- Optional user watch + Connect portal credential login
- Claim tickets bind an external ABS user to a Bookclerk portal identity

OpenAPI pin / coverage notes:
[`crates/bookclerk-integrations/openapi/PIN.md`](../crates/bookclerk-integrations/openapi/PIN.md).

## Connect portal

When `bookclerkd` (or a host that mounts the portal router) is running with
integrations configured, the Connect portal serves under
`integrations.portal_base_path` (default `/connect`).

Flow sketch:

1. Operator mints a claim ticket (`integrations tickets create`).
2. User opens the ticket URL (needs `public_origin` for a shareable link).
3. Ticket redeems into a portal session bound to that external identity.
4. Optional return visits use integration credential login when
   `allow_credential_login` is enabled.

Portal HTML/routes live in `bookclerk-integrations` (`portal` module). Keep the
daemon listen address trusted — the control plane itself is unauthenticated;
portal tickets are the user-facing claim mechanism.

## Enabling third-party integrations

External integrations **default to disabled**. Install under
`$BOOKCLERK_FILES_DIR/plugins/<name>/` and enable by id:

```toml
[integrations.echo]
enabled = true
```

See the echo example walkthrough in [plugins.md](plugins.md).
