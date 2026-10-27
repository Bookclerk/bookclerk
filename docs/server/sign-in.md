# Web app and sign-in reference

This page is the reference for the web app's roles, sign-in methods,
routes and screens. For a plain-language tour, see
[The web app](../guide/web-app.md).

The web app is a React single-page app (SPA) that `bookclerkd` serves. It
covers Discover, Wishlist, the library and account linking. It talks only
to Bookclerk's Rust HTTP API; TypeScript never hosts the API.

## Roles

| Role | Signs in with | Can do |
| --- | --- | --- |
| **Operator** | The operator token, until an Owner exists; then a sign-in link or Owner elevation | Everything on the server, except connect stores |
| **Owner** | Invite link, password, passkey, SSO or integration login | Administrator powers, plus elevation to operator and **Settings → Nodes** for the nodes they own |
| **Administrator** | Invite link, password, passkey, SSO or integration login | Member powers, plus acquire, scan, jobs, adding users and **Settings → Nodes** for the nodes they own |
| **Host** | Invite link, password, passkey, SSO or integration return visit | Member powers, plus **Status** and **Settings → Nodes** for the nodes they own |
| **Member** | Invite link, password, passkey, SSO or integration return visit | Discover, Wishlist, browsing the shared library, **Accounts**, and their own **Settings → Account** |

### The operator

The operator runs the server. They get the full library, scan, acquire,
jobs, Discover, Wishlist, **Settings** (daemon, plugins, confinement) and
impersonation. They **cannot** connect stores, because stores belong to
people.

How the operator signs in:

- **Before an Owner exists**, paste the operator token from
  `bookclerk daemon token`.
- **After that**, the web app refuses the pasted token
  (`POST /api/auth/login` returns 403). Use the tray's
  **Copy sign-in link**, run `bookclerk login`, or sign in as an Owner and
  elevate.

The Bearer API keeps accepting the token. The sign-in link is a loopback
`GET /api/auth/tray-handoff?code=` URL with a one-time code (180 seconds by
default), never the durable token.

### Owners and elevation

An Owner can **elevate** to operator, with an IdP step-up, a passkey or
their password. Elevated, they get **Server** and **Plugins** settings and
impersonation. Administrators can add users but cannot elevate.

## Sign-in methods

### Passwords and second factors

**TOTP** (an authenticator app) is a second factor for password sign-in.

- When TOTP is on, a password sign-in returns
  `{ mfa: { method: "totp", challenge_id } }` instead of a session cookie.
- Passkey sign-in never asks for a TOTP code.
- Enrolling shows a QR code, a setup key to copy, and an `otpauth://`
  **Open in authenticator app** link.

**Requiring a second factor.** Set `require_second_factor` in
**Settings → Sign-in**, in `[daemon.auth]`, or with
`BOOKCLERK_DAEMON_AUTH_REQUIRE_SECOND_FACTOR`. Then:

- A password sign-in needs a TOTP code, or the user signs in with a
  passkey.
- A user with neither still gets a session, so they can enroll. The web
  app shows a blocking dialog: set up a passkey or authenticator, or log
  out and finish later.
- Logging out doesn't lock the account.

A failed password sign-in returns
`{ error: "invalid_credentials", message: "Invalid login or password." }`.

### Passkeys

- Passkeys can have a name of up to 80 characters. An empty name shows as
  `Passkey`.
- The relying party is `integrations.public_origin`. Without one, it is
  the loopback origin the request came in on, and otherwise
  `http://localhost:8787`.
- Loopback IP addresses are rewritten to `localhost`, because a WebAuthn
  relying party can't be `127.0.0.1`.
- The web app gives up on a passkey prompt after 45 seconds, and skips
  passkeys in browsers without `PublicKeyCredential`, so people can fall
  back to a password.

> [!IMPORTANT]
> Open the web app at `http://localhost:…` (the tray does), not at the
> raw IP address. On a production server, set
> `integrations.public_origin`.

### Single sign-on into Bookclerk

People can sign in with Google, GitHub, Apple, Discord or a custom OpenID
Connect provider (configured in `[auth.oidc]` or **Settings → Sign-in**).
Bookclerk is the client.

