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

An embedded Tauri window remains deferred pending an OSV-clean GTK4 graph —
tracked in [#44](https://github.com/fritz-fritz/bookclerk/pull/44). Path notes:
[gui-desktop-path.md](gui-desktop-path.md).

## Auth (operator + users)

The SPA supports operator and first-party user sessions:

| Role | How to sign in | Capabilities |
| --- | --- | --- |
| **Operator** | Paste operator token (`bookclerk daemon token`) | Full library, scan/acquire, jobs, Discover, Wishlist, Settings (daemon/plugins/confinement), impersonate; **cannot** connect bookstore sources |
| **Owner** | Invite magic link / password / integration login | Administrator powers + **elevate** to operator with password reauth (Server/Plugins + impersonation) |
| **Administrator** | Invite magic link / password / integration login | Member powers + acquire/scan/jobs; provision users (no elevate) |
| **Member** | Invite magic link, password, or integration return-visit | Discover, Wishlist, shared library browse, Accounts (store connect); Settings Account (Profile / Security / Sessions) |

| Item | Detail |
| --- | --- |
| Operator token | `encrypted_secrets` (or `BOOKCLERK_OPERATOR_TOKEN` override); tray copies to clipboard |
| Operator cookie | `bookclerk_operator_session` (`Path=/`) — also used for elevated Owner sessions |
| Portal cookie | `bookclerk_portal_session` (`Path=/`) — federation session bound to a first-party user |
| Portal APIs | `/api/portal/*` (SPA Accounts / claim redeem) |
| User admin APIs | `GET`/`POST /api/users`, `PATCH /api/users/{id}`, `POST /api/users/{id}/claim-ticket` (provisioner: operator, owner, or administrator) |
| Elevate | `POST /api/auth/elevate` `{ password }` / `DELETE /api/auth/elevate` (Owner only) |
| Bootstrap | `POST /api/auth/bootstrap` (operator; once when no owners exist) |
| Config | `[daemon.auth]` |
| User prefs (DB) | `GET` / `PATCH /api/preferences` — `default_view`, `disabled_shelves` (subject `user:{id}` or `operator`) |

`GET /api/auth/me` returns
`{ authenticated, role, default_view, can_acquire, elevated, impersonating?, portal?, user? }`
with `role` of `operator` | `owner` | `administrator` | `member`, optional first-party
`user`, and `default_view` from the caller's preferences row.

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
| `/invite` | Sign-in / claim (magic-link ticket in `?ticket=`) |

`bookclerkd` serves `index.html` for those document paths (assets still come
from `ui/dist`). A hard refresh on `/library` therefore loads the SPA, not a
404. Unknown paths remain branded 404s.

## Default view

After auth on `/`, the SPA opens the signed-in user's **`default_view`** (default
**`discover`**). Deep links like `/library` win over the default. Change the
default via the Preferences control in the header (any signed-in page), or:

```http
PATCH /api/preferences
Content-Type: application/json

{ "default_view": "library" }
```

Values: `discover` | `wishlist` | `library` | `accounts`. Stored in
`user_preferences` (subject `operator` or `portal:{identity_id}`), not
`config.toml`.

## Screens

- **Discover** — Netflix-style shelves with progressive horizontal scroll; top
  multi-store catalog search with autocomplete (wishlist from suggestions or
  cards). Live storefront prices are viewport-gated and batched; `best` prefers
  the caller’s linked accounts when priced. No approval/triage UI.
- **Wishlist** — personal open wishes (un-wishlist removes your row and lowers
  / drops the shared queue entry) plus a sidebar **global queue** ranked by
  overall / operator Discover taste with a heavy multi-user wish boost
  (shared order for every viewer; store-agnostic; no approval flow)
- **Library** — vertical infinite scroll; operators see acquire/scan; all
  authenticated roles share the full library for browsing (store connect stays
  User-only via Accounts)
- **Accounts** — link bookstore sources, revoke connections, manage portal identity
  connections (claim ticket / credential login on the sign-in screen)
- **Settings** —
  - **Account** (all roles): Profile, Security (password + Owner elevate), Sessions
  - **User Management** (operator, owner, or administrator): bootstrap first
    Owner, create users (email + copyable invite magic link), role/status,
    remint invite, reset password. Role options follow the provisioner matrix
    (Administrator → Members only; Owner → Members and Administrators;
    Operator / elevated Owner → all roles). Presence / listening is not loaded
    on this list.
  - **Impersonate** (operator / elevated Owner)
  - **Server / Plugins** (operator or elevated): listen, auth, auto-acquire,
    plugin enablement with branded consent dialog (widen or narrow grants;
    host-capped workerd/disk limits; workerd domain allowlists), isolation +
    jail resource knobs

A future **plugin browser** (install/configure third-party plugins from a
catalog) is sketched in [plugin-registry.md](plugin-registry.md); enablement and
consent today live in Settings plus CLI / `config.toml` ([plugins.md](plugins.md)).

## Run

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

Menu: Open Bookclerk · Scan library · Print operator token · Quit. Left-click
opens the browser. The tray lives in `bookclerk-tray` (workspace member, not a
`default-member`) and is linked into `bookclerkd`.

`tray-icon` is depended on only for Windows/macOS **and** with
`default-features = false`, so the root `Cargo.lock` does not resolve the Linux
GTK3 graph. Do not enable `tray-icon`/`muda` default features in this workspace.

## Brand assets

Production logo/mark/favicons live under [`assets/brand/`](../assets/brand/).
The UI copies web assets into `ui/public/`.
