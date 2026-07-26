//! Standalone tray companion: ensure `bookclerkd`, open the web UI in a browser.
//!
//! Linux uses StatusNotifierItem via `ksni` (no GTK/WebKit). Other platforms
//! open the browser and wait for Ctrl+C.

mod daemon;
mod icon;

#[cfg(target_os = "linux")]
mod linux_tray;

use bookclerk_config::Config;
use daemon::DaemonHandle;

fn main() -> anyhow::Result<()> {
    let config = Config::load(None, None)?;
    let daemon = DaemonHandle::ensure(&config)?;
    daemon.open_ui()?;

    #[cfg(target_os = "linux")]
    {
        linux_tray::BookclerkTray::new(daemon, config).run()?;
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = config;
        eprintln!(
            "bookclerk-tray: StatusNotifier tray is Linux-only; browser opened at {}. Ctrl+C to exit.",
            daemon.base_url
        );
        wait_forever();
    }

    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn wait_forever() {
    loop {
        std::thread::park();
    }
}
