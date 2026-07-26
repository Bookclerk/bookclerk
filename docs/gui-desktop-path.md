# Desktop shell path evaluation

Decision record for native window + system tray around the shared React GUI
(`ui/` + `bookclerkd`). Companion to [gui.md](gui.md) and tracking PR
[#44](https://github.com/fritz-fritz/bookclerk/pull/44).

## What we need from a “desktop” shell

Bookclerk’s product GUI is already the **authenticated React UI** over HTTP.
A native shell only needs to:

1. Show that UI in a local window (optional if the system browser is acceptable)
2. Provide a **system tray / status-area** affordance (show/hide, scan, quit)
3. Ensure `bookclerkd` is running and can inject the operator token for
   auto-login
4. Package cleanly on Linux, macOS, and Windows without dragging unmaintained
   or EOL Linux UI stacks into the default workspace / OSV gate

The shell must not host the API in TypeScript; Rust (`bookclerkd`) remains the
control plane.

## Constraints that rule options in or out

| Constraint | Implication |
| --- | --- |
| Shared React UI with remote daemon use | Prefer embedding or opening that UI; avoid rewriting screens in iced/egui/Avalonia |
| OSV / RUSTSEC gate; no advisory ignores on mainline | Cannot merge crates.io Tauri 2 Linux graph while it pins GTK3 / `gtk-rs` 0.18 / old `glib` |
| Modern Linux defaults to Wayland | GTK3 + WebKitGTK 4.1 is a shrinking runtime target even when XWayland papers over some apps |
| Tray on Linux | StatusNotifierItem / DBus (`ksni`) is the modern path; classic libappindicator is GTK3-era and incompatible with GTK4 |
| No .NET | Avalonia / WinForms ports are out |

## Upstream Tauri status (monitor these)

| Link | Role |
| --- | --- |
| [tauri#12563](https://github.com/tauri-apps/tauri/issues/12563) | Canonical “upgrade tauri to gtk4-rs” issue (assignee + priority signal) |
| [tauri#14684](https://github.com/tauri-apps/tauri/pull/14684) | Integration PR: GTK4 + WebKitGTK 6.0; depends on draft forks of tao/wry/muda/tray-icon; **open, not merge-ready** (maintainers asked for time / prep; breaking Linux change) |
| [tao#1258](https://github.com/tauri-apps/tao/pull/1258) | Fresher GTK4 tao port (supersedes older drafts; reports real app runs under X11) |
| [wry#1474](https://github.com/tauri-apps/wry/issues/1474) / [wry#1530](https://github.com/tauri-apps/wry/pull/1530) / [wry#1765](https://github.com/tauri-apps/wry/pull/1765) | WebKitGTK 6 / wry side of the stack |
| tray-icon / muda GTK4 + **ksni** | Required for tray once libappindicator drops away |

Community forks of #14684 are reported working (including Wayland for some
testers). That proves feasibility; it does **not** make unofficial patch stacks
a good default dependency for Bookclerk’s workspace lockfile.

## Options considered

### A. Wait for crates.io Tauri on GTK4 / WebKitGTK 6 (recommended destination)

Keep `bookclerk-desktop` on [#44](https://github.com/fritz-fritz/bookclerk/pull/44)
as the implementation to bump when upstream publishes maintained Linux
bindings. Reuse window + tray + daemon spawn + token IPC already written.

**Pros:** Best long-term fit for “thin native shell around existing web UI”;
same stack on all three desktop OSes; matches Tauri’s own fix for
RUSTSEC-2024-0429 / unmaintained GTK3 bindings.

**Cons:** Merge blocked until the dependency train lands and OSV is green
without ignores. Timeline is upstream-driven.

**Unblock checklist:** tao/wry/muda/tray-icon (ksni) released → tauri release
on that graph → `cargo update` → OSV clean → smoke tray + Wayland + X11.

### B. Accept GTK3 advisories now (idento-style) — rejected for mainline

[idento#72](https://github.com/thevladbog/idento/pull/72) documents
RUSTSEC-2024-0429 as accepted risk: vulnerable `glib` API unused, Tauri
themselves ignore it for v2, real fix is GTK4 / v3.

**Why that is a poor fit for Bookclerk:** our problem is not only an audit
checkbox. Shipping a GTK3/WebKitGTK 4.1 shell fights **distro reality**
(Wayland-default sessions, disappearing GTK3/WebKit 4.1 packaging) and would
reintroduce OSV ignores we already refused. Accepting “unused API” risk does
not restore a maintained Linux UI stack.

Optional later: platform-gated packaging (macOS/Windows Tauri now, Linux
deferred) still pulls the GTK3 graph into `Cargo.lock` if the crate remains a
workspace member — so it does not clear the OSV gate unless Linux deps are
feature-stripped from the resolved graph (fragile) or the desktop crate lives
outside the scanned workspace.

### C. Vendor unofficial GTK4 Tauri / tao / wry forks — rejected as default

Possible for a personal build; wrong as Bookclerk’s supported path: multi-repo
patch ownership, release lag, CI matrix cost, and supply-chain review burden
for a thin shell.

### D. Tray companion + system browser (recommended near-term if native UX is urgent)

Small Rust binary: StatusNotifierItem via `ksni` (or OS equivalents), spawn or
attach to `bookclerkd`, open `http://127.0.0.1:…/` with the operator session
(or clipboard/token helper). **No embedded webview** → no GTK3/WebKit pin.

**Pros:** Solves tray + daemon lifecycle without waiting on Tauri; Wayland-friendly
tray path; keeps React UI unchanged; OSV-clean if deps stay modern.

**Cons:** Not a single framed “app window”; feels less “native” than Tauri;
macOS/Windows tray APIs differ (still smaller than a full webview stack).

### E. Electron / CEF / Qt WebEngine

Heavyweight, alternate security/update surface, or LGPL/Qt packaging complexity.
Does not buy enough over waiting for Tauri GTK4 or using the browser for an
MVP tray companion.

### F. Rewrite UI in iced / egui / GPUI / Dioxus native widgets

Duplicates the React investment and splits remote web vs desktop. Dioxus
desktop still bottoms out on wry/WebKit on Linux → same GTK problem today.

## Decision

1. **Supported GUI now:** authenticated web UI via `bookclerkd` ([#43](https://github.com/fritz-fritz/bookclerk/pull/43)).
2. **Preferred eventual native shell:** Tauri once GTK4 + WebKitGTK 6 (+ ksni
   tray) are on crates.io — keep and refresh `bookclerk-desktop` on #44;
   monitor [tauri#12563](https://github.com/tauri-apps/tauri/issues/12563) and
   [tauri#14684](https://github.com/tauri-apps/tauri/pull/14684) (and the
   tao/wry dependency PRs). Do **not** merge GTK3 Tauri; do **not** add OSV
   ignores for that stack.
3. **If we need Linux tray before that lands:** implement option **D** (ksni
   tray + browser) as a separate small crate/PR, rather than shipping GTK3
   webview or vendoring GTK4 forks.
4. **Do not** adopt Electron/CEF/Qt or a second native widget toolkit for the
   library UI unless the web UI strategy itself changes.

## Revisit triggers

- Tauri (or tao+wry) publishes GTK4/WebKitGTK 6 on crates.io and OSV is clean
- [tauri#12563](https://github.com/tauri-apps/tauri/issues/12563) closed with a
  release, or #14684 abandoned without a successor (then re-evaluate D vs other
  embedders)
- Distro packaging drops WebKitGTK 4.1 / GTK3 widely enough that even
  experimental GTK3 builds are useless
