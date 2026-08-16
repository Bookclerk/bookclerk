//! Talk to the local `bookclerkd` HTTP API from the tray thread.

use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Configuration for the in-process tray (no child `bookclerkd` process).
#[derive(Debug, Clone)]
pub struct TrayConfig {
    /// Daemon HTTP base URL (e.g. `http://127.0.0.1:8787`).
    pub base_url: String,
    /// When true, operator-token auth is required for tray actions.
    pub auth_enabled: bool,
    /// Operator bearer token used for authenticated daemon calls.
    pub operator_token: Option<String>,
}

/// JSON body from `POST /api/auth/tray-handoff/prepare`.
#[derive(serde::Deserialize)]
struct TrayHandoffPrepareBody {
    /// Single-use loopback handoff code to place on the GET URL.
    code: String,
    /// Seconds until the ticket expires; omitted by older daemons.
    #[serde(default)]
    expires_in_secs: u64,
}

/// Loopback sign-in URL plus the server-advertised clipboard/ticket TTL.
struct PreparedHandoff {
    /// `GET /api/auth/tray-handoff?code=` URL (never includes the durable token).
    url: String,
    /// Seconds to keep the URL on the clipboard (and until the ticket dies).
    expires_in_secs: u64,
}

/// Shared tray config so the daemon can refresh `base_url` after listen rebinds.
pub type SharedTrayConfig = Arc<Mutex<TrayConfig>>;

/// Fallback clipboard TTL when prepare omits `expires_in_secs` (matches daemon default).
const SIGN_IN_LINK_CLIPBOARD_TTL_SECS: u64 = 180;

impl TrayConfig {
    /// Builds an HTTP base URL from a daemon listen address.
    ///
    /// # Arguments
    ///
    /// * `listen` - `host:port` or an absolute `http(s)://` URL (trailing `/` stripped).
    ///
    /// # Returns
    ///
    /// Absolute base URL with an `http://` scheme when `listen` was host-only.
    #[must_use]
    pub fn base_url(listen: &str) -> String {
        let listen = listen.trim().trim_end_matches('/');
        if listen.starts_with("http://") || listen.starts_with("https://") {
            listen.to_string()
        } else {
            format!("http://{listen}")
        }
    }

