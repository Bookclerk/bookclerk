//! Source brand colors + remote favicon URLs for the connect portal.
//!
//! Content-source brands come from [`libation_source::ContentSource::portal_brand`].
//! Integration brands are owned by each integration plugin (see
//! [`crate::abs::brand`]); look them up via [`integration_brand`] or
//! [`crate::Integration::portal_brand`].

use libation_source::SourceBrand;

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

/// Brand for an integration provider id, delegated to plugin modules.
#[must_use]
pub fn integration_brand(id: &str) -> Option<Brand> {
    crate::abs::brand_for_id(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use libation_source::ContentSource;

    #[test]
    fn source_brand_from_plugin() {
        let b = Brand::from(libation_audible::AudibleSource::new().portal_brand());
        assert_eq!(b.id, "audible");
        assert!(b.icon_url.starts_with("https://"));
        assert_eq!(b.logo_href(), b.icon_url);
    }

    #[test]
    fn abs_brand_aliases() {
        let b = integration_brand("abs").expect("abs alias");
        assert_eq!(b.id, "audiobookshelf");
        assert!(b.icon_url.starts_with("https://"));
    }
}
