//! Talk to the local `bookclerkd` HTTP API from the tray thread.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Configuration for the in-process tray (no child `bookclerkd` process).
#[derive(Debug, Clone)]
pub struct TrayConfig {
    pub base_url: String,
    pub auth_enabled: bool,
    pub operator_token: Option<String>,
    pub token_path: Option<PathBuf>,
}

/// Shared tray config so the daemon can refresh `base_url` after listen rebinds.
pub type SharedTrayConfig = Arc<Mutex<TrayConfig>>;

impl TrayConfig {
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

    pub fn open_ui(&self) -> anyhow::Result<()> {
        open::that(self.ui_url())?;
        Ok(())
    }

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

    pub fn print_operator_token(&self) {
        if !self.auth_enabled {
            eprintln!("bookclerk: operator auth is disabled");
            return;
        }
        match self.operator_token.as_deref() {
            Some(token) if !token.is_empty() => {
                if let Some(path) = &self.token_path {
                    eprintln!(
                        "bookclerk: operator token (file {}):\n{token}",
                        path.display()
                    );
                } else {
                    eprintln!("bookclerk: operator token:\n{token}");
                }
            }
            _ => eprintln!("bookclerk: no operator token available"),
        }
    }

    pub fn set_listen(&mut self, listen: &str) {
        self.base_url = Self::base_url(listen);
    }

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
            token_path: None,
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
            token_path: None,
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
            token_path: None,
        };
        cfg.set_listen("127.0.0.1:9999");
        assert_eq!(cfg.base_url, "http://127.0.0.1:9999");
    }
}
