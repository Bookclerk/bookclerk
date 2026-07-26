# bookclerk-desktop

Tauri 2 shell + system tray for the shared React library UI.

## Status: blocked (preferred eventual shell)

Do **not** merge while Tauri’s Linux backend still resolves GTK3 / `gtk-rs`
0.18 (OSV/RUSTSEC), and do not paper over that with advisory ignores. Modern
Wayland-default distros are also a poor fit for a GTK3/WebKitGTK 4.1 ship
target.

This crate is the **preferred destination** once upstream publishes GTK4 +
WebKitGTK 6 (and ksni tray) on crates.io. Until then, the supported GUI is the
web UI on `bookclerkd`. Full options analysis:
[`docs/gui-desktop-path.md`](../../docs/gui-desktop-path.md).

Monitor:

- https://github.com/tauri-apps/tauri/issues/12563
- https://github.com/tauri-apps/tauri/pull/14684

If Linux tray is needed before that lands, prefer a separate ksni tray +
system-browser companion over merging this GTK3 graph or vendoring unofficial
GTK4 forks.

## Local build

```bash
# Debian/Ubuntu: libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev patchelf
cd ui && npm ci && npm run build
cargo run -p bookclerk-desktop
```

Not a workspace `default-member`; release packaging should build this crate
explicitly.
