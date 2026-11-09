//! Public Audible catalog helpers for Discover (`search_catalog` / expand / purchase).
//!
//! Uses [`bookclerk_audible_catalog`] public catalog HTTP — no account required.

use bookclerk_audible_catalog::{
    is_valid_asin, normalize_region, public_http_client, region_tld, search_catalog_asins,
    search_catalog_by_genre_name, search_catalog_by_narrator, search_catalog_by_series_asin,
    search_catalog_keywords, search_catalog_products, search_catalog_products_paged,
    search_catalog_storefront, CatalogProduct,
};
use bookclerk_storefront::records::{self, CreditRole, IdentifierScheme};
use bookclerk_storefront::{
    CatalogHit, CatalogSearchField, CatalogSearchOpts, ExpandSeed, PurchaseHintOpts, SourceError,
    SourcePurchaseHint,
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
    if q.is_empty() || opts.limit == 0 {
        return Ok(Vec::new());
    }
    let http =
        public_http_client().map_err(|e| bookclerk_storefront::SourceError::api(e.to_string()))?;
    let region = normalize_region(&opts.region);
    let fetch_limit = catalog_fetch_limit(opts.limit);
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

    Ok(products
        .into_iter()
        .filter(|p| !p.asin.trim().is_empty())
        .take(opts.limit)
        .map(|p| product_to_hit(p, String::from("search")))
        .collect())
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
    let mut by_asin = std::collections::HashMap::new();

    let push = |by_asin: &mut std::collections::HashMap<String, CatalogHit>,
                p: CatalogProduct,
                origin: String| {
        if p.asin.trim().is_empty() {
            return;
        }
        by_asin
            .entry(p.asin.to_ascii_uppercase())
            .or_insert_with(|| product_to_hit(p, origin));
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

    // Narrator search when narrators present (noisier; still useful for Audible seeds).
    if by_asin.len() < limit {
        if let Some(narrator) = primary_person(seed.narrators.as_deref()) {
            match search_catalog_by_narrator(&http, &region, narrator).await {
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

    // When the seed is an Audible ASIN (or product_id looks like one) and we still
    // have room, keyword-search the title for near-neighbors.
    if by_asin.len() < limit {
        let seed_asin = seed
            .asin
            .as_deref()
            .map(str::trim)
            .filter(|s| is_valid_asin(s))
            .or_else(|| {
                let pid = seed.product_id.trim();
                if seed.source.eq_ignore_ascii_case("audible") && is_valid_asin(pid) {
                    Some(pid)
                } else {
                    None
                }
            });
        if seed_asin.is_some() {
            let title = seed.title.trim();
            if !title.is_empty() {
                match search_catalog_products(&http, &region, title, None, Some(title)).await {
                    Ok(products) => {
                        for p in products {
                            // Skip the seed itself.
                            if seed_asin.is_some_and(|a| p.asin.eq_ignore_ascii_case(a)) {
                                continue;
                            }
                            push(
                                &mut by_asin,
                                p,
                                format!("audible related search (“{}”)", seed.title),
                            );
                        }
                    }
                    Err(err) => {
                        tracing::debug!(error = %err, "audible related title search failed");
                    }
                }
            }
        }
    }

    let mut hits: Vec<_> = by_asin.into_values().collect();
    hits.truncate(limit);
    Ok(hits)
}

/// Resolve a purchase / catalog URL (optionally with live price).
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn purchase_hint(
    opts: &PurchaseHintOpts,
) -> bookclerk_storefront::Result<Option<SourcePurchaseHint>> {
    let region = normalize_region(&opts.region);
    let title = opts
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let author = opts
        .authors
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let mut asin = opts
        .asin
        .as_deref()
        .map(str::trim)
        .filter(|s| is_valid_asin(s))
        .map(|s| s.to_ascii_uppercase())
        .or_else(|| {
            opts.product_id
                .as_deref()
                .map(str::trim)
                .filter(|s| is_valid_asin(s))
                .map(|s| s.to_ascii_uppercase())
        });

    if asin.is_none() {
        let http = match public_http_client() {
            Ok(c) => c,
            Err(_) => return Ok(None),
        };
        if let Some(t) = title {
            match search_catalog_asins(&http, &region, t, author).await {
                Ok(asins) => asin = asins.into_iter().next().map(|a| a.to_ascii_uppercase()),
                Err(err) => {
                    tracing::debug!(error = %err, "audible catalog asin search failed");
                }
            }
            if asin.is_none() {
                if let Some(isbn) = opts
                    .isbn
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    match search_catalog_keywords(&http, &region, isbn).await {
                        Ok(asins) => {
                            asin = asins.into_iter().next().map(|a| a.to_ascii_uppercase())
                        }
                        Err(err) => {
                            tracing::debug!(error = %err, "audible isbn keyword search failed");
                        }
                    }
                }
            }
        } else if let Some(isbn) = opts
            .isbn
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            match search_catalog_keywords(&http, &region, isbn).await {
                Ok(asins) => asin = asins.into_iter().next().map(|a| a.to_ascii_uppercase()),
                Err(err) => {
                    tracing::debug!(error = %err, "audible isbn keyword search failed");
                }
            }
        }
    }

    let Some(asin) = asin else {
        return Ok(None);
    };

    let mut hint = SourcePurchaseHint {
        product_id: asin.clone(),
        title: title.map(str::to_string),
        url: Some(format!(
            "https://www.audible{}/pd/{}",
            region_host_suffix(&region),
            asin
        )),
        ..Default::default()
    };

    if opts.with_price {
        if let Some(priced) = fetch_audible_prices(std::slice::from_ref(&asin), &region)
            .await?
            .remove(&asin)
        {
            apply_dual_price(&mut hint, &priced);
        }
    }

    Ok(Some(hint))
}

/// Most ASINs the public catalog prices in one `products` request.
pub const PRICE_BATCH_ASINS: usize = 50;

/// Resolves purchase hints for several titles, pricing them together.
///
/// Each title's ASIN is resolved as [`purchase_hint`] does (a search when
/// the query has none), then every priced title's ASIN goes into one
/// catalog request per [`PRICE_BATCH_ASINS`], so a page of cards costs one
/// price request instead of one per card.
///
/// # Errors
///
/// Returns [`SourceError::RateLimited`] when Audible asks to slow down;
/// a resolution that fails otherwise answers `None` for that title.
pub async fn purchase_hints(
    opts: &[PurchaseHintOpts],
) -> bookclerk_storefront::Result<Vec<Option<SourcePurchaseHint>>> {
    let mut hints = Vec::with_capacity(opts.len());
    for query in opts {
        let unpriced = PurchaseHintOpts {
            with_price: false,
            ..query.clone()
        };
        hints.push(purchase_hint(&unpriced).await?);
    }
    let asins: Vec<String> = hints
        .iter()
        .zip(opts)
        .filter(|(hint, query)| query.with_price && hint.is_some())
        .filter_map(|(hint, _)| hint.as_ref().map(|h| h.product_id.clone()))
        .collect();
    if asins.is_empty() {
        return Ok(hints);
    }
    let region = normalize_region(&opts[0].region);
    let mut prices = std::collections::HashMap::new();
    for chunk in asins.chunks(PRICE_BATCH_ASINS) {
        prices.extend(fetch_audible_prices(chunk, &region).await?);
    }
    for (hint, query) in hints.iter_mut().zip(opts) {
        if let (true, Some(h)) = (query.with_price, hint.as_mut()) {
            if let Some(priced) = prices.get(&h.product_id) {
                apply_dual_price(h, priced);
            }
        }
    }
    Ok(hints)
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
        price_cents: p.price_cents,
        currency: p.currency,
        price_label: p.price_label,
        rating_overall: p.rating_overall,
        rating_count: p.rating_count,
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
    let mut out = std::collections::HashMap::new();
    let Ok(http) = public_http_client() else {
        return Ok(out);
    };
    let region = normalize_region(region);
    let url = format!(
        "https://api.audible{}/1.0/catalog/products",
        region_tld(&region)
    );
    let Ok(resp) = http
        .get(&url)
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
