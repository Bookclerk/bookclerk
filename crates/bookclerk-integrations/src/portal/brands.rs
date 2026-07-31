//! Source brand colors + remote favicon URLs for the connect portal.
//!
//! Content-source brands come from [`bookclerk_source::ContentSource::portal_brand`].
//! Integration brands ideally come from [`crate::Integration::portal_brand`];
//! [`integration_brand`] keeps a small fallback for known first-party ids
//! (e.g. claim-ticket UI before the adapter is constructed).

use bookclerk_source::SourceBrand;

pub use crate::brand::Brand;

impl From<SourceBrand> for Brand {
    fn from(b: SourceBrand) -> Self {
        Self {
            id: b.id,
            name: b.name,
            bg: b.bg,
            fg: b.fg,
            accent: b.accent,
            icon_url: b.icon_url,
        }
    }
}

/// Audiobookshelf brand constants (mirrored from the ABS plugin package so
/// the portal crate does not depend on adapter HTTP).
const ABS_BRAND: Brand = Brand {
    id: "audiobookshelf",
    name: "Audiobookshelf",
    bg: "#1E293B",
    fg: "#F8FAFC",
    accent: "#59BC89",
    icon_url: "https://www.google.com/s2/favicons?domain=audiobookshelf.org&sz=128",
};

/// Brand for an integration provider id (first-party fallbacks).
#[must_use]
pub fn integration_brand(id: &str) -> Option<Brand> {
    match id.trim().to_ascii_lowercase().as_str() {
        "audiobookshelf" | "abs" => Some(ABS_BRAND),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_brand_from_plugin() {
        // Audible brand constants (see AudibleSource::portal_brand / docs).
        let b = Brand {
            id: "audible",
            name: "Audible",
            bg: "#F8991D",
            fg: "#111111",
            accent: "#D97706",
            icon_url: "https://www.google.com/s2/favicons?domain=audible.com&sz=128",
        };
        assert_eq!(b.id, "audible");
        assert!(b.icon_url.starts_with("https://"));
        assert_eq!(b.logo_href(), b.icon_url);
        assert_eq!(b.bg, "#F8991D");
        assert_eq!(b.fg, "#111111");
        assert_eq!(b.accent, "#D97706");
    }

    #[test]
    fn abs_brand_aliases() {
        let b = integration_brand("abs").expect("abs alias");
        assert_eq!(b.id, "audiobookshelf");
        assert!(b.icon_url.starts_with("https://"));
    }
}
