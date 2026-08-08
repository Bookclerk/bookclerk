//! In-process system tray for `bookclerkd` (opens the web UI in a browser).
//!
//! - Linux: StatusNotifierItem via `ksni` (no GTK/WebKit)
//! - Windows / macOS: `tray-icon` with default features disabled (no GTK)

mod client;
mod icon;

#[cfg(target_os = "linux")]
mod linux_tray;

#[cfg(any(windows, target_os = "macos"))]
mod native_tray;

pub use client::{SharedTrayConfig, TrayConfig};

/// Run the tray on the current thread until the user chooses Quit tray.
pub fn run_blocking(config: SharedTrayConfig) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux_tray::BookclerkTray::new(config).run()
    }

    #[cfg(any(windows, target_os = "macos"))]
    {
        native_tray::BookclerkTray::new(config).run()
    }

    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = config;
        anyhow::bail!("no tray backend on this OS")
    }
}

/// Start the tray on a dedicated OS thread (safe beside the Tokio HTTP runtime).
pub fn spawn(config: SharedTrayConfig) -> std::thread::JoinHandle<anyhow::Result<()>> {
    std::thread::Builder::new()
        .name("bookclerk-tray".into())
        .spawn(move || {
            if let Err(err) = run_blocking(config) {
                tracing::warn!(error = %err, "bookclerk tray exited with error");
                return Err(err);
            }
            Ok(())
        })
        .expect("spawn bookclerk tray thread")
}
