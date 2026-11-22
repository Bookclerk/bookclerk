//! Public Audible catalog helpers for Discover (`search_catalog` / expand / purchase).
//!
//! Uses [`bookclerk_audible_catalog`] public catalog HTTP — no account required.

use bookclerk_audible_catalog::{
    catalog_origin, fetch_products_by_asins, is_valid_asin, normalize_region, public_http_client,
    region_tld, search_catalog_by_genre_name, search_catalog_by_narrator,
    search_catalog_by_series_asin, search_catalog_products, search_catalog_products_paged,
    search_catalog_storefront, search_similar_products, CatalogProduct,
};
use bookclerk_storefront::records::{self, CreditRole, IdentifierScheme};
use bookclerk_storefront::{
    CatalogHit, CatalogSearchField, CatalogSearchOpts, ExpandSeed, PricedAnswer, PricedListing,
    PurchaseHintOpts, SourceError, SourcePurchaseHint,
};
use serde_json::Value;

/// Search the public Audible catalog for Discover typeahead / paged browse.
///
/// When [`CatalogSearchOpts::field`] is set, uses storefront-native filters
/// (`author=` / `narrator=` / series-name filter / Genres `category_id`) so facet
/// links return the right catalog slice. General queries use `keywords=` only
/// and preserve Audible relevance order (host merge soft-prefers language).
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn search_catalog(
    opts: &CatalogSearchOpts,
) -> bookclerk_storefront::Result<Vec<CatalogHit>> {
    let q = opts.query.trim();
    if q.is_empty() {
        return Ok(Vec::new());
    }
    let limit = opts.effective_limit();
    let http =
        public_http_client().map_err(|e| bookclerk_storefront::SourceError::api(e.to_string()))?;
    let region = normalize_region(&opts.region);
    let fetch_limit = catalog_fetch_limit(limit);
    let page = opts.page.max(1);
    let products_sort = opts.sort.audible_products_sort_by();
    let storefront_sort = opts.sort.audible_catalog_search_sort();
    // Always request ratings so Discover can filter (min_rating) and sort by
    // rating without a second round-trip.
    let with_rating = true;

    let products = match opts.field {
        Some(CatalogSearchField::Author) => {
            // Author facet: keep products `author=` filter (storefront search
            // ignores lone author= and returns unrelated bestsellers).
            search_catalog_products_paged(
                &http,
                &region,
                "",
                Some(q),
                None,
                None,
                None,
                None,
                page,
                products_sort,
                true,
                fetch_limit,
                with_rating,
            )
            .await
        }
        Some(CatalogSearchField::Narrator) => {
            search_catalog_products_paged(
                &http,
                &region,
                "",
                None,
                None,
                Some(q),
                None,
                None,
                page,
                products_sort,
                true,
                fetch_limit,
                with_rating,
            )
            .await
        }
        Some(CatalogSearchField::Series) => {
            // Storefront search recalls series volumes that `/catalog/products`
            // keyword filters miss (e.g. A Song of Ice and Fire).
            search_catalog_storefront(
                &http,
                &region,
                q,
                page,
                storefront_sort,
                true,
                fetch_limit,
                with_rating,
            )
            .await
        }
        Some(CatalogSearchField::Genre) => {
            search_catalog_by_genre_name(
                &http,
                &region,
                q,
                page,
                products_sort,
                fetch_limit,
                with_rating,
            )
            .await
        }
        None => {
            // Website-equivalent storefront search. `/catalog/products?keywords=`
            // does not surface many purchasable titles that still resolve by ASIN
            // (verified: B002UZZ93G *A Game of Thrones*).
            search_catalog_storefront(
                &http,
                &region,
                q,
                page,
                storefront_sort,
                true,
                fetch_limit,
                with_rating,
            )
            .await
        }
    }
    .map_err(|e| bookclerk_storefront::SourceError::api(e.to_string()))?;

    let mut hits: Vec<CatalogHit> = products
        .into_iter()
        .filter(|p| !p.asin.trim().is_empty())
        .take(limit)
        .map(|p| product_to_hit(p, String::from("search")))
        .collect();
    // Audible's order is meaningful for every sort it is asked for (its
    // relevance and bestseller rankings), so each hit carries its rank and
    // catalog search can merge it with other stores.
    opts.stamp_rank_hints(&mut hits);
    Ok(hits)
}

/// How many similar titles to ask Audible for per seed.
const SIMILAR_TITLES: usize = 20;

/// Expansion hits in the order they were found, one per ASIN.
#[derive(Default)]
struct OrderedHits {
    /// Upper-cased ASINs already kept.
    seen: std::collections::HashSet<String>,
    /// Kept hits, first found first.
    hits: Vec<CatalogHit>,
}

impl OrderedHits {
    /// Number of hits kept so far.
    fn len(&self) -> usize {
        self.hits.len()
    }

    /// Keeps `p` as a hit found by `origin`, unless it has no ASIN or an
    /// earlier source already found it (the earlier, stronger origin wins).
    fn push(&mut self, p: CatalogProduct, origin: impl FnOnce() -> String) {
        if p.asin.trim().is_empty() {
            return;
        }
        if self.seen.insert(p.asin.to_ascii_uppercase()) {
            self.hits.push(product_to_hit(p, origin()));
        }
    }

    /// The first `limit` hits, in the order they were found.
    fn into_hits(self, limit: usize) -> Vec<CatalogHit> {
        let mut hits = self.hits;
        hits.truncate(limit);
        hits
    }
}

/// The seed's Audible ASIN: its own ASIN, or its product id when the seed is
/// an Audible title whose id has an ASIN's shape.
fn seed_audible_asin(seed: &ExpandSeed) -> Option<&str> {
    seed.asin
        .as_deref()
        .map(str::trim)
        .filter(|s| is_valid_asin(s))
        .or_else(|| {
            let pid = seed.product_id.trim();
            (seed.source.eq_ignore_ascii_case("audible") && is_valid_asin(pid)).then_some(pid)
        })
}

