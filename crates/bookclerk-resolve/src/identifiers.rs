//! External identifiers: normalisation and the key they are stored under.

use crate::kinds::{IdentifierScheme, Level, Strength};

/// One normalised external identifier, as stored and compared.
///
/// `marketplace` is the lower-cased marketplace for a scheme scoped to one
/// ([`IdentifierScheme::marketplace_scoped`]) and empty for a global scheme,
/// so two spellings of one identifier always compare equal.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IdentifierKey {
    /// Scheme the value belongs to.
    pub scheme: IdentifierScheme,
    /// Normalised value (ISBN-13 without separators, upper-case ASIN, …).
    pub value: String,
    /// Lower-cased marketplace for a scoped scheme; empty otherwise.
    pub marketplace: String,
}

impl IdentifierKey {
    /// Normalises a raw identifier into its stored key.
    ///
    /// # Arguments
    ///
    /// * `scheme` - The identifier's scheme.
    /// * `raw` - The value as the source printed it.
    /// * `marketplace` - The marketplace the source scoped it to, if any;
    ///   used only for a scoped scheme.
    /// * `fallback_marketplace` - The listing's own marketplace, used when a
    ///   scoped identifier names none.
    ///
    /// # Returns
    ///
    /// `None` when the value is blank after normalising.
    #[must_use]
    pub fn new(
        scheme: IdentifierScheme,
        raw: &str,
        marketplace: Option<&str>,
        fallback_marketplace: &str,
    ) -> Option<Self> {
        let value = normalize_identifier(scheme, raw)?;
        let marketplace = if scheme.marketplace_scoped() {
            marketplace
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .unwrap_or(fallback_marketplace)
                .trim()
                .to_ascii_lowercase()
        } else {
            String::new()
        };
        Some(Self {
            scheme,
            value,
            marketplace,
        })
    }

    /// The level whose identifier table this key belongs in.
    #[must_use]
    pub fn level(&self) -> Level {
        self.scheme.level()
    }

    /// Whether this key names exactly one edition (or one work).
    #[must_use]
    pub fn strength(&self) -> Strength {
        self.scheme.strength()
    }

    /// The unique key a strong identifier is stored under, `None` for a weak one.
    ///
    /// Strong edition identifiers are unique across the library. SQL keeps
    /// that with a nullable `UNIQUE` column holding this text (`NULL` for
    /// weak rows, which never conflict).
    #[must_use]
    pub fn strong_key(&self) -> Option<String> {
        match self.strength() {
            Strength::Strong => Some(format!(
                "{}\u{1f}{}\u{1f}{}",
                self.scheme.as_str(),
                self.marketplace,
                self.value
            )),
            Strength::Weak => None,
        }
    }
}

/// Trims and normalises an identifier value for its scheme.
///
/// ASINs are upper-cased. ISBNs lose hyphens, spaces and Unicode dashes,
/// are upper-cased (`x` check digit), and a valid ISBN-10 becomes its
/// ISBN-13. OpenLibrary ids are kept as given.
///
/// # Arguments
///
/// * `scheme` - The identifier's scheme.
/// * `raw` - The value as the source printed it.
///
/// # Returns
///
/// `None` when the value is blank.
#[must_use]
pub fn normalize_identifier(scheme: IdentifierScheme, raw: &str) -> Option<String> {
    let value = raw.trim();
    if value.is_empty() {
        return None;
    }
    let normalised = match scheme {
        IdentifierScheme::Asin => value.to_ascii_uppercase(),
        IdentifierScheme::Isbn => normalize_isbn(value),
        IdentifierScheme::OpenLibraryEdition | IdentifierScheme::OpenLibraryWork => {
            value.to_string()
        }
    };
    (!normalised.is_empty()).then_some(normalised)
}

/// An ISBN without separators, as ISBN-13 when it is a valid ISBN-10.
fn normalize_isbn(value: &str) -> String {
    let compact: String = value
        .chars()
        .filter(|c| !matches!(c, '-' | ' ' | '\u{2010}'..='\u{2015}'))
        .map(|c| c.to_ascii_uppercase())
        .collect();
    isbn10_to_isbn13(&compact).unwrap_or(compact)
}

/// The ISBN-13 for a valid ISBN-10 (nine digits plus a digit or `X` check).
///
/// # Returns
///
/// `None` when `isbn10` is not a valid ISBN-10, check digit included.
fn isbn10_to_isbn13(isbn10: &str) -> Option<String> {
    let bytes = isbn10.as_bytes();
    if bytes.len() != 10 {
        return None;
    }
    let mut sum10 = 0u32;
    for (weight, byte) in (1..=10u32).rev().zip(bytes) {
        let digit = match byte {
            b'0'..=b'9' => u32::from(byte - b'0'),
            b'X' if weight == 1 => 10,
            _ => return None,
        };
        sum10 += weight * digit;
    }
    if !sum10.is_multiple_of(11) {
        return None;
    }
    // Ten ASCII bytes, so the first nine are a valid `str` prefix.
    let body = format!("978{}", isbn10.get(..9)?);
    let sum13: u32 = body
        .bytes()
        .zip([1u32, 3].into_iter().cycle())
        .map(|(byte, weight)| u32::from(byte - b'0') * weight)
        .sum();
    Some(format!("{body}{}", (10 - sum13 % 10) % 10))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isbns_normalise_to_isbn13() {
        let n = |raw| normalize_identifier(IdentifierScheme::Isbn, raw);
        assert_eq!(n("0-14-044913-2").as_deref(), Some("9780140449136"));
        assert_eq!(n("978-0-14-044913-6").as_deref(), Some("9780140449136"));
        assert_eq!(
            n("0\u{2011}14\u{2011}044913\u{2011}2").as_deref(),
            Some("9780140449136")
        );
        assert_eq!(n("080442957x").as_deref(), Some("9780804429573"));
        // An invalid ISBN-10 is kept compact rather than converted.
        assert_eq!(n("0140449136").as_deref(), Some("0140449136"));
        assert_eq!(n("  "), None);
        assert_eq!(n("--"), None);
    }

    #[test]
    fn asins_upper_case_and_scope_to_a_marketplace() {
        let key = IdentifierKey::new(IdentifierScheme::Asin, " b0abc ", None, "US").unwrap();
        assert_eq!(key.value, "B0ABC");
        assert_eq!(key.marketplace, "us");
        let uk = IdentifierKey::new(IdentifierScheme::Asin, "B0ABC", Some("UK"), "us").unwrap();
        assert_eq!(uk.marketplace, "uk");
        assert_ne!(key.strong_key(), uk.strong_key());
    }

    #[test]
    fn global_schemes_ignore_the_marketplace() {
        let key =
            IdentifierKey::new(IdentifierScheme::Isbn, "9780140449136", Some("uk"), "us").unwrap();
        assert_eq!(key.marketplace, "");
        assert_eq!(key.strong_key(), None);
        let ol =
            IdentifierKey::new(IdentifierScheme::OpenLibraryEdition, "OL1M", None, "us").unwrap();
        assert_eq!(
            ol.strong_key().as_deref(),
            Some("openlibrary_edition\u{1f}\u{1f}OL1M")
        );
    }
}
