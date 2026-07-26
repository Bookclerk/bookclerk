# bookclerk-tray

System tray companion for the Bookclerk web UI. It does **not** embed a
webview (no GTK3 / WebKitGTK / Tauri), so the root workspace OSV gate stays
clear of the advisory-pinned `gtk-rs` 0.18 stack. See
[`docs/gui-desktop-path.md`](../../docs/gui-desktop-path.md) for the longer-term
Tauri GTK4 plan ([#44](https://github.com/fritz-fritz/bookclerk/pull/44)).

## Platforms

| OS | Tray backend |
| --- | --- |
| Linux | StatusNotifierItem via `ksni` |
| Windows / macOS | `tray-icon` with **default features disabled** (no GTK) + `winit` event loop |

## Behavior

1. Load Bookclerk config (`BOOKCLERK_FILES_DIR`, etc.)
2. If `bookclerkd` is not healthy on the configured listen address, spawn it
3. Open the library UI in the **system browser**
4. Show a tray icon with: Open Bookclerk · Scan library · Print operator token · Quit
5. Left-click the tray icon opens the UI (Linux activate / Win+macOS click)

## Run

```bash
cd ui && npm ci && npm run build
cargo build -p bookclerkd -p bookclerk-tray
BOOKCLERK_FILES_DIR=/tmp/BookclerkFiles ./target/debug/bookclerk-tray
```

Linux requires a session bus with StatusNotifier support (GNOME, KDE, many
Wayland compositors with an SNI host). Workspace member, not a
`default-member`.