/// The seed's top-level genres as Audible files it, looked up by its ASIN
/// in the catalog at `origin`; empty when it has no ASIN or the lookup
/// fails.
async fn seed_genre_roots(
    http: &bookclerk_plugin_sdk::http::Client,
    origin: &str,
    seed: &ExpandSeed,
) -> Vec<String> {
    let Some(asin) = seed_audible_asin(seed) else {
        return Vec::new();
    };
    match fetch_products_by_asins(http, origin, &[asin]).await {
        Ok(products) => products
            .into_iter()
            .find(|p| p.asin.eq_ignore_ascii_case(asin))
            .map(|p| p.genre_roots)
            .unwrap_or_default(),
        Err(err) => {
            tracing::debug!(asin, error = %err, "audible seed genre lookup failed");
            Vec::new()
        }
    }
}

/// More titles read by `narrator`, kept only when a title shares a
/// top-level genre with `seed`: narrators also record children's books,
/// and a voice in common does not make a thriller's listener want *Llama
/// Llama Red Pajama*. A seed whose genres are unknown (looked up in the
/// catalog at `origin`) gets none, without searching.
///
/// # Errors
///
/// Returns the narrator search's error.
async fn narrator_matches(
    http: &bookclerk_plugin_sdk::http::Client,
    region: &str,
    origin: &str,
    seed: &ExpandSeed,
    narrator: &str,
) -> bookclerk_audible_catalog::Result<Vec<CatalogProduct>> {
    let roots = seed_genre_roots(http, origin, seed).await;
    if roots.is_empty() {
        return Ok(Vec::new());
    }
    let products = search_catalog_by_narrator(http, region, narrator).await?;
    Ok(products
        .into_iter()
        .filter(|p| shares_genre_root(&p.genre_roots, &roots))
        .collect())
}

/// Whether a title filed under `roots` shares a top-level genre with a
/// seed filed under `seed_roots` (names compared without case). A title
/// with no genres shares none.
fn shares_genre_root(roots: &[String], seed_roots: &[String]) -> bool {
    roots
        .iter()
        .any(|r| seed_roots.iter().any(|s| s.eq_ignore_ascii_case(r)))
}

/// Origin shown for a title Audible lists as similar to `seed_title`.
fn similar_origin(seed_title: &str) -> String {
    format!("audible related titles (“{seed_title}”)")
}

/// Expand author / series / narrator / series-ASIN candidates from a taste seed.
///
/// Soft-fails individual HTTP calls; budgets returned hits by `limit`.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn expand_candidates(
    seed: &ExpandSeed,
    limit: usize,
) -> bookclerk_storefront::Result<Vec<CatalogHit>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let http = match public_http_client() {
        Ok(c) => c,
        Err(_) => return Ok(Vec::new()),
    };
    let region = if seed.region.trim().is_empty() {
        String::from("us")
    } else {
        normalize_region(&seed.region)
    };
    let mut by_asin = OrderedHits::default();

    let push = |by_asin: &mut OrderedHits, p: CatalogProduct, origin: String| {
        by_asin.push(p, || origin);
    };

    // Exact series ASIN listing (strongest series signal).
    if let Some(series_asin) = seed
        .series_asin
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        match search_catalog_by_series_asin(&http, &region, series_asin).await {
            Ok(products) => {
                // A title the listing returns without a series keeps none:
                // the listing also returns loose matches (other editions,
                // unrelated titles), and stamping the seed's series on them
                // files them under a series they are not in.
                for p in products {
                    push(
                        &mut by_asin,
                        p,
                        format!("audible series ASIN ({series_asin})"),
                    );
                }
            }
            Err(err) => {
                tracing::debug!(
                    series_asin,
                    error = %err,
                    "audible series_asin search failed"
                );
            }
        }
    }

    // More by same author.
    if by_asin.len() < limit {
        if let Some(author) = primary_person(seed.authors.as_deref()) {
            match search_catalog_products(&http, &region, "", Some(author), None).await {
                Ok(products) => {
                    for mut p in products {
                        if p.authors.is_none() {
                            p.authors = Some(author.to_string());
                        }
                        push(&mut by_asin, p, format!("audible author search ({author})"));
                    }
                }
                Err(err) => {
                    tracing::debug!(author, error = %err, "audible author search failed");
                }
            }
        }
    }

    // Series keyword expansion when we lack series_asin.
    if by_asin.len() < limit && seed.series_asin.as_deref().is_none_or(str::is_empty) {
        if let Some(series) = seed
            .series
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            match search_catalog_products(&http, &region, "", None, Some(series)).await {
                Ok(products) => {
                    // The keyword search also returns loose matches, so keep
                    // only titles whose own series is the one searched for.
                    for p in products {
                        if !same_series(p.series.as_deref(), series) {
                            continue;
                        }
                        push(
                            &mut by_asin,
                            p,
                            format!("audible series search (“{series}”)"),
                        );
                    }
                }
                Err(err) => {
                    tracing::debug!(series, error = %err, "audible series search failed");
                }
            }
        }
    }

    // More read by the same narrator, in the seed's genres only.
    if by_asin.len() < limit {
        if let Some(narrator) = primary_person(seed.narrators.as_deref()) {
            let origin = catalog_origin(&region);
            match narrator_matches(&http, &region, &origin, seed, narrator).await {
                Ok(products) => {
                    for p in products {
                        push(
                            &mut by_asin,
                            p,
                            format!("audible narrator search ({narrator})"),
                        );
                    }
                }
                Err(err) => {
                    tracing::debug!(narrator, error = %err, "audible narrator search failed");
                }
            }
        }
    }

    // When the seed is an Audible ASIN (or product_id looks like one) and we
    // still have room, add the titles Audible's listeners also enjoyed. A
    // keyword search on the seed's title matched words, not books (*The Great
    // Hunt* brought *Turkey Hunting for Kids*).
    if by_asin.len() < limit {
        if let Some(seed_asin) = seed_audible_asin(seed) {
            match search_similar_products(&http, &region, seed_asin, SIMILAR_TITLES).await {
                Ok(products) => {
                    for p in products {
                        by_asin.push(p, || similar_origin(&seed.title));
                    }
                }
                Err(err) => {
                    tracing::debug!(error = %err, "audible similar titles lookup failed");
                }
            }
        }
    }

    // Keep the order the sources were asked in (series, author, narrator,
    // similar titles), so the cut drops the weakest signals first.
    Ok(by_asin.into_hits(limit))
}

