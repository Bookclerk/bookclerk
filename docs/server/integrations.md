# Integrations

This page connects Bookclerk to other apps, such as Audiobookshelf. It
also covers Bookclerk acting as the sign-in provider for those apps, and
signing in to Bookclerk with another provider.

## How integrations work

An **integration** is an outbound plugin. It can:

- react to library events, such as a finished acquire;
- report its health through probes;
- take part in the **Accounts** and claim-ticket flow, which links people
  to their identity in another app (used to personalize Discover).

There is no plugin `kind = "integration"`. An integration plugin exports
`remoteLibrary`, declares `[[events.consumers]]`, or both, and may
declare `oidc` and `cli`. See [Plugins](../plugins/README.md).

Third-party integrations install as external plugins; see
[Turning on a third-party integration](#turning-on-a-third-party-integration).

## Commands

| Task | Command |
| --- | --- |
| Show each integration's health | `bookclerk integrations status` |
| Run the probes | `bookclerk integrations test` |
| Probe one integration | `bookclerk integrations test --integration audiobookshelf` |
| Ask it to scan | `bookclerk integrations scan --integration audiobookshelf [--force]` |
| Make a claim ticket | `bookclerk integrations tickets create --provider audiobookshelf --external-user-id <id> [--label "Display name"]` |
| List claim tickets | `bookclerk integrations tickets list` |

These commands go through the `IntegrationRegistry` capabilities. Each
plugin keeps its own HTTP client inside its package.

## Audiobookshelf

The first-party integration is **Audiobookshelf**. It loads two ways: an
in-process `register()` for `cargo run`, and the plugin package in
`crates/bookclerk-plugins/optional/integration-audiobookshelf/`.

The plugin does Audiobookshelf work only: scans, listening sync, sign-in
with Audiobookshelf credentials, and watching users through `pollEvents`.
Claim tickets and the Accounts page are part of Bookclerk itself; the host
polls `pollEvents` and starts those workflows.

### Set it up

The integration is a plugin instance, with its settings in the database.
It runs on a host when its deployment there is `present`.

| Setting | Default | Notes |
| --- | --- | --- |
| `base_url` | none | Required: the Audiobookshelf address, granted to the plugin as its network endpoint |
| `api_key` | none | A secret, sealed in `encrypted_secrets` |
| `library_id` | none | Optional |
| `watch_users` | `true` | Watch Audiobookshelf users for claim tickets |
| `notify_scan_on_acquire` | `true` | Ask Audiobookshelf to scan after an acquire |
| `allow_credential_login` | `true` | Let people sign back in with Audiobookshelf credentials |

1. Set the address:

   ```sh
   bookclerk plugins config set audiobookshelf base_url http://audiobookshelf:80
   ```

2. Enter the API key (read from stdin):

   ```sh
   bookclerk plugins config set audiobookshelf api_key --secret
   ```

3. Deploy it:

   ```sh
   bookclerk plugins deploy audiobookshelf
   ```

The one setting that stays in `config.toml` is the public address, used
as the sign-in issuer and in shareable invite links:

```toml
[integrations]
# public_origin = "https://bookclerk.example.com"
```

### What it does

- Reports health through `integrations status` and `integrations test`.
- On `book_acquired`, can ask Audiobookshelf to scan the library.
- Can watch Audiobookshelf users with `pollEvents`, so the host can mint
  claim tickets.
- Lets people sign back in with Audiobookshelf credentials
  (`oidc.authenticateUser`), when allowed.
- Can sync listening progress (`supports_listening_sync`) into the shared
  `listening_progress` table. Discover uses it when it is there.

### Listening sync

Listening sync is a registry capability, open to any integration:

```sh
bookclerk discover sync-listening
```

The API equivalent is `POST /api/discover/sync-listening`. Every enabled
integration that offers listening sync contributes. Ranking reads only the
shared table and never calls a plugin's client.

API coverage notes:
[`crates/bookclerk-plugins/optional/integration-audiobookshelf/openapi/PIN.md`](../../crates/bookclerk-plugins/optional/integration-audiobookshelf/openapi/PIN.md).

## Claim tickets and Accounts

A **claim ticket** links a person to their identity in another app. With
`bookclerkd` running and an integration configured:

1. The operator makes a ticket with `integrations tickets create`.
2. The person opens the ticket link. A shareable `https://…/?ticket=`
   link needs `integrations.public_origin`.
3. The ticket is redeemed (`POST /api/portal/redeem`) into a session
   (`bookclerk_portal_session`, `Path=/`) tied to that identity.
4. On later visits, the person can sign in with the integration's
   credentials (`POST /api/portal/login/integration`), if
   `allow_credential_login` is on.
5. They manage their store accounts on **Accounts**
   (`/api/portal/sources`, `/api/portal/connections` and others).

Linking accounts and redeeming tickets happen in the web app, on
**Accounts** and the sign-in page. The portal API is `/api/portal/*`.

> [!IMPORTANT]
> Keep the daemon's listen address trusted. The control plane itself
> needs the operator's authentication; claim tickets are only how people
> claim an identity.

## Bookclerk as the sign-in provider

Bookclerk can be an OpenID Connect authorization server, so
Audiobookshelf (or another app) signs people in with their Bookclerk
user, never with the shared operator token.

Discovery document: `GET /.well-known/openid-configuration`.

| Endpoint | Notes |
| --- | --- |
| `/oidc/authorize` | Authorization code with PKCE (`S256`). Needs a user's session; the operator Bearer is refused |
| `/oidc/token` | `authorization_code` and `refresh_token` |
| `/oidc/userinfo` | Takes a Bearer access token |
| `/oidc/revoke` | Revokes a refresh token |

### Register an app

Add each app as an OIDC client in **Settings → Sign-in → OIDC clients**,
or with `GET`/`POST`/`PUT`/`DELETE /api/auth/oidc/clients`.

- Every client has an **Enabled** switch. A disabled client can't
  authorize.
- A client can turn off refresh tokens and limit its scopes to `openid`,
  `profile` and `email`.
- You define custom clients yourself.

### The Audiobookshelf client

When the Audiobookshelf plugin is installed, Bookclerk adds an
`audiobookshelf` public PKCE client. It starts **disabled** until you turn
it on.

- It comes from the plugin's `oidcClients` call, or from
  `[[oidc.clients]]` in `plugin.toml` when the plugin isn't loaded.
- Its redirect URIs are read-only. They are built from the plugin's
  `base_url` plus Audiobookshelf's callback path,
  `/auth/openid/callback`, not from Bookclerk's own port.

### Point Audiobookshelf at Bookclerk

Set Audiobookshelf's OpenID issuer to:

- `integrations.public_origin`, when you set one; or
- the loopback address you open Bookclerk at, such as
  `http://localhost:8787` under the tray or `cargo dev`.

> [!WARNING]
> Off loopback, set `integrations.public_origin`. Bookclerk never uses
> the request's `Host` or `X-Forwarded-*` headers as the issuer.

> [!NOTE]
> Audiobookshelf's web UI is on port **13378**. Bookclerk stays on
> `daemon.listen` (**8787** by default) and never binds 13378.

### Tokens

Access and ID tokens are HS256 JWTs, keyed from the operator token's
material. Rotating the operator token makes existing JWTs stop verifying.

Decision record:
[Plugin-provided OIDC clients](../adr/plugin-oidc-clients.md). Plugin
notes: `crates/bookclerk-plugins/optional/integration-audiobookshelf/`.

## Single sign-on from another provider

Bookclerk can also **use** one or more OIDC or OAuth providers for its own
sign-in, while staying the authorization server for Audiobookshelf.

- Signing in through a provider creates or links a Bookclerk user.
- Audiobookshelf still trusts Bookclerk's issuer; its `sub` is the
  Bookclerk `user_id`.
- The provider's tokens are never passed on.
- The operator is never an OAuth subject: never created, linked or shown
  on the SSO sign-in page.

### Configure providers

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
link_by_email = true
```

Owners (without elevating) and the operator can manage the same settings
in **Settings → Sign-in**. The page writes `[auth.oidc]` and
`integrations.public_origin` to `config.toml`, and keeps client secrets in
`encrypted_secrets` instead of TOML. Administrators can't change them.

### Client secrets

Give each provider's secret in one of these ways:

- the environment variable `BOOKCLERK_OIDC_<ID>_CLIENT_SECRET` (hyphens in
  the id become underscores);
- the sealed store, `encrypted_secrets` (`kind=oidc_client`,
  `name=<provider id>`);
- `client_secret` in TOML, which is redacted from logs.

**Sign in with Apple** also needs `apple_team_id`, `apple_key_id` and a
`.p8` private key (`BOOKCLERK_OIDC_<ID>_APPLE_PRIVATE_KEY`, or the sealed
store). Bookclerk uses them to mint the ES256 client-secret JWT, and
Apple's callback is a `POST` (`form_post`).

The redirect URI is `{effective origin}/api/auth/oidc/callback`. The
effective origin is `integrations.public_origin` when set, and otherwise
the bound loopback address (`localhost` under the tray or `cargo dev`).
Off loopback, set `public_origin`.

### Who gets in

| `provision` | Who gets in |
| --- | --- |
| `mapped_role` | Only people with a mapped group (`owner`, `administrator` or `member`). The default for company providers |
| `any` | Anyone the provider signs in, created as `default_role` (usually `member`). Suits social sign-in at home |
| `allowlist` | People who sign in **and** match the email, domain or `sub` allowlist |
| `invite_only` | Nobody new. SSO only links a user you created first (by `sub` or a unique email) |

- With `mapped_role`, the role syncs at every sign-in (Owner, then
  Administrator, then Member). Demoting the last Owner is blocked, though
  the sign-in still succeeds.
- With `any`, nobody becomes Owner or Administrator unless `role_map` also
  matches.
- Mapping a role to `operator` is rejected.

### Linking by email

A provider identity is keyed by `(provider id, sub)`, stored as
`portal_identities.provider = oidc:{id}`.

`link_by_email` attaches a new `sub` to the one active user with that
email, but only when the provider proves the email is **verified**:
`email_verified`, GitHub's `/user/emails`, Discord's `verified`, or
Apple's ID token.

> [!CAUTION]
> The social presets (Google, GitHub, Apple, Discord) turn
> `link_by_email` on by default in Settings. Custom providers default to
> off, so an unverified email from them can't attach to an existing
> Owner.

### Elevation and sign-out

- An Owner elevates with a provider step-up (`prompt=login` on a linked
  provider), a passkey or a local password. Bookclerk never stores a
  provider password.
- If a provider is down, passkeys are the way in for users. The operator
  token stays the host's break-glass.
- Logging out (`POST /api/auth/logout`) is local; it doesn't end the
  provider's session.

### Endpoints

| Endpoint | Notes |
| --- | --- |
| `GET /api/auth/oidc/providers` | Public list of sign-in buttons |
| `GET`/`PUT /api/auth/oidc/config` | Owner or operator: list or replace providers (secrets redacted). `public_origin` pins the issuer; `detected_origin` is this request's origin, only when it is a bound loopback address |
| `GET`/`POST /api/auth/oidc/clients` | Owner or operator: list or create clients of Bookclerk as provider (a confidential secret is returned once) |
| `PUT`/`DELETE /api/auth/oidc/clients/{id}` | Update or delete a client |
| `POST /api/auth/oidc/clients/{id}/rotate-secret` | Make a new confidential secret (returned once) |
| `GET /api/auth/oidc/login?provider=` | Start SSO (PKCE and nonce; Apple uses `response_mode=form_post`) |
| `GET /api/auth/oidc/elevate?provider=` | Owner step-up (`prompt=login`) |
| `GET`/`POST /api/auth/oidc/callback` | Token exchange, then create or link the user (Apple posts `form_post`) |
| `GET /api/auth/oidc/identities` | The current user's linked providers |
| `GET`/`POST`/`DELETE /api/auth/passkeys…` | Passkeys: register (optional `name`), sign in, elevate |
| `GET`/`PUT /api/auth/mfa-policy` | Owner or operator: require TOTP or a passkey with a password |
| `POST /api/auth/totp/enroll/*`, `GET`/`DELETE /api/auth/totp`, `POST /api/auth/totp/login` | Authenticator-app TOTP |

See [ADR: first-party identity](../adr/first-party-identity.md) and
[Web app and sign-in](sign-in.md).

## Turning on a third-party integration

An external integration enrolls with an `absent` deployment.

1. Install it under `$BOOKCLERK_FILES_DIR/plugins/<name>/`.
2. Approve what it asks for:

   ```sh
   bookclerk plugins approve echo --yes
   ```

3. Deploy it:

   ```sh
   bookclerk plugins deploy echo
   ```

The Echo example walks through this in [Plugins](../plugins/README.md).

## Next steps

- [Web app and sign-in](sign-in.md): roles, passkeys and second factors.
- [Managing plugins](plugins.md): deploy, configure and approve plugins.
- [Security checklist](security.md): before the server goes public.
