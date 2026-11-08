//! OAuth helpers for the guest login flow via audible-rs (LoginServer).

use std::net::SocketAddr;

use audible_rs::auth::device::DeviceKind;
use audible_rs::auth::login::{LoginDefaults, LoginServer};
use audible_rs::auth::Authenticator;

use crate::error::{AudibleError, Result};

/// Metadata returned after a successful Audible OAuth session.
#[derive(Debug, Clone)]
pub(crate) struct AuthSession {
    /// Stable Bookclerk account id for library rows and secret lookup.
    pub account_id: String,
    /// Marketplace used for the login (`us`, `uk`, …).
    pub marketplace: String,
    /// Optional operator-facing label the login started with.
    pub label: Option<String>,
}

/// LoginDefaults for the Audible LoginServer (Android + plain authfile).
///
/// Android registration is required for Widevine L3 drmlicense grants.
pub(crate) fn login_defaults(marketplace: &str) -> LoginDefaults {
    LoginDefaults {
        country_code: Some(marketplace.to_string()),
        device: DeviceKind::Android,
        username: false,
        // Account identity is the Audible customer id after login — do not
        // prefill / solicit a local "account name" that could diverge from it.
        name: None,
        marketplaces: None,
        default_marketplaces: None,
        plain: true,
    }
}

/// Browser URL + bound addr for a live [`LoginServer`].
pub(crate) fn login_server_url(server: &LoginServer, bind: SocketAddr) -> (String, SocketAddr) {
    let port = server.local_port();
    let path = server.landing_path();
    let ip = bind.ip();
    let host = if ip.is_unspecified() {
        "127.0.0.1".to_string()
    } else {
        ip.to_string()
    };
    let url = format!("http://{host}:{port}{path}");
    let addr = SocketAddr::new(ip, port);
    (url, addr)
}

/// Session metadata (account id, marketplace, label) from a completed login.
#[must_use]
pub(crate) fn session_from_authenticator(
    auth: &Authenticator,
    label: Option<String>,
) -> AuthSession {
    let marketplace = auth.locale().country_code.to_string();
    let account_id = auth
        .customer_id()
        .map(str::to_string)
        .or_else(|| label.clone())
        .unwrap_or_else(|| marketplace.clone());
    AuthSession {
        account_id,
        marketplace,
        label,
    }
}

/// Export authenticator as Protection::Plain authfile bytes.
pub(crate) fn export_authfile_plain_bytes(auth: &Authenticator) -> Result<Vec<u8>> {
    let data = auth.export_value();
    let plain = audible_rs::auth::authfile::write(
        &data,
        audible_rs::auth::authfile::Protection::Plain,
        None,
    )
    .map_err(|e| AudibleError::Auth(e.to_string()))?;
    Ok(plain.into_bytes())
}
