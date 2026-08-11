//! Plugin logo validation: remote `http(s)` URL or relative image path.
//!
//! The optional `logo` key in `plugin.toml` is either a browser-loadable
//! remote URL (Settings / Accounts UI loads it directly) or a path under the
//! plugin install root that the host serves via
//! [`embedded_logo_api_path`]. Absolute filesystem paths, parent segments,
//! and non-http(s) URL schemes are rejected.

use url::Url;

use crate::error::{Error, Result};

/// Allowed image extensions for embedded logos (lowercase, including the dot).
///
/// Used by [`validate_logo`] when classifying a relative path. Remote URLs are
/// not restricted to these extensions.
pub const LOGO_EXTENSIONS: &[&str] = &[".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg", ".ico"];

/// Maximum byte size when the host serves an embedded logo over the API.
///
/// Enforcement is host-side at read time; this constant documents the shared
/// budget (`512 KiB`).
pub const MAX_EMBEDDED_LOGO_BYTES: u64 = 512 * 1024;

/// Classified `plugin.toml` `logo` value after successful validation.
///
/// Produced by [`validate_logo`] / [`PluginManifest::logo_kind`](crate::PluginManifest::logo_kind).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogoKind {
    /// Browser-loadable `http://` or `https://` URL (no userinfo).
    ///
    /// The string is the original trimmed input (scheme and path preserved).
    RemoteUrl(String),

    /// Path relative to the plugin install root (forward-slash normalized).
    ///
    /// The host serves the file via [`embedded_logo_api_path`]; must end with
    /// one of [`LOGO_EXTENSIONS`].
    EmbeddedPath(String),
}

impl LogoKind {
    /// Returns the underlying URL or relative path string.
    ///
    /// # Returns
    ///
    /// Borrowed contents of [`LogoKind::RemoteUrl`] or [`LogoKind::EmbeddedPath`].
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::RemoteUrl(s) | Self::EmbeddedPath(s) => s.as_str(),
        }
    }

    /// Returns `true` when this logo is a remote `http(s)` URL.
    #[must_use]
    pub fn is_remote(&self) -> bool {
        matches!(self, Self::RemoteUrl(_))
    }

    /// Returns `true` when this logo is an embedded path under the plugin root.
    #[must_use]
    pub fn is_embedded(&self) -> bool {
        matches!(self, Self::EmbeddedPath(_))
    }
}

/// Validates an optional logo string from `plugin.toml`.
///
/// Absolute URLs (any scheme) are parsed with `url::Url`; only `http` /
/// `https` are accepted, and userinfo is forbidden. Values that fail URL
/// parse are treated as relative embedded image paths (must stay under the
/// plugin root and end with an allowed extension).
///
/// # Arguments
///
/// * `raw` - Raw `logo` field value (leading/trailing whitespace is trimmed).
///
/// # Returns
///
/// A classified [`LogoKind`].
///
/// # Errors
///
/// Returns [`Error::Message`] when the value is empty, contains NUL, uses a
/// disallowed URL scheme, includes userinfo, escapes the plugin root (`..`,
/// absolute / drive / UNC paths), or lacks an allowed image extension.
///
/// # Examples
///
/// ```
/// use bookclerk_plugin_manifest::{validate_logo, LogoKind};
///
/// assert!(matches!(
///     validate_logo("https://example.com/logo.png").unwrap(),
///     LogoKind::RemoteUrl(_)
/// ));
/// assert_eq!(
///     validate_logo("assets/logo.png").unwrap(),
///     LogoKind::EmbeddedPath("assets/logo.png".into())
/// );
/// assert!(validate_logo("javascript:alert(1)").is_err());
/// ```
pub fn validate_logo(raw: &str) -> Result<LogoKind> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(Error::message(
            "plugin.toml: `logo` must not be empty (omit the key instead)",
        ));
    }
    if trimmed.contains('\0') {
        return Err(Error::message("plugin.toml: `logo` must not contain NUL"));
    }
    // Absolute URLs (any scheme) go through `url::Url` — only http/https allowed.
    // Relative image paths fail `Url::parse` and use path validation instead.
    if let Ok(parsed) = Url::parse(trimmed) {
        return validate_parsed_url(parsed, trimmed);
    }
    validate_embedded_path(trimmed)
}

