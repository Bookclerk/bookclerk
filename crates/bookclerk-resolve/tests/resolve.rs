//! Resolver behaviour, driven through the in-memory catalog.

// Helpers shared by the tests unwrap like the tests do.
#![allow(clippy::unwrap_used)]

use bookclerk_resolve::memory::MemoryCatalog;
use bookclerk_resolve::{
    Abridgement, Conflict, CreditRole, Entitlement, Event, ExpressionKind, Holder, HoldingDraft,
    IdentifierScheme, LinkReason, Listing, ListingDraft, ListingKey, MediaKind, Override, WorkKind,
    WorkReason, WorkRow,
};

/// An Audible-style unabridged English narration of Dune.
fn dune(plugin: &str, product: &str) -> ListingDraft {
    ListingDraft {
        plugin_key: plugin.into(),
        marketplace: Some("us".into()),
        product_id: product.into(),
        language: Some("en".into()),
        abridgement: Some(Abridgement::Unabridged),
        title: "Dune".into(),
        credits: vec![
            ("Frank Herbert".into(), CreditRole::Author, None),
            ("Scott Brick".into(), CreditRole::Narrator, None),
        ],
        ..ListingDraft::default()
    }
}

fn with_id(mut d: ListingDraft, scheme: IdentifierScheme, value: &str) -> ListingDraft {
    d.identifiers.push((scheme, value.into(), None));
    d
}

fn holding(holder: &str) -> HoldingDraft {
    HoldingDraft {
        holder: Holder::Account(holder.into()),
        entitlement: Entitlement::Purchase,
        purchased_at: None,
    }
}

fn record(
    catalog: &mut MemoryCatalog,
    draft: &ListingDraft,
    holder: &str,
) -> bookclerk_resolve::Plan {
    let listing = Listing::from_draft(draft).unwrap();
    catalog.record(&listing, &holding(holder)).unwrap()
}

fn work_of(catalog: &MemoryCatalog, edition: &str) -> String {
    catalog.work_of_edition(edition).unwrap().id.clone()
}

#[test]
fn two_accounts_holding_one_listing_share_it() {
    let mut c = MemoryCatalog::default();
    let a = record(&mut c, &dune("audible", "B001"), "a1");
    assert_eq!(a.link, LinkReason::New);
    assert!(a
        .events
        .iter()
        .any(|e| matches!(e, Event::EditionLinked { .. })));
    let b = record(&mut c, &dune("audible", "B001"), "a2");
    assert_eq!(b.link, LinkReason::Listing);
    assert!(b.holding_is_new);
    assert_eq!(b.events.len(), 1);
    assert_eq!(c.listings.len(), 1);
    assert_eq!(c.holdings.len(), 2);
    assert!(record(&mut c, &dune("audible", "B001"), "a2").is_noop());
}

#[test]
fn two_stores_of_one_narration_share_work_and_expression() {
    let mut c = MemoryCatalog::default();
    let a = record(&mut c, &dune("audible", "B001"), "a1");
    let b = record(&mut c, &dune("libro", "9780000000001"), "l1");
    assert_eq!(b.link, LinkReason::New);
    assert_eq!(b.work_reason, Some(WorkReason::Matcher));
    assert!(b.new_work.is_none());
    assert!(
        b.new_expression.is_none(),
        "same narrator, language and abridgement"
    );
    assert_ne!(a.listing.edition_id, b.listing.edition_id);
}

#[test]
fn unknown_language_makes_a_new_expression() {
    let mut c = MemoryCatalog::default();
    record(&mut c, &dune("audible", "B001"), "a1");
    let mut other = dune("libro", "L1");
    other.language = None;
    let b = record(&mut c, &other, "l1");
    assert_eq!(b.work_reason, Some(WorkReason::Matcher));
    assert!(b.new_expression.is_some());
}

#[test]
fn a_shared_isbn_joins_a_compatible_edition_only() {
    let mut c = MemoryCatalog::default();
    let a = record(
        &mut c,
        &with_id(
            dune("audible", "B001"),
            IdentifierScheme::Isbn,
            "0-14-044913-2",
        ),
        "a1",
    );
    let b = record(
        &mut c,
        &with_id(dune("libro", "L1"), IdentifierScheme::Isbn, "9780140449136"),
        "l1",
    );
    assert_eq!(b.link, LinkReason::WeakIdentifier);
    assert_eq!(b.listing.edition_id, a.listing.edition_id);

    // The abridged release carries the same ISBN but is another expression.
    let mut abridged = with_id(dune("chirp", "C1"), IdentifierScheme::Isbn, "9780140449136");
    abridged.abridgement = Some(Abridgement::Abridged);
    let d = record(&mut c, &abridged, "c1");
    assert_eq!(d.link, LinkReason::New);
    assert_ne!(d.listing.edition_id, a.listing.edition_id);
    assert_eq!(
        work_of(&c, &d.listing.edition_id),
        work_of(&c, &a.listing.edition_id)
    );

    // An ebook with that ISBN is never joined to the audiobook.
    let mut ebook = with_id(dune("store", "E1"), IdentifierScheme::Isbn, "9780140449136");
    ebook.media_kind = Some(MediaKind::Ebook);
    ebook.expression_kind = Some(ExpressionKind::Text);
    ebook.credits.truncate(1);
    let e = record(&mut c, &ebook, "s1");
    assert_eq!(e.link, LinkReason::New);
    assert_eq!(
        work_of(&c, &e.listing.edition_id),
        work_of(&c, &a.listing.edition_id)
    );
}

