# GUI (web + tray companion)

Bookclerk ships a shared React UI for Discover, Wishlist, library, and account
linking, served by `bookclerkd`. The UI talks to the Rust HTTP API only
(TypeScript never hosts the API).

On a graphical session, `bookclerkd` can start an optional **in-process tray**
(library crate `bookclerk-tray`) that opens the UI in the system browser.
There is no separate tray binary:

| OS | Backend |
| --- | --- |
| Linux | StatusNotifierItem (`ksni`, no GTK/WebKit) |
| Windows / macOS | `tray-icon` with default features **disabled** (no GTK) |

Disable with `[daemon] tray = false`, `BOOKCLERK_NO_TRAY=1`, or
`BOOKCLERK_DAEMON_TRAY=0`. Headless hosts (no display / session bus) skip the
tray automatically.

### Desktop shell path

The product GUI is the authenticated React UI on `bookclerkd`; native wrappers
are optional.

| Path | Status |
| --- | --- |
| Web UI (`bookclerkd` + `ui/`) | Supported |
| In-process tray (`bookclerkd` via `bookclerk-tray` lib → system browser) | Supported on Linux (`ksni`), Windows, and macOS (`tray-icon` without GTK features); skipped on headless hosts |
| Tauri embedded window + tray | Tracked in [#44](https://github.com/Bookclerk/bookclerk/pull/44); blocked until an OSV-clean GTK4 / WebKitGTK 6 graph lands (**no** advisory ignores, **no** OSV path excludes) |

Monitor upstream: [tauri#12563](https://github.com/tauri-apps/tauri/issues/12563),
[tauri#14684](https://github.com/tauri-apps/tauri/pull/14684).

Rejected shortcuts:

- Merge GTK3/WebKitGTK 4.1 Tauri into the root (or nested) lockfile and ignore
  or exclude the advisories — not acceptable for a shipped binary
- Win/macOS-only Tauri packaging that still records GTK in a lockfile kept out
  of OSV — same bar as ignores (see [#46](https://github.com/Bookclerk/bookclerk/pull/46))

## Auth (operator + users)

The SPA supports operator and first-party user sessions:

| Role | How to sign in | Capabilities |
| --- | --- | --- |
| **Operator** | Paste operator token (`bookclerk daemon token`) **before** an Owner exists; afterwards the system tray **Copy sign-in link**, `bookclerk login`, or Owner elevate | Full library, scan/acquire, jobs, Discover, Wishlist, Settings (daemon/plugins/confinement), impersonate; **cannot** connect bookstore sources. SPA operator-token paste is refused once an active Owner exists (`POST /api/auth/login` → 403). Bearer API and loopback `GET /api/auth/tray-handoff?code=` (default 180s one-time code, never the durable token) keep working. |
| **Owner** | Invite magic link / password / passkey / SSO / integration login | Administrator powers + **elevate** to operator (IdP step-up, passkey, or password). Server/Plugins + impersonation |
| **Administrator** | Invite magic link / password / passkey / SSO / integration login | Member powers + acquire/scan/jobs; provision users (no elevate) |
| **Host** | Invite magic link, password, passkey, SSO, or integration return-visit | Member powers + **Status** and **Settings → Nodes** for the nodes they own (budgets, drain, name) |
| **Member** | Invite magic link, password, passkey, SSO, or integration return-visit | Discover, Wishlist, shared library browse, Accounts (store connect); Settings Account (Profile / Security / Sessions) |

| Item | Detail |
| --- | --- |
| Operator token | `encrypted_secrets` (or `BOOKCLERK_OPERATOR_TOKEN` override); `bookclerk daemon token` prints it. Tray **Copy sign-in link** and `bookclerk login` mint a loopback `?code=` URL (default 180s, then the tray removes that exact URL if still present) |
| Operator cookie | `bookclerk_operator_session` (`Path=/`) — also used for elevated Owner sessions |
| Portal cookie | `bookclerk_portal_session` (`Path=/`) — federation session bound to a first-party user |
| Portal APIs | `/api/portal/*` (SPA Accounts / claim redeem) |
| User admin APIs | `GET`/`POST /api/users`, `PATCH /api/users/{id}`, `POST /api/users/{id}/claim-ticket` (provisioner: operator, owner, or administrator) |
| Profile | `PATCH /api/auth/profile` `{ display_name?, email?, avatar_source? }`; `PUT`/`DELETE /api/auth/profile/avatar`; `GET /api/users/{id}/avatar` (any signed-in user) |
| Elevate | `POST /api/auth/elevate` `{ password }` / `DELETE /api/auth/elevate`; `GET /api/auth/oidc/elevate`; `POST /api/auth/passkeys/elevate/*` (Owner only) |
| SSO | `GET /api/auth/oidc/providers` / `login` / `GET`+`POST /callback` (optional `[auth.oidc]`); Owner/operator `GET`/`PUT /api/auth/oidc/config` (providers plus `public_origin` / `detected_origin` / issuer URLs) and `GET`/`POST`/`PUT`/`DELETE /api/auth/oidc/clients` (Bookclerk-as-IdP clients) |
| Sign-in picker | Public `GET /api/auth/signin` — `{ operator_token, oidc, integrations }`. SPA shows username/password first; SSO and integration credential logins as buttons beneath. Social SSO buttons use each vendor’s official mark, colors, and copy (**Continue with {Brand}**), switching with the **resolved app theme** (not the OS hint alone). **Light:** Google’s 2025 gradient Super G on a white `#FFFFFF` button with `#747775` stroke, `#1F1F1F` Google Sans Medium; Sign in with Apple black; GitHub Invertocat on `#1F2328`; Discord Clyde on Blurple `#5865F2`. **Dark:** Google `#131314` fill / `#8E918F` stroke / `#E3E3E3` label (G stays the gradient Super G on a white tile); Apple white with black logo and title; GitHub black Invertocat on white; Discord white Clyde on black. Invite claim is `/invite?ticket=` only — other paths (including `/discover?ticket=`) ignore `ticket` and strip it from the URL. Operator-token paste is included only when `operator_token` is true (no active Owner). |
| Passkeys | `GET`/`POST`/`DELETE /api/auth/passkeys…` (login + register + elevate). Register-finish accepts optional `name` (max 80 chars; empty stores as unlabeled and the UI shows `Passkey`). Relying party is `integrations.public_origin`, else a bound loopback request origin, else `http://localhost:8787`. Loopback IPs are rewritten to `localhost` — WebAuthn RP IDs cannot be `127.0.0.1`. Open the SPA via `http://localhost:…` (tray default), not the raw IP. Production hosts must pin `public_origin`. The SPA aborts a hanging WebAuthn prompt after 45s and fails fast when `PublicKeyCredential` is missing, so browsers without passkeys can fall back to a password. |
| TOTP | `POST /api/auth/totp/enroll/begin` / `finish`, `GET`/`DELETE /api/auth/totp`, `POST /api/auth/totp/login`. Password login returns `{ mfa: { method: "totp", challenge_id } }` instead of a session cookie when TOTP is enabled. Passkey sign-in does not require a TOTP code. Enroll UI shows a QR, a copyable setup key, and an `otpauth://` “Open in authenticator app” link. |
| MFA policy | Owner/operator `GET`/`PUT /api/auth/mfa-policy` `{ require_second_factor }`. Also `[daemon.auth] require_second_factor` / `BOOKCLERK_DAEMON_AUTH_REQUIRE_SECOND_FACTOR`. When true, password login requires TOTP, or the user must sign in with a passkey; users with neither still get a session so they can enroll. The SPA shows a blocking dialog (set up a passkey or authenticator, or log out and finish later). Logging out does not lock the account. Failed password login returns `{ error: "invalid_credentials", message: "Invalid login or password." }` rather than the operator-token 401 copy. |
| Bootstrap | `POST /api/auth/bootstrap` (operator; once when no owners exist) |
| Config | `[daemon.auth]` |
| User prefs (DB) | `GET` / `PATCH /api/preferences` — `default_view`, `disabled_shelves`, `theme` (`system` / `light` / `dark`; subject `user:{id}` or `operator`) |

`GET /api/auth/me` returns
`{ authenticated, role, default_view, can_acquire, elevated, impersonating?, portal?, user?, second_factor? }`
with `role` of `operator` | `owner` | `administrator` | `member`, optional first-party
`user`, and `default_view` from the caller's preferences row. Portal users include
`second_factor: { required, totp, passkey_count, enrolled }`.

## Client routes

The SPA keeps the URL bar in sync with the active screen via the History API:

| Path | View |
| --- | --- |
| `/` | Uses the signed-in user's `default_view` (then rewrites to that path) |
| `/discover` | Discover |
| `/library` | Library |
| `/wishlist` | Wishlist |
| `/accounts` | Accounts |
| `/settings` | Settings |
| `/invite` | Claim magic-link (`?ticket=`). Missing, invalid, expired, or already-used tickets return branded **400** / **410** HTML instead of the accept-invite form. Invite HTML is `Cache-Control: no-store`. `?ticket=` on any other path (including `/discover`) does not open the accept-invite form. After a successful claim the SPA replaces the URL with `/` (ticket query dropped) so `default_view` applies; it does not force `/discover`. |

`bookclerkd` serves `index.html` for those document paths (assets still come
from `ui/dist`), except `/invite` which inspects `?ticket=` first. A hard
refresh on `/library` therefore loads the SPA, not a 404. Unknown paths remain
branded 404s.

## Default view

After auth on `/`, the SPA opens the signed-in user's **`default_view`** (default
**`discover`**). Deep links like `/library` win over the default. The header logo
goes to that start page (same as `/`). Change the
default via the Preferences control in the header (any signed-in page), or:

```http
PATCH /api/preferences
Content-Type: application/json

{ "default_view": "library" }
```

Values: `discover` | `wishlist` | `library` | `accounts`. Stored in
`user_preferences` (subject `operator` or `portal:{identity_id}`), not
`config.toml`.

## Appearance

The SPA is designed in **light**. Dark is an adaptation of the same ink / teal /
brick / parchment palette. Preference is `system` | `light` | `dark` (default
**`system`**): System follows `prefers-color-scheme`, and uses light when the
OS hint is missing, `no-preference`, or not explicitly dark. An explicit Light
choice stays light on a dark OS. The sign-in screen stores the choice in
`localStorage` (`bookclerk-theme`); after sign-in it roams via
`PATCH /api/preferences` `{ "theme": "dark" }` on the same preferences row.
Branded daemon HTML (invite / 404) cannot read that row and only queries
`prefers-color-scheme`.

## Screens

- **Discover** — Netflix-style shelves with progressive horizontal scroll; top
  multi-store catalog search with autocomplete (wishlist from suggestions or
  cards). Live storefront prices are viewport-gated and batched; `best` prefers
  the caller’s linked accounts when priced. No approval/triage UI. **On the
  wishlist** cards show tiny profile pictures for the people who wishlisted
  each title.
- **Wishlist** — personal open wishes (un-wishlist removes your row and lowers
  / drops the shared queue entry) plus a sidebar **global queue** ranked by
  overall / operator Discover taste with a heavy multi-user wish boost
  (shared order for every viewer; store-agnostic; no approval flow). Each
  queue row shows tiny profile pictures for the people who wishlisted it.
- **Library** — vertical infinite scroll; operators see acquire/scan; all
  authenticated roles share the full library for browsing (store connect stays
  User-only via Accounts)
- **Accounts** — link bookstore sources, revoke connections, manage portal identity
  connections (invite magic link; SSO and integration buttons on the sign-in screen)
- **Settings** —
  - **Account** (all roles): Profile (display name, email, picture; role is shown, sequential user ids are not), Security (password, named passkeys, authenticator-app TOTP, linked IdPs, Owner elevate), Sessions. Profile defaults to a rendered view (avatar, name, role, email) with hover edit icons; name and email save on blur / Enter (Escape cancels). Security and profile edits are hidden/disabled while impersonating. `PATCH /api/auth/profile` and `PUT`/`DELETE /api/auth/profile/avatar` are self-service (portal or elevated Owner). Clicking the picture opens a chooser (monogram, Gravatar when an email is set, SSO-provider pictures, upload). Auto-resolve prefers a stored upload, then the last-used SSO picture, then Gravatar (`https://www.gravatar.com/avatar/{sha256}?d=404`), then the monogram. Uploads are JPEG/PNG/WebP under `$BOOKCLERK_FILES_DIR/avatars/{id}.{ext}` served at `GET /api/users/{id}/avatar`. `users.avatar_source` stores an explicit choice (`auto` / `monogram` / `gravatar` / `upload` / `sso:{id}`).
  - **User Management** (operator, owner, or administrator): bootstrap first
    Owner, create users (email + copyable invite magic link), role/status,
    remint invite, reset password. Role options follow the provisioner matrix
    (Administrator → Members only; Owner → Members, Hosts, and Administrators;
    Operator / elevated Owner → all roles). Presence on each row: dashed grey
    ring (never signed in), solid grey (last seen, session expired),
    brick/orange (signed in but idle), teal (active in the last ~5 minutes),
    logo waveform (unfinished listening updated in that window, typically from
    a connected player integration), and a brick X when the account is
    disabled. `users.last_seen_at` is durable across logout.
  - **Sign-in** (operator or Owner): password second-factor policy (`require_second_factor`: TOTP or passkey), then **SSO into Bookclerk** (Google/GitHub/Apple/Discord or a custom IdP — Bookclerk is the client; collapsible branded cards), then **Bookclerk as identity provider** (empty `public_origin` uses a bound loopback origin — `localhost` in tray/`cargo dev`; production behind TLS must pin `https://…` — plus copyable discovery URLs and operator-managed OIDC clients). Loopback IPs are rewritten to `localhost`. Saving an empty origin *clears* a pin so loopback detection keeps working. Social presets default **Link by verified email** on; custom OIDC stays off. Preset cards ask for a client ID and secret; Advanced still has group mapping. Custom issuers keep the full OpenID form. Client secrets
    are sealed in
    `encrypted_secrets` (never shown again). Each IdP client card has an enable toggle, redirect URIs, public PKCE vs confidential (secret shown once), refresh tokens, and `openid` / `profile` / `email`. Installed player plugins (Audiobookshelf) contribute a read-only client whose callbacks come from that plugin’s server URL, not Bookclerk’s listen port. Custom clients remain fully editable. Port **13378** is Audiobookshelf’s UI, not Bookclerk (`daemon.listen` defaults to **8787**). Administrators cannot change IdP
    settings. Impersonating an Owner keeps this tab (with Owner privileges);
    impersonating a member does not.
  - **Impersonate** (operator / elevated Owner): a top-anchored brick banner
    shows the target name and role (`Impersonating Alex as Owner`) with Stop
    to end. Settings tabs match the impersonated role (User Management for
    owner/administrator, Sign-in for owner). Server/Plugins stay operator-only
    until impersonation ends.
  - **Server / Plugins** (operator or elevated): listen, auth, auto-acquire,
    plugin enablement with branded consent dialog (widen or narrow grants;
    host-capped workerd/disk limits; workerd domain allowlists), isolation +
    jail resource knobs
  - **Library** (operator or elevated): the libraries this server is a
    branch of (owner clusters it joined as a host node), with **Join a
    library** (address, fingerprint for a self-signed library, then a
    sign-in on the library; or "I have an invite") and **Leave library**,
    and each library plugin's request to run here, with a deny checkbox per
    requested domain and binding, a deny-network switch, Approve (for the
    request shown, archive included) and Revoke. A refused approval shows the daemon's reason.
  - **Branches** (operator or Owner): "Let new branches join", the address
    to join, and the self-signed fingerprint a joining operator checks.
    Branch invites are minted with **Add a branch** in User management (and
    each user's own menu), shown once. A fresh install's operator first sees
    a welcome screen: Start a new library, or Join a library. See
    [Libraries and branches](cluster-hosts.md).

**Live updates.** The SPA holds one WebSocket, `GET /api/live`
(subprotocol `bookclerk-live.v1`), that names topics whose data changed
(`library`, `jobs`, `deliveries`, `nodes`, `config`, `consents`, `clusters`,
`users`); Status, Library, Settings → Nodes, User Management and Settings →
Library re-read their routes when told. The socket carries topic names only, requires an
exact `Origin` match and a signed-in session, and is scoped to the caller's
role. While it is down, panels poll on their old timers and the client
reconnects with backoff (1 s to 30 s). The Vite dev server proxies it
(`/api/live` with `ws: true`). Details:
[Libraries and branches](cluster-hosts.md#the-live-socket-apilive).

A future **plugin browser** (install/configure third-party plugins from a
catalog) is sketched in [plugin-registry.md](plugin-registry.md); enablement and
consent today live in Settings plus CLI / `config.toml` ([plugins.md](plugins.md)).

## Run

`cargo dev` (and `cargo build-app --platform`) rebuild `ui/dist` when SPA sources
are newer than the last Vite output. You can still build the SPA by hand:

```bash
cd ui && npm ci && npm run build
export BOOKCLERK_FILES_DIR=/tmp/BookclerkFiles
cargo run -p bookclerkd
# open http://127.0.0.1:8787/
```

Dev (Vite with API proxy):

```bash
# terminal 1
BOOKCLERK_FILES_DIR=/tmp/BookclerkFiles cargo run -p bookclerkd
# terminal 2
cd ui && npm run dev   # http://127.0.0.1:5173
```

Override the static dist directory with `BOOKCLERK_UI_DIST`.

## Tray companion (in-process)

```bash
cd ui && npm ci && npm run build
cargo build -p bookclerkd
BOOKCLERK_FILES_DIR=/tmp/BookclerkFiles cargo run -p bookclerkd
```

Menu: Open Bookclerk · Scan library · Copy sign-in link (removed after the
handoff TTL, default 180s, if still present) · Hide tray. Left-click
opens `http://localhost:<port>/api/auth/tray-handoff?code=…` (no durable token in the URL). The
tray first `POST`s `/api/auth/tray-handoff/prepare` with the durable Bearer token
to mint a hashed one-time code (lifetime `[daemon.auth] tray_handoff_ttl_secs`,
default 180, clamped 30..=900; wildcard `0.0.0.0`/`::` binds still
use `localhost`), then the GET consumes that exact code and sets
`bookclerk_operator_session` on localhost (`Referrer-Policy: no-referrer`).
Copy sign-in link and Open Bookclerk each replace any unused previous code.
Same-host reverse proxies are refused (`Host` must be a single loopback
authority; any `X-Forwarded-*` / `Forwarded` / `Via` / `X-Real-IP` header fails
closed, including empty values). Use the
public origin for Users; Operator UI is localhost (or Owner elevate on the
public hostname). Headless/VPS: SSH tunnel (`ssh -L 8787:127.0.0.1:8787 …`) then
`bookclerk login` and paste the printed URL in a browser that hits loopback. The
tray lives in `bookclerk-tray` (workspace member, not a
`default-member`) and is linked into `bookclerkd`.

`tray-icon` is depended on only for Windows/macOS **and** with
`default-features = false`, so the root `Cargo.lock` does not resolve the Linux
GTK3 graph. Do not enable `tray-icon`/`muda` default features in this workspace.

## Brand assets

Production logo/mark/favicons live under [`assets/brand/`](../assets/brand/).
The UI copies web assets into `ui/public/`. Dark surfaces recolor the wordmark
to parchment (inverted ink); the mark keeps the approved navy / teal / brick /
parchment colors.