fn validate_parsed_url(parsed: Url, original: &str) -> Result<LogoKind> {
    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(Error::message(format!(
                "plugin.toml: `logo` URL must use http:// or https:// (got scheme `{other}`)"
            )));
        }
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(Error::message(
            "plugin.toml: `logo` URL must not include userinfo (user:pass@host)",
        ));
    }
    let Some(host) = parsed.host() else {
        return Err(Error::message("plugin.toml: `logo` URL is missing a host"));
    };
    match host {
        url::Host::Domain(d) if d.is_empty() || d == "." || d == ".." => {
            return Err(Error::message("plugin.toml: `logo` URL is missing a host"));
        }
        url::Host::Domain(_) | url::Host::Ipv4(_) | url::Host::Ipv6(_) => {}
    }
    Ok(LogoKind::RemoteUrl(original.to_string()))
}

fn validate_embedded_path(s: &str) -> Result<LogoKind> {
    let path = s.replace('\\', "/");
    if path.starts_with('/') || path.starts_with('~') {
        return Err(Error::message(
            "plugin.toml: embedded `logo` must be a relative path under the plugin root",
        ));
    }
    // Windows drive / UNC
    if path.len() >= 2 && path.as_bytes()[1] == b':' {
        return Err(Error::message(
            "plugin.toml: embedded `logo` must be a relative path (no drive letter)",
        ));
    }
    if path.starts_with("//") {
        return Err(Error::message(
            "plugin.toml: embedded `logo` must be a relative path (no UNC)",
        ));
    }
    let mut segments = Vec::new();
    for seg in path.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            return Err(Error::message(
                "plugin.toml: embedded `logo` must not contain `..` segments",
            ));
        }
        segments.push(seg);
    }
    if segments.is_empty() {
        return Err(Error::message(
            "plugin.toml: embedded `logo` path is empty after normalization",
        ));
    }
    let normalized = segments.join("/");
    let lower = normalized.to_ascii_lowercase();
    if !LOGO_EXTENSIONS.iter().any(|ext| lower.ends_with(ext)) {
        return Err(Error::message(format!(
            "plugin.toml: embedded `logo` must end with one of {}",
            LOGO_EXTENSIONS.join(", ")
        )));
    }
    Ok(LogoKind::EmbeddedPath(normalized))
}

/// Returns the MIME type for an embedded logo path based on its extension.
///
/// Unrecognized extensions map to `application/octet-stream`. Case of the
/// path is ignored.
///
/// # Arguments
///
/// * `path` - Relative logo path (typically from [`LogoKind::EmbeddedPath`]).
///
/// # Returns
///
/// A static MIME type string suitable for `Content-Type` when serving the file.
#[must_use]
pub fn logo_content_type(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".svg") {
        "image/svg+xml"
    } else if lower.ends_with(".ico") {
        "image/x-icon"
    } else {
        "application/octet-stream"
    }
}

/// Builds the Settings / UI href for an embedded logo.
///
/// Format: `/api/plugins/{kind}/{id}/logo`. Remote logos are not rewritten;
/// callers should use the remote URL string directly.
///
/// # Arguments
///
/// * `kind` - Plugin kind wire name (`source`, `integration`, `output`, `database`).
/// * `id` - Validated plugin id.
///
/// # Returns
///
/// Absolute path (no host) for the operator SPA or daemon static route.
#[must_use]
pub fn embedded_logo_api_path(kind: &str, id: &str) -> String {
    format!("/api/plugins/{kind}/{id}/logo")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_https_favicon() {
        let k =
            validate_logo("https://www.google.com/s2/favicons?domain=audible.com&sz=128").unwrap();
        assert!(k.is_remote());
    }

    #[test]
    fn rejects_javascript() {
        let err = validate_logo("javascript:alert(1)").unwrap_err();
        assert!(err.to_string().contains("http"), "{err}");
    }

    #[test]
    fn rejects_vbscript() {
        let err = validate_logo("vbscript:msgbox(1)").unwrap_err();
        assert!(err.to_string().contains("scheme"), "{err}");
    }

    #[test]
    fn rejects_data_url() {
        let err = validate_logo("data:image/png;base64,aaaa").unwrap_err();
        assert!(err.to_string().contains("http"), "{err}");
    }

    #[test]
    fn rejects_userinfo() {
        let err = validate_logo("https://evil:pw@example.com/x.png").unwrap_err();
        assert!(err.to_string().contains("userinfo"), "{err}");
    }

    #[test]
    fn accepts_relative_png() {
        let k = validate_logo("assets/logo.png").unwrap();
        assert_eq!(k, LogoKind::EmbeddedPath("assets/logo.png".into()));
    }

    #[test]
    fn rejects_parent_segments() {
        let err = validate_logo("../etc/passwd.png").unwrap_err();
        assert!(err.to_string().contains(".."), "{err}");
    }

    #[test]
    fn rejects_absolute_path() {
        assert!(validate_logo("/etc/logo.png").is_err());
    }
}