#[test]
fn a_weak_match_never_merges_two_asins() {
    let mut c = MemoryCatalog::default();
    let mut a = with_id(
        dune("audible", "B001"),
        IdentifierScheme::Isbn,
        "9780140449136",
    );
    a = with_id(a, IdentifierScheme::Asin, "B001");
    let a = record(&mut c, &a, "a1");
    let mut b = with_id(
        dune("audible", "B002"),
        IdentifierScheme::Isbn,
        "9780140449136",
    );
    b = with_id(b, IdentifierScheme::Asin, "B002");
    let b = record(&mut c, &b, "a1");
    assert_eq!(b.link, LinkReason::New);
    assert_ne!(a.listing.edition_id, b.listing.edition_id);
}

#[test]
fn strong_identifiers_naming_two_editions_are_recorded_not_merged() {
    let mut c = MemoryCatalog::default();
    let a = record(
        &mut c,
        &with_id(dune("audible", "B001"), IdentifierScheme::Asin, "B001"),
        "a1",
    );
    let mut other = with_id(
        dune("ol", "OL1M"),
        IdentifierScheme::OpenLibraryEdition,
        "OL1M",
    );
    other.language = Some("de".into());
    let b = record(&mut c, &other, "o1");
    let both = with_id(
        with_id(dune("libro", "L1"), IdentifierScheme::Asin, "B001"),
        IdentifierScheme::OpenLibraryEdition,
        "OL1M",
    );
    let d = record(&mut c, &both, "l1");
    assert_eq!(d.link, LinkReason::StrongIdentifier);
    assert_eq!(d.listing.edition_id, a.listing.edition_id);
    assert!(d.edition_identifiers.is_empty());
    assert_eq!(
        d.conflicts,
        vec![Conflict::StrongIdentifiers {
            edition_ids: vec![a.listing.edition_id.clone(), b.listing.edition_id.clone()],
        }]
    );
}

#[test]
fn episodes_match_only_within_their_show() {
    let mut c = MemoryCatalog::default();
    let show = |product: &str| ListingDraft {
        plugin_key: "audible".into(),
        product_id: product.into(),
        title: format!("Show {product}"),
        work_kind: Some(WorkKind::PodcastShow),
        expression_kind: Some(ExpressionKind::PodcastAudio),
        media_kind: Some(MediaKind::PodcastAudio),
        credits: vec![("Host Person".into(), CreditRole::Author, None)],
        ..ListingDraft::default()
    };
    let episode = |product: &str, parent: &str| ListingDraft {
        product_id: product.into(),
        title: "Trailer".into(),
        work_kind: Some(WorkKind::PodcastEpisode),
        parent_product_id: Some(parent.into()),
        ..show(product)
    };
    record(&mut c, &show("S1"), "a1");
    record(&mut c, &show("S2"), "a1");
    let e1 = record(&mut c, &episode("E1", "S1"), "a1");
    let e2 = record(&mut c, &episode("E2", "S2"), "a1");
    assert_eq!(e2.work_reason, Some(WorkReason::New));
    let mut uk = episode("E1", "S1");
    uk.marketplace = Some("uk".into());
    // The UK store has no stored parent show, so the episode stays apart.
    let e3 = record(&mut c, &uk, "a1");
    assert_eq!(e3.work_reason, Some(WorkReason::New));
    let mut libro = episode("E9", "S1");
    libro.plugin_key = "audible".into();
    let e4 = record(&mut c, &libro, "a2");
    assert_eq!(e4.work_reason, Some(WorkReason::Matcher));
    assert_eq!(
        work_of(&c, &e4.listing.edition_id),
        work_of(&c, &e1.listing.edition_id)
    );
    let parent = c
        .work_of_edition(&e1.listing.edition_id)
        .unwrap()
        .parent_work_id
        .clone();
    assert!(parent.is_some());
}

