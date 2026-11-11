//! Precomputed Discover shelves: the two queued jobs and the stored read.
//!
//! Discover used to rebuild every shelf inside the page request. Now:
//!
//! 1. [`fetch_subject_candidates`] (the `discover_candidates` job) picks one
//!    subject's seeds (their books, listening, ratings and wishlist, with the
//!    household's books blended in while they have fewer than
//!    [`OWN_TASTE_THRESHOLD`] of their own), asks **every** deployed
//!    storefront about them concurrently, and stores each store's answer as
//!    soon as it arrives. A thin taste also browses each store's
//!    [`STARTER_GENRES`]. Storefront answers are cached for a day in
//!    `discover_store_calls`, shared by every subject.
//! 2. [`rank_subject_feed`] (the `discover_feed` job) ranks the stored
//!    candidates against the same taste, with no network, and stores the
//!    shelves.
//! 3. [`stored_subject_feed`] reads them back for the page, dropping the
//!    subject's hidden shelves and joining each card's catalog metadata
//!    (description, cover, …) from the stored candidates, which the stored
//!    feed leaves out.
//!
//! See `docs/internals/discovery.md`, *Precomputed shelves*.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use bookclerk_library::{
    BookRecord, DiscoverSubject, LibraryStore, ListeningProgressRecord, RequestStatus, SeedScope,
};
use bookclerk_source::{
    CatalogHit, CatalogSearchField, CatalogSearchOpts, CatalogSearchSort, ContentSource,
    PurchaseHintOpts, SourceError, SourceRegistry,
};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use tokio::task::JoinSet;

use crate::candidates::{
    apply_seed_title, hit_to_candidate, insert_candidate, looks_like_series_set,
    select_taste_seeds, SeedTitle, StorefrontCandidate,
};
use crate::error::Result;
use crate::recommend::{
    engagement_by_book, feed_from_recommendations, owned_identity_sets, rank_candidates,
    shelf_taste, RecommendOptions, TasteWeights,
};
use crate::shelves::{shelf_is_disabled, DiscoverFeed};
use crate::stored_feed::{
    decode_stored_feed, encode_stored_feed, feed_needs_pool, join_feed, CandidatePool,
};

/// Longest one storefront call may take before its store moves on.
pub const STORE_CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// How long one store may keep starting new calls in a fetch.
///
/// A store that is slow on every call stops asking once this has passed
/// and saves what it has, so it delays the subject's ranking by at most
/// this plus one [`STORE_CALL_TIMEOUT`].
pub const STORE_FETCH_DEADLINE: Duration = Duration::from_secs(20);

/// How long a cached storefront answer is reused (the daily refresh).
pub const STORE_CALL_MAX_AGE: chrono::Duration = chrono::Duration::hours(24);

/// Most open wishlist titles used as seeds, after the subject's books.
const WISHLIST_SEED_LIMIT: usize = 4;

/// Books and listens of their own a subject needs before the household's
/// books stop blending into their taste.
pub const OWN_TASTE_THRESHOLD: usize = 5;

/// How much a household book counts in a member's taste while it blends in,
/// relative to one of their own.
pub const HOUSEHOLD_BLEND_WEIGHT: f64 = 0.5;

/// Seeds below which a fetch also browses each store's [`STARTER_GENRES`],
/// so a new library still gets shelves.
pub const STARTER_GENRE_MIN_SEEDS: usize = 5;

/// Genres browsed for a thin taste, in the order the stores are asked (the
/// call budget may stop before the end).
pub const STARTER_GENRES: [&str; 6] = [
    "Science Fiction",
    "Fantasy",
    "Mystery & Thriller",
    "Romance",
    "History & Biography",
    "Self-help",
];

/// Hits asked of a store for one starter genre.
const STARTER_GENRE_HITS: usize = 24;

/// Most distinct titles one store is asked to price at the end of a fetch,
/// in one `purchase_hints` call. Audible and Chirp take this in a few
/// upstream requests; a store that loops stops at its own budget and the
/// rest stay unpriced until a card asks live.
pub const PRICE_PRECOMPUTE_LIMIT: usize = 120;

/// Most remembered prices one store keeps in its row of the store-call
/// cache, oldest dropped first. About 130 bytes each once escaped, so the
/// row stays well under the database plugins' 256 KiB cell limit.
const PRICE_MEMO_MAX_ENTRIES: usize = 1_000;

/// One candidate's price as last fetched from its store.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct RememberedPrice {
    /// List price in integer cents of `currency`.
    cents: Option<i64>,
    /// ISO 4217 currency code.
    currency: Option<String>,
    /// Display price from the store.
    label: Option<String>,
    /// When the store answered; older than [`STORE_CALL_MAX_AGE`] is asked again.
    at: DateTime<Utc>,
}

impl RememberedPrice {
    /// Writes this price onto `candidate`.
    fn apply(&self, candidate: &mut StorefrontCandidate) {
        candidate.price_cents = self.cents;
        candidate.currency.clone_from(&self.currency);
        candidate.price_label.clone_from(&self.label);
    }
}

/// Most bytes a stored feed's JSON may take once escaped as a JSON string.
///
/// The feed is one `discover_feeds` cell. The first-party database plugins
/// refuse a cell or query result over 256 KiB (`maxResultBytes`) and count
/// a row as JSON, so the budget applies to the escaped feed. The row read
/// back carries a few more columns, so the feed keeps 64 KiB of headroom
/// under that. Cards are stored without the metadata the candidate pool
/// holds, so a feed normally takes a fraction of this; the budget is a
/// backstop.
pub const STORED_FEED_MAX_BYTES: usize = 192 * 1024;

/// One subject's taste: whose seeds Discover uses and what they read.
#[derive(Debug, Clone)]
pub struct SubjectTaste {
    /// Whether the taste is the subject's own or the household's.
    pub scope: SeedScope,
    /// Books whose finishes, ratings, series and authors shape the taste:
    /// the subject's own, then the household's blended in while the
    /// subject's own are fewer than [`OWN_TASTE_THRESHOLD`].
    pub books: Vec<BookRecord>,
    /// How much each book in `books` counts; a blended household book is
    /// listed at [`HOUSEHOLD_BLEND_WEIGHT`], the subject's own count in full.
    pub weights: TasteWeights,
    /// The subject's listening rows (all rows for the household). While
    /// household books blend in, the household's rows for those books are
    /// added too, and count at their book's weight.
    pub listening: Vec<ListeningProgressRecord>,
    /// The subject's open wishlist titles, newest first, used as extra seeds.
    pub wishlist: Vec<SeedTitle>,
    /// Display titles of `wishlist`, newest first.
    pub wish_titles: Vec<String>,
    /// Lowercase storefront ids the subject has an account with.
    pub linked_sources: HashSet<String>,
}

impl SubjectTaste {
    /// The whole household's taste, for a subject with no signal of their own.
    fn household(
        books: Vec<BookRecord>,
        listening: Vec<ListeningProgressRecord>,
        linked_sources: HashSet<String>,
    ) -> Self {
        Self {
            scope: SeedScope::Household,
            books,
            weights: TasteWeights::new(),
            listening,
            wishlist: Vec::new(),
            wish_titles: Vec::new(),
            linked_sources,
        }
    }

