# bookclerk-tray

In-process system tray library used by `bookclerkd`. It does **not** embed a
webview (no GTK3 / WebKitGTK / Tauri), so the root workspace OSV gate stays
clear of the advisory-pinned `gtk-rs` 0.18 stack. See
[`docs/gui-desktop-path.md`](../../docs/gui-desktop-path.md).

There is no separate tray binary — start `bookclerkd` on a desktop session and
the tray appears when `[daemon].tray` is enabled (default).

## Platforms

| OS | Tray backend |
| --- | --- |
| Linux | StatusNotifierItem via `ksni` |
| Windows / macOS | `tray-icon` with **default features disabled** (no GTK) + `winit` event loop |

## Run

```bash
cd ui && npm ci && npm run build
cargo build -p bookclerkd
BOOKCLERK_FILES_DIR=/tmp/BookclerkFiles ./target/debug/bookclerkd
```

Disable with `BOOKCLERK_NO_TRAY=1` or `[daemon] tray = false`.
