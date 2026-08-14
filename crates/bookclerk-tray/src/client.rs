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
    /// Prefer `/api/auth/tray-handoff?token=…` over a `#token=` fragment: Linux
    /// `xdg-open` commonly strips fragments before the browser sees them.
    #[must_use]
    pub fn ui_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if self.auth_enabled {
            if let Some(token) = self.operator_token.as_deref().filter(|t| !t.is_empty()) {
                let encoded = encode_token_fragment(token);
                return format!("{base}/api/auth/tray-handoff?token={encoded}");
            }
        }
        format!("{base}/")
    }

    /// Opens the daemon web UI in the default browser.
    ///
    /// # Errors
    ///
    /// Returns an error when the OS cannot launch a browser for [`Self::ui_url`].
    pub fn open_ui(&self) -> anyhow::Result<()> {
        open::that(self.ui_url())?;
        Ok(())
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

/// RFC 3986 unreserved characters — safe unencoded in a fragment value.
fn token_is_url_safe(token: &str) -> bool {
    !token.is_empty()
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// Percent-encodes an operator token for a URL fragment; unreserved characters stay raw.
fn encode_token_fragment(token: &str) -> String {
    if token_is_url_safe(token) {
        return token.to_string();
    }
    let mut out = String::with_capacity(token.len() * 3);
    for &b in token.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            const HEX: &[u8] = b"0123456789ABCDEF";
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{encode_token_fragment, token_is_url_safe, TrayConfig};

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
    fn ui_url_embeds_token_fragment() {
        let cfg = TrayConfig {
            base_url: "http://127.0.0.1:8787".into(),
            auth_enabled: true,
            operator_token: Some("abc".into()),
        };
        assert_eq!(
            cfg.ui_url(),
            "http://127.0.0.1:8787/api/auth/tray-handoff?token=abc"
        );
    }

    #[test]
    fn ui_url_percent_encodes_unsafe_tokens() {
        let cfg = TrayConfig {
            base_url: "http://127.0.0.1:8787".into(),
            auth_enabled: true,
            operator_token: Some("a&b=c+d".into()),
        };
        assert_eq!(
            cfg.ui_url(),
            "http://127.0.0.1:8787/api/auth/tray-handoff?token=a%26b%3Dc%2Bd"
        );
        assert!(!token_is_url_safe("a&b=c+d"));
        assert_eq!(encode_token_fragment("a&b=c+d"), "a%26b%3Dc%2Bd");
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
