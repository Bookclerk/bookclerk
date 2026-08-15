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
}

/// Shared tray config so the daemon can refresh `base_url` after listen rebinds.
pub type SharedTrayConfig = Arc<Mutex<TrayConfig>>;

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
            self.prepare_tray_handoff()?
        } else {
            self.ui_url()
        };
        open::that(url)?;
        Ok(())
    }

    /// Mint a short-lived loopback handoff code (Bearer) and return the GET URL.
    fn prepare_tray_handoff(&self) -> anyhow::Result<String> {
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
            return Ok(format!("{base}/"));
        }
        let body: TrayHandoffPrepareBody = response.body_mut().read_json()?;
        let code = body.code.trim();
        anyhow::ensure!(!code.is_empty(), "daemon returned an empty handoff code");
        Ok(format!("{base}/api/auth/tray-handoff?code={code}"))
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

    /// Copy the operator token to the system clipboard (never prints the value).
    pub fn copy_operator_token(&self) {
        if !self.auth_enabled {
            eprintln!("bookclerk: operator auth is disabled");
            return;
        }
        match self.operator_token.as_deref() {
            Some(token) if !token.is_empty() => match arboard::Clipboard::new()
                .and_then(|mut cb| cb.set_text(token.to_owned()))
            {
                Ok(()) => eprintln!("bookclerk: operator token copied to clipboard"),
                Err(_) => {
                    eprintln!("bookclerk: clipboard unavailable — run `bookclerk daemon token`");
                }
            },
            _ => eprintln!("bookclerk: no operator token available"),
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

#[cfg(test)]
mod tests {
    use super::TrayConfig;

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
}