    /// Open the UI via a loopback handoff URL that sets the session cookie.
    ///
    /// The durable operator token is never placed in the URL. [`Self::open_ui`]
    /// POSTs `/api/auth/tray-handoff/prepare` with Bearer first, then opens the
    /// returned one-time `?code=` GET (Linux `xdg-open` does not need a fragment).
    #[must_use]
    pub fn ui_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if self.auth_enabled
            && self
                .operator_token
                .as_deref()
                .is_some_and(|t| !t.is_empty())
        {
            format!("{base}/api/auth/tray-handoff")
        } else {
            format!("{base}/")
        }
    }

    /// Opens the daemon web UI in the default browser.
    ///
    /// # Errors
    ///
    /// Returns an error when prepare fails or the OS cannot launch a browser.
    pub fn open_ui(&self) -> anyhow::Result<()> {
        let url = if self.auth_enabled {
            self.prepare_tray_handoff()?.url
        } else {
            self.ui_url()
        };
        open::that(url)?;
        Ok(())
    }

    /// Mint a short-lived loopback handoff code (Bearer) and return the GET URL.
    fn prepare_tray_handoff(&self) -> anyhow::Result<PreparedHandoff> {
        let base = self.base_url.trim_end_matches('/');
        let url = format!("{base}/api/auth/tray-handoff/prepare");
        let mut req = ureq::post(&url)
            .config()
            .timeout_global(Some(Duration::from_secs(10)))
            .build();
        if let Some(token) = self.operator_token.as_deref().filter(|t| !t.is_empty()) {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        let mut response = req.send_empty()?;
        if response.status().as_u16() == 204 {
            return Ok(PreparedHandoff {
                url: format!("{base}/"),
                expires_in_secs: SIGN_IN_LINK_CLIPBOARD_TTL_SECS,
            });
        }
        let body: TrayHandoffPrepareBody = response.body_mut().read_json()?;
        let code = body.code.trim();
        anyhow::ensure!(!code.is_empty(), "daemon returned an empty handoff code");
        let expires_in_secs = if body.expires_in_secs == 0 {
            SIGN_IN_LINK_CLIPBOARD_TTL_SECS
        } else {
            body.expires_in_secs.clamp(30, 900)
        };
        Ok(PreparedHandoff {
            url: format!("{base}/api/auth/tray-handoff?code={code}"),
            expires_in_secs,
        })
    }

    /// POSTs an authenticated library scan request to the daemon.
    ///
    /// # Errors
    ///
    /// Returns an error when the HTTP request fails or the daemon rejects it.
    pub fn trigger_scan(&self) -> anyhow::Result<()> {
        let url = format!("{}/api/library/scan", self.base_url.trim_end_matches('/'));
        let mut req = ureq::post(&url)
            .header("Content-Type", "application/json")
            .config()
            .timeout_global(Some(Duration::from_secs(10)))
            .build();
        if self.auth_enabled {
            if let Some(token) = self.operator_token.as_deref() {
                req = req.header("Authorization", format!("Bearer {token}"));
            }
        }
        req.send("{}")?;
        Ok(())
    }

    /// Copy a loopback sign-in link to the clipboard (never the durable token).
    pub fn copy_sign_in_link(&self) {
        if !self.auth_enabled {
            tracing::info!("operator auth is disabled");
            return;
        }
        match self.prepare_tray_handoff() {
            Ok(PreparedHandoff {
                url,
                expires_in_secs,
            }) => {
                if std::thread::Builder::new()
                    .name("bookclerk-clipboard".into())
                    .spawn(move || {
                        persist_clipboard_secret(url, Duration::from_secs(expires_in_secs))
                    })
                    .is_err()
                {
                    tracing::warn!("clipboard unavailable — run `bookclerk login`");
                }
            }
            Err(err) => tracing::warn!(
                %err,
                "could not mint sign-in link — start bookclerkd, then run `bookclerk login`"
            ),
        }
    }

    /// Updates the configured daemon listen address used to derive [`Self::base_url`].
    ///
    /// # Arguments
    ///
    /// * `listen` - Daemon listen address (`host:port` or URL).
    pub fn set_listen(&mut self, listen: &str) {
        self.base_url = Self::base_url(listen);
    }

    /// Overrides the daemon HTTP base URL used for subsequent calls.
    ///
    /// # Arguments
    ///
    /// * `base_url` - Absolute HTTP base URL for the daemon.
    pub fn set_base_url(&mut self, base_url: impl Into<String>) {
        self.base_url = base_url.into();
    }
}

/// Writes `secret` to the clipboard, keeps the selection alive for `ttl`, then
/// removes that value if it is still present.
///
/// Holding the handle for the TTL avoids arboard's Linux warning (and missed
/// pastes) when `Clipboard` is dropped immediately after `set_text`. On Linux,
/// `exclude_from_history` asks Klipper / GNOME not to persist the link; after
/// the TTL we only `clear()` (relinquish ownership). Overwriting with `""`
/// first would stamp a new write and trigger arboard's "dropped very quickly"
/// warning on drop.
fn persist_clipboard_secret(secret: String, ttl: Duration) {
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(clipboard) => clipboard,
        Err(_) => {
            tracing::warn!("clipboard unavailable — run `bookclerk login`");
            return;
        }
    };
    if place_secret_on_clipboard(&mut clipboard, &secret).is_err() {
        tracing::warn!("clipboard unavailable — run `bookclerk login`");
        return;
    }
    tracing::info!(ttl_secs = ttl.as_secs(), "sign-in link copied to clipboard");
    std::thread::sleep(ttl);
    match remove_secret_from_clipboard(&mut clipboard, &secret) {
        ClipboardSecretRemoval::Removed => {
            tracing::info!("sign-in link cleared from clipboard");
        }
        ClipboardSecretRemoval::NotPresent => {
            tracing::debug!("sign-in link no longer on clipboard");
        }
        ClipboardSecretRemoval::ClearFailed => {
            tracing::warn!("could not clear sign-in link from clipboard");
        }
    }
}

