//! Best-effort User-Agent → device / OS (or API) labels for session lists.

/// Parsed client presentation for a session row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionClientInfo {
    /// Raw User-Agent (or synthetic `API` / empty).
    pub user_agent: Option<String>,
    /// High-level device class: `desktop`, `mobile`, `tablet`, `api`, or `unknown`.
    pub device_type: String,
    /// Human label such as `Windows`, `macOS`, `Android`, `iPhone`, or `API`.
    pub client_label: String,
}

/// Classify a User-Agent (or missing UA for API/bearer clients).
///
/// # Arguments
///
/// * `user_agent` - Optional `User-Agent` header value.
/// * `is_api` - When true (no browser UA / bearer-style client), label as API.
///
/// # Panics
///
/// Panics when an internal invariant does not hold.
#[must_use]
pub fn classify_session_client(user_agent: Option<&str>, is_api: bool) -> SessionClientInfo {
    let ua_trim = user_agent.map(str::trim).filter(|s| !s.is_empty());
    if is_api || ua_trim.is_none() {
        return SessionClientInfo {
            user_agent: ua_trim.map(str::to_string),
            device_type: String::from("api"),
            client_label: String::from("API"),
        };
    }
    let ua = ua_trim.unwrap();
    let lower = ua.to_ascii_lowercase();

    let device_type = if lower.contains("ipad")
        || lower.contains("tablet")
        || (lower.contains("android") && !lower.contains("mobile"))
    {
        "tablet"
    } else if lower.contains("mobi")
        || lower.contains("iphone")
        || lower.contains("ipod")
        || lower.contains("android")
    {
        "mobile"
    } else {
        "desktop"
    };

    let client_label = if lower.contains("windows") {
        "Windows"
    } else if lower.contains("android") {
        "Android"
    } else if lower.contains("iphone") || lower.contains("ipod") {
        "iPhone"
    } else if lower.contains("ipad") {
        "iPad"
    } else if lower.contains("mac os") || lower.contains("macintosh") {
        "macOS"
    } else if lower.contains("cros") {
        "ChromeOS"
    } else if lower.contains("linux") {
        "Linux"
    } else {
        "Unknown"
    };

    SessionClientInfo {
        user_agent: Some(ua.to_string()),
        device_type: device_type.to_string(),
        client_label: client_label.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_agents() {
        let win = classify_session_client(
            Some("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36"),
            false,
        );
        assert_eq!(win.device_type, "desktop");
        assert_eq!(win.client_label, "Windows");

        let android = classify_session_client(
            Some("Mozilla/5.0 (Linux; Android 13; Pixel 7) Mobile Safari/537.36"),
            false,
        );
        assert_eq!(android.device_type, "mobile");
        assert_eq!(android.client_label, "Android");

        let api = classify_session_client(None, true);
        assert_eq!(api.device_type, "api");
        assert_eq!(api.client_label, "API");
    }
}
