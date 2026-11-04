//! Builders and flat views for the structured bibliographic records that
//! `ScanBook` and `CatalogHit` carry on the plugin ABI.
//!
//! # Audience
//!
//! Storefront guests that turn store payloads into credits, identifiers and
//! series entries, and host code that still reads the flat `books` row view
//! until the library schema stores works, expressions and editions
//! (`docs/adr/library-data-model.md`).

use bookclerk_plugin_abi as abi;

pub use abi::{
    Credit, CreditLevel, CreditRole, ExpressionKind, Identifier, IdentifierScheme, MediaKind,
    SeriesEntry, WorkKind,
};

/// Separator the flat `books` row view uses between contributor names.
pub const NAME_SEPARATOR: &str = ", ";

/// Name suffixes that belong to the name before them when a display string
/// is split on commas (`Martin Luther King, Jr.`).
const NAME_SUFFIXES: &[&str] = &[
    "jr", "jr.", "sr", "sr.", "ii", "iii", "iv", "phd", "ph.d.", "md", "m.d.", "esq", "esq.",
];

/// One credit, or `None` when `name` is blank after trimming.
///
/// # Arguments
///
/// * `name` - contributor display name, one person or body.
/// * `role` - what the contributor did; the level is the role's usual one.
#[must_use]
pub fn credit(name: &str, role: CreditRole) -> Option<Credit> {
    let name = name.trim();
    (!name.is_empty()).then(|| Credit {
        name: name.to_string(),
        role,
        level: CreditLevel::RoleDefault,
    })
}

/// Credits for a list of names that a store already returns one per entry,
/// in billing order, skipping blank names.
#[must_use]
pub fn credits_from_names<I, S>(names: I, role: CreditRole) -> Vec<Credit>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    names
        .into_iter()
        .filter_map(|name| credit(name.as_ref(), role))
        .collect()
}

/// Credits for a store's display string that lists several names in one
/// field (`"Jane Doe, John Roe & Ann Poe"`).
///
/// Splits on `,`, `;`, `&` and the word `and`, and keeps a name suffix such
/// as `Jr.` with the name before it. Prefer [`credits_from_names`] when the
/// store returns a list.
#[must_use]
pub fn credits_from_display(display: Option<&str>, role: CreditRole) -> Vec<Credit> {
    display
        .map(split_display_names)
        .map(|names| credits_from_names(names, role))
        .unwrap_or_default()
}

/// Splits a display string of contributor names into one name per entry.
///
/// # Returns
///
/// The trimmed, non-empty names in the order they appear. HTML entities
/// are decoded first, so `O&#39;Brien` stays one name.
#[must_use]
pub fn split_display_names(display: &str) -> Vec<String> {
    let mut decoded = display.to_string();
    bookclerk_library::decode_html_entities_in_place(&mut decoded);
    // Any run of whitespace (tabs, newlines, no-break spaces) reads as one
    // space, so the word `and` splits however the store spaced it.
    let decoded = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut names: Vec<String> = Vec::new();
    for part in decoded.split([',', ';', '&']) {
        for piece in split_on_and(part) {
            let piece = piece.trim();
            if piece.is_empty() {
                continue;
            }
            let is_suffix = NAME_SUFFIXES.contains(&piece.to_ascii_lowercase().as_str());
            match names.last_mut() {
                Some(previous) if is_suffix => {
                    previous.push_str(NAME_SEPARATOR);
                    previous.push_str(piece);
                }
                _ => names.push(piece.to_string()),
            }
        }
    }
    names
}

/// Splits on the standalone word `and`, matched without regard to case.
fn split_on_and(part: &str) -> Vec<&str> {
    let lower = part.to_ascii_lowercase();
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut search = 0;
    while let Some(found) = lower[search..].find(" and ") {
        let at = search + found;
        pieces.push(&part[start..at]);
        start = at + " and ".len();
        search = start;
    }
    pieces.push(&part[start..]);
    pieces
}

/// One identifier, or `None` when `value` is blank after trimming.
///
/// # Arguments
///
/// * `scheme` - scheme the value belongs to.
/// * `value` - the identifier as the store printed it.
/// * `marketplace` - marketplace an ASIN is scoped to; ignored when blank.
#[must_use]
pub fn identifier(
    scheme: IdentifierScheme,
    value: &str,
    marketplace: Option<&str>,
) -> Option<Identifier> {
    let value = value.trim();
    (!value.is_empty()).then(|| Identifier {
        scheme,
        value: value.to_string(),
        marketplace: marketplace
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string),
    })
}

/// A single-entry series list, or empty when `name` is missing or blank.
#[must_use]
pub fn series_entries(name: Option<&str>, position: Option<&str>) -> Vec<SeriesEntry> {
    name.map(str::trim)
        .filter(|n| !n.is_empty())
        .map(|name| {
            vec![SeriesEntry {
                name: name.to_string(),
                position: position
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(str::to_string),
                store_series_id: None,
            }]
        })
        .unwrap_or_default()
}

