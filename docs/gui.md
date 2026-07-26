# GUI (web + desktop)

Bookclerk ships a shared React UI for library management. The UI talks to the
Rust HTTP API on `bookclerkd` only (TypeScript never hosts the API).

## Status: desktop blocked

The **web UI** (served by `bookclerkd`) is the supported path on mainline
([#43](https://github.com/fritz-fritz/bookclerk/pull/43)).

This branch / [#44](https://github.com/fritz-fritz/bookclerk/pull/44) preserves
the **Tauri desktop shell / system tray** (`bookclerk-desktop`) for tracking and
future updates, but it is **not mergeable** while Tauri’s Linux backend still
pulls the unmaintained GTK3 / `gtk-rs` 0.18 stack (OSV/RUSTSEC advisories). We
intentionally do **not** ignore those findings.

Unblock when upstream ships a maintained Linux path (GTK4 / maintained
bindings, or equivalent) without advisory-pinned crates.

Path evaluation (Tauri GTK4 PRs, idento-style risk acceptance, tray+browser
alternative, rejected options): [gui-desktop-path.md](gui-desktop-path.md).

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

## Desktop (`bookclerk-desktop`) — WIP / blocked

Tauri 2 shell: loads the same UI, shows a tray icon (Show / Hide / Scan / Quit),
spawns `bookclerkd` when the configured listen address is unreachable, and can
inject the operator token for auto-login.

```bash
# Linux deps (Debian/Ubuntu):
#   libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev patchelf
cd ui && npm ci && npm run build
cargo run -p bookclerk-desktop
```

`bookclerk-desktop` is a workspace member but not a `default-members` binary.
Release CI builds CLI + daemon; build the desktop app explicitly when packaging
native installs.

**Do not merge** while `Cargo.lock` still resolves GTK3 / `gtk-rs` 0.18 (or
other unmaintained advisory-pinned deps) via Tauri. Bump Tauri when upstream
clears that path, re-run OSV, then revisit merge.

## Brand assets

Production logo/mark/favicons live under [`assets/brand/`](../assets/brand/).
The UI copies web assets into `ui/public/` at build time in the repo layout.
