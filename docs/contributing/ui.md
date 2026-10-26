# Web UI development

How the React app in `ui/` is built and served, the in-process tray, and the
brand assets. The roles, sign-in and screens are described in
[Web app and sign-in reference](../server/sign-in.md).

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

## Desktop shell path

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

Production logo/mark/favicons live under [`assets/brand/`](../../assets/brand/).
The UI copies web assets into `ui/public/`. Dark surfaces recolor the wordmark
to parchment (inverted ink); the mark keeps the approved navy / teal / brick /
parchment colors.