    /// The seeds the storefronts are asked about: the subject's books by
    /// engagement, then the household's blended-in books, each capped.
    fn book_seeds(&self, limit: usize, region: &str) -> Vec<SeedTitle> {
        let engagement = engagement_by_book(&self.listening);
        let ranked = select_taste_seeds(&self.books, &engagement);
        // Own books first: a blended household book never displaces one.
        let (own, blended): (Vec<BookRecord>, Vec<BookRecord>) = ranked
            .into_iter()
            .partition(|b| !self.weights.contains_key(&b.uuid));
        own.iter()
            .chain(blended.iter())
            .take(limit)
            .map(|b| SeedTitle::from_book(b, region))
            .collect()
    }
}

/// Loads one subject's taste.
///
/// A user or portal identity uses the books held by store accounts linked to
/// them plus the household books they have listened to, the listening rows
/// recorded for them (by identity id, or by provider and external user id on
/// rows recorded before the identity was known), and their open wishlist.
/// While they have fewer than [`OWN_TASTE_THRESHOLD`] books and listens of
/// their own, the rest of the household's books blend in at
/// [`HOUSEHOLD_BLEND_WEIGHT`], so a first wish or book adds to the shelves
/// they already had instead of replacing them. With no signal of their own
/// at all (a new member, or a visitor who has connected no store yet), and
/// always for the operator, the household library is used as it stands.
///
/// # Arguments
///
/// * `library` - Library store.
/// * `subject` - Whose taste to load.
/// * `opts` - `include_listening` and `listening_providers` apply as on the page.
///
/// # Errors
///
/// Returns the library read error.
pub async fn load_subject_taste(
    library: &LibraryStore,
    subject: &DiscoverSubject,
    opts: &RecommendOptions,
) -> Result<SubjectTaste> {
    let household = library.list_books(None).await?;
    let all_listening = if opts.include_listening {
        let mut rows = library.list_listening_progress(None).await?;
        if !opts.listening_providers.is_empty() {
            rows.retain(|r| {
                opts.listening_providers
                    .iter()
                    .any(|p| p.eq_ignore_ascii_case(&r.provider))
            });
        }
        rows
    } else {
        Vec::new()
    };
    let identities = match subject {
        DiscoverSubject::Operator => Vec::new(),
        DiscoverSubject::User(user_id) => library.list_portal_identities_for_user(*user_id).await?,
        DiscoverSubject::Portal(id) => library
            .get_portal_identity_by_id(*id)
            .await?
            .into_iter()
            .collect(),
    };
    if identities.is_empty() {
        // The operator compares prices as a member of every store the
        // household has an account with.
        let linked = library
            .list_accounts()
            .await?
            .into_iter()
            .map(|a| a.source.to_ascii_lowercase())
            .collect();
        return Ok(SubjectTaste::household(household, all_listening, linked));
    }

    let identity_ids: HashSet<i64> = identities.iter().map(|i| i.id).collect();
    // External user ids are provider-local, so a legacy row without an
    // identity id matches only on the (provider, external id) pair.
    let external_ids: HashSet<(&str, &str)> = identities
        .iter()
        .map(|i| (i.provider.as_str(), i.external_user_id.as_str()))
        .collect();
    let mut held: HashSet<(String, String)> = HashSet::new();
    let mut linked_sources: HashSet<String> = HashSet::new();
    for identity in &identities {
        for link in library.list_account_links(identity.id).await? {
            linked_sources.insert(link.source.to_ascii_lowercase());
            held.insert((link.plugin_key, link.account_id));
        }
    }
    let listening: Vec<ListeningProgressRecord> = all_listening
        .iter()
        .filter(|r| match r.identity_id {
            Some(id) => identity_ids.contains(&id),
            None => external_ids.contains(&(r.provider.as_str(), r.external_user_id.as_str())),
        })
        .cloned()
        .collect();
    // Their books: those on store accounts linked to them, plus any household
    // book they have listened to (a member may listen without owning).
    let listened: HashSet<&str> = listening
        .iter()
        .filter_map(|r| r.book_uuid.as_deref())
        .collect();
    let (own, rest): (Vec<BookRecord>, Vec<BookRecord>) = household.into_iter().partition(|b| {
        held.contains(&(b.plugin_key.clone(), b.account_id.clone()))
            || listened.contains(b.uuid.as_str())
    });
    let wishes: Vec<_> = library
        .list_title_requests(Some(RequestStatus::Open))
        .await?
        .into_iter()
        .filter(|r| r.identity_id.is_some_and(|id| identity_ids.contains(&id)))
        .take(WISHLIST_SEED_LIMIT)
        .collect();
    let wish_titles: Vec<String> = wishes.iter().map(|r| r.title.clone()).collect();
    let wishlist: Vec<SeedTitle> = wishes
        .into_iter()
        .map(|r| SeedTitle::from_wish(&r.title, r.authors, r.asin, r.isbn))
        .collect();

    if own.is_empty() && listening.is_empty() && wishlist.is_empty() {
        // Nothing of their own yet: the whole household's taste, listening
        // included, as for a subject with no identity.
        let mut household = own;
        household.extend(rest);
        return Ok(SubjectTaste::household(
            household,
            all_listening,
            linked_sources,
        ));
    }
    // `own` already holds every book they listened to, so it is their signal.
    let own_signal = own.len();
    let mut books = own;
    let mut weights = TasteWeights::new();
    let mut listening = listening;
    if own_signal < OWN_TASTE_THRESHOLD {
        for book in rest {
            weights.insert(book.uuid.clone(), HOUSEHOLD_BLEND_WEIGHT);
            books.push(book);
        }
        // The household's listening to the blended books blends in with
        // them, at their weight. The subject's own rows are for books in
        // `own`, which carry no weight, so they still count in full.
        listening.extend(all_listening.into_iter().filter(|r| {
            r.book_uuid
                .as_ref()
                .is_some_and(|u| weights.contains_key(u))
        }));
    }
    Ok(SubjectTaste {
        scope: SeedScope::Own,
        books,
        weights,
        listening,
        wishlist,
        wish_titles,
        linked_sources,
    })
}

/// What one candidate fetch did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchReport {
    /// Seeds the subject's taste offered (books, then wishlist titles).
    pub seeds: usize,
    /// Stores whose answers were stored.
    pub stores_saved: usize,
    /// Candidates stored across those stores.
    pub candidates: usize,
    /// Stores whose fetch failed outright, by plugin id.
    pub stores_failed: Vec<String>,
    /// Candidates that carry a price from the fetch, across stores.
    pub priced: usize,
}

