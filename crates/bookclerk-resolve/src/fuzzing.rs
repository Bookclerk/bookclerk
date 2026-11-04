//! Fuzz input decoding and the oracle the `library_resolve` target checks.
//!
//! [`run_case`] turns arbitrary bytes into a sequence of plugin-reported
//! listings drawn from small pools (so identifiers, titles and authors
//! collide often), records each one into a [`MemoryCatalog`], and checks
//! the resolver's invariants after every step:
//!
//! - one strong identifier never maps to two editions, and no edition holds
//!   two values of one strong scheme in one marketplace;
//! - a weak identifier alone never joins an edition of another media kind,
//!   work kind or an incompatible expression;
//! - strong identifiers that name different editions never merge them;
//!   the listing reaches one and the conflict is recorded;
//! - a work found by identifier or matcher has the listing's kind, and an
//!   episode matched by text shares its parent show;
//! - resolving the same input twice changes nothing.
//!
//! The fuzz target and the replay test in `tests/fuzz_corpus.rs` both call
//! [`run_case`], so a corpus file replays exactly what the fuzzer ran.

use crate::kinds::{
    Abridgement, CreditRole, Entitlement, ExpressionKind, IdentifierScheme, MediaKind, Strength,
    WorkKind,
};
use crate::listing::{Holder, HoldingDraft, Listing, ListingDraft};
use crate::memory::MemoryCatalog;
use crate::model::{Conflict, LinkReason, WorkReason};

/// Most listings decoded from one input.
const MAX_LISTINGS: usize = 64;

/// Reads bytes off the front of a fuzz input, yielding zero once it is spent.
struct Cursor<'a> {
    /// The bytes not yet read.
    rest: &'a [u8],
}

impl Cursor<'_> {
    /// The next byte, or 0 when the input is spent.
    fn byte(&mut self) -> u8 {
        match self.rest.split_first() {
            Some((first, rest)) => {
                self.rest = rest;
                *first
            }
            None => 0,
        }
    }

    /// The next `len` bytes as lossy UTF-8.
    fn text(&mut self, len: usize) -> String {
        let take = len.min(self.rest.len());
        let (head, rest) = self.rest.split_at(take);
        self.rest = rest;
        String::from_utf8_lossy(head).into_owned()
    }

    /// One of `pool`, chosen by the next byte.
    fn pick<T: Clone>(&mut self, pool: &[T]) -> T {
        let i = usize::from(self.byte()) % pool.len();
        pool[i].clone()
    }
}

/// Titles listings share, so the work matcher sees collisions.
const TITLES: &[&str] = &[
    "Dune",
    "dune",
    "Dune (Unabridged)",
    "Trailer",
    "Les Misérables",
    "The Hobbit: There and Back Again",
];
/// Author names, including spellings that clean to the same key.
const AUTHORS: &[&str] = &[
    "Frank Herbert",
    "frank  herbert",
    "J.R.R. Tolkien",
    "Victor Hugo",
];
/// Narrator names.
const NARRATORS: &[&str] = &["Scott Brick", "Simon Vance", "Andy Serkis"];
/// ISBNs; the first two normalise to the same ISBN-13.
const ISBNS: &[&str] = &[
    "9780140449136",
    "0-14-044913-2",
    "9780261103344",
    "978 0 00 000000 2",
];
/// ASINs; the first two normalise to the same value.
const ASINS: &[&str] = &["B001", "b001", "B002", "B003"];
/// OpenLibrary edition ids.
const OL_EDITIONS: &[&str] = &["OL1M", "OL2M"];
/// OpenLibrary work ids.
const OL_WORKS: &[&str] = &["OL1W", "OL2W"];
/// Product ids, shared across plugins and marketplaces.
const PRODUCTS: &[&str] = &["P1", "P2", "P3", "P4", "P5", "P6", "SHOW1", "SHOW2"];
/// Languages, including a spelling that differs only by case.
const LANGUAGES: &[Option<&str>] = &[None, Some("en"), Some("de"), Some("EN")];

