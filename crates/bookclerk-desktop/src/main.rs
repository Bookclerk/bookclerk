//! `bookclerk-desktop` — Bookclerk in its own window, with no listener.
//!
//! The process runs the whole host in-process ([`bookclerkd::embedded`]) on
//! its own Tokio runtime and opens one Tauri window. The window loads the
//! web app from the `bookclerk` custom scheme, and every request on that
//! scheme goes straight to the host's router: no socket is bound, so nothing
//! else on the machine or the network can reach this host's web API.
//!
//! The window may only show pages from that scheme. Navigation anywhere
//! else is refused, and the window grants the page no Tauri commands (the
//! capability set is empty).
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