/// Resolve a purchase / catalog URL (optionally with live price).
///
/// The bare-hint form of [`purchase_answer`]: its pick, without the
/// title-search records.
///
/// # Errors
///
/// As [`purchase_answer`].
pub async fn purchase_hint(
    opts: &PurchaseHintOpts,
) -> bookclerk_storefront::Result<Option<SourcePurchaseHint>> {
    purchase_answer(opts).await.map(|answer| answer.hint)
}

/// Resolve a title's Audible product and price, with every listing a title
/// search found.
///
/// - A known ASIN (the query's `asin`, else an ASIN-shaped `product_id`) is
///   priced as it is, with no search. This is the exact ASIN lookup the
///   store declares ([`bookclerk_storefront::PriceLookup::Asin`]).
/// - Otherwise the title and author are searched (the ISBN as keywords
///   when that misses or there is no title), and up to [`TITLE_RECORDS`]
///   listings are returned as priced records, each with its own ASIN,
///   credits and series, for the host to place. The first is also the
///   store's pick, so a host that reads only the hint still gets one.
///
/// # Errors
///
/// Returns [`SourceError::RateLimited`] when Audible asks to slow down
/// while pricing; a failed search answers nothing.
pub async fn purchase_answer(
    opts: &PurchaseHintOpts,
) -> bookclerk_storefront::Result<PricedAnswer> {
    let found = find_listings(opts).await;
    Ok(price_found(vec![found], std::slice::from_ref(opts))
        .await?
        .into_iter()
        .next()
        .unwrap_or_default())
}

/// Most listings one title search answers as records. A page of cards
/// stays within a couple of [`PRICE_BATCH_ASINS`] price requests.
pub const TITLE_RECORDS: usize = 10;

/// What one query found, before pricing.
#[derive(Debug, Default)]
struct Found {
    /// The store's pick for the query.
    hint: Option<SourcePurchaseHint>,
    /// Every listing a title search found, best first; empty when the
    /// query named its ASIN.
    records: Vec<CatalogHit>,
}

/// A hint for `asin`, linking its product page in `region`.
fn asin_hint(asin: &str, title: Option<&str>, region: &str) -> SourcePurchaseHint {
    SourcePurchaseHint {
        product_id: asin.to_string(),
        title: title.map(str::to_string),
        url: Some(format!(
            "https://www.audible{}/pd/{}",
            region_host_suffix(region),
            asin
        )),
        ..Default::default()
    }
}

/// Finds the query's product, unpriced: its known ASIN, else the listings
/// a title (then ISBN keyword) search returns.
///
/// Searches fail soft: a failed search finds nothing.
async fn find_listings(opts: &PurchaseHintOpts) -> Found {
    let region = normalize_region(&opts.region);
    let title = opts
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(asin) = known_asin(opts) {
        return Found {
            hint: Some(asin_hint(&asin, title, &region)),
            records: Vec::new(),
        };
    }
    let author = opts
        .authors
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let isbn = opts
        .isbn
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Ok(http) = public_http_client() else {
        return Found::default();
    };
    let mut products = Vec::new();
    if let Some(t) = title {
        match search_catalog_products(&http, &region, t, author, None).await {
            Ok(found) => products = found,
            Err(err) => tracing::debug!(error = %err, "audible catalog title search failed"),
        }
    }
    if products.is_empty() {
        if let Some(isbn) = isbn {
            match search_catalog_products(&http, &region, "", None, Some(isbn)).await {
                Ok(found) => products = found,
                Err(err) => tracing::debug!(error = %err, "audible isbn keyword search failed"),
            }
        }
    }
    let records = listing_records(products);
    Found {
        hint: records
            .first()
            .map(|first| asin_hint(&first.product_id, title, &region)),
        records,
    }
}

/// The search's products as records, best first: at most
/// [`TITLE_RECORDS`], one per valid ASIN.
fn listing_records(products: Vec<CatalogProduct>) -> Vec<CatalogHit> {
    let mut records: Vec<CatalogHit> = Vec::new();
    for mut product in products {
        if records.len() >= TITLE_RECORDS {
            break;
        }
        let asin = product.asin.trim().to_ascii_uppercase();
        if !is_valid_asin(&asin) || records.iter().any(|r| r.product_id == asin) {
            continue;
        }
        product.asin = asin;
        records.push(product_to_hit(product, String::from("purchase search")));
    }
    records
}

/// Prices what each query found, per marketplace, and pairs every record
/// with its own priced hint.
///
/// # Errors
///
/// [`SourceError::RateLimited`] when Audible asks to slow down.
async fn price_found(
    found: Vec<Found>,
    opts: &[PurchaseHintOpts],
) -> bookclerk_storefront::Result<Vec<PricedAnswer>> {
    let by_region = asins_by_region(&found, opts);
    let mut prices: std::collections::HashMap<(String, String), DualPriced> =
        std::collections::HashMap::new();
    for (region, asins) in by_region {
        for chunk in asins.chunks(PRICE_BATCH_ASINS) {
            for (asin, priced) in fetch_audible_prices(chunk, &region).await? {
                prices.insert((region.clone(), asin), priced);
            }
        }
    }
    Ok(found
        .into_iter()
        .zip(opts)
        .map(|(found, query)| {
            let region = normalize_region(&query.region);
            let priced = |mut hint: SourcePurchaseHint| {
                if query.with_price {
                    if let Some(p) = prices.get(&(region.clone(), hint.product_id.clone())) {
                        apply_dual_price(&mut hint, p);
                    }
                }
                hint
            };
            PricedAnswer {
                hint: found.hint.map(&priced),
                records: found
                    .records
                    .into_iter()
                    .map(|record| PricedListing {
                        hint: priced(asin_hint(&record.product_id, Some(&record.title), &region)),
                        record,
                    })
                    .collect(),
            }
        })
        .collect())
}

/// Most ASINs the public catalog prices in one `products` request.
pub const PRICE_BATCH_ASINS: usize = 50;

/// How long [`purchase_hints`] keeps starting catalog searches for titles
/// without an ASIN. Well under the host's 10 s batch deadline, so the price
/// request for every resolved title still lands in time.
pub const RESOLVE_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// The ASIN `opts` already names (its `asin`, else an ASIN-shaped
/// `product_id`), which needs no catalog search.
fn known_asin(opts: &PurchaseHintOpts) -> Option<String> {
    [opts.asin.as_deref(), opts.product_id.as_deref()]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| is_valid_asin(s))
        .map(str::to_ascii_uppercase)
}