/// Decodes one listing and its holder from the cursor.
fn decode(cursor: &mut Cursor<'_>) -> (ListingDraft, HoldingDraft) {
    let flags = cursor.byte();
    let raw_text = flags & 0b100 != 0;
    let work_kind = cursor.pick(WorkKind::ALL);
    let expression_kind = cursor.pick(ExpressionKind::ALL);
    let media_kind = cursor.pick(MediaKind::ALL);
    let abridgement = cursor.pick(Abridgement::ALL);
    let language = cursor.pick(LANGUAGES).map(str::to_string);
    let title = if raw_text {
        let len = usize::from(cursor.byte() % 32);
        cursor.text(len)
    } else {
        cursor.pick(TITLES).to_string()
    };
    let authors = cursor.byte();
    let narrators = cursor.byte();
    let mut credits = Vec::new();
    for (bit, name) in AUTHORS.iter().enumerate() {
        if authors & (1 << bit) != 0 {
            credits.push(((*name).to_string(), CreditRole::Author, None));
        }
    }
    if raw_text && authors & 0x80 != 0 {
        let len = usize::from(cursor.byte() % 16);
        credits.push((cursor.text(len), CreditRole::Author, None));
    }
    for (bit, name) in NARRATORS.iter().enumerate() {
        if narrators & (1 << bit) != 0 {
            credits.push(((*name).to_string(), CreditRole::Narrator, None));
        }
    }
    let mut identifiers = Vec::new();
    for _ in 0..cursor.byte() % 5 {
        let scheme = cursor.pick(IdentifierScheme::ALL);
        let value = match scheme {
            IdentifierScheme::Isbn => cursor.pick(ISBNS),
            IdentifierScheme::Asin => cursor.pick(ASINS),
            IdentifierScheme::OpenLibraryEdition => cursor.pick(OL_EDITIONS),
            IdentifierScheme::OpenLibraryWork => cursor.pick(OL_WORKS),
        };
        let marketplace = cursor.pick(&[None, Some("us"), Some("uk")]);
        identifiers.push((scheme, value.to_string(), marketplace.map(str::to_string)));
    }
    let draft = ListingDraft {
        plugin_key: if flags & 1 == 0 { "audible" } else { "libro" }.to_string(),
        marketplace: Some(if flags & 0b10 == 0 { "us" } else { "uk" }.to_string()),
        product_id: cursor.pick(PRODUCTS).to_string(),
        work_kind: Some(work_kind),
        expression_kind: Some(expression_kind),
        media_kind: Some(media_kind),
        language,
        abridgement: Some(abridgement),
        title,
        subtitle: None,
        published_at: (flags & 0b1000 != 0).then(|| "2020-01-01T00:00:00Z".to_string()),
        duration_minutes: (flags & 0b1_0000 != 0).then_some(600),
        categories: None,
        identifiers,
        credits,
        series: Vec::new(),
        parent_product_id: Some(cursor.pick(&["SHOW1", "SHOW2", ""]).to_string()),
    };
    let holding = HoldingDraft {
        holder: cursor.pick(&[
            Holder::Account("a1".into()),
            Holder::Account("a2".into()),
            Holder::Node("n1".into()),
        ]),
        entitlement: Entitlement::Purchase,
        purchased_at: None,
    };
    (draft, holding)
}

