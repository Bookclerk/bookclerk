# bookclerk-tray

Linux **StatusNotifierItem** tray companion for the Bookclerk web UI. It does
**not** embed a webview (no GTK3 / WebKitGTK), so it stays clear of the Tauri
Linux advisory pin. See [`docs/gui-desktop-path.md`](../../docs/gui-desktop-path.md)
for the longer-term Tauri GTK4 plan ([#44](https://github.com/fritz-fritz/bookclerk/pull/44)).

## Behavior

1. Load Bookclerk config (`BOOKCLERK_FILES_DIR`, etc.)
2. If `bookclerkd` is not healthy on the configured listen address, spawn it
3. Open the library UI in the **system browser**
4. Show a tray icon with: Open Bookclerk · Scan library · Print operator token · Quit

Non-Linux builds compile and still ensure the daemon + open the browser, but
there is no tray icon (Ctrl+C to exit).

## Run

```bash
cd ui && npm ci && npm run build
cargo build -p bookclerkd -p bookclerk-tray
BOOKCLERK_FILES_DIR=/tmp/BookclerkFiles ./target/debug/bookclerk-tray
```

Requires a session bus with StatusNotifier support (GNOME, KDE, many Wayland
compositors with an SNI host). Workspace member, not a `default-member`.