/// Fetches one subject's storefront candidates and stores them store by store.
///
/// Every deployed storefront is asked, whether or not the subject has an
/// account there. Stores run concurrently; within a store each seed's call
/// has [`STORE_CALL_TIMEOUT`], each store stops starting calls after
/// [`STORE_FETCH_DEADLINE`], and the store's share of
/// `opts.storefront_max_remote_calls` bounds how many calls it makes. A
/// store's rows replace its previous rows only when it finishes, so a store
/// that fails keeps what it returned last time.
///
/// # Arguments
///
/// * `library` - Library store (taste, cache, candidate rows).
/// * `registry` - The deployed storefronts.
/// * `subject` - Whose candidates to fetch.
/// * `opts` - Seed limit, call budget, region and series-set filter.
///
/// # Errors
///
/// Returns a library error from loading the taste. Store failures are
/// reported in [`FetchReport::stores_failed`], not as errors.
pub async fn fetch_subject_candidates(
    library: &LibraryStore,
    registry: &SourceRegistry,
    subject: &DiscoverSubject,
    opts: &RecommendOptions,
) -> Result<FetchReport> {
    let taste = load_subject_taste(library, subject, opts).await?;
    let household = library.list_books(None).await?;
    let owned = owned_identity_sets(&household);
    let book_seeds = taste.book_seeds(opts.storefront_seed_limit, &opts.region);
    let wish_seeds: Vec<SeedTitle> = taste
        .wishlist
        .iter()
        .cloned()
        .map(|mut s| {
            s.seed.region.clone_from(&opts.region);
            s
        })
        .collect();
    let seeds = interleave_seeds(book_seeds, wish_seeds);
    // A thin taste browses the starter genres too, so a new library still
    // gets shelves; the stores' call budget decides how many.
    let starter: Vec<String> = if seeds.len() < STARTER_GENRE_MIN_SEEDS {
        STARTER_GENRES.iter().map(|g| (*g).to_string()).collect()
    } else {
        Vec::new()
    };
    let mut report = FetchReport {
        seeds: seeds.len(),
        ..FetchReport::default()
    };
    let stores = registry.all();
    if stores.is_empty() {
        library.mark_discover_fetch(subject, Utc::now()).await?;
        return Ok(report);
    }
    let total = opts.storefront_max_remote_calls;
    let base = total / stores.len();
    let extra = total % stores.len();

    let mut tasks: JoinSet<(String, Vec<StorefrontCandidate>, bool, usize)> = JoinSet::new();
    for (index, source) in stores.into_iter().enumerate() {
        // Split the total budget so the stores together never exceed it; a
        // store whose share is zero is not asked and keeps its stored rows.
        let per_store = base + usize::from(index < extra);
        if per_store == 0 {
            continue;
        }
        let seeds = seeds.clone();
        let starter = starter.clone();
        let library = library.clone();
        let region = opts.region.clone();
        let exclude_sets = opts.exclude_graphicaudio_series_sets;
        tasks.spawn(async move {
            let id = source.id().to_string();
            let (mut hits, ok) = fetch_one_store(
                &library,
                source.as_ref(),
                &seeds,
                &starter,
                &region,
                per_store,
                exclude_sets,
            )
            .await;
            let priced = if ok {
                price_candidates(&library, source.as_ref(), &mut hits, &region).await
            } else {
                0
            };
            (id, hits, ok, priced)
        });
    }
    while let Some(joined) = tasks.join_next().await {
        let Ok((store, candidates, ok, priced)) = joined else {
            tracing::warn!("a discover store fetch task stopped before it finished");
            continue;
        };
        if !ok {
            report.stores_failed.push(store);
            continue;
        }
        report.priced += priced;
        let mut map: HashMap<String, StorefrontCandidate> = HashMap::new();
        for c in candidates {
            insert_candidate(&mut map, c, &owned.0, &owned.1, &owned.2);
        }
        let mut rows = Vec::with_capacity(map.len());
        for (key, c) in map {
            match serde_json::to_string(&c) {
                Ok(json) => rows.push((key, json)),
                Err(err) => {
                    tracing::warn!(%store, error = %err, "skip a candidate that does not serialize")
                }
            }
        }
        library
            .replace_discover_candidates(subject, &store, taste.scope, &rows)
            .await?;
        report.stores_saved += 1;
        report.candidates += rows.len();
    }
    report.stores_failed.sort();
    library.mark_discover_fetch(subject, Utc::now()).await?;
    tracing::info!(
        subject = %subject.key(),
        scope = taste.scope.as_str(),
        seeds = report.seeds,
        stores = report.stores_saved,
        candidates = report.candidates,
        priced = report.priced,
        failed = ?report.stores_failed,
        "stored discover candidates"
    );
    Ok(report)
}

/// Prices one store's candidates, so cards show a price before any live
/// lookup.
///
/// Prices answered in the last day come from the store's row in the
/// store-call cache, shared by every subject, so a refresh after a wish asks
/// the store only about titles it has not priced. The rest, up to
/// [`PRICE_PRECOMPUTE_LIMIT`], go in one `purchase_hints` call. A store
/// paused for rate limiting is not asked; one that asks to slow down is
/// paused for live lookups too. A failure or timeout leaves the rest
/// unpriced.
///
/// # Returns
///
/// How many candidates carry a price from this pass.
async fn price_candidates(
    library: &LibraryStore,
    source: &dyn ContentSource,
    candidates: &mut [StorefrontCandidate],
    region: &str,
) -> usize {
    let id = source.id().to_string();
    let memo_key = call_key(&id, "prices", &[region]);
    let now = Utc::now();
    let mut memo: HashMap<String, RememberedPrice> = library
        .get_discover_store_call(&memo_key, STORE_CALL_MAX_AGE)
        .await
        .ok()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default();
    memo.retain(|_, price| price.at + STORE_CALL_MAX_AGE > now);
    let mut priced = 0;
    for candidate in candidates.iter_mut() {
        if candidate.price_cents.is_none() {
            if let Some(price) = memo.get(&candidate.product_id) {
                price.apply(candidate);
                priced += 1;
            }
        }
    }
    // One question per title: seeds that found the same product share it.
    let mut asked = HashSet::new();
    let unpriced: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| c.price_cents.is_none() && !c.product_id.trim().is_empty())
        .filter(|(_, c)| asked.insert(c.product_id.clone()))
        .map(|(i, _)| i)
        .take(PRICE_PRECOMPUTE_LIMIT)
        .collect();
    if unpriced.is_empty() || crate::purchase::store_breaker_open(&id, std::time::Instant::now()) {
        return priced;
    }
    let opts: Vec<PurchaseHintOpts> = unpriced
        .iter()
        .map(|&i| {
            let c = &candidates[i];
            PurchaseHintOpts {
                product_id: Some(c.product_id.clone()),
                title: Some(c.title.clone()),
                authors: c.authors.clone(),
                asin: c.asin.clone(),
                isbn: c.isbn.clone(),
                region: region.to_string(),
                with_price: true,
            }
        })
        .collect();
    let answers = match tokio::time::timeout(STORE_CALL_TIMEOUT, source.purchase_hints(&opts)).await
    {
        Ok(Ok(answers)) => answers,
        Ok(Err(SourceError::RateLimited(wait))) => {
            crate::purchase::note_store_rate_limit(&id, wait, std::time::Instant::now());
            return priced;
        }
        Ok(Err(err)) => {
            tracing::debug!(store = %id, error = %err, "candidate pricing failed");
            return priced;
        }
        Err(_) => {
            tracing::debug!(store = %id, "candidate pricing timed out");
            return priced;
        }
    };
    let mut learned = false;
    for (&i, answer) in unpriced.iter().zip(answers) {
        let Some(hint) = answer else { continue };
        // The list price: what someone without an account there pays.
        let cents = hint.list_price_cents.or(hint.price_cents);
        let label = hint.list_price_label.or(hint.price_label);
        if cents.is_none() && label.is_none() {
            continue;
        }
        let price = RememberedPrice {
            cents,
            currency: hint.currency,
            label,
            at: now,
        };
        let product_id = candidates[i].product_id.clone();
        for candidate in candidates.iter_mut().filter(|c| c.product_id == product_id) {
            price.apply(candidate);
            priced += 1;
        }
        memo.insert(product_id, price);
        learned = true;
    }
    if learned {
        if memo.len() > PRICE_MEMO_MAX_ENTRIES {
            let mut ages: Vec<DateTime<Utc>> = memo.values().map(|p| p.at).collect();
            ages.sort_unstable();
            let cutoff = ages[memo.len() - PRICE_MEMO_MAX_ENTRIES];
            memo.retain(|_, price| price.at >= cutoff);
        }
        if let Ok(json) = serde_json::to_string(&memo) {
            if let Err(err) = library.put_discover_store_call(&memo_key, &id, &json).await {
                tracing::debug!(store = %id, error = %err, "candidate price memo write failed");
            }
        }
    }
    priced
}