/// The ASINs to price (each query's pick and records), grouped by each
/// query's marketplace and listed once per marketplace, for the queries
/// that asked for a price.
fn asins_by_region(
    found: &[Found],
    opts: &[PurchaseHintOpts],
) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut by_region: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for (found, query) in found.iter().zip(opts) {
        if !query.with_price {
            continue;
        }
        let asins = by_region
            .entry(normalize_region(&query.region))
            .or_default();
        let named = found
            .hint
            .iter()
            .map(|h| &h.product_id)
            .chain(found.records.iter().map(|r| &r.product_id));
        for asin in named {
            if !asins.contains(asin) {
                asins.push(asin.clone());
            }
        }
    }
    by_region.retain(|_, asins| !asins.is_empty());
    by_region
}

/// Resolves purchase hints for several titles, pricing them together.
///
/// The bare-hint form of [`purchase_answers`].
///
/// # Errors
///
/// As [`purchase_answers`].
pub async fn purchase_hints(
    opts: &[PurchaseHintOpts],
) -> bookclerk_storefront::Result<Vec<Option<SourcePurchaseHint>>> {
    Ok(purchase_answers(opts)
        .await?
        .into_iter()
        .map(|answer| answer.hint)
        .collect())
}

/// Resolves several titles, pricing them together, with each title
/// search's records.
///
/// Titles whose ASIN is known resolve first, without a search. The rest
/// are searched one by one (as [`purchase_answer`] does) until
/// [`RESOLVE_BUDGET`] runs out; a title not searched by then answers
/// nothing, which the host does not take as an answer. Then every ASIN
/// found (picks and records) is priced per marketplace, each query in its
/// own region, one catalog request per [`PRICE_BATCH_ASINS`], so a page of
/// cards costs one or two price requests per region instead of one per
/// card.
///
/// # Errors
///
/// Returns [`SourceError::RateLimited`] when Audible asks to slow down;
/// a search that fails otherwise answers nothing for that title.
pub async fn purchase_answers(
    opts: &[PurchaseHintOpts],
) -> bookclerk_storefront::Result<Vec<PricedAnswer>> {
    purchase_answers_within(opts, RESOLVE_BUDGET).await
}

/// [`purchase_answers`] with a chosen search budget.
///
/// # Errors
///
/// As [`purchase_answers`].
async fn purchase_answers_within(
    opts: &[PurchaseHintOpts],
    budget: std::time::Duration,
) -> bookclerk_storefront::Result<Vec<PricedAnswer>> {
    let deadline = tokio::time::Instant::now() + budget;
    let mut found: Vec<Found> = std::iter::repeat_with(Found::default)
        .take(opts.len())
        .collect();
    // Known ASINs first, so a slow search never costs them their price.
    let mut order: Vec<usize> = (0..opts.len()).collect();
    order.sort_by_key(|&i| known_asin(&opts[i]).is_none());
    for i in order {
        if known_asin(&opts[i]).is_some() {
            found[i] = find_listings(&opts[i]).await;
            continue;
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        match tokio::time::timeout_at(deadline, find_listings(&opts[i])).await {
            Ok(listings) => found[i] = listings,
            // Out of search time: the rest stay untried.
            Err(_) => break,
        }
    }
    price_found(found, opts).await
}

/// Marketplaces a detail lookup by ASIN tries, in order, until one knows
/// the product: a product id carries no marketplace.
const DETAIL_REGIONS: [&str; 11] = [
    "us", "uk", "ca", "au", "de", "fr", "it", "es", "in", "jp", "br",
];

/// Longest a detail lookup spends trying marketplaces.
const DETAIL_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);

/// The public catalog record for one ASIN (`catalogDetail` by product id):
/// its bibliographic fields, cover and audio sample.
///
/// # Arguments
///
/// * `product_id` - The ASIN.
///
/// # Returns
///
/// `None` when `product_id` is not an ASIN, or no marketplace tried within
/// eight seconds knows it.
///
/// # Errors
///
/// Returns an error when no HTTP client can be built.
pub async fn catalog_detail(product_id: &str) -> bookclerk_storefront::Result<Option<CatalogHit>> {
    let asin = product_id.trim().to_ascii_uppercase();
    if !is_valid_asin(&asin) {
        return Ok(None);
    }
    let http = public_http_client().map_err(|err| SourceError::Api(err.to_string()))?;
    let deadline = tokio::time::Instant::now() + DETAIL_BUDGET;
    let asins = [asin.as_str()];
    for region in DETAIL_REGIONS {
        let origin = catalog_origin(region);
        let lookup = fetch_products_by_asins(&http, &origin, &asins);
        let Ok(found) = tokio::time::timeout_at(deadline, lookup).await else {
            break;
        };
        // A marketplace that fails or does not list the ASIN: try the next.
        if let Some(product) = found.ok().and_then(|products| {
            products
                .into_iter()
                .find(|p| p.asin.eq_ignore_ascii_case(&asin))
        }) {
            return Ok(Some(product_to_hit(product, "detail".to_owned())));
        }
    }
    Ok(None)
}

/// Maps an Audible catalog product to a [`CatalogHit`] (the ASIN is both `product_id` and an identifier).
///
/// The catalog product carries display strings, so names are split with
/// [`records::credits_from_display`].
fn product_to_hit(p: CatalogProduct, origin: String) -> CatalogHit {
    CatalogHit {
        product_id: p.asin.clone(),
        title: p.title.unwrap_or_else(|| p.asin.clone()),
        credits: {
            let mut credits =
                records::credits_from_display(p.authors.as_deref(), CreditRole::Author);
            credits.extend(records::credits_from_display(
                p.narrators.as_deref(),
                CreditRole::Narrator,
            ));
            credits
        },
        series_entries: {
            let mut entries =
                records::series_entries(p.series.as_deref(), p.series_sequence.as_deref());
            if let Some(entry) = entries.first_mut() {
                entry.store_series_id = p.series_asin.clone();
            }
            entries
        },
        identifiers: records::identifier(IdentifierScheme::Asin, &p.asin, None)
            .into_iter()
            .chain(
                p.isbn
                    .as_deref()
                    .and_then(|isbn| records::identifier(IdentifierScheme::Isbn, isbn, None)),
            )
            .collect(),
        cover_url: p.cover_url,
        origin,
        subtitle: p.subtitle,
        description: p.description,
        publisher: p.publisher,
        length_minutes: p.length_minutes,
        published_at: p.published_at,
        categories: p.categories,
        language: p.language,
        is_abridged: p.is_abridged,
        price_cents: p.price_cents,
        currency: p.currency,
        price_label: p.price_label,
        rating_overall: p.rating_overall,
        rating_count: p.rating_count,
        is_explicit: p.is_explicit,
        sample_url: p.sample_url,
        work_kind: bookclerk_storefront::records::WorkKind::Book,
        expression_kind: bookclerk_storefront::records::ExpressionKind::Narration,
        media_kind: bookclerk_storefront::records::MediaKind::Audio,
        ..Default::default()
    }
}

