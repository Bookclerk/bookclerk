//! Portal / UI brand metadata owned by content-source plugins.

use serde::Serialize;

/// Visual identity for a content source in the connect portal.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct SourceBrand {
    pub id: &'static str,
    pub name: &'static str,
    pub bg: &'static str,
    pub fg: &'static str,
    pub accent: &'static str,
    /// Remote favicon / app-icon URL (not stored in-repo).
    pub icon_url: &'static str,
}

impl SourceBrand {
    #[must_use]
    pub fn logo_href(&self) -> &'static str {
        self.icon_url
    }
}
