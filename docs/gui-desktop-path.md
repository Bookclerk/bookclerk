# Desktop shell path

Bookclerk’s product GUI is the authenticated React UI on `bookclerkd`. Native
wrappers are optional.

| Path | Status |
| --- | --- |
| Web UI (`bookclerkd` + `ui/`) | Supported |
| `bookclerk-tray` (tray → system browser) | Supported on Linux (`ksni`), Windows, and macOS (`tray-icon` without GTK features) |
| Tauri embedded window + tray | Tracked in [#44](https://github.com/fritz-fritz/bookclerk/pull/44); blocked until an OSV-clean GTK4 / WebKitGTK 6 graph lands (**no** advisory ignores, **no** OSV path excludes) |

Monitor upstream: [tauri#12563](https://github.com/tauri-apps/tauri/issues/12563),
[tauri#14684](https://github.com/tauri-apps/tauri/pull/14684).

## Rejected shortcuts

- Merge GTK3/WebKitGTK 4.1 Tauri into the root (or nested) lockfile and ignore
  or exclude the advisories — not acceptable for a shipped binary
- Win/macOS-only Tauri packaging that still records GTK in a lockfile kept out
  of OSV — same bar as ignores (see [#46](https://github.com/fritz-fritz/bookclerk/pull/46))

The tray companion avoids that by never depending on GTK: Linux uses `ksni`;
Windows/macOS use `tray-icon` / `muda` with **default features disabled**.