/// Words of a series name, lowercased, without punctuation or a leading
/// "the" / "a" / "an".
fn series_words(name: &str) -> Vec<String> {
    let mut words: Vec<String> = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    if words
        .first()
        .is_some_and(|w| matches!(w.as_str(), "the" | "a" | "an"))
    {
        words.remove(0);
    }
    words
}

/// Whether a catalog title's series is the series a keyword search asked for.
///
/// The names match when their words are equal once a leading article and
/// trailing generic words ("series", "saga", "sequence" and the like, see
/// [`SERIES_SUFFIXES`]) are dropped, so "Dune" matches "The Dune Sequence"
/// but "Midnight" does not match "Midnight World". A title with no series
/// never matches.
///
/// # Arguments
///
/// * `product_series` - Series the catalog lists for the title, if any.
/// * `searched` - Series name the search asked for.
///
/// # Returns
///
/// `true` when the title belongs to the searched series.
fn same_series(product_series: Option<&str>, searched: &str) -> bool {
    let Some(product_series) = product_series else {
        return false;
    };
    let a = series_core(product_series);
    !a.is_empty() && a == series_core(searched)
}

/// Most results Audible's catalog endpoints return for one page.
const CATALOG_PAGE_MAX: usize = 50;

/// How many catalog rows to request for a search that wants `limit` hits.
///
/// Over-fetches twice the limit so language filtering on the host still
/// fills the page, capped at Audible's page size. A limit above the cap gets
/// the cap rather than an inverted range.
fn catalog_fetch_limit(limit: usize) -> usize {
    limit.saturating_mul(2).min(CATALOG_PAGE_MAX)
}

/// Generic trailing words a storefront appends to a series name, which do
/// not tell two series apart ("The Dune Sequence" is the "Dune" series).
const SERIES_SUFFIXES: &[&str] = &[
    "series",
    "saga",
    "sequence",
    "chronicles",
    "cycle",
    "trilogy",
    "duology",
    "quartet",
    "books",
    "novels",
    "collection",
];

/// A series name's distinguishing words: [`series_words`] without trailing
/// [`SERIES_SUFFIXES`].
fn series_core(name: &str) -> Vec<String> {
    let mut words = series_words(name);
    while words
        .last()
        .is_some_and(|w| SERIES_SUFFIXES.contains(&w.as_str()))
    {
        words.pop();
    }
    words
}

/// First author/narrator token split on comma, semicolon, or ampersand.
fn primary_person(people: Option<&str>) -> Option<&str> {
    people?
        .split([',', ';', '&'])
        .map(str::trim)
        .find(|s| !s.is_empty())
}

/// Audible storefront host suffix for a marketplace (`us` → `.com`, `uk` → `.co.uk`, …).
fn region_host_suffix(region: &str) -> &'static str {
    match normalize_region(region).as_str() {
        "uk" => ".co.uk",
        "ca" => ".ca",
        "au" => ".com.au",
        "fr" => ".fr",
        "de" => ".de",
        "jp" => ".co.jp",
        "it" => ".it",
        "in" => ".in",
        "es" => ".es",
        _ => ".com",
    }
}

/// List vs member prices from the Audible catalog `price` object.
struct DualPriced {
    /// ISO 4217 code from `list_price` / `lowest_price`, defaulting to USD.
    currency: String,
    /// Catalog list price in integer cents.
    list_cents: Option<i64>,
    /// Formatted list price (`$25.22`) when list cents are known.
    list_label: Option<String>,
    /// Member / lowest price in integer cents when distinct from list.
    member_cents: Option<i64>,
    /// Formatted member price when member cents are known.
    member_label: Option<String>,
}

/// Writes list and member prices onto a purchase hint; primary price prefers member.
fn apply_dual_price(hint: &mut SourcePurchaseHint, priced: &DualPriced) {
    hint.currency = Some(priced.currency.clone());
    hint.list_price_cents = priced.list_cents;
    hint.list_price_label = priced.list_label.clone();
    hint.member_price_cents = priced.member_cents;
    hint.member_price_label = priced.member_label.clone();
    let primary_cents = priced.member_cents.or(priced.list_cents);
    let primary_label = priced
        .member_label
        .clone()
        .or_else(|| priced.list_label.clone());
    hint.price_cents = primary_cents;
    hint.price_label = primary_label;
}

/// Fetches list/member prices from the public catalog products API; `None` on HTTP or parse failure.
/// Prices `asins` (at most [`PRICE_BATCH_ASINS`]) in one catalog request.
///
/// # Returns
///
/// The prices Audible reported, by ASIN (upper case); ASINs it did not
/// return are absent. A failed request that is not a rate limit answers an
/// empty map, since a missing price is not an error on a card.
///
/// # Errors
///
/// Returns [`SourceError::RateLimited`] when Audible answers 429, with its
/// `Retry-After` when it sent one.
async fn fetch_audible_prices(
    asins: &[String],
    region: &str,
) -> bookclerk_storefront::Result<std::collections::HashMap<String, DualPriced>> {
    let region = normalize_region(region);
    let url = format!(
        "https://api.audible{}/1.0/catalog/products",
        region_tld(&region)
    );
    fetch_audible_prices_from(&url, asins).await
}