/// Names credited with `role`, joined for the flat `books` row view.
///
/// # Returns
///
/// `None` when no credit has the role.
#[must_use]
pub fn joined_names(credits: &[Credit], role: CreditRole) -> Option<String> {
    let names: Vec<&str> = credits
        .iter()
        .filter(|c| c.role == role)
        .map(|c| c.name.as_str())
        .collect();
    (!names.is_empty()).then(|| names.join(NAME_SEPARATOR))
}

/// First non-blank identifier value in `scheme`, normalised, for the flat
/// `books` row view.
///
/// Values are normalised with [`normalize_identifier`], so a guest that
/// sends a padded, hyphenated ISBN-10 still fills the column with the
/// ISBN-13 the library keys on.
#[must_use]
pub fn identifier_value(identifiers: &[Identifier], scheme: IdentifierScheme) -> Option<String> {
    identifiers
        .iter()
        .filter(|id| id.scheme == scheme)
        .find_map(|id| normalize_identifier(scheme, &id.value))
}

/// The first ASIN scoped to `marketplace`, else the first unscoped ASIN,
/// else the first ASIN of any marketplace, normalised.
///
/// Marketplaces compare without regard to case or surrounding space.
#[must_use]
pub fn asin_for_marketplace(identifiers: &[Identifier], marketplace: &str) -> Option<String> {
    /// How well an ASIN's scope fits the wanted marketplace; lower is better.
    fn rank(id: &Identifier, marketplace: &str) -> u8 {
        match id.marketplace.as_deref().map(str::trim) {
            Some(scope) if scope.eq_ignore_ascii_case(marketplace) => 0,
            None | Some("") => 1,
            Some(_) => 2,
        }
    }
    let marketplace = marketplace.trim();
    identifiers
        .iter()
        .filter(|id| id.scheme == IdentifierScheme::Asin)
        .filter_map(|id| {
            normalize_identifier(IdentifierScheme::Asin, &id.value)
                .map(|value| (rank(id, marketplace), value))
        })
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, value)| value)
}

/// The canonical form of an identifier value as a source printed it.
///
/// Every value is trimmed. ASINs are upper-cased. ISBNs lose spaces and
/// hyphens, and a valid ISBN-10 becomes its ISBN-13; an ISBN of any other
/// shape is kept as the stripped text, since rejecting it would lose data.
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
    Some(match scheme {
        IdentifierScheme::Asin => value.to_ascii_uppercase(),
        IdentifierScheme::Isbn => normalize_isbn(value),
        IdentifierScheme::OpenLibraryEdition | IdentifierScheme::OpenLibraryWork => {
            value.to_string()
        }
    })
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
    let body = format!("978{}", &isbn10[..9]);
    let sum13: u32 = body
        .bytes()
        .zip([1u32, 3].into_iter().cycle())
        .map(|(byte, weight)| u32::from(byte - b'0') * weight)
        .sum();
    Some(format!("{body}{}", (10 - sum13 % 10) % 10))
}

/// The flat `books.content_kind` value for a work kind.
///
/// Drama has no flat kind of its own and reads as `book`.
#[must_use]
pub fn content_kind(kind: WorkKind) -> &'static str {
    match kind {
        WorkKind::Book | WorkKind::Drama => "book",
        WorkKind::PodcastShow => "podcast",
        WorkKind::PodcastEpisode => "episode",
    }
}

/// The work kind for a store's own item-kind label (`episode`, `podcast`).
///
/// Labels a storefront uses for podcast shows (`podcast`, `parent`,
/// `podcastparent`, `season`) read as [`WorkKind::PodcastShow`]; anything
/// else is a book.
#[must_use]
pub fn work_kind_from_label(label: &str) -> WorkKind {
    if bookclerk_library::is_episode(label) {
        WorkKind::PodcastEpisode
    } else if bookclerk_library::is_podcast_parent(label) {
        WorkKind::PodcastShow
    } else {
        WorkKind::Book
    }
}

/// The expression and media kinds an audio storefront's item has, given its
/// work kind: podcast shows and episodes are podcast audio, everything else
/// is a narrated audiobook.
#[must_use]
pub fn audio_kinds(work: WorkKind) -> (ExpressionKind, MediaKind) {
    match work {
        WorkKind::PodcastShow | WorkKind::PodcastEpisode => {
            (ExpressionKind::PodcastAudio, MediaKind::PodcastAudio)
        }
        WorkKind::Book | WorkKind::Drama => (ExpressionKind::Narration, MediaKind::Audiobook),
    }
}

