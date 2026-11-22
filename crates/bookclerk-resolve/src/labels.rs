//! Store labels that name the same thing in different spellings.
//!
//! Stores label one series and one recording differently: Audible files the
//! abridged First Family under "King and Maxwell (abridged)", Chirp under
//! "King & Maxwell" and Libro.fm under "King & Maxwell Series", and Libro.fm
//! titles that recording "First Family - Abridged". These helpers remove the
//! edition markers and generic words a store adds, so one series is one
//! series row and one book matches its own work, whichever store reported it.

/// Edition markers a store appends to a title or series name, lowercased.
const EDITION_MARKERS: &[&str] = &[
    "abridged",
    "unabridged",
    "abridged edition",
    "unabridged edition",
    "abridged version",
    "unabridged version",
];

/// Separators a store puts before a trailing edition marker.
const MARKER_SEPARATORS: &[&str] = &[" - ", " – ", " — ", ": ", ", "];

/// Generic trailing words a store adds to a series name, lowercased.
const SERIES_WORDS: &[&str] = &["series"];

/// Words that cannot stand alone as a series name once a generic word is
/// removed (`The Series` stays as it is).
const ARTICLES: &[&str] = &["the", "a", "an"];

/// Whether `text` is one of the [`EDITION_MARKERS`], ignoring case and
/// surrounding space.
fn is_edition_marker(text: &str) -> bool {
    let lower = text.trim().to_lowercase();
    EDITION_MARKERS.contains(&lower.as_str())
}

/// Removes one trailing edition marker from `text`, if it ends with one.
///
/// # Returns
///
/// The text before the marker, trimmed, or `None` when `text` ends with no
/// marker or nothing would be left.
fn strip_one_marker(text: &str) -> Option<&str> {
    let text = text.trim_end();
    for (open, close) in [('(', ')'), ('[', ']')] {
        if let Some(inner_end) = text.strip_suffix(close) {
            if let Some(start) = inner_end.rfind(open) {
                let before = inner_end.get(..start)?.trim_end();
                let inner = inner_end.get(start + open.len_utf8()..)?;
                if is_edition_marker(inner) && !before.is_empty() {
                    return Some(before);
                }
            }
        }
    }
    MARKER_SEPARATORS.iter().find_map(|sep| {
        let (before, after) = text.rsplit_once(sep)?;
        let before = before.trim_end();
        (is_edition_marker(after) && !before.is_empty()).then_some(before)
    })
}

/// A title or series name without the edition markers a store appended.
///
/// Removes trailing `(Abridged)`, `[Unabridged]`, ` - Abridged`,
/// `: Unabridged Edition` and the like, so `First Family - Abridged` and
/// `First Family (Abridged)` both read `First Family`. A marker inside the
/// text, or one that is the whole text, is kept.
///
/// # Arguments
///
/// * `text` - A display title or series name.
///
/// # Returns
///
/// The text without trailing edition markers, trimmed.
#[must_use]
pub fn strip_edition_marker(text: &str) -> &str {
    let mut rest = text.trim();
    while let Some(shorter) = strip_one_marker(rest) {
        rest = shorter;
    }
    rest
}

/// A series name as Bookclerk shows and stores it.
///
/// Collapses whitespace, removes a trailing edition marker
/// (`King and Maxwell (abridged)` reads `King and Maxwell`) and a trailing
/// generic word (`King & Maxwell Series` reads `King & Maxwell`). The word
/// stays when only an article would be left (`The Series`). `&` and `and`
/// are kept as the store spelled them; [`series_key`] compares them equal.
///
/// # Arguments
///
/// * `name` - The series name a store reported.
///
/// # Returns
///
/// The cleaned name; empty only when `name` is blank.
#[must_use]
pub fn clean_series_name(name: &str) -> String {
    let collapsed = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut cleaned = strip_edition_marker(&collapsed).to_string();
    if let Some((before, last)) = cleaned.rsplit_once(' ') {
        let before = before.trim_end();
        let generic = SERIES_WORDS.contains(&last.to_lowercase().as_str());
        let stands_alone =
            !before.is_empty() && !ARTICLES.contains(&before.to_lowercase().as_str());
        if generic && stands_alone {
            cleaned = before.to_string();
        }
    }
    cleaned
}

/// The key series names are matched by.
///
/// The [`clean_series_name`] of `name`, with `&` read as `and`, then
/// lowercased without accents, apostrophes or parenthesised text, so
/// `King & Maxwell Series`, `King and Maxwell` and
/// `King and Maxwell (abridged)` share one key.
///
/// # Arguments
///
/// * `name` - A series name, as a store reported it or as stored.
///
/// # Returns
///
/// The key; empty when `name` is blank.
#[must_use]
pub fn series_key(name: &str) -> String {
    let cleaned = clean_series_name(name).replace('&', " and ");
    crate::normalize::clean_title(&cleaned, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_trailing_edition_markers() {
        assert_eq!(
            strip_edition_marker("First Family - Abridged"),
            "First Family"
        );
        assert_eq!(
            strip_edition_marker("First Family (Abridged)"),
            "First Family"
        );
        assert_eq!(strip_edition_marker("Dune [Unabridged]"), "Dune");
        assert_eq!(strip_edition_marker("Dune: Unabridged Edition"), "Dune");
        assert_eq!(strip_edition_marker("  First Family  "), "First Family");
        // Not a marker, or nothing would be left: kept.
        assert_eq!(strip_edition_marker("Dune (Book One)"), "Dune (Book One)");
        assert_eq!(strip_edition_marker("Abridged"), "Abridged");
        assert_eq!(strip_edition_marker("(Abridged)"), "(Abridged)");
        assert_eq!(
            strip_edition_marker("The Abridged Life"),
            "The Abridged Life"
        );
    }

    #[test]
    fn cleans_series_names_from_each_store() {
        assert_eq!(
            clean_series_name("King and Maxwell (abridged)"),
            "King and Maxwell"
        );
        assert_eq!(clean_series_name("King & Maxwell Series"), "King & Maxwell");
        assert_eq!(clean_series_name("King &  Maxwell"), "King & Maxwell");
        assert_eq!(clean_series_name("The Series"), "The Series");
        assert_eq!(clean_series_name("Series"), "Series");
        assert_eq!(
            clean_series_name("A Series of Unfortunate Events"),
            "A Series of Unfortunate Events"
        );
        assert_eq!(clean_series_name("   "), "");
    }

    #[test]
    fn series_keys_ignore_ampersands_and_store_words() {
        let key = series_key("King and Maxwell");
        assert_eq!(key, "king and maxwell");
        for variant in [
            "King & Maxwell",
            "King & Maxwell Series",
            "King and Maxwell (abridged)",
            "king  AND maxwell",
        ] {
            assert_eq!(series_key(variant), key, "{variant}");
        }
        assert_ne!(series_key("King and Maxwell"), series_key("Kingsbridge"));
        assert_eq!(series_key(""), "");
    }
}
