# Desktop shell path

Bookclerk’s product GUI is the authenticated React UI on `bookclerkd`. Native
wrappers are optional.

| Path | Status |
| --- | --- |
| Web UI (`bookclerkd` + `ui/`) | Supported |
| `bookclerk-tray` (Linux StatusNotifier → system browser) | Short-term native affordance (this workspace) |
| Tauri embedded window + tray | Tracked in [#44](https://github.com/fritz-fritz/bookclerk/pull/44); blocked on upstream GTK4 / WebKitGTK 6 (no OSV ignores) |

Monitor upstream: [tauri#12563](https://github.com/tauri-apps/tauri/issues/12563),
[tauri#14684](https://github.com/tauri-apps/tauri/pull/14684).

Do **not** merge GTK3/WebKitGTK 4.1 shells or advisory ignores for that stack.
Idento-style “accept RUSTSEC-2024-0429” is insufficient for Bookclerk because
modern Wayland-default distros are a poor GTK3 ship target — see discussion on
[#44](https://github.com/fritz-fritz/bookclerk/pull/44).
