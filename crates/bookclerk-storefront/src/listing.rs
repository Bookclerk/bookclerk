//! Plugin ABI records as resolver listing drafts.
//!
//! The capnp enums and the resolver's stored enums are separate types: the
//! wire may grow a variant before the host stores it. Each mapping here is
//! an exhaustive `match`, so a new variant on either side fails to compile
//! until it is mapped.

use bookclerk_plugin_abi as abi;
use bookclerk_resolve as resolve;

/// The resolver's work kind for a wire work kind.
///
/// # Returns
///
/// `None` for `unspecified`, so [`bookclerk_resolve::Listing::from_draft`]
/// refuses the record.
#[must_use]
pub fn work_kind(kind: abi::WorkKind) -> Option<resolve::WorkKind> {
    match kind {
        abi::WorkKind::Unspecified => None,
        abi::WorkKind::Book => Some(resolve::WorkKind::Book),
        abi::WorkKind::Podcast => Some(resolve::WorkKind::Podcast),
    }
}

/// The resolver's expression kind for a wire expression kind.
///
/// # Returns
///
/// `None` for `unspecified`, so the record is refused.
#[must_use]
pub fn expression_kind(kind: abi::ExpressionKind) -> Option<resolve::ExpressionKind> {
    match kind {
        abi::ExpressionKind::Unspecified => None,
        abi::ExpressionKind::Text => Some(resolve::ExpressionKind::Text),
        abi::ExpressionKind::Narration => Some(resolve::ExpressionKind::Narration),
        abi::ExpressionKind::Dramatisation => Some(resolve::ExpressionKind::Dramatisation),
        abi::ExpressionKind::Graphic => Some(resolve::ExpressionKind::Graphic),
        abi::ExpressionKind::OriginalAudio => Some(resolve::ExpressionKind::OriginalAudio),
    }
}

/// The resolver's media kind for a wire media kind.
///
/// # Returns
///
/// `None` for `unspecified`, so the record is refused.
#[must_use]
pub fn media_kind(kind: abi::MediaKind) -> Option<resolve::MediaKind> {
    match kind {
        abi::MediaKind::Unspecified => None,
        abi::MediaKind::Audio => Some(resolve::MediaKind::Audio),
        abi::MediaKind::Ebook => Some(resolve::MediaKind::Ebook),
        abi::MediaKind::Print => Some(resolve::MediaKind::Print),
    }
}

/// The resolver's abridgement for a wire abridgement.
#[must_use]
pub fn abridgement(value: abi::Abridgement) -> resolve::Abridgement {
    match value {
        abi::Abridgement::Unknown => resolve::Abridgement::Unknown,
        abi::Abridgement::Unabridged => resolve::Abridgement::Unabridged,
        abi::Abridgement::Abridged => resolve::Abridgement::Abridged,
    }
}

/// The resolver's identifier scheme for a wire scheme.
#[must_use]
pub fn identifier_scheme(scheme: abi::IdentifierScheme) -> resolve::IdentifierScheme {
    match scheme {
        abi::IdentifierScheme::Isbn => resolve::IdentifierScheme::Isbn,
        abi::IdentifierScheme::Asin => resolve::IdentifierScheme::Asin,
        abi::IdentifierScheme::OpenLibraryEdition => resolve::IdentifierScheme::OpenLibraryEdition,
        abi::IdentifierScheme::OpenLibraryWork => resolve::IdentifierScheme::OpenLibraryWork,
    }
}

/// The resolver's credit role for a wire credit role.
#[must_use]
pub fn credit_role(role: abi::CreditRole) -> resolve::CreditRole {
    match role {
        abi::CreditRole::Author => resolve::CreditRole::Author,
        abi::CreditRole::Narrator => resolve::CreditRole::Narrator,
        abi::CreditRole::Translator => resolve::CreditRole::Translator,
        abi::CreditRole::Editor => resolve::CreditRole::Editor,
        abi::CreditRole::Illustrator => resolve::CreditRole::Illustrator,
        abi::CreditRole::Publisher => resolve::CreditRole::Publisher,
        abi::CreditRole::Host => resolve::CreditRole::Host,
        abi::CreditRole::Cast => resolve::CreditRole::Cast,
        abi::CreditRole::Adaptor => resolve::CreditRole::Adaptor,
    }
}