/// [`fetch_audible_prices`] against the catalog `products` endpoint at `url`.
///
/// # Errors
///
/// As [`fetch_audible_prices`].
async fn fetch_audible_prices_from(
    url: &str,
    asins: &[String],
) -> bookclerk_storefront::Result<std::collections::HashMap<String, DualPriced>> {
    let mut out = std::collections::HashMap::new();
    let Ok(http) = public_http_client() else {
        return Ok(out);
    };
    let Ok(resp) = http
        .get(url)
        .query(&[
            ("asins", asins.join(",").as_str()),
            ("num_results", asins.len().to_string().as_str()),
            ("response_groups", "price,product_desc,product_attrs"),
        ])
        .send()
        .await
    else {
        return Ok(out);
    };
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if let Some(limited) =
        SourceError::from_rate_limit(resp.status().as_u16(), retry_after.as_deref())
    {
        return Err(limited);
    }
    if !resp.status().is_success() {
        return Ok(out);
    }
    let Ok(body) = resp.json::<Value>().await else {
        return Ok(out);
    };
    let Some(products) = body.get("products").and_then(Value::as_array) else {
        return Ok(out);
    };
    for product in products {
        let Some(asin) = product.get("asin").and_then(Value::as_str) else {
            continue;
        };
        if let Some(priced) = product.get("price").and_then(parse_audible_price_value) {
            out.insert(asin.to_ascii_uppercase(), priced);
        }
    }
    Ok(out)
}

/// Cents and currency of one Audible price node (`base` and `currency_code`).
fn audible_amount_node(node: &Value) -> Option<(i64, String)> {
    let amount = node.get("base")?.as_f64()?;
    let currency = node
        .get("currency_code")
        .and_then(Value::as_str)
        .unwrap_or("USD")
        .to_string();
    let cents = (amount * 100.0).round() as i64;
    Some((cents.max(0), currency))
}

/// Splits catalog `list_price` and `lowest_price` into list vs member cents when they differ.
fn parse_audible_price_value(price: &Value) -> Option<DualPriced> {
    let list = price.get("list_price").and_then(audible_amount_node);
    let lowest = price.get("lowest_price").and_then(|node| {
        let (cents, currency) = audible_amount_node(node)?;
        let kind = node
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        Some((cents, currency, kind))
    });

    let currency = list
        .as_ref()
        .map(|(_, c)| c.clone())
        .or_else(|| lowest.as_ref().map(|(_, c, _)| c.clone()))
        .unwrap_or_else(|| String::from("USD"));

    let list_cents = list.map(|(c, _)| c);
    let list_label = list_cents.map(|c| format_money_label(c, &currency));

    let member_cents = lowest.as_ref().and_then(|(cents, _, kind)| {
        let distinct_from_list = list_cents.is_none_or(|list| list != *cents);
        let memberish = kind == "member" || kind.is_empty() || kind == "member_price";
        if distinct_from_list && (memberish || list_cents.is_some()) {
            Some(*cents)
        } else {
            None
        }
    });
    let member_label = member_cents.map(|c| format_money_label(c, &currency));

    if list_cents.is_none() && member_cents.is_none() {
        let (cents, cur) = price
            .get("lowest_price")
            .or_else(|| price.get("list_price"))
            .and_then(audible_amount_node)?;
        return Some(DualPriced {
            currency: cur.clone(),
            list_cents: Some(cents),
            list_label: Some(format_money_label(cents, &cur)),
            member_cents: None,
            member_label: None,
        });
    }

    Some(DualPriced {
        currency,
        list_cents,
        list_label,
        member_cents,
        member_label,
    })
}