/// Alternates book and wishlist seeds, so a store's call budget reaches
/// both kinds even when it runs out before the end of the list.
///
/// # Returns
///
/// Every seed of both lists: a book seed, then a wish seed, and so on, with
/// the longer list's remainder at the end.
fn interleave_seeds(books: Vec<SeedTitle>, wishes: Vec<SeedTitle>) -> Vec<SeedTitle> {
    let mut out = Vec::with_capacity(books.len() + wishes.len());
    let mut books = books.into_iter();
    let mut wishes = wishes.into_iter();
    loop {
        match (books.next(), wishes.next()) {
            (None, None) => break,
            (book, wish) => out.extend(book.into_iter().chain(wish)),
        }
    }
    out
}

/// Asks one store for its deals, then about each seed, then for each
/// starter genre, in at most `budget` calls.
///
/// # Returns
///
/// The store's candidates, and `false` when no call returned a title (so
/// the store's stored rows are kept rather than wiped). An empty answer
/// counts as no answer: plugin adapters report some guest failures as an
/// empty success, and those must not erase the store's shelves.
async fn fetch_one_store(
    library: &LibraryStore,
    source: &dyn ContentSource,
    seeds: &[SeedTitle],
    starter_genres: &[String],
    region: &str,
    budget: usize,
    exclude_series_sets: bool,
) -> (Vec<StorefrontCandidate>, bool) {
    let id = source.id().to_string();
    let mut out = Vec::new();
    let mut calls = 0usize;
    let mut answered = 0usize;
    // Deals first: one call, and they fill the deals shelf whatever the seeds.
    if budget == 0 {
        return (out, false);
    }
    let deal_key = call_key(&id, "deals", &[]);
    let skip_set = |hit: &CatalogHit| {
        exclude_series_sets && id.eq_ignore_ascii_case("graphicaudio") && looks_like_series_set(hit)
    };
    if let Some(hits) = cached_or_call(library, &id, &deal_key, || source.list_deals(32)).await {
        answered += usize::from(!hits.is_empty());
        out.extend(
            hits.into_iter()
                .filter(|h| !skip_set(h))
                .map(|h| hit_to_candidate(&id, h)),
        );
    }
    calls += 1;
    let started = std::time::Instant::now();
    for seed in seeds {
        if calls >= budget || started.elapsed() >= STORE_FETCH_DEADLINE {
            break;
        }
        let s = &seed.seed;
        let key = call_key(
            &id,
            "expand",
            &[
                &s.source,
                &s.product_id,
                &s.title,
                s.authors.as_deref().unwrap_or(""),
                s.narrators.as_deref().unwrap_or(""),
                s.series.as_deref().unwrap_or(""),
                s.series_asin.as_deref().unwrap_or(""),
                s.asin.as_deref().unwrap_or(""),
                s.isbn.as_deref().unwrap_or(""),
                &s.region,
            ],
        );
        let hits = cached_or_call(library, &id, &key, || source.expand_candidates(s, 24)).await;
        calls += 1;
        let Some(hits) = hits else { continue };
        answered += usize::from(!hits.is_empty());
        for hit in hits {
            if skip_set(&hit) {
                continue;
            }
            out.push(apply_seed_title(hit_to_candidate(&id, hit), seed));
        }
    }
    for genre in starter_genres {
        if calls >= budget || started.elapsed() >= STORE_FETCH_DEADLINE {
            break;
        }
        let key = call_key(&id, "starter", &[genre, region]);
        let search = CatalogSearchOpts {
            query: genre.clone(),
            region: region.to_string(),
            limit: STARTER_GENRE_HITS,
            page: 1,
            sort: CatalogSearchSort::Popularity,
            field: Some(CatalogSearchField::Genre),
            language: None,
        };
        let hits = cached_or_call(library, &id, &key, || source.search_catalog(&search)).await;
        calls += 1;
        let Some(hits) = hits else { continue };
        answered += usize::from(!hits.is_empty());
        for hit in hits {
            if skip_set(&hit) {
                continue;
            }
            let mut c = hit_to_candidate(&id, hit);
            c.starter_genre = Some(genre.clone());
            if c.origin.is_empty() {
                c.origin = format!("{id} popular in {genre}");
            }
            out.push(c);
        }
    }
    (out, answered > 0)
}

/// Returns a cached answer, or makes the call (with [`STORE_CALL_TIMEOUT`]) and caches it.
///
/// # Returns
///
/// `None` when the call failed or timed out; the failure is logged, not
/// cached. An empty answer is returned but not cached either, since it may
/// be a guest failure the adapter softened, so the next subject asks again.
async fn cached_or_call<F, Fut>(
    library: &LibraryStore,
    store: &str,
    key: &str,
    call: F,
) -> Option<Vec<CatalogHit>>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = bookclerk_source::Result<Vec<CatalogHit>>>,
{
    match library
        .get_discover_store_call(key, STORE_CALL_MAX_AGE)
        .await
    {
        Ok(Some(json)) => match serde_json::from_str::<Vec<CatalogHit>>(&json) {
            Ok(hits) => return Some(hits),
            Err(err) => {
                tracing::debug!(%store, error = %err, "ignore an unreadable cached store answer")
            }
        },
        Ok(None) => {}
        Err(err) => tracing::debug!(%store, error = %err, "discover store cache read failed"),
    }
    match tokio::time::timeout(STORE_CALL_TIMEOUT, call()).await {
        Ok(Ok(hits)) => {
            if hits.is_empty() {
                return Some(hits);
            }
            if let Ok(json) = serde_json::to_string(&hits) {
                if let Err(err) = library.put_discover_store_call(key, store, &json).await {
                    tracing::debug!(%store, error = %err, "discover store cache write failed");
                }
            }
            Some(hits)
        }
        Ok(Err(err)) => {
            tracing::debug!(%store, error = %err, "discover store call failed");
            None
        }
        Err(_) => {
            tracing::debug!(%store, "discover store call timed out");
            None
        }
    }
}

/// SHA-256 hex over the store, call kind and query parts (NUL-separated).
fn call_key(store: &str, kind: &str, parts: &[&str]) -> String {
    let mut h = Sha256::new();
    h.update(store.as_bytes());
    h.update([0]);
    h.update(kind.as_bytes());
    for part in parts {
        h.update([0]);
        h.update(part.as_bytes());
    }
    hex::encode(h.finalize())
}

/// What one ranking did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankReport {
    /// Seeds the feed was ranked against.
    pub scope: SeedScope,
    /// Shelves stored.
    pub shelves: usize,
    /// Stored candidates read.
    pub candidates: usize,
    /// Cards on the stored shelves, after any trimming to fit the budget.
    pub cards: usize,
    /// Bytes of the stored feed as the database counts it (an escaped JSON
    /// string), at most the 192 KiB stored-feed budget.
    pub stored_bytes: usize,
}

