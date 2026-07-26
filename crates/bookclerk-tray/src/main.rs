//! Standalone tray companion: ensure `bookclerkd`, open the web UI in a browser.
//!
//! - Linux: StatusNotifierItem via `ksni` (no GTK/WebKit)
//! - Windows / macOS: `tray-icon` with default features disabled (no GTK)

mod daemon;
mod icon;

#[cfg(target_os = "linux")]
mod linux_tray;

#[cfg(any(windows, target_os = "macos"))]
mod native_tray;

use bookclerk_config::Config;
use daemon::DaemonHandle;

fn main() -> anyhow::Result<()> {
    let config = Config::load(None, None)?;
    let daemon = DaemonHandle::ensure(&config)?;
    // Keep the tray up even if the first browser open fails so the user can
    // retry via the menu (and still Quit).
    if let Err(err) = daemon.open_ui() {
        eprintln!(
            "bookclerk-tray: failed to open browser at {}: {err} \
             (use tray menu \"Open Bookclerk\" to retry)",
            daemon.base_url
        );
    }

    #[cfg(target_os = "linux")]
    {
        linux_tray::BookclerkTray::new(daemon, config).run()?;
    }

    #[cfg(any(windows, target_os = "macos"))]
    {
        native_tray::BookclerkTray::new(daemon, config).run()?;
    }

    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = config;
        eprintln!(
            "bookclerk-tray: no tray backend on this OS; UI at {}. Ctrl+C to exit.",
            daemon.base_url
        );
        loop {
            std::thread::park();
        }
    }

    Ok(())
}
