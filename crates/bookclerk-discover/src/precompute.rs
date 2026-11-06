//! Precomputed Discover shelves: the two queued jobs and the stored read.
//!
//! Discover used to rebuild every shelf inside the page request. Now:
//!
//! 1. [`fetch_subject_candidates`] (the `discover_candidates` job) picks one
//!    subject's own seeds (their books, listening, ratings and wishlist, or
//!    the household's while they have none), asks **every** deployed
//!    storefront about them concurrently, and stores each store's answer as
//!    soon as it arrives. Storefront answers are cached for a day in
//!    `discover_store_calls`, shared by every subject.
//! 2. [`rank_subject_feed`] (the `discover_feed` job) ranks the stored
//!    candidates against the same taste, with no network, and stores the
//!    shelves.
//! 3. [`stored_subject_feed`] reads them back for the page, dropping the
//!    subject's hidden shelves.
//!
//! See `docs/internals/discovery.md`, *Precomputed shelves*.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use bookclerk_library::{
    BookRecord, DiscoverSubject, LibraryStore, ListeningProgressRecord, RequestStatus, SeedScope,
};
use bookclerk_source::{CatalogHit, ContentSource, SourceRegistry};
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
    RecommendOptions,
};
use crate::shelves::{shelf_is_disabled, DiscoverFeed};

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

/// One subject's taste: whose seeds Discover uses and what they read.
#[derive(Debug, Clone)]
pub struct SubjectTaste {
    /// Whether the taste is the subject's own or the household's.
    pub scope: SeedScope,
    /// Books whose finishes, ratings, series and authors shape the taste.
    pub books: Vec<BookRecord>,
    /// The subject's listening rows (all rows for the household).
    pub listening: Vec<ListeningProgressRecord>,
    /// The subject's open wishlist titles, used as extra seeds.
    pub wishlist: Vec<SeedTitle>,
}