/// Formats integer cents as `$`/`£`/`€` or `{amount} CODE`; non-positive is `FREE`.
fn format_money_label(cents: i64, currency: &str) -> String {
    if cents <= 0 {
        return String::from("FREE");
    }
    let major = cents / 100;
    let minor = (cents % 100).unsigned_abs();
    match currency.to_ascii_uppercase().as_str() {
        "USD" | "" => format!("${major}.{minor:02}"),
        "GBP" => format!("£{major}.{minor:02}"),
        "EUR" => format!("€{major}.{minor:02}"),
        other => format!("{major}.{minor:02} {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        listing_records, seed_audible_asin, similar_origin, CatalogProduct, ExpandSeed,
        OrderedHits, TITLE_RECORDS,
    };

    /// A catalog product with only an ASIN and a title.
    fn titled(asin: &str, title: &str) -> CatalogProduct {
        CatalogProduct {
            asin: asin.into(),
            title: Some(title.into()),
            authors: None,
            narrators: None,
            series: None,
            series_asin: None,
            series_sequence: None,
            cover_url: None,
            subtitle: None,
            description: None,
            publisher: None,
            length_minutes: None,
            published_at: None,
            categories: None,
            genre_roots: Vec::new(),
            language: None,
            isbn: None,
            is_abridged: None,
            price_cents: None,
            currency: None,
            price_label: None,
            rating_overall: None,
            rating_count: None,
            is_explicit: None,
            sample_url: None,
        }
    }

    /// A title search's products become records in search order: one per
    /// ASIN (upper-cased), none without a valid ASIN, at most
    /// [`TITLE_RECORDS`], each carrying its ASIN as an identifier and its
    /// narrators, so the host can tell recordings apart.
    #[test]
    fn title_search_products_become_distinct_records() {
        let mut narrated = titled("b0d1kv49fl", "The Sunlit Man");
        narrated.authors = Some("Brandon Sanderson".into());
        narrated.narrators = Some("Joe Jameson".into());
        let mut products = vec![
            narrated,
            titled("B0D1KV49FL", "The Sunlit Man"),
            titled("not-an-asin", "Broken"),
            titled("B0CAST0001", "The Sunlit Man (Dramatized)"),
        ];
        products.extend((0..TITLE_RECORDS).map(|i| titled(&format!("B0FILL{i:04}"), "Filler")));
        let records = listing_records(products);
        assert_eq!(records.len(), TITLE_RECORDS);
        assert_eq!(records[0].product_id, "B0D1KV49FL");
        assert_eq!(records[1].product_id, "B0CAST0001");
        assert_eq!(records[0].asin().as_deref(), Some("B0D1KV49FL"));
        assert_eq!(records[0].narrators().as_deref(), Some("Joe Jameson"));
        assert_eq!(records[0].origin, "purchase search");
    }

    /// A narrator's other title is kept only when it shares a top-level
    /// genre with the seed: a thriller narrator's picture book is not.
    #[test]
    fn narrator_titles_need_a_genre_root_in_common() {
        let roots = |names: &[&str]| names.iter().map(|n| (*n).to_string()).collect::<Vec<_>>();
        let seed = roots(&["Mystery, Thriller & Suspense", "Literature & Fiction"]);
        assert!(shares_genre_root(&roots(&["literature & fiction"]), &seed));
        assert!(!shares_genre_root(
            &roots(&["Children's Audiobooks"]),
            &seed
        ));
        assert!(!shares_genre_root(&[], &seed));
        assert!(!shares_genre_root(&roots(&["Literature & Fiction"]), &[]));
    }

    #[test]
    fn expansion_hits_keep_the_first_source_and_its_order() {
        let mut hits = OrderedHits::default();
        hits.push(titled("B0SERIES01", "Book Two"), || "series".into());
        hits.push(titled("B0AUTHOR01", "Other Book"), || "author".into());
        hits.push(titled("b0series01", "Book Two"), || "similar".into());
        hits.push(titled("  ", "No ASIN"), || "similar".into());
        hits.push(titled("B0SIMILAR1", "Neighbour"), || {
            similar_origin("The Great Hunt")
        });
        assert_eq!(hits.len(), 3);
        let kept: Vec<(String, String)> = hits
            .into_hits(2)
            .into_iter()
            .map(|h| (h.product_id, h.origin))
            .collect();
        assert_eq!(
            kept,
            [
                ("B0SERIES01".to_string(), "series".to_string()),
                ("B0AUTHOR01".to_string(), "author".to_string()),
            ]
        );
    }

    /// The Discover "Because you wishlisted" shelf claims "related" origins.
    #[test]
    fn similar_titles_are_filed_as_related() {
        assert!(similar_origin("Dune").contains("related"));
    }

    #[test]
    fn the_seed_asin_comes_from_its_asin_or_an_audible_product_id() {
        let mut seed = ExpandSeed {
            source: "libro".into(),
            product_id: "9780000000001".into(),
            title: "The Great Hunt".into(),
            authors: None,
            narrators: None,
            series: None,
            series_asin: None,
            asin: Some(" B002UP1LXS ".into()),
            isbn: None,
            region: "us".into(),
        };
        assert_eq!(seed_audible_asin(&seed), Some("B002UP1LXS"));
        seed.asin = None;
        assert_eq!(seed_audible_asin(&seed), None);
        seed.product_id = "B002UP1LXS".into();
        assert_eq!(seed_audible_asin(&seed), None);
        seed.source = "Audible".into();
        assert_eq!(seed_audible_asin(&seed), Some("B002UP1LXS"));
    }

    /// Empty requests answer without any catalog HTTP.
    #[tokio::test]
    async fn empty_requests_answer_without_calling_the_store() {
        let search = super::CatalogSearchOpts {
            query: "   ".into(),
            limit: 10,
            ..super::CatalogSearchOpts::default()
        };
        assert!(super::search_catalog(&search).await.unwrap().is_empty());
        let seed = super::ExpandSeed {
            source: "audible".into(),
            product_id: "B0BOOK0001".into(),
            title: "Seed".into(),
            authors: Some("Author".into()),
            narrators: None,
            series: None,
            series_asin: None,
            asin: Some("B0BOOK0001".into()),
            isbn: None,
            region: "us".into(),
        };
        assert!(super::expand_candidates(&seed, 0).await.unwrap().is_empty());
    }

    /// Searches over-fetch twice the limit, capped at Audible's page size,
    /// and a limit above the cap gets the cap instead of panicking.
    #[test]
    fn fetch_limit_doubles_up_to_the_page_size() {
        assert_eq!(super::catalog_fetch_limit(10), 20);
        assert_eq!(super::catalog_fetch_limit(25), 50);
        assert_eq!(super::catalog_fetch_limit(30), 50);
        assert_eq!(super::catalog_fetch_limit(60), 50);
        assert_eq!(super::catalog_fetch_limit(usize::MAX), 50);
    }

    /// Loose keyword matches from another (or no) series are not stamped
    /// into the searched series.
    #[test]
    fn series_search_keeps_only_the_searched_series() {
        assert!(same_series(Some("The Dune Sequence"), "Dune"));
        assert!(same_series(Some("Dune"), "The Dune Sequence"));
        assert!(same_series(
            Some("the midnight world"),
            "The Midnight World"
        ));
        assert!(!same_series(None, "The Dune Sequence"));
        assert!(!same_series(
            Some("The Midnight Library"),
            "The Midnight World"
        ));
        assert!(same_series(Some("Dune Chronicles"), "Dune"));
        assert!(!same_series(Some("Midnight World"), "Midnight"));
        assert!(!same_series(Some("Midnight"), "The Midnight World"));
        assert!(!same_series(Some("Series"), "Saga"));
        assert!(!same_series(Some("Dunesday"), "Dune"));
        assert!(!same_series(Some("The"), "The"));
    }

    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// One `products` entry priced at list 20.00, member 12.00.
    fn product(asin: &str) -> Value {
        json!({ "asin": asin, "price": {
            "list_price": { "base": 20.0, "currency_code": "USD" },
            "lowest_price": { "base": 12.0, "currency_code": "USD", "type": "member" }
        }})
    }

    /// A seed with an ASIN (`narrators` set), as Discover hands one over.
    fn narrated_seed(asin: Option<&str>) -> ExpandSeed {
        ExpandSeed {
            source: "audible".into(),
            product_id: String::new(),
            title: "The Silent Patient".into(),
            authors: None,
            narrators: Some("Jack Hawkins".into()),
            series: None,
            series_asin: None,
            asin: asin.map(Into::into),
            isbn: None,
            region: "us".into(),
        }
    }

    /// The seed's genres come from the catalog's ladders for its ASIN.
    #[tokio::test]
    async fn seed_genres_come_from_its_catalog_ladders() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("asins", "B07D8QRT1W"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "products": [
                    { "asin": "B000000009" },
                    { "asin": "b07d8qrt1w", "category_ladders": [
                        { "ladder": [{ "name": "Mystery, Thriller & Suspense" }] }
                    ]}
                ]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let http = public_http_client().unwrap();
        let roots =
            seed_genre_roots(&http, &server.uri(), &narrated_seed(Some("B07D8QRT1W"))).await;
        assert_eq!(roots, vec!["Mystery, Thriller & Suspense".to_string()]);
    }

    /// Without known genres the narrator step answers nothing and never
    /// searches: no ASIN, a failed lookup, or a title filed under none.
    #[tokio::test]
    async fn a_seed_without_genres_gets_no_narrator_titles() {
        let http = public_http_client().unwrap();
        let unused = MockServer::start().await;
        let none = narrator_matches(
            &http,
            "us",
            &unused.uri(),
            &narrated_seed(None),
            "Jack Hawkins",
        )
        .await
        .unwrap();
        assert!(none.is_empty());
        assert!(unused.received_requests().await.unwrap().is_empty());

        let failing = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&failing)
            .await;
        let seed = narrated_seed(Some("B07D8QRT1W"));
        let failed = narrator_matches(&http, "us", &failing.uri(), &seed, "Jack Hawkins")
            .await
            .unwrap();
        assert!(failed.is_empty());

        let unfiled = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "products": [{ "asin": "B07D8QRT1W" }] })),
            )
            .mount(&unfiled)
            .await;
        let found = narrator_matches(&http, "us", &unfiled.uri(), &seed, "Jack Hawkins")
            .await
            .unwrap();
        assert!(found.is_empty());
    }

    #[tokio::test]
    async fn one_request_prices_several_asins() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("asins", "B000000001,B000000002"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({ "products": [product("b000000001"), { "asin": "B000000002" }] }),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let prices = fetch_audible_prices_from(
            &server.uri(),
            &["B000000001".to_string(), "B000000002".to_string()],
        )
        .await
        .unwrap();
        assert_eq!(prices.len(), 1, "the unpriced ASIN is absent");
        let priced = &prices["B000000001"];
        assert_eq!(priced.list_cents, Some(2000));
        assert_eq!(priced.member_cents, Some(1200));
    }

    #[tokio::test]
    async fn a_429_keeps_its_wait_and_other_failures_are_no_price() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "45"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let asins = ["B000000001".to_string()];
        let limited = fetch_audible_prices_from(&server.uri(), &asins).await;
        assert!(
            matches!(limited, Err(SourceError::RateLimited(Some(w))) if w.as_secs() == 45),
            "{:?}",
            limited.map(|m| m.len())
        );
        let failed = fetch_audible_prices_from(&server.uri(), &asins)
            .await
            .unwrap();
        assert!(failed.is_empty());
    }

    #[tokio::test]
    async fn a_batch_without_prices_resolves_links_and_asks_nothing() {
        let opts = PurchaseHintOpts {
            asin: Some("B000000001".into()),
            region: "us".into(),
            with_price: false,
            ..PurchaseHintOpts::default()
        };
        let hints = purchase_hints(&[opts]).await.unwrap();
        assert_eq!(hints[0].as_ref().unwrap().product_id, "B000000001");
    }

    /// With no search time left, a title without an ASIN is left untried
    /// while one with a known ASIN still resolves.
    #[tokio::test]
    async fn known_asins_resolve_before_the_search_budget() {
        let searched = PurchaseHintOpts {
            title: Some("A Title Only".into()),
            region: "us".into(),
            with_price: false,
            ..PurchaseHintOpts::default()
        };
        let known = PurchaseHintOpts {
            asin: Some("B000000001".into()),
            region: "us".into(),
            with_price: false,
            ..PurchaseHintOpts::default()
        };
        let answers = purchase_answers_within(&[searched, known], std::time::Duration::ZERO)
            .await
            .unwrap();
        assert!(answers[0].hint.is_none());
        assert!(answers[0].records.is_empty());
        assert_eq!(answers[1].hint.as_ref().unwrap().product_id, "B000000001");
        assert!(
            answers[1].records.is_empty(),
            "a known ASIN is not searched"
        );
    }

    /// Each query is priced in its own marketplace; an ASIN two queries in
    /// one region share is asked for once, and unpriced queries not at all.
    /// A title search's records are priced with its pick.
    #[test]
    fn prices_are_asked_per_region() {
        let hint = |asin: &str| Found {
            hint: Some(SourcePurchaseHint {
                product_id: asin.into(),
                ..SourcePurchaseHint::default()
            }),
            records: Vec::new(),
        };
        let searched = Found {
            hint: Some(SourcePurchaseHint {
                product_id: "B4".into(),
                ..SourcePurchaseHint::default()
            }),
            records: ["B4", "B5", "B1"]
                .into_iter()
                .map(|asin| CatalogHit {
                    product_id: asin.into(),
                    ..CatalogHit::default()
                })
                .collect(),
        };
        let query = |region: &str, with_price: bool| PurchaseHintOpts {
            region: region.into(),
            with_price,
            ..PurchaseHintOpts::default()
        };
        let found = [
            hint("B1"),
            hint("B2"),
            hint("B1"),
            hint("B3"),
            Found::default(),
            searched,
        ];
        let opts = [
            query("us", true),
            query("uk", true),
            query("us", true),
            query("us", false),
            query("us", true),
            query("us", true),
        ];
        let by_region = asins_by_region(&found, &opts);
        assert_eq!(by_region.len(), 2);
        assert_eq!(by_region[&normalize_region("us")], ["B1", "B4", "B5"]);
        assert_eq!(by_region[&normalize_region("uk")], ["B2"]);
    }

    #[test]
    fn audible_price_json_member_and_list() {
        let price = json!({
            "credit_price": 1.0,
            "list_price": {
                "base": 25.219999313354492,
                "currency_code": "USD",
                "type": "list"
            },
            "lowest_price": {
                "base": 14.95,
                "currency_code": "USD",
                "type": "member"
            }
        });
        let priced = parse_audible_price_value(&price).unwrap();
        assert_eq!(priced.member_cents, Some(1495));
        assert_eq!(priced.member_label.as_deref(), Some("$14.95"));
        assert_eq!(priced.list_cents, Some(2522));
        assert_eq!(priced.list_label.as_deref(), Some("$25.22"));
    }
}
