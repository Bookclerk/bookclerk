//! Shared portal brand metadata (colors + remote favicon URL).

use serde::Serialize;

/// Visual identity for a store or integration in the portal UI.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Brand {
    pub id: &'static str,
    pub name: &'static str,
    /// Button background.
    pub bg: &'static str,
    /// Button foreground / text.
    pub fg: &'static str,
    /// Optional accent (border / focus).
    pub accent: &'static str,
    /// Remote favicon / app-icon URL (not stored in-repo).
    pub icon_url: &'static str,
}

impl Brand {
    /// Favicon URL for `<img src>`.
    #[must_use]
    pub fn logo_href(&self) -> &'static str {
        self.icon_url
    }
}