- The social presets ask only for a client ID and secret, and turn on
  **Link by verified email** by default. **Advanced** adds group mapping.
- A custom provider gets the full OpenID form, with linking by email off.

### Bookclerk as an identity provider

Bookclerk can also be the identity provider for other apps, such as
Audiobookshelf.

- Each client card has an on/off switch, redirect URIs, public (PKCE) or
  confidential (the secret is shown once), refresh tokens, and the
  `openid`, `profile` and `email` scopes.
- Client secrets are sealed in `encrypted_secrets` and never shown again.
- An installed player plugin, such as Audiobookshelf, adds a read-only
  client whose callbacks come from that plugin's server address. Custom
  clients stay fully editable.
- With an empty `public_origin`, Bookclerk uses the loopback origin
  (`localhost` under the tray or `cargo dev`). Saving an empty origin
  clears a pin, so loopback detection keeps working.

> [!NOTE]
> Port **13378** is Audiobookshelf's own web UI. Bookclerk listens on
> **8787** by default (`daemon.listen`).

### The sign-in screen

`GET /api/auth/signin` (public) returns
`{ operator_token, oidc, integrations }`. The screen shows username and
password first, then SSO and integration logins as buttons. The
operator-token box appears only while `operator_token` is true, that is,
while no Owner is active.

The SSO buttons use each brand's own mark, colors and wording
(**Continue with {Brand}**), and follow the app's resolved theme rather
than the OS hint alone:

| Brand | Light theme | Dark theme |
| --- | --- | --- |
| Google | 2025 gradient Super G on white `#FFFFFF`, `#747775` stroke, `#1F1F1F` Google Sans Medium | `#131314` fill, `#8E918F` stroke, `#E3E3E3` label; the G stays on a white tile |
| Apple | Black button | White button, black logo and title |
| GitHub | Invertocat on `#1F2328` | Black Invertocat on white |
| Discord | Clyde on Blurple `#5865F2` | White Clyde on black |

## Sessions and storage

| Item | Detail |
| --- | --- |
| Operator token | Sealed in `encrypted_secrets`, or set by `BOOKCLERK_OPERATOR_TOKEN`. `bookclerk daemon token` prints it |
| Sign-in link | Tray **Copy sign-in link** or `bookclerk login`: a loopback `?code=` URL, valid 180 seconds by default; the tray then removes that URL if it is still on the clipboard |
| Operator cookie | `bookclerk_operator_session` (`Path=/`), also used by elevated Owners |
| Portal cookie | `bookclerk_portal_session` (`Path=/`), the session of a first-party user |
| Config | `[daemon.auth]` |
| Preferences | Stored in the database (`user_preferences`), not `config.toml` |

## API endpoints

| Area | Endpoints |
| --- | --- |
| Who am I | `GET /api/auth/me` |
| First Owner | `POST /api/auth/bootstrap` (operator, once, while no Owner exists) |
| Users | `GET`/`POST /api/users`, `PATCH /api/users/{id}`, `POST /api/users/{id}/claim-ticket` (operator, Owner or Administrator) |
| Profile | `PATCH /api/auth/profile` `{ display_name?, email?, avatar_source? }`; `PUT`/`DELETE /api/auth/profile/avatar`; `GET /api/users/{id}/avatar` (any signed-in user) |
| Elevation | `POST /api/auth/elevate` `{ password }`, `DELETE /api/auth/elevate`, `GET /api/auth/oidc/elevate`, `POST /api/auth/passkeys/elevate/*` (Owner only) |
| SSO | `GET /api/auth/oidc/providers`, `/login`, `GET`/`POST /callback` |
| SSO settings | `GET`/`PUT /api/auth/oidc/config` (providers, `public_origin`, `detected_origin`, issuer URLs; Owner or operator) |
| Bookclerk as IdP | `GET`/`POST`/`PUT`/`DELETE /api/auth/oidc/clients` |
| Passkeys | `GET`/`POST`/`DELETE /api/auth/passkeys…` (sign-in, register, elevate) |
| TOTP | `POST /api/auth/totp/enroll/begin` and `/finish`, `GET`/`DELETE /api/auth/totp`, `POST /api/auth/totp/login` |
| Second-factor policy | `GET`/`PUT /api/auth/mfa-policy` `{ require_second_factor }` (Owner or operator) |
| Preferences | `GET`/`PATCH /api/preferences`: `default_view`, `disabled_shelves`, `theme` (subject `user:{id}`, `portal:{identity_id}` or `operator`) |
| Portal | `/api/portal/*` (Accounts, invite claims) |

