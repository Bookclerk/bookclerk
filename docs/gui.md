# GUI (web + tray companion)

Bookclerk ships a shared React UI for library management, served by
`bookclerkd`. The UI talks to the Rust HTTP API only (TypeScript never hosts
the API).

An optional Linux **tray companion** (`bookclerk-tray`) uses StatusNotifierItem
(`ksni`, no GTK/WebKit) to spawn/attach `bookclerkd` and open the UI in the
system browser. An embedded Tauri window/tray remains deferred pending upstream
GTK4 — tracked in [#44](https://github.com/fritz-fritz/bookclerk/pull/44). Path
notes: [gui-desktop-path.md](gui-desktop-path.md).

## Operator auth

The GUI / `/api/*` routes use a separate **operator** credential from the
Connect portal:

| Item | Detail |
| --- | --- |
| Token file | `$BOOKCLERK_FILES_DIR/operator.token` (or `[daemon.auth].token_file`) |
| Env | `BOOKCLERK_OPERATOR_TOKEN` / `BOOKCLERK_OPERATOR_TOKEN_FILE` |
| Browser | `POST /api/auth/login` → HttpOnly cookie `bookclerk_operator_session` |
| CLI / automation | `Authorization: Bearer <token>` |
| Config | `[daemon.auth] enabled`, `token_file`, `session_ttl_hours` |

On first start with auth enabled, `bookclerkd` mints the token file (`0600`) and
prints its path to stderr. Auth is required by default; binding a non-loopback
address with `daemon.auth.enabled = false` is rejected at startup.

Prefer TLS termination at a reverse proxy when exposing the UI remotely.

## Web UI (served by `bookclerkd`)

```bash
cd ui && npm ci && npm run build
export BOOKCLERK_FILES_DIR=/tmp/BookclerkFiles
cargo run -p bookclerkd
# open http://127.0.0.1:8787/ and paste the operator token
```

Dev (Vite with API proxy):

```bash
# terminal 1
BOOKCLERK_FILES_DIR=/tmp/BookclerkFiles cargo run -p bookclerkd
# terminal 2
cd ui && npm run dev   # http://127.0.0.1:5173
```

Override the static dist directory with `BOOKCLERK_UI_DIST`.

### MVP screens

- Operator login
- Dense book rows (cover, title, authors/narrators, series, source, status)
- Search + status filter
- Scan / acquire pending / acquire one title
- Jobs + status strip

## Tray companion (`bookclerk-tray`)

```bash
cd ui && npm ci && npm run build
cargo build -p bookclerkd -p bookclerk-tray
BOOKCLERK_FILES_DIR=/tmp/BookclerkFiles cargo run -p bookclerk-tray
```

Menu: Open Bookclerk · Scan library · Print operator token · Quit. Left-click
opens the browser. Workspace member, not a `default-member`. Non-Linux builds
open the browser only (no tray icon).

## Brand assets

Production logo/mark/favicons live under [`assets/brand/`](../assets/brand/).
The UI copies web assets into `ui/public/`.
