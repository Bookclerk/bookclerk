# bookclerk-desktop

Tauri 2 shell + system tray for the shared React library UI.

## Status: blocked

Do **not** merge while Tauri’s Linux backend still resolves GTK3 / `gtk-rs`
0.18 (OSV/RUSTSEC). This crate is kept so the implementation can be bumped and
re-checked when upstream ships maintained bindings. See [`docs/gui.md`](../../docs/gui.md).

We intentionally do **not** add `osv-scanner.toml` ignores for those advisories.

## Local build

```bash
# Debian/Ubuntu: libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev patchelf
cd ui && npm ci && npm run build
cargo run -p bookclerk-desktop
```

Not a workspace `default-member`; release packaging should build this crate
explicitly.
