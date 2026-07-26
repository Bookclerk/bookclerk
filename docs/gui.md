# GUI (web)

Bookclerk ships a shared React UI for library management, served by
`bookclerkd`. The UI talks to the Rust HTTP API only (TypeScript never hosts
the API).

A **native desktop shell / system tray** is intentionally deferred. Candidate
wrappers that pull unmaintained GTK3/`gtk-rs` 0.18 stacks (including current
Tauri on Linux) are not acceptable while OSV/RUSTSEC advisories remain
unfixed. Revisit when a maintained native shell is available (for example a
future Tauri/WebKitGTK stack on maintained bindings, or a carefully scoped Qt
WebEngine shell).

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

## Brand assets

Production logo/mark/favicons live under [`assets/brand/`](../assets/brand/).
The UI copies web assets into `ui/public/`.
