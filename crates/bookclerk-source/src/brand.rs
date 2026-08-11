//! Portal / UI brand metadata owned by content-source plugins.

use serde::Serialize;

/// Visual identity for a content source in the connect portal.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct SourceBrand {
    /// Stable plugin id matching [`crate::ContentSource::id`] (`audible`, …).
    pub id: &'static str,
    /// Human-facing store name shown on the connect button.
    pub name: &'static str,
    /// CSS background color (hex) for the portal button.
    pub bg: &'static str,
    /// CSS foreground / text color (hex) for the portal button.
    pub fg: &'static str,
    /// CSS accent color (hex) for focus / highlights.
    pub accent: &'static str,
    /// Remote favicon / app-icon URL (not stored in-repo).
    pub icon_url: &'static str,
}

impl SourceBrand {
    /// Favicon / app-icon URL used as the portal button logo (`href`).
    #[must_use]
    pub fn logo_href(&self) -> &'static str {
        self.icon_url
    }
}
