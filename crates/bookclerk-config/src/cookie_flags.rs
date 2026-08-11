//! Shared Set-Cookie attribute helpers for operator and portal sessions.

/// Append `; Secure` when the configured public origin is HTTPS.
///
/// Local loopback development (no `public_origin`, or `http://…`) keeps cookies
/// usable over plain HTTP.
#[must_use]
pub fn cookie_secure_suffix(public_origin: Option<&str>) -> &'static str {
    match public_origin.map(str::trim) {
        Some(origin) if origin.to_ascii_lowercase().starts_with("https://") => "; Secure",
        _ => "",
    }
}

/// Build common session cookie attributes (`HttpOnly; SameSite=Lax` + optional Secure).
#[must_use]
pub fn session_cookie_flags(public_origin: Option<&str>) -> String {
    format!(
        "Path=/; HttpOnly; SameSite=Lax{}",
        cookie_secure_suffix(public_origin)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secure_only_for_https_origin() {
        assert_eq!(cookie_secure_suffix(None), "");
        assert_eq!(cookie_secure_suffix(Some("http://localhost:8787")), "");
        assert_eq!(
            cookie_secure_suffix(Some("https://books.example.com")),
            "; Secure"
        );
        assert_eq!(
            cookie_secure_suffix(Some("HTTPS://Books.Example.com")),
            "; Secure"
        );
    }
}