`GET /api/auth/me` returns
`{ authenticated, role, default_view, can_acquire, elevated, impersonating?, portal?, user?, second_factor? }`:

- `role` is `operator`, `owner`, `administrator` or `member`.
- `user` is the first-party user, when there is one.
- `default_view` comes from the caller's preferences.
- Portal users also get
  `second_factor: { required, totp, passkey_count, enrolled }`.

## Client routes

The web app keeps the address bar in step with the screen:

| Path | Screen |
| --- | --- |
| `/` | The user's `default_view`; the address then changes to that path |
| `/discover` | Discover |
| `/library` | Library |
| `/wishlist` | Wishlist |
| `/accounts` | Accounts |
| `/settings` | Settings |
| `/invite` | Claim an invite (`?ticket=`) |

`bookclerkd` serves `index.html` for these paths, so a hard refresh on
`/library` loads the app instead of a 404. Assets still come from
`ui/dist`, and unknown paths get a branded 404.

### Invite links

- Only `/invite?ticket=` claims an invite. On any other path, including
  `/discover`, the `ticket` is ignored and dropped from the address.
- A missing, invalid, expired or used ticket gets a branded **400** or
  **410** page instead of the form.
- The invite page is sent with `Cache-Control: no-store`.
- After a claim, the address becomes `/` (without the ticket), so the
  user's `default_view` applies.

## Start page

On `/`, the web app opens the user's **`default_view`**: `discover` (the
default), `wishlist`, `library` or `accounts`. A deep link such as
`/library` wins over it, and the header logo goes to the start page.

Change it with **Preferences** in the header, or:

```http
PATCH /api/preferences
Content-Type: application/json

{ "default_view": "library" }
```

It is stored per user (subject `user:{id}` for a linked user, `portal:{identity_id}` for an
unlinked identity, or `operator`).

## Appearance

The web app is designed in light; dark is an adaptation of the same ink,
teal, brick and parchment palette.

- The theme is `system` (the default), `light` or `dark`.
- **System** follows `prefers-color-scheme`. It uses light unless the OS
  explicitly asks for dark.
- An explicit **Light** stays light on a dark OS.
- The sign-in screen stores the choice in `localStorage`
  (`bookclerk-theme`). After sign-in it roams with
  `PATCH /api/preferences` `{ "theme": "dark" }`.
- Branded daemon pages (invite, 404) can't read preferences, so they
  follow `prefers-color-scheme` only.

## Screens

### Discover

- Shelves that scroll sideways, with catalog search across stores and
  autocomplete. Wishlist a title from a suggestion or a card.
- Live store prices load for visible cards only, in batches. `best`
  prefers the stores the viewer has linked.
- **On the wishlist** cards show small profile pictures of the people who
  want each title.
- There is no approval or triage step.

### Wishlist

- **Your wishes**: removing one removes your row and lowers or drops the
  title in the shared queue.
- **Global queue** (sidebar): one shared order for everyone, ranked by
  Discover taste with a strong boost for titles several people want. It is
  store-agnostic, and each row shows who wishlisted it.

### Library

The full library, in one scrolling list, for every signed-in role.
Operators also see scan and acquire. Connecting stores stays in
**Accounts**.

### Accounts

Link and revoke stores, and manage linked sign-in identities.

### Settings → Account (everyone)

- **Profile**: display name, email and picture. The role is shown;
  numeric user ids are not. Name and email save on blur or Enter, and
  Escape cancels.
- **Security**: password, named passkeys, authenticator app, linked
  identity providers, and Owner elevation.
