//! The public address a branch reports in its heartbeat.
//!
//! A branch whose operator set `integrations.public_origin` is a **core
//! branch**: other installs can reach it there. Its library labels it a
//! remote branch; a branch that reports no address is a local branch. The
//! address arrives from another node, so it is parsed here, sans-IO, into a
//! [`BranchOrigin`] before the library stores or shows it.

use url::Url;

/// Longest origin a branch may report, in bytes.
pub const MAX_BRANCH_ORIGIN_BYTES: usize = 255;

/// A branch's public origin: `http` or `https`, a host, an optional port and
/// nothing else (no user, path, query or fragment), at most
/// [`MAX_BRANCH_ORIGIN_BYTES`] bytes, in the form [`Url`] serializes it
/// (lowercase scheme and host, default port dropped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchOrigin(String);

impl BranchOrigin {
    /// Parses the origin a branch reported.
    ///
    /// Surrounding whitespace and one trailing `/` are ignored, as in the
    /// branch's own `integrations.public_origin` setting.
    ///
    /// # Arguments
    ///
    /// * `raw` - The `public_origin` text from a heartbeat frame.
    ///
    /// # Returns
    ///
    /// The normalized origin, or `None` when the text is not a plain
    /// `http(s)` origin within the size limit.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        let trimmed = trimmed.strip_suffix('/').unwrap_or(trimmed);
        if trimmed.is_empty()
            || trimmed.len() > MAX_BRANCH_ORIGIN_BYTES
            || trimmed.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return None;
        }
        let url = Url::parse(trimmed).ok()?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none_or(str::is_empty)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || trimmed.ends_with('/')
            || trimmed.ends_with('?')
            || trimmed.ends_with('#')
        {
            return None;
        }
        let origin = url.origin().ascii_serialization();
        if origin.len() > MAX_BRANCH_ORIGIN_BYTES {
            return None;
        }
        Some(Self(origin))
    }

    /// The origin as text, such as `https://branch.example:8443`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The origin as owned text.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_origins_parse_and_normalize() {
        for (raw, want) in [
            ("https://branch.example", "https://branch.example"),
            ("  HTTPS://Branch.Example/ ", "https://branch.example"),
            ("https://branch.example:443", "https://branch.example"),
            ("http://192.168.1.20:8787", "http://192.168.1.20:8787"),
            ("https://[::1]:8443", "https://[::1]:8443"),
        ] {
            assert_eq!(
                BranchOrigin::parse(raw).map(BranchOrigin::into_string),
                Some(want.to_string()),
                "{raw}"
            );
        }
    }

    #[test]
    fn anything_beyond_an_origin_is_refused() {
        let long = format!("https://{}.example", "a".repeat(MAX_BRANCH_ORIGIN_BYTES));
        for raw in [
            "",
            "   ",
            "branch.example",
            "ftp://branch.example",
            "file:///etc/passwd",
            "https://user@branch.example",
            "https://user:pw@branch.example",
            "https://branch.example/library",
            "https://branch.example/?q=1",
            "https://branch.example?",
            "https://branch.example#top",
            "https://branch.example//",
            "https://bra nch.example",
            "https://branch\u{7}.example",
            "https://",
            long.as_str(),
        ] {
            assert_eq!(BranchOrigin::parse(raw), None, "{raw:?}");
        }
    }

    #[test]
    fn as_str_matches_into_string() {
        let origin = BranchOrigin::parse("https://branch.example").expect("parses");
        assert_eq!(origin.as_str(), "https://branch.example");
    }
}