/// Adds the credits of `fill` whose role `base` has none of, so a merge of
/// two hits for one edition never overwrites the credits already known.
pub fn fill_credits(base: &mut Vec<Credit>, fill: Vec<Credit>) {
    let known: Vec<CreditRole> = base.iter().map(|c| c.role).collect();
    base.extend(fill.into_iter().filter(|c| !known.contains(&c.role)));
}

/// Adds the identifiers of `fill` whose scheme `base` has none of.
pub fn fill_identifiers(base: &mut Vec<Identifier>, fill: Vec<Identifier>) {
    let known: Vec<IdentifierScheme> = base.iter().map(|id| id.scheme).collect();
    base.extend(fill.into_iter().filter(|id| !known.contains(&id.scheme)));
}

/// Decodes HTML entities in credit and series names in place.
pub(crate) fn decode_html_entities(credits: &mut [Credit], series: &mut [SeriesEntry]) {
    for credit in credits {
        bookclerk_library::decode_html_entities_in_place(&mut credit.name);
    }
    for entry in series {
        bookclerk_library::decode_html_entities_in_place(&mut entry.name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_split_on_separators_and_keep_suffixes() {
        assert_eq!(
            split_display_names("Martin Luther King, Jr.; Jane Doe & John Roe and Ann Poe"),
            vec!["Martin Luther King, Jr.", "Jane Doe", "John Roe", "Ann Poe"]
        );
        assert_eq!(split_display_names("  ,; "), Vec::<String>::new());
        assert_eq!(
            split_display_names("Brandon Sanderson"),
            vec!["Brandon Sanderson"]
        );
        assert_eq!(
            split_display_names("Penn AND Teller"),
            vec!["Penn", "Teller"]
        );
    }

    #[test]
    fn blank_values_make_no_record() {
        assert!(credit("  ", CreditRole::Author).is_none());
        assert!(identifier(IdentifierScheme::Isbn, " ", None).is_none());
        assert!(series_entries(Some(" "), Some("1")).is_empty());
        assert!(series_entries(None, Some("1")).is_empty());
        assert!(credits_from_display(None, CreditRole::Narrator).is_empty());
    }

    #[test]
    fn records_trim_and_scope() {
        let id = identifier(IdentifierScheme::Asin, " B0X ", Some(" us ")).expect("asin");
        assert_eq!(id.value, "B0X");
        assert_eq!(id.marketplace.as_deref(), Some("us"));
        let global = identifier(IdentifierScheme::Isbn, "978", Some("")).expect("isbn");
        assert_eq!(global.marketplace, None);
        let entry = series_entries(Some(" Stormlight "), Some(" "));
        assert_eq!(entry[0].name, "Stormlight");
        assert_eq!(entry[0].position, None);
    }

    #[test]
    fn flat_views_join_by_role_and_pick_by_scheme() {
        let mut credits = credits_from_names(["A", " ", "B"], CreditRole::Author);
        credits.extend(credits_from_display(Some("N"), CreditRole::Narrator));
        assert_eq!(
            joined_names(&credits, CreditRole::Author).as_deref(),
            Some("A, B")
        );
        assert_eq!(
            joined_names(&credits, CreditRole::Narrator).as_deref(),
            Some("N")
        );
        assert_eq!(joined_names(&credits, CreditRole::Translator), None);
        let ids: Vec<Identifier> = [
            identifier(IdentifierScheme::Asin, "B1", None),
            identifier(IdentifierScheme::Isbn, "978", None),
        ]
        .into_iter()
        .flatten()
        .collect();
        assert_eq!(
            identifier_value(&ids, IdentifierScheme::Isbn).as_deref(),
            Some("978")
        );
        assert_eq!(
            identifier_value(&ids, IdentifierScheme::OpenLibraryWork),
            None
        );
    }

    #[test]
    fn fills_add_only_missing_roles_and_schemes() {
        let mut base = credits_from_names(["A"], CreditRole::Author);
        let mut fill = credits_from_names(["Other"], CreditRole::Author);
        fill.extend(credits_from_names(["N"], CreditRole::Narrator));
        fill_credits(&mut base, fill);
        assert_eq!(
            joined_names(&base, CreditRole::Author).as_deref(),
            Some("A")
        );
        assert_eq!(
            joined_names(&base, CreditRole::Narrator).as_deref(),
            Some("N")
        );
        let mut ids: Vec<Identifier> = identifier(IdentifierScheme::Isbn, "1", None)
            .into_iter()
            .collect();
        let more: Vec<Identifier> = [
            identifier(IdentifierScheme::Isbn, "2", None),
            identifier(IdentifierScheme::Asin, "B", None),
        ]
        .into_iter()
        .flatten()
        .collect();
        fill_identifiers(&mut ids, more);
        assert_eq!(
            identifier_value(&ids, IdentifierScheme::Isbn).as_deref(),
            Some("1")
        );
        assert_eq!(
            identifier_value(&ids, IdentifierScheme::Asin).as_deref(),
            Some("B")
        );
    }

    #[test]
    fn work_kinds_round_trip_through_the_flat_label() {
        for kind in [
            WorkKind::Book,
            WorkKind::PodcastShow,
            WorkKind::PodcastEpisode,
        ] {
            assert_eq!(work_kind_from_label(content_kind(kind)), kind);
        }
        assert_eq!(content_kind(WorkKind::Drama), "book");
        assert_eq!(work_kind_from_label("season"), WorkKind::PodcastShow);
        assert_eq!(work_kind_from_label("anything"), WorkKind::Book);
    }

    #[test]
    fn html_entities_decode_in_names() {
        let mut credits = credits_from_names(["O&#39;Brien"], CreditRole::Author);
        let mut series = series_entries(Some("Memory&#39;s Blade"), None);
        decode_html_entities(&mut credits, &mut series);
        assert_eq!(credits[0].name, "O'Brien");
        assert_eq!(series[0].name, "Memory's Blade");
    }

    #[test]
    fn identifier_values_are_normalised() {
        let ids = vec![
            Identifier {
                scheme: IdentifierScheme::Isbn,
                value: "   ".into(),
                marketplace: None,
            },
            Identifier {
                scheme: IdentifierScheme::Isbn,
                value: " 0-14-044913-2 ".into(),
                marketplace: None,
            },
            Identifier {
                scheme: IdentifierScheme::Asin,
                value: " b00abc1234".into(),
                marketplace: None,
            },
        ];
        assert_eq!(
            identifier_value(&ids, IdentifierScheme::Isbn).as_deref(),
            Some("9780140449136")
        );
        assert_eq!(
            identifier_value(&ids, IdentifierScheme::Asin).as_deref(),
            Some("B00ABC1234")
        );
        assert_eq!(
            identifier_value(&ids, IdentifierScheme::OpenLibraryWork),
            None
        );
    }

    #[test]
    fn isbn_normalisation_keeps_what_it_cannot_convert() {
        let isbn = |raw| normalize_identifier(IdentifierScheme::Isbn, raw);
        assert_eq!(isbn("978-0-14-044913-6").as_deref(), Some("9780140449136"));
        assert_eq!(isbn("080442957x").as_deref(), Some("9780804429573"));
        // A bad check digit is not converted.
        assert_eq!(isbn("0140449137").as_deref(), Some("0140449137"));
        assert_eq!(isbn("12345").as_deref(), Some("12345"));
        assert_eq!(isbn("01404491X6").as_deref(), Some("01404491X6"));
        assert_eq!(isbn(" "), None);
        assert_eq!(
            normalize_identifier(IdentifierScheme::OpenLibraryEdition, " OL1M ").as_deref(),
            Some("OL1M")
        );
    }

    #[test]
    fn podcasts_are_podcast_audio() {
        for work in [WorkKind::PodcastShow, WorkKind::PodcastEpisode] {
            assert_eq!(
                audio_kinds(work),
                (ExpressionKind::PodcastAudio, MediaKind::PodcastAudio)
            );
        }
        for work in [WorkKind::Book, WorkKind::Drama] {
            assert_eq!(
                audio_kinds(work),
                (ExpressionKind::Narration, MediaKind::Audiobook)
            );
        }
    }

    #[test]
    fn html_entities_are_decoded_before_splitting() {
        assert_eq!(
            split_display_names("Tim O&#39;Brien &amp; Jane Doe"),
            vec!["Tim O'Brien".to_string(), "Jane Doe".to_string()]
        );
    }

    #[test]
    fn and_splits_across_any_whitespace() {
        assert_eq!(
            split_display_names("Jane Doe\tand\nJohn\u{a0}Roe"),
            vec!["Jane Doe".to_string(), "John Roe".to_string()]
        );
    }

    #[test]
    fn asin_prefers_the_book_marketplace() {
        let asin = |value: &str, marketplace: Option<&str>| Identifier {
            scheme: IdentifierScheme::Asin,
            value: value.into(),
            marketplace: marketplace.map(str::to_string),
        };
        let ids = vec![
            asin("B0US", Some("us")),
            asin("B0ANY", None),
            asin("b0uk", Some(" UK ")),
        ];
        assert_eq!(asin_for_marketplace(&ids, "uk").as_deref(), Some("B0UK"));
        assert_eq!(asin_for_marketplace(&ids, "de").as_deref(), Some("B0ANY"));
        assert_eq!(
            asin_for_marketplace(&ids[..1], "de").as_deref(),
            Some("B0US")
        );
        assert_eq!(asin_for_marketplace(&[], "us"), None);
    }
}
