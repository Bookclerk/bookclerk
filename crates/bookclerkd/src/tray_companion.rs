//! Optionally start the in-process system tray when a graphical session is present.

use std::sync::{Arc, Mutex};

use bookclerk_config::{graphical_session_available, Config};
use bookclerk_tray::{SharedTrayConfig, TrayConfig};

use crate::auth::OperatorAuthState;

/// Start the tray companion on a background OS thread when appropriate.
///
/// Returns a shared config handle so the daemon can refresh `base_url` / token
/// after a listen rebind or auth reload. No-ops (returns `None`) when:
/// - `[daemon].tray = false` / `BOOKCLERK_NO_TRAY` / `BOOKCLERK_DAEMON_TRAY=0`
/// - no windowing / session-bus environment (typical systemd/Docker hosts)
pub fn maybe_spawn_tray(config: &Config, auth: &OperatorAuthState) -> Option<SharedTrayConfig> {
    if !config.daemon.tray {
        tracing::debug!("daemon.tray disabled; not starting tray");
        return None;
    }
    if !graphical_session_available() {
        tracing::debug!(
            "no graphical session (DISPLAY/WAYLAND_DISPLAY + session bus); \
             not starting tray"
        );
        return None;
    }

    let base_url = config.daemon.listen.tray_base_url();
    let auth_enabled = auth.enabled;
    let operator_token = if auth_enabled && !auth.token.is_empty() {
        Some(auth.token.clone())
    } else {
        None
    };

    let tray = TrayConfig {
        base_url: base_url.clone(),
        auth_enabled,
        operator_token,
    };

    let shared: SharedTrayConfig = Arc::new(Mutex::new(tray));
    let _handle = bookclerk_tray::spawn(Arc::clone(&shared));

    // Open the browser after a brief delay on a background thread so we never
    // block the Tokio runtime, and so axum::serve has started accepting.
    let open_shared = Arc::clone(&shared);
    std::thread::Builder::new()
        .name("bookclerk-tray-open".into())
        .spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(400));
            match open_shared.lock() {
                Ok(guard) => {
                    if let Err(err) = guard.open_ui() {
                        tracing::warn!(
                            url = %guard.ui_url(),
                            error = %err,
                            "failed to open browser; use the tray menu to retry"
                        );
                    }
                }
                Err(err) => tracing::warn!(error = %err, "tray config lock poisoned on open"),
            }
        })
        .ok();

    tracing::info!(%base_url, "started in-process system tray");
    Some(shared)
}

/// Refresh tray listen URL + operator auth after a successful reload / rebind.
pub fn update_tray_after_reload(
    tray: &SharedTrayConfig,
    config: &Config,
    auth: &OperatorAuthState,
) {
    match tray.lock() {
        Ok(mut guard) => {
            let prev = guard.base_url.clone();
            guard.set_base_url(config.daemon.listen.tray_base_url());
            guard.auth_enabled = auth.enabled;
            guard.operator_token = if auth.enabled && !auth.token.is_empty() {
                Some(auth.token.clone())
            } else {
                None
            };
            if prev != guard.base_url {
                tracing::info!(
                    from = %prev,
                    to = %guard.base_url,
                    "updated tray base URL after listen rebind"
                );
            }
        }
        Err(err) => tracing::warn!(error = %err, "tray config lock poisoned; not updating"),
    }
}