/// Places `secret` on the clipboard, excluding it from desktop clipboard history.
fn place_secret_on_clipboard(
    clipboard: &mut arboard::Clipboard,
    secret: &str,
) -> Result<(), arboard::Error> {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        use arboard::SetExtLinux;
        clipboard.set().exclude_from_history().text(secret)
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    {
        clipboard.set_text(secret)
    }
}

/// Whether [`remove_secret_from_clipboard`] found and deleted the secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardSecretRemoval {
    /// Clipboard text matched the secret and was cleared.
    Removed,
    /// Clipboard was empty, unreadable, or held a different value.
    NotPresent,
    /// Clipboard still held the secret but `clear` failed.
    ClearFailed,
}

/// Removes `secret` from `clipboard` only when the current text is that value.
fn remove_secret_from_clipboard(
    clipboard: &mut arboard::Clipboard,
    secret: &str,
) -> ClipboardSecretRemoval {
    match clipboard.get_text() {
        Ok(current) if clipboard_text_is_secret(&current, secret) => {
            if clipboard.clear().is_ok() {
                ClipboardSecretRemoval::Removed
            } else {
                ClipboardSecretRemoval::ClearFailed
            }
        }
        Ok(_) => ClipboardSecretRemoval::NotPresent,
        Err(_) => ClipboardSecretRemoval::NotPresent,
    }
}

/// True when `current` is the secret the tray placed on the clipboard.
///
/// Clipboard managers often append a trailing newline; ignore CR/LF on both sides.
fn clipboard_text_is_secret(current: &str, secret: &str) -> bool {
    fn normalize(s: &str) -> &str {
        s.trim_end_matches(['\r', '\n'])
    }
    let current = normalize(current);
    let secret = normalize(secret);
    !secret.is_empty() && current == secret
}

#[cfg(test)]
mod tests {
    use super::{clipboard_text_is_secret, TrayConfig};

    #[test]
    fn base_url_normalizes() {
        assert_eq!(
            TrayConfig::base_url("127.0.0.1:8787"),
            "http://127.0.0.1:8787"
        );
        assert_eq!(
            TrayConfig::base_url("http://127.0.0.1:8787/"),
            "http://127.0.0.1:8787"
        );
    }

    #[test]
    fn ui_url_has_no_query_token() {
        let cfg = TrayConfig {
            base_url: "http://127.0.0.1:8787".into(),
            auth_enabled: true,
            operator_token: Some("abc".into()),
        };
        assert_eq!(cfg.ui_url(), "http://127.0.0.1:8787/api/auth/tray-handoff");
        assert!(!cfg.ui_url().contains("token="));
    }

    #[test]
    fn ui_url_without_auth_is_root() {
        let cfg = TrayConfig {
            base_url: "http://127.0.0.1:8787".into(),
            auth_enabled: false,
            operator_token: Some("abc".into()),
        };
        assert_eq!(cfg.ui_url(), "http://127.0.0.1:8787/");
    }

    #[test]
    fn set_listen_updates_base_url() {
        let mut cfg = TrayConfig {
            base_url: "http://127.0.0.1:8787".into(),
            auth_enabled: false,
            operator_token: None,
        };
        cfg.set_listen("127.0.0.1:9999");
        assert_eq!(cfg.base_url, "http://127.0.0.1:9999");
    }

    #[test]
    fn clipboard_removal_targets_the_exact_secret() {
        let url = "http://localhost:8787/api/auth/tray-handoff?code=abc";
        assert!(clipboard_text_is_secret(url, url));
        assert!(clipboard_text_is_secret(&format!("{url}\n"), url));
        assert!(clipboard_text_is_secret(&format!("{url}\r\n"), url));
        assert!(!clipboard_text_is_secret("other", url));
        assert!(!clipboard_text_is_secret("", url));
        assert!(!clipboard_text_is_secret(url, "durable-operator-token"));
    }
}
