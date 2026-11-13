//! `bookclerk-desktop` — Bookclerk in its own window, with no listener.
//!
//! The process runs the whole host in-process (`bookclerkd::embedded`) on
//! its own Tokio runtime and opens one Tauri window. The window loads the
//! web app from the `bookclerk` custom scheme, and every request on that
//! scheme goes straight to the host's router: no socket is bound, so nothing
//! else on the machine or the network can reach this host's web API.
//!
//! The window shows only pages from that scheme, except while joining a
//! library, when it shows the library's own sign-in pages until the library
//! sends it back (the host decides: `bookclerkd::embedded::Navigation`).
//! The page may call one Tauri command, `live`, for its live updates; the
//! capability in `capabilities/main.json` allows it on the app's own pages only.
//!
//! The window needs the `window` feature, which needs the platform webview's
//! development files to build; without it this binary only says so.

#[cfg(feature = "window")]
mod window;

#[cfg(feature = "window")]
fn main() -> anyhow::Result<()> {
    window::run()
}

#[cfg(not(feature = "window"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!(
        "this bookclerk-desktop was built without its window; build it with `cargo build -p bookclerk-desktop --features window`"
    )
}
