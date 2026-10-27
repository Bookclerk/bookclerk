# Web UI development

This page explains how the React app in `ui/` is built and served, how the
in-process tray works, and where the brand assets live. The roles,
sign-in and screens are described in the
[Web app and sign-in reference](../server/sign-in.md).

## Run

`cargo dev` (and `cargo build-app --platform`) rebuild `ui/dist` when the
SPA sources are newer than the last Vite output. You can still build the
SPA by hand:

```sh
cd ui
npm ci
npm run build
cd ..
export BOOKCLERK_FILES_DIR=/tmp/BookclerkFiles
cargo run -p bookclerkd
# open http://127.0.0.1:8787/
```

For development, run Vite with its API proxy in a second terminal:

1. In the first terminal, start the daemon:

   ```sh
   BOOKCLERK_FILES_DIR=/tmp/BookclerkFiles cargo run -p bookclerkd
   ```

2. In the second terminal, start Vite, then open
   `http://127.0.0.1:5173`:

   ```sh
   cd ui
   npm run dev
   ```

Override the static dist folder with `BOOKCLERK_UI_DIST`.

## Desktop shell path

The product GUI is the authenticated React UI on `bookclerkd`. Native
wrappers are optional.

| Path | Status |
| --- | --- |
| Web UI (`bookclerkd` + `ui/`) | Supported |
| In-process tray (`bookclerkd` through the `bookclerk-tray` library → the system browser) | Supported on Linux (`ksni`), Windows and macOS (`tray-icon` without GTK features); skipped on headless hosts |
| Tauri embedded window + tray | Tracked in [#44](https://github.com/Bookclerk/bookclerk/pull/44). Blocked until an OSV-clean GTK4 / WebKitGTK 6 graph lands (**no** advisory ignores, **no** OSV path excludes) |

Watch upstream:
[tauri#12563](https://github.com/tauri-apps/tauri/issues/12563) and
[tauri#14684](https://github.com/tauri-apps/tauri/pull/14684).

Rejected shortcuts:

- Merging GTK3/WebKitGTK 4.1 Tauri into the root (or a nested) lockfile and
  ignoring or excluding the advisories. That isn't acceptable for a shipped
  binary.
- Windows- or macOS-only Tauri packaging that still records GTK in a
  lockfile kept out of OSV. That is the same bar as ignores (see
  [#46](https://github.com/Bookclerk/bookclerk/pull/46)).

## The tray

On a graphical session, `bookclerkd` can start an optional **in-process
tray** (the library crate `bookclerk-tray`) that opens the UI in the
system browser. There is no separate tray binary.

| OS | Backend |
| --- | --- |
| Linux | StatusNotifierItem (`ksni`, no GTK or WebKit) |
| Windows / macOS | `tray-icon` with default features **disabled** (no GTK) |

- Turn it off with `[daemon] tray = false`, `BOOKCLERK_NO_TRAY=1` or
  `BOOKCLERK_DAEMON_TRAY=0`.
- Headless hosts (no display or session bus) skip the tray on their own.
- The tray lives in `bookclerk-tray` (a workspace member, not a
  `default-member`) and is linked into `bookclerkd`.

To run it:

```sh
cd ui
npm ci
npm run build
cd ..
cargo build -p bookclerkd
BOOKCLERK_FILES_DIR=/tmp/BookclerkFiles cargo run -p bookclerkd
```

The menu has **Open Bookclerk**, **Scan library**, **Copy sign-in link**
(removed after the handoff lifetime, 180 s by default, if still present)
and **Hide tray**.

### How the tray signs you in

Left-clicking opens
`http://localhost:<port>/api/auth/tray-handoff?code=…`, with no durable
token in the URL:

1. The tray first `POST`s `/api/auth/tray-handoff/prepare` with the
   durable Bearer token, to mint a hashed one-time code. Its lifetime is
   `[daemon.auth] tray_handoff_ttl_secs`: 180 by default, clamped to
   30–900.
2. The GET consumes that exact code and sets `bookclerk_operator_session`
   on localhost (`Referrer-Policy: no-referrer`).

- Wildcard `0.0.0.0` and `::` binds still use `localhost`.
- **Copy sign-in link** and **Open Bookclerk** each replace any unused
  previous code.

> [!IMPORTANT]
> Same-host reverse proxies are refused. `Host` must be a single loopback
> authority, and any `X-Forwarded-*`, `Forwarded`, `Via` or `X-Real-IP`
> header fails closed, including empty values.

Users use the public origin. The operator UI is on localhost (or Owner
elevation on the public hostname). On a headless host or VPS, open an SSH
tunnel (`ssh -L 8787:127.0.0.1:8787 …`), run `bookclerk login`, and paste
the printed URL into a browser that reaches loopback.

### Keep GTK out of the lockfile

`tray-icon` is a dependency only on Windows and macOS, **and** with
`default-features = false`, so the root `Cargo.lock` doesn't resolve the
Linux GTK3 graph.

> [!WARNING]
> Don't enable the `tray-icon` or `muda` default features in this
> workspace.

## Brand assets

The production logo, mark and favicons live under
[`assets/brand/`](../../assets/brand/), and the UI copies the web assets
into `ui/public/`.

- Dark surfaces recolor the wordmark to parchment (inverted ink).
- The mark keeps the approved navy, teal, brick and parchment colors.

## Next steps

- [Web app and sign-in reference](../server/sign-in.md): the screens and
  routes the UI serves.
- [Build from source and use the CLI](build-from-source.md): building the
  daemon the UI runs on.