/// The level a wire credit names; `None` for the role's usual level.
#[must_use]
pub fn credit_level(level: abi::CreditLevel) -> Option<resolve::Level> {
    match level {
        abi::CreditLevel::RoleDefault => None,
        abi::CreditLevel::Work => Some(resolve::Level::Work),
        abi::CreditLevel::Expression => Some(resolve::Level::Expression),
        abi::CreditLevel::Edition => Some(resolve::Level::Edition),
    }
}

/// HTML-entity-decoded copy of a human-readable scan field.
///
/// Storefronts such as Libro.fm send `O&#39;Brien`; the flat row decodes
/// it on write, so the listing must too or its match keys would differ
/// from the ones derived from the flat row.
fn decode(raw: &str) -> String {
    crate::text::decode_html_entities(raw)
}

/// The listing draft a storefront's scan record describes.
///
/// The plugin key is left blank: the host sets it from the plugin it ran,
/// never from the record. A publisher named only in `publisher` becomes a
/// publisher credit. Human-readable text is HTML-entity decoded, as the
/// flat row is.
///
/// # Arguments
///
/// * `book` - The scan record as the guest returned it.
///
/// # Returns
///
/// The unchecked draft; [`bookclerk_resolve::Listing::from_draft`] validates it.
#[must_use]
pub fn scan_book_draft(book: &abi::ScanBook) -> resolve::ListingDraft {
    let mut credits: Vec<(String, resolve::CreditRole, Option<resolve::Level>)> = book
        .credits
        .iter()
        .map(|c| (decode(&c.name), credit_role(c.role), credit_level(c.level)))
        .collect();
    let has_publisher = credits
        .iter()
        .any(|(name, role, _)| *role == resolve::CreditRole::Publisher && !name.trim().is_empty());
    if let Some(publisher) = book.publisher.as_ref().filter(|p| !p.trim().is_empty()) {
        if !has_publisher {
            credits.push((decode(publisher), resolve::CreditRole::Publisher, None));
        }
    }
    resolve::ListingDraft {
        plugin_key: String::new(),
        marketplace: book.marketplace.clone(),
        product_id: book.product_id.clone(),
        work_kind: work_kind(book.work_kind),
        expression_kind: expression_kind(book.expression_kind),
        media_kind: media_kind(book.media_kind),
        language: book.language.clone(),
        abridgement: Some(abridgement(book.abridgement)),
        title: decode(&book.title),
        subtitle: book.subtitle.as_deref().map(decode),
        published_at: book.published_at.clone(),
        duration_minutes: book.length_minutes,
        categories: book.categories.as_deref().map(decode),
        identifiers: book
            .identifiers
            .iter()
            .map(|id| {
                (
                    identifier_scheme(id.scheme),
                    id.value.clone(),
                    id.marketplace.clone(),
                )
            })
            .collect(),
        credits,
        series: book
            .series_entries
            .iter()
            .map(|s| (decode(&s.name), s.position.clone()))
            .collect(),
        parent_product_id: book.parent_product_id.clone(),
    }
}

