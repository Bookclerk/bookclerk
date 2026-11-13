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
        work_kind: Some(WorkKind::Book),
        expression_kind: Some(ExpressionKind::Narration),
        media_kind: Some(MediaKind::Audio),
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
        work_kind: Some(WorkKind::Podcast),
        expression_kind: Some(ExpressionKind::OriginalAudio),
        media_kind: Some(MediaKind::Audio),
        credits: vec![("Host Person".into(), CreditRole::Author, None)],
        ..ListingDraft::default()
    };
    let episode = |product: &str, parent: &str| ListingDraft {
        product_id: product.into(),
        title: "Trailer".into(),
        work_kind: Some(WorkKind::Podcast),
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

    let mut podcast_show = with_id(dune("ga", "G1"), IdentifierScheme::OpenLibraryWork, "OL1W");
    podcast_show.work_kind = Some(WorkKind::Podcast);
    let d = record(&mut c, &podcast_show, "g1");
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

#[test]
fn a_pin_to_a_removed_edition_is_ignored() {
    let mut c = MemoryCatalog::default();
    let a = record(&mut c, &dune("audible", "B001"), "a1");
    c.overrides.push(Override::PinListing {
        listing: a.listing.key.clone(),
        edition_id: "edition-that-was-removed".into(),
    });
    let again = record(&mut c, &dune("audible", "B001"), "a1");
    assert_eq!(again.link, LinkReason::Listing);
    assert_eq!(again.listing.edition_id, a.listing.edition_id);
    assert!(again.is_noop());
}

#[test]
fn a_holding_that_gains_a_purchase_date_is_not_a_noop() {
    let mut c = MemoryCatalog::default();
    let listing = Listing::from_draft(&dune("audible", "B001")).unwrap();
    c.record(&listing, &holding("a1")).unwrap();
    let dated = HoldingDraft {
        purchased_at: Some("2024-01-01T00:00:00+00:00".into()),
        ..holding("a1")
    };
    let plan = c.record(&listing, &dated).unwrap();
    assert!(plan.holding_changed);
    assert!(!plan.is_noop());
    assert!(c.record(&listing, &dated).unwrap().is_noop());
    let loan = HoldingDraft {
        entitlement: Entitlement::Loan,
        ..dated
    };
    assert!(c.record(&listing, &loan).unwrap().holding_changed);
}

#[test]
fn an_episode_seen_before_its_show_gains_it_later() {
    let mut c = MemoryCatalog::default();
    let show = ListingDraft {
        plugin_key: "audible".into(),
        product_id: "S1".into(),
        title: "Show".into(),
        work_kind: Some(WorkKind::Podcast),
        expression_kind: Some(ExpressionKind::OriginalAudio),
        media_kind: Some(MediaKind::Audio),
        credits: vec![("Host Person".into(), CreditRole::Author, None)],
        ..ListingDraft::default()
    };
    let episode = ListingDraft {
        product_id: "E1".into(),
        title: "Trailer".into(),
        work_kind: Some(WorkKind::Podcast),
        parent_product_id: Some("S1".into()),
        ..show.clone()
    };
    let first = record(&mut c, &episode, "a1");
    assert!(first.new_work.unwrap().parent_work_id.is_none());
    let show_plan = record(&mut c, &show, "a1");
    let linked = record(&mut c, &episode, "a1");
    let link = linked.work_parent.clone().unwrap();
    assert_eq!(link.parent_work_id, show_plan.new_work.unwrap().id);
    assert!(!linked.is_noop());
    assert_eq!(
        c.work_of_edition(&linked.listing.edition_id)
            .unwrap()
            .parent_work_id
            .as_deref(),
        Some(link.parent_work_id.as_str())
    );
    assert!(record(&mut c, &episode, "a1").is_noop());
}

#[test]
fn swapped_roles_make_a_different_expression() {
    let mut c = MemoryCatalog::default();
    let roles = |product: &str, narrator: &str, translator: &str| ListingDraft {
        credits: vec![
            ("Frank Herbert".into(), CreditRole::Author, None),
            (narrator.into(), CreditRole::Narrator, None),
            (translator.into(), CreditRole::Translator, None),
        ],
        ..dune("audible", product)
    };
    let a = record(&mut c, &roles("B001", "Alice Reader", "Bob Writer"), "a1");
    let b = record(&mut c, &roles("B002", "Bob Writer", "Alice Reader"), "a1");
    assert_eq!(b.work_reason, Some(WorkReason::Matcher));
    assert!(b.new_expression.is_some());
    assert_ne!(
        a.new_expression.unwrap().credit_key,
        b.new_expression.unwrap().credit_key
    );
}

#[test]
fn a_work_stored_without_an_author_gains_its_match_key_later() {
    let mut c = MemoryCatalog::default();
    let anonymous = ListingDraft {
        credits: Vec::new(),
        ..dune("audible", "B001")
    };
    let first = record(&mut c, &anonymous, "a1");
    let work = first.new_work.unwrap();
    assert!(work.match_key.is_empty());

    let named = record(&mut c, &dune("audible", "B001"), "a1");
    let fill = named.work_match_key.clone().unwrap();
    assert_eq!(fill.work_id, work.id);
    assert_eq!(fill.match_key, "dune\u{1f}frank herbert");
    assert!(!named.is_noop());
    assert!(record(&mut c, &dune("audible", "B001"), "a1").is_noop());

    // Another store's listing of the same book now finds the work.
    let other = record(&mut c, &dune("libro", "L1"), "a1");
    assert_eq!(other.work_reason, Some(WorkReason::Matcher));
    assert_eq!(work_of(&c, &other.listing.edition_id), work.id);
}

#[test]
fn a_match_key_another_work_holds_is_not_copied() {
    let mut c = MemoryCatalog::default();
    let anonymous = ListingDraft {
        credits: Vec::new(),
        ..dune("audible", "B001")
    };
    let first = record(&mut c, &anonymous, "a1");
    let named = record(&mut c, &dune("libro", "L1"), "a1");
    assert_ne!(
        work_of(&c, &named.listing.edition_id),
        work_of(&c, &first.listing.edition_id)
    );
    let again = record(&mut c, &dune("audible", "B001"), "a1");
    assert!(again.work_match_key.is_none());
}

#[test]
fn a_later_work_identifier_of_the_listings_kind_still_joins() {
    let mut c = MemoryCatalog::default();
    let mut podcast_show = with_id(dune("ga", "G1"), IdentifierScheme::OpenLibraryWork, "OL2W");
    podcast_show.work_kind = Some(WorkKind::Podcast);
    let d = record(&mut c, &podcast_show, "g1");
    let book = record(
        &mut c,
        &with_id(
            dune("audible", "B001"),
            IdentifierScheme::OpenLibraryWork,
            "OL1W",
        ),
        "a1",
    );
    let book_work = work_of(&c, &book.listing.edition_id);

    let mut both = with_id(
        dune("libro", "L1"),
        IdentifierScheme::OpenLibraryWork,
        "OL2W",
    );
    both = with_id(both, IdentifierScheme::OpenLibraryWork, "OL1W");
    both.title = "Dune: Deluxe".into();
    let joined = record(&mut c, &both, "l1");
    assert_eq!(joined.work_reason, Some(WorkReason::Identifier));
    assert_eq!(work_of(&c, &joined.listing.edition_id), book_work);
    assert!(joined.conflicts.contains(&Conflict::WorkIdentifiers {
        work_ids: vec![book_work, work_of(&c, &d.listing.edition_id)],
    }));
}

/// A listing of `kind` sold as `product`, titled `title`; with `parent`
/// a podcast is an episode of that show.
fn podcast(product: &str, title: &str, kind: WorkKind, parent: Option<&str>) -> ListingDraft {
    ListingDraft {
        plugin_key: "audible".into(),
        product_id: product.into(),
        title: title.into(),
        work_kind: Some(kind),
        expression_kind: Some(ExpressionKind::OriginalAudio),
        media_kind: Some(MediaKind::Audio),
        credits: vec![("Host Person".into(), CreditRole::Author, None)],
        parent_product_id: parent.map(Into::into),
        ..ListingDraft::default()
    }
}

#[test]
fn an_episode_takes_only_a_show_as_its_parent() {
    let mut c = MemoryCatalog::default();
    let mut not_a_show = podcast("S1", "Show", WorkKind::Book, None);
    not_a_show.expression_kind = Some(ExpressionKind::Narration);
    not_a_show.media_kind = Some(MediaKind::Audio);
    record(&mut c, &not_a_show, "a1");
    let episode = record(
        &mut c,
        &podcast("E1", "Trailer", WorkKind::Podcast, Some("S1")),
        "a1",
    );
    assert!(episode.new_work.unwrap().parent_work_id.is_none());
}

#[test]
fn a_shared_isbn_does_not_join_episodes_of_two_shows() {
    let mut c = MemoryCatalog::default();
    record(&mut c, &podcast("S1", "One", WorkKind::Podcast, None), "a1");
    record(&mut c, &podcast("S2", "Two", WorkKind::Podcast, None), "a1");
    let first = record(
        &mut c,
        &with_id(
            podcast("E1", "Trailer", WorkKind::Podcast, Some("S1")),
            IdentifierScheme::Isbn,
            "9780593288368",
        ),
        "a1",
    );
    let second = record(
        &mut c,
        &with_id(
            podcast("E2", "Trailer", WorkKind::Podcast, Some("S2")),
            IdentifierScheme::Isbn,
            "9780593288368",
        ),
        "a1",
    );
    assert_ne!(second.link, LinkReason::WeakIdentifier);
    assert_ne!(second.listing.edition_id, first.listing.edition_id);
}

#[test]
fn two_values_of_one_strong_scheme_record_a_conflict_on_first_sight() {
    let mut c = MemoryCatalog::default();
    let mut twice = with_id(dune("audible", "B001"), IdentifierScheme::Asin, "B001");
    twice = with_id(twice, IdentifierScheme::Asin, "B002");
    let plan = record(&mut c, &twice, "a1");
    assert!(plan.conflicts.contains(&Conflict::StrongIdentifiers {
        edition_ids: vec![plan.listing.edition_id.clone()],
    }));
}

#[test]
fn a_rescan_fills_what_the_expression_did_not_know() {
    let mut c = MemoryCatalog::default();
    // First seen without a narrator, language or abridgement.
    let mut bare = with_id(dune("libro", "L1"), IdentifierScheme::Isbn, "9780140449136");
    bare.credits.truncate(1);
    bare.language = None;
    bare.abridgement = None;
    let a = record(&mut c, &bare, "l1");
    assert!(a.expression_update.is_none());

    // A rescan of the same listing learns them; the expression is filled.
    let full = with_id(dune("libro", "L1"), IdentifierScheme::Isbn, "9780140449136");
    let b = record(&mut c, &full, "l1");
    let filled = b.expression_update.clone().unwrap();
    assert_eq!(filled.language.as_deref(), Some("en"));
    assert_eq!(filled.abridgement, Abridgement::Unabridged);
    assert_eq!(filled.credit_key, "narrator:scott brick");
    assert!(b
        .events
        .iter()
        .any(|e| matches!(e, Event::EditionUpdated { .. })));
    assert!(record(&mut c, &full, "l1").is_noop());

    // Another store's listing with that ISBN now joins the edition.
    let other = record(
        &mut c,
        &with_id(dune("chirp", "C1"), IdentifierScheme::Isbn, "9780140449136"),
        "c1",
    );
    assert_eq!(other.link, LinkReason::WeakIdentifier);
    assert_eq!(other.listing.edition_id, a.listing.edition_id);

    // Known values are never overwritten by a disagreeing rescan.
    let mut abridged = full.clone();
    abridged.abridgement = Some(Abridgement::Abridged);
    assert!(record(&mut c, &abridged, "l1").expression_update.is_none());
}

#[test]
fn a_catalog_listing_holds_nothing_and_a_later_scan_joins_its_edition() {
    let mut c = MemoryCatalog::default();
    let listing = Listing::from_draft(&dune("audible", "B001")).unwrap();
    let catalog = c.record_catalog(&listing).unwrap();
    assert!(catalog.holding.is_none());
    assert!(!catalog.holding_is_new);
    assert!(
        catalog.events.is_empty(),
        "a catalog plan announces nothing"
    );
    assert_eq!(c.holdings.len(), 0);
    assert_eq!(c.listings.len(), 1);
    // Seeing it in the catalog again changes nothing.
    assert!(c.record_catalog(&listing).unwrap().is_noop());
    // A scan of the same product reuses the listing and edition and adds
    // only the holding.
    let scanned = record(&mut c, &dune("audible", "B001"), "a1");
    assert_eq!(scanned.link, LinkReason::Listing);
    assert_eq!(scanned.listing.edition_id, catalog.listing.edition_id);
    assert!(scanned.holding_is_new);
    assert!(scanned
        .events
        .iter()
        .any(|e| matches!(e, Event::HoldingAdded { .. })));
    assert_eq!(c.holdings.len(), 1);
}

#[test]
fn a_catalog_listing_from_another_store_joins_the_held_work() {
    let mut c = MemoryCatalog::default();
    let held = record(&mut c, &dune("audible", "B001"), "a1");
    let other = Listing::from_draft(&dune("libro", "9780441172719")).unwrap();
    let plan = c.record_catalog(&other).unwrap();
    assert_eq!(
        work_of(&c, &plan.listing.edition_id),
        work_of(&c, &held.listing.edition_id)
    );
    assert_eq!(c.holdings.len(), 1);
}

#[test]
fn a_show_never_matches_an_episode_waiting_for_its_show() {
    let mut c = MemoryCatalog::default();
    // The trailer carries the show's own title and host, and arrives first.
    let first = record(
        &mut c,
        &podcast("E1", "Show", WorkKind::Podcast, Some("S1")),
        "a1",
    );
    assert!(first.new_work.as_ref().unwrap().match_key.is_empty());
    let show = record(
        &mut c,
        &podcast("S1", "Show", WorkKind::Podcast, None),
        "a1",
    );
    assert_eq!(show.work_reason, Some(WorkReason::New));
    // Linked to its show on the next sight, the episode gains its key.
    let linked = record(
        &mut c,
        &podcast("E1", "Show", WorkKind::Podcast, Some("S1")),
        "a1",
    );
    assert!(linked.work_parent.is_some());
    assert!(linked.work_match_key.is_some());
}

#[test]
fn an_episode_never_moves_a_show_with_episodes_under_another_show() {
    let mut c = MemoryCatalog::default();
    // Show S2 already has an episode; its edition carries the ASIN a
    // mislabelled episode of show S1 shares.
    let other_show = record(
        &mut c,
        &with_id(
            podcast("S2", "Other Show", WorkKind::Podcast, None),
            IdentifierScheme::Asin,
            "B0SHARED00",
        ),
        "a1",
    );
    record(
        &mut c,
        &podcast("E2", "Other Episode", WorkKind::Podcast, Some("S2")),
        "a1",
    );
    record(
        &mut c,
        &podcast("S1", "Show", WorkKind::Podcast, None),
        "a1",
    );
    let episode = record(
        &mut c,
        &with_id(
            podcast("E1", "Episode", WorkKind::Podcast, Some("S1")),
            IdentifierScheme::Asin,
            "B0SHARED00",
        ),
        "a1",
    );
    let other_show_work = other_show.new_work.unwrap().id;
    assert_eq!(
        c.work_of_edition(&episode.listing.edition_id).unwrap().id,
        other_show_work
    );
    assert!(episode.work_parent.is_none());
    assert!(c.works[&other_show_work].parent_work_id.is_none());
}
