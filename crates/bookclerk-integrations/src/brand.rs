//! Shared portal brand metadata (colors + remote favicon URL).

use serde::Serialize;

/// Visual identity for a store or integration in the portal UI.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Brand {
    /// Stable identifier for this item.
    pub id: &'static str,
    /// Operator-facing brand name shown on portal connect buttons.
    pub name: &'static str,
    /// CSS background color for portal connect buttons (`#RRGGBB`).
    pub bg: &'static str,
    /// CSS foreground / text color for portal connect buttons.
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
