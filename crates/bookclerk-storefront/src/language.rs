//! Preferred-language ranking helpers for catalog merge.
//!
//! # Audience
//!
//! Host discover / catalog code soft-prioritising hits by language.

pub use bookclerk_resolve::normalize_language;

/// Preferred language for Discover when the client omits one.
#[must_use]
pub fn default_preferred_language() -> &'static str {
    "en"
}

/// Rank a hit's language relative to the preferred code.
///
/// - `0` — matches preferred
/// - `1` — unknown / missing
/// - `2` — other language
#[must_use]
pub fn language_rank(hit_language: Option<&str>, preferred: &str) -> u8 {
    let pref =
        normalize_language(preferred).unwrap_or_else(|| default_preferred_language().to_string());
    match hit_language.and_then(normalize_language) {
        None => 1,
        Some(code) if code == pref => 0,
        Some(_) => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_bcp47_and_names() {
        assert_eq!(normalize_language("en-US").as_deref(), Some("en"));
        assert_eq!(normalize_language("EN").as_deref(), Some("en"));
        assert_eq!(normalize_language("english").as_deref(), Some("en"));
        assert_eq!(normalize_language("Spanish").as_deref(), Some("es"));
        // Libro.fm's MARC codes.
        assert_eq!(normalize_language("fre").as_deref(), Some("fr"));
        assert_eq!(normalize_language("ger").as_deref(), Some("de"));
        assert_eq!(normalize_language("spa").as_deref(), Some("es"));
        assert_eq!(normalize_language("  ").as_deref(), None);
        assert_eq!(normalize_language("unknown-lang").as_deref(), None);
    }

    #[test]
    fn rank_preferred_unknown_other() {
        assert_eq!(language_rank(Some("english"), "en"), 0);
        assert_eq!(language_rank(Some("en-GB"), "en"), 0);
        assert_eq!(language_rank(None, "en"), 1);
        assert_eq!(language_rank(Some(""), "en"), 1);
        assert_eq!(language_rank(Some("spanish"), "en"), 2);
        assert_eq!(language_rank(Some("english"), "es"), 2);
        assert_eq!(language_rank(Some("español"), "es"), 0);
    }
}
