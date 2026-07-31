//! Audiobookshelf portal brand — owned by this plugin package.

use bookclerk_integrations::Brand;

/// Canonical ABS brand for connect-portal buttons.
pub const BRAND: Brand = Brand {
    id: "audiobookshelf",
    name: "Audiobookshelf",
    bg: "#1E293B",
    fg: "#F8FAFC",
    accent: "#59BC89",
    icon_url: "https://www.google.com/s2/favicons?domain=audiobookshelf.org&sz=128",
};

/// Whether `id` refers to this integration (`audiobookshelf` / `abs`).
#[must_use]
pub fn matches_id(id: &str) -> bool {
    matches!(
        id.trim().to_ascii_lowercase().as_str(),
        "audiobookshelf" | "abs"
    )
}

/// Brand for a provider id when it aliases this plugin.
#[must_use]
pub fn brand_for_id(id: &str) -> Option<Brand> {
    if matches_id(id) {
        Some(BRAND)
    } else {
        None
    }
}