#[test]
fn two_matching_works_join_neither() {
    let mut c = MemoryCatalog::default();
    for id in ["w-a", "w-b"] {
        c.works.insert(
            id.into(),
            WorkRow {
                id: id.into(),
                kind: WorkKind::Book,
                title: "Dune".into(),
                match_key: Listing::from_draft(&dune("x", "x"))
                    .unwrap()
                    .work_match_key(),
                parent_work_id: None,
            },
        );
    }
    let p = record(&mut c, &dune("audible", "B001"), "a1");
    assert_eq!(p.work_reason, Some(WorkReason::New));
    assert_eq!(
        p.conflicts,
        vec![Conflict::AmbiguousWork {
            work_ids: vec!["w-a".into(), "w-b".into()],
        }]
    );
}

#[test]
fn overrides_pin_listings_and_isolate_works() {
    let mut c = MemoryCatalog::default();
    let a = record(&mut c, &dune("audible", "B001"), "a1");
    let work = work_of(&c, &a.listing.edition_id);
    c.overrides.push(Override::IsolateWork {
        work_id: work.clone(),
    });
    let b = record(&mut c, &dune("libro", "L1"), "l1");
    assert_eq!(b.work_reason, Some(WorkReason::New));

    c.overrides.push(Override::PinListing {
        listing: ListingKey {
            plugin_key: "libro".into(),
            marketplace: "us".into(),
            product_id: "L1".into(),
        },
        edition_id: a.listing.edition_id.clone(),
    });
    let again = record(&mut c, &dune("libro", "L1"), "l1");
    assert_eq!(again.link, LinkReason::Pinned);
    assert_eq!(again.listing.edition_id, a.listing.edition_id);
    assert!(matches!(again.events[0], Event::EditionLinked { .. }));
}

#[test]
fn work_identifiers_join_works_of_the_same_kind_only() {
    let mut c = MemoryCatalog::default();
    let a = record(
        &mut c,
        &with_id(
            dune("audible", "B001"),
            IdentifierScheme::OpenLibraryWork,
            "OL1W",
        ),
        "a1",
    );
    let mut renamed = with_id(
        dune("libro", "L1"),
        IdentifierScheme::OpenLibraryWork,
        "OL1W",
    );
    renamed.title = "Dune: Deluxe".into();
    let b = record(&mut c, &renamed, "l1");
    assert_eq!(b.work_reason, Some(WorkReason::Identifier));
    assert_eq!(
        work_of(&c, &b.listing.edition_id),
        work_of(&c, &a.listing.edition_id)
    );

    let mut drama = with_id(dune("ga", "G1"), IdentifierScheme::OpenLibraryWork, "OL1W");
    drama.work_kind = Some(WorkKind::Drama);
    let d = record(&mut c, &drama, "g1");
    assert_eq!(d.work_reason, Some(WorkReason::New));
    assert!(d
        .conflicts
        .iter()
        .any(|x| matches!(x, Conflict::WorkKindMismatch { .. })));
}

#[test]
fn a_rescan_fills_missing_edition_metadata_once() {
    let mut c = MemoryCatalog::default();
    record(&mut c, &dune("audible", "B001"), "a1");
    let mut fuller = dune("audible", "B001");
    fuller.duration_minutes = Some(1260);
    fuller.title = "Dune (renamed)".into();
    let p = record(&mut c, &fuller, "a1");
    let update = p.edition_update.clone().unwrap();
    assert_eq!(update.duration_minutes, Some(1260));
    assert_eq!(
        update.title, "Dune",
        "titles are never overwritten by a rescan"
    );
    assert!(matches!(p.events[0], Event::EditionUpdated { .. }));
    assert!(record(&mut c, &fuller, "a1").is_noop());
}

#[test]
fn new_entities_carry_their_credits_and_series() {
    let mut c = MemoryCatalog::default();
    let mut d = dune("audible", "B001");
    d.series = vec![("Dune Chronicles".into(), Some("1".into()))];
    d.credits.push(("Ace".into(), CreditRole::Publisher, None));
    let p = record(&mut c, &d, "a1");
    assert_eq!(p.credits.len(), 3);
    assert_eq!(p.series.len(), 1);
    let roles: Vec<_> = p.credits.iter().map(|w| (w.level, w.credit.role)).collect();
    assert!(roles.contains(&(bookclerk_resolve::Level::Edition, CreditRole::Publisher)));
}

/// A small deterministic generator for the corpus-free smoke run.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u8 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 56) as u8
    }
}

#[test]
fn random_inputs_keep_every_invariant() {
    let mut rng = Lcg(0x5eed);
    for _ in 0..3000 {
        let len = usize::from(rng.next()) * 2;
        let data: Vec<u8> = (0..len).map(|_| rng.next()).collect();
        if let Err(err) = bookclerk_resolve::fuzzing::run_case(&data) {
            panic!("{err}\ninput: {data:?}");
        }
    }
}