/// Ranks one subject's stored candidates into shelves and stores the feed.
///
/// No storefront is called (`include_purchase_hints` is ignored: cards fetch
/// live prices when they scroll into view). Owned titles are filtered again,
/// so a title bought since the fetch drops out.
///
/// # Arguments
///
/// * `library` - Library store.
/// * `registry` - Unused for network; passed through to ranking.
/// * `subject` - Whose feed to rank.
/// * `opts` - Ranking options; `disabled_shelves` is ignored (applied on read).
///
/// # Errors
///
/// Returns the library read or write error.
pub async fn rank_subject_feed(
    library: &LibraryStore,
    registry: &SourceRegistry,
    subject: &DiscoverSubject,
    opts: &RecommendOptions,
) -> Result<RankReport> {
    let taste = load_subject_taste(library, subject, opts).await?;
    let household = library.list_books(None).await?;
    let (asins, isbns, products) = owned_identity_sets(&household);
    // With storefront candidates turned off, stored rows from before stay
    // out of the feed; rows fetched for another seed scope (before the
    // subject had taste of their own, or after they lost it) do too.
    // Read the fetch time before the rows: a fetch that finishes between
    // the two reads then leaves the feed older than its rows, so the next
    // read ranks again instead of taking stale shelves as current.
    let newest = library.newest_discover_candidate_fetch(subject).await?;
    let stored = if opts.fetch_storefront_candidates {
        library.list_discover_candidates(subject).await?
    } else {
        Vec::new()
    };
    let mut map: HashMap<String, StorefrontCandidate> = HashMap::new();
    for row in stored.iter().filter(|row| row.seed_scope == taste.scope) {
        match serde_json::from_str::<StorefrontCandidate>(&row.candidate_json) {
            Ok(c) => insert_candidate(&mut map, c, &asins, &isbns, &products),
            Err(err) => tracing::debug!(error = %err, "skip an unreadable stored candidate"),
        }
    }
    let mut rank_opts = opts.clone();
    rank_opts.include_purchase_hints = false;
    rank_opts.disabled_shelves = Vec::new();
    let candidates: Vec<StorefrontCandidate> = map.into_values().collect();
    let recs = rank_candidates(
        library,
        registry,
        &taste.books,
        &taste.weights,
        &taste.listening,
        candidates,
        &rank_opts,
    )
    .await?;
    let mut shelf = shelf_taste(&taste.books, &taste.weights, &taste.listening);
    shelf.wish_titles.clone_from(&taste.wish_titles);
    shelf.linked_sources.clone_from(&taste.linked_sources);
    let mut feed = feed_from_recommendations(&recs, &shelf, &rank_opts);
    // Cards keep only what is personal to the subject; catalog metadata the
    // pool holds is joined back when the page reads the feed.
    let pool = CandidatePool::from_rows(&stored);
    let json = encode_stored_feed(&mut feed, &pool, STORED_FEED_MAX_BYTES).map_err(|e| {
        crate::error::DiscoverError::message(format!("encode the discover feed: {e}"))
    })?;
    library
        .put_discover_feed(subject, &json, taste.scope, newest)
        .await?;
    let stored_bytes = serde_json::to_string(&json).map_or(json.len(), |escaped| escaped.len());
    Ok(RankReport {
        scope: taste.scope,
        shelves: feed.shelves.len(),
        candidates: stored.len(),
        cards: feed.shelves.iter().map(|s| s.items.len()).sum(),
        stored_bytes,
    })
}

/// One subject's stored feed, ready for the page.
#[derive(Debug, Clone)]
pub struct StoredSubjectFeed {
    /// The shelves, without the subject's hidden ones.
    pub feed: DiscoverFeed,
    /// Seeds the feed was ranked from.
    pub scope: SeedScope,
    /// When it was ranked.
    pub computed_at: DateTime<Utc>,
    /// Newest storefront fetch it used, when any.
    pub candidates_fetched_at: Option<DateTime<Utc>>,
}

/// Reads one subject's stored feed, drops their hidden shelves and joins
/// each card's catalog metadata from the candidate pool.
///
/// The stored feed keeps card references and what is personal to the
/// subject; the description, cover and other metadata the subject's
/// `discover_candidates` rows hold are read from them here, in pages that
/// stay under the database plugin's result limit.
///
/// # Returns
///
/// `None` when the subject has no stored feed yet, or it does not decode
/// (written by another release); the caller then shows placeholders and
/// queues a ranking.
///
/// # Errors
///
/// Returns the library read error.
pub async fn stored_subject_feed(
    library: &LibraryStore,
    subject: &DiscoverSubject,
    disabled_shelves: &[String],
) -> Result<Option<StoredSubjectFeed>> {
    let Some(mut stored) = stored_subject_feed_state(library, subject).await? else {
        return Ok(None);
    };
    stored
        .feed
        .shelves
        .retain(|shelf| !shelf_is_disabled(&shelf.id, disabled_shelves));
    if feed_needs_pool(&stored.feed) {
        let rows = library.list_discover_candidates(subject).await?;
        join_feed(&mut stored.feed, &CandidatePool::from_rows(&rows));
    }
    Ok(Some(stored))
}