/// Checks the catalog's invariants after the latest link.
fn check(catalog: &MemoryCatalog) -> Result<(), String> {
    // Strong identifiers: one edition each, one value per scheme and
    // marketplace on an edition.
    let mut strong = std::collections::BTreeMap::new();
    let mut per_edition = std::collections::BTreeMap::new();
    for row in &catalog.edition_identifiers {
        if row.key.strength() != Strength::Strong {
            continue;
        }
        if let Some(other) = strong.insert(row.key.clone(), row.edition_id.clone()) {
            return Err(format!(
                "{:?} is on {other} and {}",
                row.key, row.edition_id
            ));
        }
        let slot = (
            row.edition_id.clone(),
            row.key.scheme,
            row.key.marketplace.clone(),
        );
        if let Some(other) = per_edition.insert(slot, row.key.value.clone()) {
            return Err(format!(
                "edition {} holds {other} and {} for {:?}",
                row.edition_id, row.key.value, row.key.scheme
            ));
        }
    }

    let Some(last) = catalog.links.last() else {
        return Ok(());
    };
    let listing = &last.listing;
    let edition = catalog
        .editions
        .get(&last.edition_id)
        .ok_or_else(|| format!("edition {} missing", last.edition_id))?;
    let expression = catalog
        .expressions
        .get(&edition.expression_id)
        .ok_or_else(|| format!("expression {} missing", edition.expression_id))?;
    let work = catalog
        .works
        .get(&expression.work_id)
        .ok_or_else(|| format!("work {} missing", expression.work_id))?;

    match last.link {
        LinkReason::WeakIdentifier => {
            if edition.media_kind != listing.media_kind || work.kind != listing.work_kind {
                return Err(format!(
                    "weak identifier joined {} across kinds",
                    edition.id
                ));
            }
            let compatible = expression.kind == listing.expression_kind
                && expression.language.is_some()
                && expression.language == listing.language
                && expression.abridgement != Abridgement::Unknown
                && expression.abridgement == listing.abridgement
                && expression.credit_key == listing.expression_credit_key();
            if !compatible {
                return Err(format!(
                    "weak identifier joined incompatible {}",
                    expression.id
                ));
            }
        }
        LinkReason::New => {
            if edition.media_kind != listing.media_kind {
                return Err(format!(
                    "new edition {} has the wrong media kind",
                    edition.id
                ));
            }
            match last.work_reason {
                Some(WorkReason::Identifier | WorkReason::Matcher)
                    if work.kind != listing.work_kind =>
                {
                    return Err(format!("joined work {} of another kind", work.id));
                }
                Some(WorkReason::Matcher)
                    if listing.work_kind == WorkKind::PodcastEpisode
                        && (last.parent_work_id.is_none()
                            || work.parent_work_id != last.parent_work_id) =>
                {
                    return Err(format!("episode matched work {} of another show", work.id));
                }
                Some(WorkReason::Identifier | WorkReason::Matcher | WorkReason::New) | None => {}
            }
        }
        LinkReason::Pinned | LinkReason::Listing | LinkReason::StrongIdentifier => {}
    }

    // Strong identifiers naming another edition: never merged, always recorded.
    for key in listing
        .edition_identifiers()
        .filter(|key| key.strength() == Strength::Strong)
    {
        let Some(owner) = strong.get(key) else {
            continue;
        };
        if *owner == last.edition_id {
            continue;
        }
        let recorded = catalog.conflicts.iter().any(|c| match c {
            Conflict::StrongIdentifiers { edition_ids } => {
                edition_ids.contains(owner) && edition_ids.contains(&last.edition_id)
            }
            Conflict::WorkIdentifiers { .. }
            | Conflict::AmbiguousWork { .. }
            | Conflict::WorkKindMismatch { .. } => false,
        });
        if !recorded {
            return Err(format!(
                "{key:?} names {owner}, not {}, unrecorded",
                last.edition_id
            ));
        }
    }
    Ok(())
}

/// Resolves the listings `data` decodes to and checks every invariant.
///
/// # Arguments
///
/// * `data` - Arbitrary bytes: a fuzz input or a corpus file.
///
/// # Returns
///
/// The number of listings recorded.
///
/// # Errors
///
/// Returns a description of the first broken invariant.
pub fn run_case(data: &[u8]) -> Result<usize, String> {
    let mut cursor = Cursor { rest: data };
    let mut catalog = MemoryCatalog::default();
    let mut recorded = 0usize;
    while !cursor.rest.is_empty() && recorded < MAX_LISTINGS {
        let (draft, holding) = decode(&mut cursor);
        let Ok(listing) = Listing::from_draft(&draft) else {
            continue;
        };
        catalog
            .record(&listing, &holding)
            .map_err(|err| err.to_string())?;
        check(&catalog)?;
        let again = catalog
            .record(&listing, &holding)
            .map_err(|err| err.to_string())?;
        if !again.is_noop() {
            return Err(format!(
                "resolving {:?} twice changed {again:?}",
                listing.key
            ));
        }
        recorded += 1;
    }
    // Every earlier listing still resolves to a plan that changes nothing.
    for link in catalog.links.clone() {
        let plan = catalog
            .record(&link.listing, &link.holding)
            .map_err(|err| err.to_string())?;
        if !plan.is_noop() {
            return Err(format!(
                "rescanning {:?} changed {plan:?}",
                link.listing.key
            ));
        }
    }
    Ok(recorded)
}
