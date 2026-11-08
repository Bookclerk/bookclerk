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

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_auth() -> Authenticator {
        Authenticator::from_value(serde_json::json!({
            "country_code": "uk",
            "identity": {"customer_id": "amzn1.account.TEST"},
            "bearer": {
                "access_token": "Atna|synthetic",
                "refresh_token": "Atnr|synthetic",
                "expires": 9999999999.0
            }
        }))
        .unwrap()
    }

    #[test]
    fn login_registers_an_android_device_with_a_plain_authfile() {
        let defaults = login_defaults("de");
        assert_eq!(defaults.country_code.as_deref(), Some("de"));
        assert!(matches!(defaults.device, DeviceKind::Android));
        assert!(defaults.plain);
        assert!(!defaults.username);
        assert!(defaults.name.is_none());
    }

    #[test]
    fn session_is_keyed_by_customer_id() {
        let session = session_from_authenticator(&synthetic_auth(), Some("Main".into()));
        assert_eq!(session.account_id, "amzn1.account.TEST");
        assert_eq!(session.marketplace, "uk");
        assert_eq!(session.label.as_deref(), Some("Main"));
    }

    #[test]
    fn plain_authfile_round_trips() {
        let bytes = export_authfile_plain_bytes(&synthetic_auth()).unwrap();
        let back = Authenticator::load_from_bytes(&bytes, None).unwrap();
        assert_eq!(back.customer_id(), Some("amzn1.account.TEST"));
    }
}