/// Reads one subject's stored feed without joining its catalog metadata,
/// for callers that need only whether it exists and when it was ranked.
///
/// The cards come back as stored: references and the subject's own fields,
/// without the description, cover and other metadata
/// [`stored_subject_feed`] joins from the candidate pool. Hidden shelves
/// are kept. No candidate row is read.
///
/// # Returns
///
/// `None` when the subject has no stored feed yet, or it does not decode.
///
/// # Errors
///
/// Returns the library read error.
pub async fn stored_subject_feed_state(
    library: &LibraryStore,
    subject: &DiscoverSubject,
) -> Result<Option<StoredSubjectFeed>> {
    let Some(stored) = library.get_discover_feed(subject).await? else {
        return Ok(None);
    };
    let Some(feed) = decode_stored_feed(&stored.feed_json) else {
        tracing::debug!(subject = %subject.key(), "stored discover feed does not decode");
        return Ok(None);
    };
    Ok(Some(StoredSubjectFeed {
        feed,
        scope: stored.seed_scope,
        computed_at: stored.computed_at,
        candidates_fetched_at: stored.candidates_fetched_at,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_alternate_books_and_wishes() {
        let seed = |t: &str| SeedTitle::from_wish(t, None, None, None);
        let titles =
            |v: Vec<SeedTitle>| -> Vec<String> { v.into_iter().map(|s| s.seed.title).collect() };
        assert_eq!(
            titles(interleave_seeds(
                vec![seed("b1"), seed("b2"), seed("b3")],
                vec![seed("w1")]
            )),
            vec!["b1", "w1", "b2", "b3"]
        );
        assert_eq!(
            titles(interleave_seeds(Vec::new(), vec![seed("w1"), seed("w2")])),
            vec!["w1", "w2"]
        );
    }

    #[test]
    fn call_keys_separate_their_parts() {
        assert_ne!(
            call_key("a", "expand", &["bc"]),
            call_key("a", "expand", &["b", "c"])
        );
        assert_ne!(
            call_key("audible", "deals", &[]),
            call_key("chirp", "deals", &[])
        );
        assert_eq!(call_key("a", "k", &["x"]), call_key("a", "k", &["x"]));
        assert_eq!(call_key("a", "k", &[]).len(), 64);
    }

    /// A storefront double: deals answer from `deals`, or every call fails.
    struct FakeStore {
        /// Store id.
        id: &'static str,
        /// Product ids `list_deals` returns; `None` makes every call fail.
        deals: Option<Vec<&'static str>>,
        /// Calls made to the store (cached answers do not count).
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        /// Price lookups made to the store.
        price_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ContentSource for FakeStore {
        fn id(&self) -> &str {
            self.id
        }

        fn portal_auth_mode(&self) -> bookclerk_source::PortalAuthMode {
            bookclerk_source::PortalAuthMode::Password
        }

        fn portal_brand(&self) -> bookclerk_source::SourceBrand {
            bookclerk_source::SourceBrand {
                id: "fake",
                name: "Fake",
                bg: "#000000",
                fg: "#ffffff",
                accent: "#000000",
                icon_url: "",
            }
        }

        async fn login(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _opts: bookclerk_source::LoginOptions,
        ) -> bookclerk_source::Result<bookclerk_source::SourceAccount> {
            Err(bookclerk_source::SourceError::api("unused"))
        }

        async fn list_accounts(
            &self,
            _scope: &bookclerk_library::SourceScope,
        ) -> bookclerk_source::Result<Vec<bookclerk_source::SourceAccount>> {
            Ok(Vec::new())
        }

        async fn scan(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _opts: bookclerk_source::ScanOptions,
        ) -> bookclerk_source::Result<bookclerk_source::ScanSummary> {
            Ok(bookclerk_source::ScanSummary::default())
        }

        async fn fetch_title(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _account_id: &str,
            _title_id: &str,
            _opts: &bookclerk_source::FetchOptions,
        ) -> bookclerk_source::Result<bookclerk_source::SourceFetch> {
            Err(bookclerk_source::SourceError::api("unused"))
        }

        async fn list_deals(&self, _limit: usize) -> bookclerk_source::Result<Vec<CatalogHit>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let Some(deals) = &self.deals else {
                return Err(bookclerk_source::SourceError::api("store is down"));
            };
            Ok(deals
                .iter()
                .map(|id| CatalogHit {
                    product_id: (*id).to_string(),
                    title: format!("Deal {id}"),
                    origin: String::from("deals"),
                    cover_url: Some(format!("https://covers.example/{id}.jpg")),
                    description: Some(format!("The long blurb of deal {id}. ").repeat(20)),
                    ..CatalogHit::default()
                })
                .collect())
        }

        async fn expand_candidates(
            &self,
            seed: &bookclerk_source::ExpandSeed,
            _limit: usize,
        ) -> bookclerk_source::Result<Vec<CatalogHit>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.deals.is_none() {
                return Err(bookclerk_source::SourceError::api("store is down"));
            }
            Ok(vec![CatalogHit {
                product_id: format!("near-{}", seed.product_id),
                title: format!("Near {}", seed.title),
                origin: String::from("related"),
                ..CatalogHit::default()
            }])
        }

        /// Every product costs 12.34 at this store.
        async fn purchase_hint(
            &self,
            opts: &PurchaseHintOpts,
        ) -> bookclerk_source::Result<Option<bookclerk_source::SourcePurchaseHint>> {
            self.price_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.id == "limited" {
                return Err(SourceError::RateLimited(Some(Duration::from_secs(60))));
            }
            Ok(opts
                .product_id
                .clone()
                .map(|product_id| bookclerk_source::SourcePurchaseHint {
                    product_id,
                    title: opts.title.clone(),
                    url: None,
                    price_cents: Some(1_234),
                    currency: Some(String::from("USD")),
                    price_label: Some(String::from("$12.34")),
                    ..Default::default()
                }))
        }
    }

    /// A library with two members' books: Alice's account is linked to her
    /// identity, Bob's is not linked to anyone.
    async fn two_member_library() -> (LibraryStore, bookclerk_library::PortalIdentity) {
        let library = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        for (account, product, title) in [
            ("alice@example.com", "a1", "Alice Book"),
            ("bob@example.com", "b1", "Bob Book"),
        ] {
            library
                .upsert_account("libro", account, "us", None, true, "libro")
                .await
                .unwrap();
            let mut book = bookclerk_library::NewBook::minimal(product, account, "us", title);
            book.source = "libro".into();
            book.plugin_key = "libro".into();
            book.asin = None;
            book.isbn = Some(format!("97800000000{}", &product[1..]));
            book.authors = Some("Ann Author".into());
            library.upsert_book(&book).await.unwrap();
        }
        let alice = library
            .upsert_portal_identity("oidc", "alice", Some("Alice"))
            .await
            .unwrap();
        library
            .link_account(alice.id, "libro", "alice@example.com", "libro")
            .await
            .unwrap();
        (library, alice)
    }

    #[tokio::test]
    async fn a_member_seeds_from_their_own_books_and_others_from_the_household() {
        let (library, alice) = two_member_library().await;
        let opts = RecommendOptions::default();
        let user = alice.user_id.unwrap();

        let own = load_subject_taste(&library, &DiscoverSubject::User(user), &opts)
            .await
            .unwrap();
        assert_eq!(own.scope, SeedScope::Own);
        // Her one book is too little on its own, so Bob's blends in behind
        // it at half weight.
        let titles: Vec<&str> = own.books.iter().map(|b| b.title.as_str()).collect();
        assert_eq!(titles, vec!["Alice Book", "Bob Book"]);
        assert!(!own.weights.contains_key(&own.books[0].uuid));
        assert_eq!(
            own.weights.get(&own.books[1].uuid),
            Some(&HOUSEHOLD_BLEND_WEIGHT)
        );
        assert_eq!(own.linked_sources, HashSet::from([String::from("libro")]));
        let seeds: Vec<String> = own
            .book_seeds(8, "us")
            .into_iter()
            .map(|s| s.seed.title)
            .collect();
        assert_eq!(
            seeds,
            vec!["Alice Book", "Bob Book"],
            "own books seed first"
        );

        let portal = load_subject_taste(&library, &DiscoverSubject::Portal(alice.id), &opts)
            .await
            .unwrap();
        assert_eq!(portal.scope, SeedScope::Own);
        assert_eq!(portal.books.len(), 2);

        let operator = load_subject_taste(&library, &DiscoverSubject::Operator, &opts)
            .await
            .unwrap();
        assert_eq!(operator.scope, SeedScope::Household);
        assert_eq!(operator.books.len(), 2);
        assert!(operator.weights.is_empty());

        // An identity with nothing linked, and a user with no identity, fall
        // back to the household.
        let carol = library
            .upsert_portal_identity("oidc", "carol", Some("Carol"))
            .await
            .unwrap();
        for subject in [
            DiscoverSubject::Portal(carol.id),
            DiscoverSubject::User(9_999),
        ] {
            let taste = load_subject_taste(&library, &subject, &opts).await.unwrap();
            assert_eq!(taste.scope, SeedScope::Household, "{subject:?}");
            assert_eq!(taste.books.len(), 2);
        }
    }

    /// A first wish adds a seed to the household taste the member already
    /// had instead of replacing it, so their shelves stay.
    #[tokio::test]
    async fn a_wish_alone_keeps_the_household_taste_behind_it() {
        let (library, _) = two_member_library().await;
        let carol = library
            .upsert_portal_identity("oidc", "carol", Some("Carol"))
            .await
            .unwrap();
        library
            .create_title_request(&bookclerk_library::NewTitleRequest {
                uuid: None,
                identity_id: Some(carol.id),
                title: "Hail Mary".into(),
                authors: Some("Andy Weir".into()),
                asin: None,
                isbn: None,
                notes: None,
                status: RequestStatus::Open,
                work_key: String::new(),
                work_id: None,
                resolved_book_uuid: None,
                cover_url: None,
            })
            .await
            .unwrap();
        // Alice's listening to her own book blends in with that book.
        let alice_book = library
            .list_books(None)
            .await
            .unwrap()
            .into_iter()
            .find(|b| b.title == "Alice Book")
            .unwrap()
            .uuid;
        library
            .upsert_listening_progress(&listen(None, "oidc", "alice", "i1", &alice_book))
            .await
            .unwrap();
        let taste = load_subject_taste(
            &library,
            &DiscoverSubject::Portal(carol.id),
            &RecommendOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(taste.scope, SeedScope::Own);
        assert_eq!(taste.books.len(), 2, "both household books blend in");
        assert_eq!(taste.listening.len(), 1, "household listening blends in");
        assert_eq!(
            taste.listening[0].book_uuid.as_deref(),
            Some(alice_book.as_str())
        );
        assert!(taste
            .books
            .iter()
            .all(|b| taste.weights.get(&b.uuid) == Some(&HOUSEHOLD_BLEND_WEIGHT)));
        assert_eq!(taste.wish_titles, vec!["Hail Mary"]);
        assert_eq!(taste.wishlist.len(), 1);
    }

    /// A listening row on `book_uuid` for `(provider, external)`.
    fn listen(
        identity_id: Option<i64>,
        provider: &str,
        external: &str,
        item: &str,
        book_uuid: &str,
    ) -> bookclerk_library::NewListeningProgress {
        bookclerk_library::NewListeningProgress {
            identity_id,
            provider: provider.to_string(),
            external_user_id: external.to_string(),
            book_uuid: Some(book_uuid.to_string()),
            work_id: None,
            external_item_id: item.to_string(),
            title: None,
            authors: None,
            asin: None,
            isbn: None,
            progress: Some(0.5),
            current_time_seconds: Some(3_600.0),
            duration_seconds: Some(7_200.0),
            is_finished: false,
            last_listened_at: None,
        }
    }

    #[tokio::test]
    async fn listening_seeds_a_member_by_identity_or_by_provider_and_external_id() {
        let (library, alice) = two_member_library().await;
        let opts = RecommendOptions::default();
        let bob_book = library
            .list_books(None)
            .await
            .unwrap()
            .into_iter()
            .find(|b| b.title == "Bob Book")
            .unwrap()
            .uuid;
        let carol = library
            .upsert_portal_identity("oidc", "carol", Some("Carol"))
            .await
            .unwrap();
        // Rows that are not Carol's: the same external id on another provider,
        // and a row attributed to Alice that reuses Carol's external id.
        for row in [
            listen(None, "abs", "carol", "i1", &bob_book),
            listen(Some(alice.id), "oidc", "carol", "i2", &bob_book),
        ] {
            library.upsert_listening_progress(&row).await.unwrap();
        }
        let none = load_subject_taste(&library, &DiscoverSubject::Portal(carol.id), &opts)
            .await
            .unwrap();
        assert_eq!(none.scope, SeedScope::Household);
        // The household fallback keeps the household's listening.
        assert_eq!(none.listening.len(), 2);

        // A legacy row on Carol's own provider and external id is hers, and
        // the book she listened to seeds her fetch without her owning it.
        library
            .upsert_listening_progress(&listen(None, "oidc", "carol", "i3", &bob_book))
            .await
            .unwrap();
        let taste = load_subject_taste(&library, &DiscoverSubject::Portal(carol.id), &opts)
            .await
            .unwrap();
        assert_eq!(taste.scope, SeedScope::Own);
        assert_eq!(taste.listening.len(), 1);
        let titles: Vec<&str> = taste.books.iter().map(|b| b.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["Bob Book", "Alice Book"],
            "her own listen first"
        );
        assert!(!taste.weights.contains_key(&taste.books[0].uuid));
        assert!(taste.weights.contains_key(&taste.books[1].uuid));
    }

    #[tokio::test]
    async fn a_ranked_feed_is_stored_and_read_without_hidden_shelves() {
        let (library, alice) = two_member_library().await;
        let subject = DiscoverSubject::User(alice.user_id.unwrap());
        let mut registry = SourceRegistry::new();
        registry.register(std::sync::Arc::new(FakeStore {
            id: "up",
            deals: Some(vec!["d1", "d2", "d3"]),
            calls: std::sync::Arc::default(),
            price_calls: std::sync::Arc::default(),
        }));
        let opts = RecommendOptions::default();
        assert!(stored_subject_feed(&library, &subject, &[])
            .await
            .unwrap()
            .is_none());

        let fetched = fetch_subject_candidates(&library, &registry, &subject, &opts)
            .await
            .unwrap();
        // Alice's book and Bob's blended-in book each seed an expand call;
        // the deals come regardless, and every candidate was priced in the
        // same fetch.
        assert_eq!(fetched.seeds, 2);
        assert_eq!(fetched.candidates, 5);
        assert_eq!(fetched.priced, 5);
        let stored = library.list_discover_candidates(&subject).await.unwrap();
        assert!(stored.iter().all(|c| {
            serde_json::from_str::<StorefrontCandidate>(&c.candidate_json)
                .unwrap()
                .price_label
                .as_deref()
                == Some("$12.34")
        }));
        let near = stored
            .iter()
            .map(|c| serde_json::from_str::<StorefrontCandidate>(&c.candidate_json).unwrap())
            .find(|c| c.product_id == "near-a1")
            .unwrap();
        assert_eq!(near.seed_title.as_deref(), Some("Alice Book"));
        let ranked = rank_subject_feed(&library, &registry, &subject, &opts)
            .await
            .unwrap();
        assert_eq!(ranked.scope, SeedScope::Own);
        assert_eq!(ranked.candidates, 5);
        assert!(ranked.shelves > 0);
        assert!(ranked.cards >= ranked.shelves);
        assert!(ranked.stored_bytes > 0 && ranked.stored_bytes <= STORED_FEED_MAX_BYTES);

        let read = stored_subject_feed(&library, &subject, &[])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read.scope, SeedScope::Own);
        assert_eq!(read.feed.shelves.len(), ranked.shelves);
        assert!(read.candidates_fetched_at.is_some());
        // The state read skips the join: same feed, no blurbs.
        let state = stored_subject_feed_state(&library, &subject)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.computed_at, read.computed_at);
        assert_eq!(state.feed.shelves.len(), read.feed.shelves.len());
        assert!(state
            .feed
            .shelves
            .iter()
            .flat_map(|s| &s.items)
            .all(|c| c.description.is_none()));
        // The stored feed leaves the deals' blurbs and covers to the pool,
        // and the read joins them back.
        let raw = library.get_discover_feed(&subject).await.unwrap().unwrap();
        assert!(!raw.feed_json.contains("long blurb"), "{}", raw.feed_json);
        let deals: Vec<_> = read
            .feed
            .shelves
            .iter()
            .flat_map(|s| &s.items)
            .filter(|c| c.title.starts_with("Deal "))
            .collect();
        assert!(!deals.is_empty());
        for card in deals {
            let id = card.candidate_product_id.as_deref().unwrap();
            assert!(card.description.as_deref().unwrap().contains(id));
            assert_eq!(
                card.cover_url.as_deref(),
                Some(format!("https://covers.example/{id}.jpg").as_str())
            );
        }
        let hidden: Vec<String> = read.feed.shelves.iter().map(|s| s.id.clone()).collect();
        let filtered = stored_subject_feed(&library, &subject, &hidden)
            .await
            .unwrap()
            .unwrap();
        assert!(filtered.feed.shelves.is_empty());

        // A feed another release wrote, which no longer decodes, reads as none.
        library
            .put_discover_feed(&subject, "not json", SeedScope::Own, None)
            .await
            .unwrap();
        assert!(stored_subject_feed(&library, &subject, &[])
            .await
            .unwrap()
            .is_none());
        assert!(stored_subject_feed_state(&library, &subject)
            .await
            .unwrap()
            .is_none());
    }

    /// A candidate stub at `product_id`, unpriced.
    fn unpriced(product_id: &str) -> StorefrontCandidate {
        hit_to_candidate(
            "fake",
            CatalogHit {
                product_id: product_id.into(),
                title: format!("Title {product_id}"),
                ..CatalogHit::default()
            },
        )
    }

    /// Prices answered once are remembered for every later fetch, so a
    /// refresh only asks the store about titles it has not priced.
    #[tokio::test]
    async fn a_price_is_asked_for_once_a_day() {
        let library = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        let price_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let store = FakeStore {
            id: "memo",
            deals: Some(Vec::new()),
            calls: std::sync::Arc::default(),
            price_calls: std::sync::Arc::clone(&price_calls),
        };
        // Two seeds found "a": one question, both copies priced.
        let mut first = vec![unpriced("a"), unpriced("b"), unpriced("a")];
        assert_eq!(
            price_candidates(&library, &store, &mut first, "us").await,
            3
        );
        assert_eq!(price_calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        let mut again = vec![unpriced("a"), unpriced("b"), unpriced("c")];
        assert_eq!(
            price_candidates(&library, &store, &mut again, "us").await,
            3
        );
        // Only "c" reached the store; "a" and "b" came from the memo.
        assert_eq!(price_calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert!(again
            .iter()
            .all(|c| c.price_label.as_deref() == Some("$12.34")));
    }

    /// A 429 during a fetch pauses the store, and a paused store is not asked.
    #[tokio::test]
    async fn a_rate_limited_store_is_left_alone_by_the_fetch() {
        let library = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        let price_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let store = FakeStore {
            id: "limited",
            deals: Some(Vec::new()),
            calls: std::sync::Arc::default(),
            price_calls: std::sync::Arc::clone(&price_calls),
        };
        for _ in 0..2 {
            let mut batch = vec![unpriced("a"), unpriced("b")];
            assert_eq!(
                price_candidates(&library, &store, &mut batch, "us").await,
                0
            );
            assert!(batch.iter().all(|c| c.price_cents.is_none()));
        }
        assert_eq!(price_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn each_store_saves_alone_and_a_failed_store_keeps_its_rows() {
        let library = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        let subject = DiscoverSubject::User(1);
        library
            .replace_discover_candidates(
                &subject,
                "down",
                SeedScope::Household,
                &[(String::from("old"), String::from("{}"))],
            )
            .await
            .unwrap();
        let up_calls = std::sync::Arc::default();
        let down_calls = std::sync::Arc::default();
        let mut registry = SourceRegistry::new();
        registry.register(std::sync::Arc::new(FakeStore {
            id: "up",
            deals: Some(vec!["a", "b"]),
            calls: std::sync::Arc::clone(&up_calls),
            price_calls: std::sync::Arc::default(),
        }));
        registry.register(std::sync::Arc::new(FakeStore {
            id: "down",
            deals: None,
            calls: std::sync::Arc::clone(&down_calls),
            price_calls: std::sync::Arc::default(),
        }));
        let opts = RecommendOptions::default();

        let report = fetch_subject_candidates(&library, &registry, &subject, &opts)
            .await
            .unwrap();
        assert_eq!(report.stores_saved, 1);
        assert_eq!(report.stores_failed, vec![String::from("down")]);
        assert_eq!(report.candidates, 2);
        let rows = library.list_discover_candidates(&subject).await.unwrap();
        let mut by_store: Vec<(&str, &str)> = rows
            .iter()
            .map(|r| (r.source.as_str(), r.candidate_key.as_str()))
            .collect();
        by_store.sort_unstable();
        assert_eq!(by_store.len(), 3, "{by_store:?}");
        assert!(by_store.contains(&("down", "old")));

        // A second person with the same seeds reuses the cached answers.
        let before = up_calls.load(std::sync::atomic::Ordering::SeqCst);
        fetch_subject_candidates(&library, &registry, &DiscoverSubject::User(2), &opts)
            .await
            .unwrap();
        assert_eq!(up_calls.load(std::sync::atomic::Ordering::SeqCst), before);
        // Failures are not cached: the down store is asked again.
        assert!(down_calls.load(std::sync::atomic::Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn an_empty_answer_keeps_the_store_rows_and_is_not_cached() {
        let library = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        let subject = DiscoverSubject::User(1);
        library
            .replace_discover_candidates(
                &subject,
                "quiet",
                SeedScope::Household,
                &[(String::from("old"), String::from("{}"))],
            )
            .await
            .unwrap();
        let calls = std::sync::Arc::default();
        let mut registry = SourceRegistry::new();
        registry.register(std::sync::Arc::new(FakeStore {
            id: "quiet",
            deals: Some(Vec::new()),
            calls: std::sync::Arc::clone(&calls),
            price_calls: std::sync::Arc::default(),
        }));
        let opts = RecommendOptions::default();
        for _ in 0..2 {
            let report = fetch_subject_candidates(&library, &registry, &subject, &opts)
                .await
                .unwrap();
            assert_eq!(report.stores_failed, vec![String::from("quiet")]);
        }
        let rows = library.list_discover_candidates(&subject).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].candidate_key, "old");
        // The empty deals answer was not cached, so both fetches asked.
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn the_total_call_budget_is_shared_by_the_stores() {
        let subject = DiscoverSubject::Operator;
        let calls = std::sync::Arc::default();
        let mut registry = SourceRegistry::new();
        for id in ["one", "two", "three"] {
            registry.register(std::sync::Arc::new(FakeStore {
                id,
                deals: Some(vec!["d1"]),
                calls: std::sync::Arc::clone(&calls),
                price_calls: std::sync::Arc::default(),
            }));
        }
        for (total, expected) in [(0, 0), (1, 1), (4, 4)] {
            // A fresh library each round, so no answer is cached yet.
            let (library, _) = two_member_library().await;
            calls.store(0, std::sync::atomic::Ordering::SeqCst);
            let opts = RecommendOptions {
                storefront_max_remote_calls: total,
                ..RecommendOptions::default()
            };
            fetch_subject_candidates(&library, &registry, &subject, &opts)
                .await
                .unwrap();
            assert_eq!(
                calls.load(std::sync::atomic::Ordering::SeqCst),
                expected,
                "budget {total}"
            );
        }
    }
}