/// The listing draft a storefront's catalog hit describes, for the shared
/// catalog pool.
///
/// Like [`scan_book_draft`], the plugin key is left blank for the host to
/// set from the plugin it asked. The marketplace is the one the host asked
/// the store about (catalog hits carry none of their own). Abridgement is
/// unknown unless the store flagged it. Human-readable text is HTML-entity
/// decoded, as for scans, so match keys agree with a later scan of the same
/// title.
///
/// # Arguments
///
/// * `hit` - The catalog hit as the guest returned it.
/// * `marketplace` - Marketplace the store was asked about (`us`).
///
/// # Returns
///
/// The unchecked draft; [`bookclerk_resolve::Listing::from_draft`] validates it.
#[must_use]
pub fn catalog_hit_draft(hit: &crate::CatalogHit, marketplace: &str) -> resolve::ListingDraft {
    let mut credits: Vec<(String, resolve::CreditRole, Option<resolve::Level>)> = hit
        .credits
        .iter()
        .map(|c| (decode(&c.name), credit_role(c.role), credit_level(c.level)))
        .collect();
    let has_publisher = credits
        .iter()
        .any(|(name, role, _)| *role == resolve::CreditRole::Publisher && !name.trim().is_empty());
    if let Some(publisher) = hit.publisher.as_ref().filter(|p| !p.trim().is_empty()) {
        if !has_publisher {
            credits.push((decode(publisher), resolve::CreditRole::Publisher, None));
        }
    }
    resolve::ListingDraft {
        plugin_key: String::new(),
        marketplace: Some(marketplace.to_string()),
        product_id: hit.product_id.clone(),
        work_kind: work_kind(hit.work_kind),
        expression_kind: expression_kind(hit.expression_kind),
        media_kind: media_kind(hit.media_kind),
        language: hit.language.clone(),
        abridgement: Some(match hit.is_abridged {
            Some(true) => resolve::Abridgement::Abridged,
            Some(false) => resolve::Abridgement::Unabridged,
            None => resolve::Abridgement::Unknown,
        }),
        title: decode(&hit.title),
        subtitle: hit.subtitle.as_deref().map(decode),
        published_at: hit.published_at.clone(),
        duration_minutes: hit.length_minutes,
        categories: hit.categories.as_deref().map(decode),
        identifiers: hit
            .identifiers
            .iter()
            .map(|id| {
                (
                    identifier_scheme(id.scheme),
                    id.value.clone(),
                    id.marketplace.clone(),
                )
            })
            .collect(),
        credits,
        series: hit
            .series_entries
            .iter()
            .map(|s| (decode(&s.name), s.position.clone()))
            .collect(),
        parent_product_id: hit.parent_product_id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_wire_variant_maps() {
        assert_eq!(
            work_kind(abi::WorkKind::Podcast),
            Some(resolve::WorkKind::Podcast)
        );
        assert_eq!(
            expression_kind(abi::ExpressionKind::Graphic),
            Some(resolve::ExpressionKind::Graphic)
        );
        assert_eq!(
            media_kind(abi::MediaKind::Print),
            Some(resolve::MediaKind::Print)
        );
        // Unspecified kinds leave the draft without one, so it is refused.
        assert_eq!(work_kind(abi::WorkKind::Unspecified), None);
        assert_eq!(expression_kind(abi::ExpressionKind::Unspecified), None);
        assert_eq!(media_kind(abi::MediaKind::Unspecified), None);
        assert_eq!(
            abridgement(abi::Abridgement::Abridged),
            resolve::Abridgement::Abridged
        );
        assert_eq!(
            identifier_scheme(abi::IdentifierScheme::OpenLibraryWork),
            resolve::IdentifierScheme::OpenLibraryWork
        );
        assert_eq!(
            credit_role(abi::CreditRole::Cast),
            resolve::CreditRole::Cast
        );
        assert_eq!(credit_level(abi::CreditLevel::RoleDefault), None);
        assert_eq!(
            credit_level(abi::CreditLevel::Edition),
            Some(resolve::Level::Edition)
        );
    }

    #[test]
    fn catalog_hits_become_drafts_in_the_asked_marketplace() {
        let hit = crate::CatalogHit {
            product_id: "9780441172719".into(),
            title: "Dune &amp; More".into(),
            publisher: Some("Ace".into()),
            is_abridged: Some(false),
            credits: vec![abi::Credit {
                name: "Frank Herbert".into(),
                role: abi::CreditRole::Author,
                level: abi::CreditLevel::RoleDefault,
            }],
            identifiers: vec![abi::Identifier {
                scheme: abi::IdentifierScheme::Isbn,
                value: "9780441172719".into(),
                marketplace: None,
            }],
            work_kind: abi::WorkKind::Book,
            expression_kind: abi::ExpressionKind::Text,
            media_kind: abi::MediaKind::Print,
            ..crate::CatalogHit::default()
        };
        let draft = catalog_hit_draft(&hit, "us");
        assert_eq!(draft.marketplace.as_deref(), Some("us"));
        assert_eq!(draft.title, "Dune & More");
        assert_eq!(draft.abridgement, Some(resolve::Abridgement::Unabridged));
        assert_eq!(draft.identifiers.len(), 1);
        assert!(draft
            .credits
            .iter()
            .any(|(n, r, _)| n == "Ace" && *r == resolve::CreditRole::Publisher));
        assert!(resolve::Listing::from_draft(&resolve::ListingDraft {
            plugin_key: "libro".into(),
            ..draft
        })
        .is_ok());
    }

    #[test]
    fn scan_records_become_drafts_with_a_publisher_credit() {
        let book = abi::ScanBook {
            account_id: "a1".into(),
            product_id: "B001".into(),
            title: "Dune".into(),
            publisher: Some("Macmillan Audio".into()),
            identifiers: vec![abi::Identifier {
                scheme: abi::IdentifierScheme::Asin,
                value: "B001".into(),
                marketplace: None,
            }],
            credits: vec![abi::Credit {
                name: "Frank Herbert".into(),
                role: abi::CreditRole::Author,
                level: abi::CreditLevel::RoleDefault,
            }],
            series_entries: vec![abi::SeriesEntry {
                name: "Dune".into(),
                position: Some("1".into()),
                store_series_id: None,
            }],
            ..abi::ScanBook::default()
        };
        let draft = scan_book_draft(&book);
        assert_eq!(draft.credits.len(), 2);
        assert_eq!(draft.credits[1].1, resolve::CreditRole::Publisher);
        assert_eq!(draft.identifiers[0].0, resolve::IdentifierScheme::Asin);
        assert_eq!(draft.series[0].1.as_deref(), Some("1"));
        assert!(draft.plugin_key.is_empty());
    }

    #[test]
    fn a_blank_publisher_credit_does_not_hide_the_publisher_field() {
        let book = abi::ScanBook {
            account_id: "a1".into(),
            product_id: "B001".into(),
            title: "Dune".into(),
            publisher: Some("Macmillan Audio".into()),
            credits: vec![abi::Credit {
                name: "  ".into(),
                role: abi::CreditRole::Publisher,
                level: abi::CreditLevel::RoleDefault,
            }],
            ..abi::ScanBook::default()
        };
        let draft = scan_book_draft(&book);
        assert!(draft
            .credits
            .iter()
            .any(|(name, role, _)| name == "Macmillan Audio"
                && *role == resolve::CreditRole::Publisher));
    }

    #[test]
    fn scan_drafts_decode_html_entities_like_the_flat_row() {
        let book = abi::ScanBook {
            account_id: "a1".into(),
            product_id: "9780000000001".into(),
            title: "O&#39;Brien&#39;s War".into(),
            subtitle: Some("Tom &amp; Jerry".into()),
            publisher: Some("Smith &amp; Sons".into()),
            credits: vec![abi::Credit {
                name: "Fl&eacute;ur".into(),
                role: abi::CreditRole::Author,
                level: abi::CreditLevel::RoleDefault,
            }],
            series_entries: vec![abi::SeriesEntry {
                name: "Kings &amp; Queens".into(),
                position: None,
                store_series_id: None,
            }],
            ..abi::ScanBook::default()
        };
        let draft = scan_book_draft(&book);
        assert_eq!(draft.title, "O'Brien's War");
        assert_eq!(draft.subtitle.as_deref(), Some("Tom & Jerry"));
        assert_eq!(draft.credits[0].0, "Fl\u{e9}ur");
        assert_eq!(draft.credits[1].0, "Smith & Sons");
        assert_eq!(draft.series[0].0, "Kings & Queens");
    }
}
