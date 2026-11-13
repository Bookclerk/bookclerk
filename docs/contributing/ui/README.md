# Web UI development

This page explains how the React app in `ui/` is built and served, how the
in-process tray and the desktop window work, and where the brand assets
live. The roles,
sign-in and screens are described in the
[Web app and sign-in reference](../../server/sign-in.md).

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
| Desktop window (`bookclerk-desktop`, Tauri 2) | Early: runs the web app in its own window with no listener (below). Not packaged yet; no tray, live updates or uploads |

### The desktop window

`bookclerk-desktop` runs the whole host in its own process and shows the
web app in a Tauri window. It binds **no socket**: not `daemon.listen` and
not the node API's TLS listener, so nothing else on the machine or the
network can reach this host's web API.

- The window loads the web app from the `bookclerk` custom scheme
  (`bookclerk://localhost/`; `http://bookclerk.localhost/` on Windows, where
  WebView2 requires that form). Every request on the scheme goes straight to
  the host's router in-process (`bookclerkd::embedded`).
- Whoever runs the app is the operator, the same person the tray hands an
  operator session to. The host adds the operator token to each request
  itself; the page never sees it. Cookies, `Authorization`, `Origin`,
  `Referer` and forwarding headers from the page are dropped, and so is
  `Set-Cookie` on the way back, so the page holds no credential.
- The node API (`/api/node/...`) and the live socket (`/api/live`) are
  refused. A custom scheme cannot carry a WebSocket, so screens fall back to
  their polling timers until live updates move to a Tauri channel.
- A request a page from anywhere else sends to the scheme (its `Origin`, or
  for a write its `Referer`, is not the app's) is refused with `403`, since
  the host would otherwise add the operator token to it.
- The window only shows pages from the scheme, with one exception: joining a
  library opens the library's own sign-in pages in the window until the
  library sends it back, and that return (to `http://localhost/join/callback`)
  is handed to the app scheme. Navigation anywhere else is refused, and no
  page gets Tauri commands (the capability set is empty).

#### First run in the window

On a fresh install the window opens on **Join your library**: the person
enters the library's address (with its port, if it has one) and signs in on
the library as a member, through the same join flow a server uses (see
[Branches](../../guide/branches.md)). The library must let new branches join.
**Host a library on this computer instead** sets up a library here, as
`bookclerkd`'s first run does.

A computer that joins from the window is a **window** of the library: it shows
the person's books and downloads nothing. Turning on **Use this computer to
download** in Settings → Library makes it a branch that downloads their own
books.

Not there yet: the tray, live updates over a Tauri channel, uploads (when
they come, through the native file picker), opening external links in the
system browser, passkeys (WebAuthn needs a real origin), impersonation (it
needs a session cookie), and packaging. Joining a library whose node API uses
a self-signed certificate does not work from the window yet: the webview does
not trust that certificate for the sign-in pages. Links the host writes for other
people, such as invite links, still name `integrations.public_origin` or the
`daemon.listen` address, which the window does not serve. Joining a library
as a clerk finishes the next time the app starts, where `bookclerkd` restarts
itself.

To build and run it:

1. Install the webview's development files. On Debian and Ubuntu:

   ```sh
   sudo apt-get install libwebkit2gtk-4.1-dev libxdo-dev librsvg2-dev
   ```

   Windows (WebView2) and macOS (WKWebView) need nothing extra.

2. Build the platform once (`cargo build-app --platform`) so the helpers,
   platform plugins and `ui/dist` are in place, as for `bookclerkd`.

3. Build and run the window. It reads the same `BOOKCLERK_FILES_DIR` and
   `BOOKCLERK_CONFIG` as `bookclerkd`; don't run both on one files
   directory at once.

   ```sh
   cargo run -p bookclerk-desktop --features window
   ```

The window is behind the crate's `window` feature, off by default, so
`cargo build --workspace` and CI's workspace checks don't need the webview's
development files. Without it the binary only says how to build it.

### Advisories the desktop window brings in

On Linux, Tauri 2 (tao, wry, webkit2gtk and muda) is built on gtk-rs 0.18,
which adds two OSV findings to the root `Cargo.lock`. Both are ignored in
[`osv-scanner.toml`](../../../osv-scanner.toml) with their reasons, until
2026-12-23 like the other entries:

- `RUSTSEC-2024-0429` (alias `GHSA-wrw7-89jp-8q8g`): unsound iterators on
  `glib::VariantStrIter`. Nothing in the graph calls
  `Variant::array_iter_str()`, the only way to get one.
- `RUSTSEC-2024-0370`: `proc-macro-error` is unmaintained. It is a build-time
  dependency of `glib-macros` 0.18 and is not linked into any binary.

The gtk-rs 0.18 "unmaintained" advisories that used to block this window
were withdrawn on 2026-08-14. Take both entries out once tao, wry,
webkit2gtk and muda move to gtk 0.19, or the window moves to Tauri's GTK4 /
WebKitGTK 6 backend
([tauri#12563](https://github.com/tauri-apps/tauri/issues/12563),
[tauri#14684](https://github.com/tauri-apps/tauri/pull/14684)), which is the
preferred target.

> [!NOTE]
> This reverses the earlier rule of no advisory ignores for the window. That
> exception covers only these two findings. The desktop window stays in the
> root lockfile, and OSV path excludes are still not allowed: a lockfile kept
> out of OSV hides its findings rather than explaining them.

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

### Keep GTK out of the tray

`tray-icon` is a dependency only on Windows and macOS, **and** with
`default-features = false`, so the tray (and so `bookclerkd`) never links
the Linux GTK3 graph. Only the desktop window brings gtk-rs into the root
`Cargo.lock` (see
[Advisories the desktop window brings in](#advisories-the-desktop-window-brings-in)).

> [!WARNING]
> Don't enable the `tray-icon` or `muda` default features in the tray.

## Brand assets

The production logo, mark and favicons live under
[`assets/brand/`](../../../assets/brand/), and the UI copies the web assets
into `ui/public/`.

- Dark surfaces recolor the wordmark to parchment (inverted ink).
- The mark keeps the approved navy, teal, brick and parchment colors.

## Next steps

- [Web app and sign-in reference](../../server/sign-in.md): the screens and
  routes the UI serves.
- [Build from source and use the CLI](../build-from-source.md): building the
  daemon the UI runs on.

## Library reference

The sections below are generated from `ui/src/lib`, the API client and
helpers the screens share, one page per module.
