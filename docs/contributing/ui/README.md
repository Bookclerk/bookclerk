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

| Path                                                                                     | Status                                                                                                                                                         |
| ---------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Web UI (`bookclerkd` + `ui/`)                                                            | Supported                                                                                                                                                      |
| In-process tray (`bookclerkd` through the `bookclerk-tray` library → the system browser) | Supported on Linux (`ksni`), Windows and macOS (`tray-icon` without GTK features); skipped on headless hosts                                                   |
| Desktop window (`bookclerk-desktop`, Tauri 2)                                            | Early: runs the web app in its own window with no listener (below). Unsigned packages for Linux, Windows and macOS ([Packages](#packages)); no tray or uploads |

### The desktop window

`bookclerk-desktop` runs the whole host in its own process and shows the
web app in a Tauri window. It binds **no socket**: not `daemon.listen` and
not the node API's TLS listener, so nothing else on the machine or the
network can reach this host's web API.

- The window loads the web app from the `bookclerk` custom scheme
  (`bookclerk://localhost/`; `http://bookclerk.localhost/` on Windows, where
  WebView2 requires that form). Every request on the scheme goes straight to
  the host's router in-process (`bookclerkd::embedded`).
- The window is the person signed in to the computer's OS account (see the
  [ADR](../../adr/desktop-window-identity.md)). Until the library has an
  active Owner it acts as the operator: the host adds the operator token to
  each request itself. After that it acts as the patron whose session the
  branch keeps sealed for its window, and the host adds that patron's
  session cookie instead. Cookies, `Authorization`, `Origin`, `Referer` and forwarding
  headers from the page are dropped, and `Set-Cookie` never reaches the
  page: the host keeps those cookies itself, so the page holds no
  credential.
- The node API (`/api/node/...`) and the live socket (`/api/live`) are
  refused. A custom scheme cannot carry a WebSocket, so the window sends the
  same frames over a Tauri channel instead: the page calls the window's one
  command, `live`, with a channel, and each frame arrives on it as the text
  the socket would have sent (`ui/src/lib/desktopLive.ts`). A later call
  replaces the earlier feed, so a reloaded page doesn't leave one running.
- A request a page from anywhere else sends to the scheme (its `Origin`, or
  for a write its `Referer`, is not the app's) is refused with `403`, since
  the host would otherwise add its own credential to it. Because of that
  check, the window's cookie requests skip the web app's CSRF origin check.
- The window only shows pages from the scheme, with one exception: joining a
  library, or signing in to one with a library card, opens the library's own
  sign-in pages in the window until the library sends it back, and that
  return (to `http://localhost/join/callback` or
  `http://localhost/clerk/callback`) is handed to the app scheme. Navigation
  anywhere else is refused.
- The page may call four Tauri commands: `live`, which takes nothing from
  the page but its channel, `notify`, which posts one system notification
  for a new inbox entry (below), and `player` and `player_events` for the
  window's own audio player (below). The capability in
  `crates/bookclerk-desktop/capabilities/main.json` allows them for the
  main window's own pages only, so the library's sign-in pages, which are
  remote, can't call them, and each command checks the calling webview and
  page again before it runs.
- The custom scheme can't stream audio to the webview either (no `Range`),
  so the window plays books itself (`crates/bookclerk-desktop/src/player.rs`).
  The web app's player drives it through `ui/src/lib/nativePlayer.ts`:
  `player` takes one checked command (load a session's track, play, pause,
  seek, speed, stop, probe a track's length, or what the system's media
  controls show), and `player_events` hands over a channel for the
  position, play, pause, the end, errors and the media controls' buttons.
  The host reads the track through the session's stream route and a
  confined `bookclerk-media-worker pcm` process decodes it. The
  `bookclerk-player-engine` crate changes the speed and the sample rate,
  and `cpal` plays it on the default output device. `souvlaki` drives
  MPRIS (over zbus), the Windows media overlay and Now Playing. See
  [Playback](../../server/playback.md#the-desktop-window).
- The bell's alerts ([Notifications](../../server/notifications.md#the-bell-browser-and-desktop-notifications))
  use the system's notifications in the window: `ui/src/lib/notifications.ts`
  calls `notify` with the entry's title and text, and
  `crates/bookclerk-desktop/src/alert.rs` folds the title to one line,
  trims both to what a notification shows (120 and 500 characters) and
  posts it with `notify-rust` (D-Bus on Linux over the zbus that `souvlaki`
  already brings, the notification center on macOS, a toast on Windows).
  The title and text were bounded by the library's notification rules
  before they reached the inbox.
- These rules (which requests are refused, the headers the router sees,
  where the window may go, who may start the live feed) live in the `bookclerk-window-policy` crate,
  which the `window_policy` fuzz target checks (see
  [Fuzzing](../fuzzing.md)).

#### First run in the window

On a fresh install the window opens on **Sign in to your library**: the
person enters the library's address (with its port, if it has one) and
signs in on the library with their account there, through the same library
card sign-in a server uses (see
[Signing in with a library card](../../server/sign-in.md#signing-in-with-a-library-card)).
The computer becomes a **clerk** of the library: it shows the library's
books, the person's own marked, and takes none of the library's work. It
is not a branch, so nothing the library deploys ever runs there. A clerk
has no patrons of its own, so the window stays the operator of its own
install and keeps no window session.

**Start a library on this computer instead** sets up a library here, as
`bookclerkd`'s first run does. Creating the Owner there signs the window in
as that Owner and keeps that session.

Before the Owner, the window asks for a recovery copy of the library key,
which it keeps in the OS keychain where no backup of the library folder
reaches it (`RecoveryCopySetupStep`). The operator picks a recovery
password and the window downloads `bookclerk-recovery-key.bck`, usually
into the Downloads folder. **Skip for now** shows a warning first; Settings
then reminds the operator (Server Settings) or an Owner (Account) until a
copy is saved. See
[Database → The key in the OS keychain](../../server/database.md#the-key-in-the-os-keychain).

#### Who the window is after setup

Once the library on this computer has an active Owner, the window is a
patron, never the operator by default:

- **The kept session.** The branch seals the patron's session with the
  library's key in its own database (`encrypted_secrets`, `kind =
  window_session`, keyed by the branch's node id), as a clerk keeps its
  library card. The files directory is already this OS account's own, so
  no other file is needed, and branches sharing one database keep separate
  rows.
- **Each launch** swaps that session for a fresh one lasting 90 days. It is
  listed in the patron's **Settings → Account → Sessions** as
  **Desktop app on <computer>**; revoking it there signs the window out at
  its next launch.
- **No kept session, or an ended one**, opens the app's sign-in screen. A
  password sign-in there is swapped for a window session and kept.
- **Elevation and impersonation** work as on the web: an Owner elevates
  with their password, and impersonates from the elevated session. Neither
  changes the kept session.
- **Signing out** deletes the kept session.

Each person on a shared computer uses their own OS account. To share one
library between them, one account's install is the branch and the others
sign in to it with library cards.

Not there yet: the tray, uploads (when
they come, through the native file picker), opening external links in the
system browser, passkeys and sign-in providers (WebAuthn needs a real
origin, and a provider's sign-in leaves the app), and signed packages. Joining or signing in to a library whose node API uses
a self-signed certificate does not work from the window yet: the webview does
not trust that certificate for the sign-in pages. Links the host writes for other
people, such as invite links, still name `integrations.public_origin` or the
`daemon.listen` address, which the window does not serve. Joining a library
as a shared branch finishes the next time the app starts, where `bookclerkd` restarts
itself.

To build and run it:

1. Install the webview's and ALSA's development files. On Debian and Ubuntu:

   ```sh
   sudo apt-get install libwebkit2gtk-4.1-dev libxdo-dev librsvg2-dev libasound2-dev
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
development files. Without it the binary only says how to build it. CI's
[desktop job](../ci.md#desktop-job) runs Clippy with the feature on Linux.

#### Packages

`cargo package-desktop` builds the desktop app's packages for the platform
it runs on. They are **unsigned**, so the system warns before the first
start:

| Platform | Packages                          | Before the first start                                                                                                         |
| -------- | --------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| Linux    | `.deb`, AppImage                  | The `.deb` depends on WebKitGTK 4.1, which the package manager installs; the AppImage needs FUSE 2 (`libfuse2`)                |
| Windows  | NSIS installer (`.exe`), per user | SmartScreen asks to confirm an unknown publisher                                                                               |
| macOS    | `.app` and `.dmg`                 | Gatekeeper blocks an unsigned app: try to open it once, then choose **Open Anyway** under System Settings → Privacy & Security |

To build them:

1. Install the Tauri CLI:

   ```sh
   cargo install tauri-cli --version 2.12.4 --locked
   ```

2. Install what the platform build needs (`capnproto`, and on Linux the
   webview's development files above).

3. Build the packages:

   ```sh
   cargo package-desktop
   ```

   This stages the host's runtime in `target/desktop-runtime/`: the
   `cargo package-platform` bundle (the helpers, `workerd` and the platform
   plugins, without the `bookclerk` and `bookclerkd` binaries the app
   doesn't use) plus `ui/dist`. It then runs `cargo tauri build` with
   `crates/bookclerk-desktop/tauri.packaging.json`, which bundles that folder
   as the app's `runtime/` resource (on macOS,
   `tauri.packaging.macos.json` lists only the 16 to 512 px icons, since the
   `.icns` has no slot for a 1024 px image at 1x). The packages land in
   `target/release/bundle/`.

A Linux package installs the executable in `/usr/bin` and its resources in
`/usr/lib/Bookclerk/`, and a macOS app keeps them in `Contents/Resources/`, so
the helpers can't sit beside the executable as they do for `bookclerkd`. For
the executable named `bookclerk-desktop` only, the host also looks in
`runtime/` in each of those places (beside the executable on Windows):
helpers through `bookclerk_sandbox::helper_search_dirs`, the bundled platform
plugins and `ui/dist` through `bookclerk_sandbox::packaged_runtime_dirs`.
Every candidate is a fixed path relative to the executable, never one from
the environment or the working directory.

The [Desktop packages](../packaging.md#desktop-packages) workflow builds
all three nightly when `main` has moved, on demand, and for a published
release, which they are attached to. Nothing is signed yet.

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

| OS              | Backend                                                 |
| --------------- | ------------------------------------------------------- |
| Linux           | StatusNotifierItem (`ksni`, no GTK or WebKit)           |
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

## The player

The web player is split so that everything with behavior is plain
TypeScript under `ui/src/lib`, tested without a browser:

| Module              | Role                                                                                    |
| ------------------- | --------------------------------------------------------------------------------------- |
| `playback.ts`       | The daemon's playback routes ([Playback reference](../../server/playback.md))           |
| `player.ts`         | The engine: one audio element, one play session, position reports, the sleep timer      |
| `playerTimeline.ts` | One timeline over an edition's files, clock text, speeds                                |
| `playTarget.ts`     | Which edition of a work plays; the Continue listening shelf                             |
| `mediaSession.ts`   | The browser's Media Session: lock screen, OS media panel, media keys                    |
| `webPlayer.ts`      | The page's one engine, wired to `fetch`, `Audio` and `localStorage`, and its React hook |
| `nativePlayer.ts` | The desktop window's engine and system media controls behind the same interfaces |

`Player` takes its routes, clock and audio element as `PlayerDeps`, so
`player.test.ts` drives it with a fake element. The components
(`PlayerBar`, `NowPlaying`, `PlayerControls`, `ContinueListening`) only
render `Player.state` and call its methods.

Rules the engine keeps:

- The audio element streams `GET /api/playback/sessions/{id}/tracks/{index}`
  from the same origin with the session cookie. No token goes in a URL,
  and the app's content security policy needs no exception.
- A report goes out every 15 seconds while playing, on pause and at the
  end of the book. Reports for one session go out one at a time, and
  never wait on a report for an earlier session.
- Stopping, signing out, starting or stopping impersonation, and leaving
  the page close the session. The close on page hide is a `keepalive`
  request. A change of identity waits up to three seconds for the close
  (`beforeSignOut` in `api.ts`) and then goes ahead. The registered work
  runs a second time within the same wait, or without a wait once it has
  run out, so a book started meanwhile is closed as the identity being
  left. From that close until the identity request has answered, the
  player starts nothing (`Player.retire` and `Player.readmit`).
- `409 session_ended` on a report, or a stream error, opens one new
  session at the place the listener is, and a report the ended session
  refused is sent once to the new one. A second stream error within a
  minute closes the session and is shown to the listener instead; play
  then opens a new session at the place the listener reached, whatever
  the server last stored. A stream error while a replacement is already
  opening joins that replacement.
- Listened time is counted once. A report holds the time it carries until
  the server answers: an accepted report takes it out of the count, even
  when the session was replaced meanwhile, and a failed one gives it back
  for the next report. A close waits up to two seconds (`CLOSE_WAIT_MS`)
  for reports still on their way, so it carries the time of one that
  failed; the `keepalive` close on page hide cannot wait and assumes they
  were accepted, and a close still waiting when the page hides is sent at
  once the same way. `stop()` also waits for closes sent earlier, so the
  wait before a change of identity covers them.
- Playing again a book played to its end opens a new session, because
  only opening one marks a finished edition as in progress again. That
  holds after a seek back from the end too: the new session goes on from
  the place sought.
- The next book waits for the close of the one before it, but no longer
  than five seconds (`SWITCH_WAIT_MS`).
- A request to play that reads first (`playWork`) asks `Player.claim()`
  whether it is still the listener's latest choice. Choosing, playing and
  stopping a book replace a choice; a session that fails does not.
- An edition in several files has every file's length read from its
  stream before a position is placed, because the server does not store
  per-file lengths yet.

## Brand assets

The production logo, mark and favicons live under
[`assets/brand/`](../../../assets/brand/), and the UI copies the web assets
into `ui/public/`.

- Dark surfaces recolor the wordmark to parchment (inverted ink).
- The mark keeps the approved navy, teal, brick and parchment colors.

## Next steps

- [Visual and accessibility testing](visual-testing.md): write a Storybook
  story for a component, and how CI checks it.
- [Web app and sign-in reference](../../server/sign-in.md): the screens and
  routes the UI serves.
- [Build from source and use the CLI](../build-from-source.md): building the
  daemon the UI runs on.

## Library reference

The sections below are generated from `ui/src/lib`, the API client and
helpers the screens share, one page per module.