/// Loads one subject's taste.
///
/// A user or portal identity uses the books held by store accounts linked to
/// them plus the household books they have listened to, the listening rows
/// recorded for them (by identity id, or by provider and external user id on
/// rows recorded before the identity was known), and their open wishlist. When
/// all three are empty (a new member, or a visitor who has connected no store
/// yet), and always for the operator, the household library is used instead.
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
        return Ok(SubjectTaste {
            scope: SeedScope::Household,
            books: household,
            listening: all_listening,
            wishlist: Vec::new(),
        });
    }

    let identity_ids: HashSet<i64> = identities.iter().map(|i| i.id).collect();
    // External user ids are provider-local, so a legacy row without an
    // identity id matches only on the (provider, external id) pair.
    let external_ids: HashSet<(&str, &str)> = identities
        .iter()
        .map(|i| (i.provider.as_str(), i.external_user_id.as_str()))
        .collect();
    let mut held: HashSet<(String, String)> = HashSet::new();
    for identity in &identities {
        for link in library.list_account_links(identity.id).await? {
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
    let books: Vec<BookRecord> = household
        .iter()
        .filter(|b| {
            held.contains(&(b.plugin_key.clone(), b.account_id.clone()))
                || listened.contains(b.uuid.as_str())
        })
        .cloned()
        .collect();
    let wishlist: Vec<SeedTitle> = library
        .list_title_requests(Some(RequestStatus::Open))
        .await?
        .into_iter()
        .filter(|r| r.identity_id.is_some_and(|id| identity_ids.contains(&id)))
        .take(WISHLIST_SEED_LIMIT)
        .map(|r| SeedTitle::from_wish(&r.title, r.authors, r.asin, r.isbn))
        .collect();

    if books.is_empty() && listening.is_empty() && wishlist.is_empty() {
        // Nothing of their own yet: fall back to the whole household's
        // taste, listening included, as for a subject with no identity.
        return Ok(SubjectTaste {
            scope: SeedScope::Household,
            books: household,
            listening: all_listening,
            wishlist: Vec::new(),
        });
    }
    Ok(SubjectTaste {
        scope: SeedScope::Own,
        books,
        listening,
        wishlist,
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
    let book_seeds: Vec<SeedTitle> =
        select_taste_seeds(&taste.books, &engagement_by_book(&taste.listening))
            .iter()
            .take(opts.storefront_seed_limit)
            .map(|b| SeedTitle::from_book(b, &opts.region))
            .collect();
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

    let mut tasks: JoinSet<(String, Vec<StorefrontCandidate>, bool)> = JoinSet::new();
    for (index, source) in stores.into_iter().enumerate() {
        // Split the total budget so the stores together never exceed it; a
        // store whose share is zero is not asked and keeps its stored rows.
        let per_store = base + usize::from(index < extra);
        if per_store == 0 {
            continue;
        }
        let seeds = seeds.clone();
        let library = library.clone();
        let exclude_sets = opts.exclude_graphicaudio_series_sets;
        tasks.spawn(async move {
            let id = source.id().to_string();
            let (hits, ok) =
                fetch_one_store(&library, source.as_ref(), &seeds, per_store, exclude_sets).await;
            (id, hits, ok)
        });
    }
    while let Some(joined) = tasks.join_next().await {
        let Ok((store, candidates, ok)) = joined else {
            tracing::warn!("a discover store fetch task stopped before it finished");
            continue;
        };
        if !ok {
            report.stores_failed.push(store);
            continue;
        }
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
        failed = ?report.stores_failed,
        "stored discover candidates"
    );
    Ok(report)
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

/// Asks one store for its deals, then about each seed, in at most `budget` calls.
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
        &household,
        &taste.books,
        &taste.listening,
        candidates,
        &rank_opts,
    )
    .await?;
    let feed = feed_from_recommendations(&recs, &taste.books, &taste.listening, &rank_opts);
    let json = serde_json::to_string(&feed).map_err(|e| {
        crate::error::DiscoverError::message(format!("encode the discover feed: {e}"))
    })?;
    library
        .put_discover_feed(subject, &json, taste.scope, newest)
        .await?;
    Ok(RankReport {
        scope: taste.scope,
        shelves: feed.shelves.len(),
        candidates: stored.len(),
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

/// Reads one subject's stored feed and drops their hidden shelves.
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
    let Some(stored) = library.get_discover_feed(subject).await? else {
        return Ok(None);
    };
    let Ok(mut feed) = serde_json::from_str::<DiscoverFeed>(&stored.feed_json) else {
        tracing::debug!(subject = %subject.key(), "stored discover feed does not decode");
        return Ok(None);
    };
    feed.shelves
        .retain(|shelf| !shelf_is_disabled(&shelf.id, disabled_shelves));
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
        let titles: Vec<&str> = own.books.iter().map(|b| b.title.as_str()).collect();
        assert_eq!(titles, vec!["Alice Book"]);

        let portal = load_subject_taste(&library, &DiscoverSubject::Portal(alice.id), &opts)
            .await
            .unwrap();
        assert_eq!(portal.scope, SeedScope::Own);
        assert_eq!(portal.books.len(), 1);

        let operator = load_subject_taste(&library, &DiscoverSubject::Operator, &opts)
            .await
            .unwrap();
        assert_eq!(operator.scope, SeedScope::Household);
        assert_eq!(operator.books.len(), 2);

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
        assert_eq!(titles, vec!["Bob Book"]);
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
        }));
        let opts = RecommendOptions::default();
        assert!(stored_subject_feed(&library, &subject, &[])
            .await
            .unwrap()
            .is_none());

        let fetched = fetch_subject_candidates(&library, &registry, &subject, &opts)
            .await
            .unwrap();
        // Alice's book seeds one expand call; the deals come regardless.
        assert_eq!(fetched.seeds, 1);
        assert_eq!(fetched.candidates, 4);
        let stored = library.list_discover_candidates(&subject).await.unwrap();
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
        assert_eq!(ranked.candidates, 4);
        assert!(ranked.shelves > 0);

        let read = stored_subject_feed(&library, &subject, &[])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read.scope, SeedScope::Own);
        assert_eq!(read.feed.shelves.len(), ranked.shelves);
        assert!(read.candidates_fetched_at.is_some());
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
        }));
        registry.register(std::sync::Arc::new(FakeStore {
            id: "down",
            deals: None,
            calls: std::sync::Arc::clone(&down_calls),
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