- **Sessions**.

Profile edits are self-service. Security and profile edits are hidden or
disabled while impersonating.

**Profile pictures.** Clicking the picture offers a monogram, Gravatar
(when there is an email), SSO pictures or an upload. Automatic choice
prefers an upload, then the last SSO picture, then Gravatar
(`https://www.gravatar.com/avatar/{sha256}?d=404`), then the monogram.

- Uploads are JPEG, PNG or WebP, saved as
  `$BOOKCLERK_FILES_DIR/avatars/{id}.{ext}`.
- `users.avatar_source` stores an explicit choice: `auto`, `monogram`,
  `gravatar`, `upload` or `sso:{id}`.

### Settings → User Management

The operator adds the first Owner. The operator, Owners and Administrators
create users with an invite link, change role and status, send a new invite, and
reset passwords.

Who can grant which role:

- An Administrator adds Members only.
- An Owner adds Members, Hosts and Administrators.
- The operator, or an elevated Owner, can grant any role.

Each row shows presence:

| Mark | Meaning |
| --- | --- |
| Dashed grey ring | Never signed in |
| Solid grey | Seen before; session expired |
| Brick (orange) | Signed in, idle |
| Teal | Active in the last 5 minutes or so |
| Logo waveform | Listening progress changed in that time, usually from a player integration |
| Brick X | Account disabled |

`users.last_seen_at` survives logout.

### Settings → Sign-in

For the operator and Owners. In order: the second-factor policy, SSO into
Bookclerk, and Bookclerk as an identity provider (with copyable discovery
URLs and the client cards). Administrators can't change these.

### Impersonating

The operator or an elevated Owner can act as another user. A brick
banner at the top names them (`Impersonating Alex as Owner`), with
**Stop**.

- The Settings tabs follow the impersonated role: User Management for
  Owners and Administrators, Sign-in for Owners.
- **Server** and **Plugins** stay operator-only until impersonation ends.

### Settings → Server and Plugins

For the operator, or an elevated Owner: listen addresses, sign-in,
auto-acquire, isolation and jail limits, and turning plugins on.

Turning on a plugin shows a branded consent dialog. You can widen or
narrow its grants, within the host's workerd and disk caps and its domain
allowlists.

### Settings → Library

For the operator, or an elevated Owner: the libraries this server is a
branch of.

- **Join a library**: the address, the fingerprint for a self-signed
  library, then a sign-in on that library. Or choose **I have an invite**.
- **Leave library**.
- Each library plugin that asks to run here, with a deny checkbox for each
  domain and binding, a switch to deny network, **Approve** (for the
  request shown, archive included) and **Revoke**. A refused approval
  shows the daemon's reason.

### Settings → Branches

For the operator and Owners: **Let new branches join**, the address to
join, and the fingerprint a joining operator checks.

- Branch invites come from **Add a branch** in User Management (and each
  user's own menu), and are shown once.
- On a fresh install, the operator first sees a welcome screen: **Start a
  new library** or **Join a library**.

See [Libraries and branches](cluster-hosts.md).

## Live updates

The web app keeps one WebSocket open: `GET /api/live`, subprotocol
`bookclerk-live.v1`. It names topics whose data changed: `library`,
`jobs`, `deliveries`, `nodes`, `config`, `consents`, `clusters` and
`users`. Status, Library, Settings → Nodes, User Management and
Settings → Library re-read their data when told.

- The socket carries topic names only.
- It needs an exact `Origin` match and a signed-in session, and is
  scoped to the caller's role.
- While it is down, screens poll on their old timers, and the client
  reconnects with backoff from 1 to 30 seconds.
- The Vite dev server proxies it (`/api/live` with `ws: true`).

Details:
[Node API and the live socket](../internals/node-api.md#the-live-socket-apilive).

## Next steps

- [Security checklist](security.md): sign-in settings for a public
  server.
- [Managing plugins](plugins.md): turning plugins on and approving them.
- A future plugin browser is sketched in
  [Plugin registry](../plugins/registry.md). Today, plugins are managed in
  Settings and the CLI; see [Plugins](../plugins/README.md).
