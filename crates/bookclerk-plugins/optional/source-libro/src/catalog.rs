//! Libro.fm catalog search, detail, related titles and purchase links.
//!
//! Every call goes to the Android app's signed-out catalog API
//! (`/api/vN/explore/…`, the same prefix as [`LIBRARY_PATH`]). Requests carry
//! an honest Bookclerk user agent and the app's `X-LibroFm-AppVer` header,
//! which is part of the app's API contract (the signed-in client sends the
//! same value) and without which `explore/audiobook_details` answers `404`.
//! No `Authorization`, device or OS headers are sent. Libro.fm's load
//! balancer refuses automated requests to its website pages, so this module
//! never fetches storefront HTML and never presents itself as a browser.
//!
//! Search rows are lean (ISBN, title, authors, narrators, cover);
//! `explore/audiobook_details` adds description, genres, runtime, publisher,
//! series, abridged, language, list price and related titles.
//!
//! This crate intentionally does **not** depend on `bookclerk-enrich`.

use bookclerk_plugin_sdk::http::{Client as HttpClient, RequestBuilder};
use bookclerk_storefront::records::{self, CreditRole, IdentifierScheme};
use bookclerk_storefront::{
    CatalogHit, CatalogSearchOpts, ExpandSeed, PurchaseHintOpts, SourcePurchaseHint,
};
use serde::Deserialize;
use serde_json::Value;

use crate::client::{APP_VER, LIBRARY_PATH};

/// User agent for signed-out catalog calls: names Bookclerk and its site so
/// Libro.fm can identify (and contact) the client.
pub const CATALOG_USER_AGENT: &str = concat!(
    "bookclerk/",
    env!("CARGO_PKG_VERSION"),
    " (+https://bookclerk.app)"
);

/// Public storefront origin used for the product links shown to people.
const STOREFRONT_ORIGIN: &str = "https://libro.fm";

/// The app API prefix (`/api/vN/`) taken from [`LIBRARY_PATH`], so the APK
/// probe's version bump moves catalog calls along with the library call.
fn api_prefix() -> &'static str {
    LIBRARY_PATH.strip_suffix("library").unwrap_or("/api/v12/")
}

/// Path of the app's catalog search (`GET …/explore/search?q=&page=`).
#[must_use]
pub fn explore_search_path() -> String {
    format!("{}explore/search", api_prefix())
}

/// GET builder for one catalog API path under `base_url`, with Bookclerk's
/// own user agent, a JSON `Accept` header and the app's `X-LibroFm-AppVer`.
fn catalog_get(http: &HttpClient, base_url: &str, path_and_query: &str) -> RequestBuilder {
    http.get(format!(
        "{}{path_and_query}",
        base_url.trim_end_matches('/')
    ))
    .header("user-agent", CATALOG_USER_AGENT)
    .header("accept", "application/json")
    .header("x-librofm-appver", APP_VER)
}

/// Catalog search → [`CatalogHit`]s keyed by ISBN.
///
/// # Arguments
///
/// * `base_url` - Libro.fm API origin (the plugin's `base_url` setting).
/// * `opts` - Query, page and hit limit.
///
/// # Errors
///
/// Returns an error when Libro.fm answers with a non-success status or a body
/// that is not the search envelope.
pub async fn search_catalog(
    base_url: &str,
    opts: &CatalogSearchOpts,
) -> bookclerk_storefront::Result<Vec<CatalogHit>> {
    let q = opts.query.trim();
    if q.is_empty() || opts.limit == 0 {
        return Ok(Vec::new());
    }
    let http = HttpClient::new();
    let hits = libro_search_hits(&http, base_url, q, opts.limit, opts.page.max(1)).await?;
    Ok(hits
        .into_iter()
        .map(|mut h| {
            h.origin = String::from("search");
            h.decode_html_entities()
        })
        .collect())
}

/// Catalog record for one ISBN (or ISBN-leading slug).
///
/// Prefers the app's `explore/audiobook_details` (description, genres,
/// runtime, publisher, series, abridged, language, list price). When that
/// misses or fails, falls back to the catalog search row whose ISBN matches
/// exactly, so a fuzzy hit never stands in for the requested book.
///
/// # Arguments
///
/// * `base_url` - Libro.fm API origin (the plugin's `base_url` setting).
/// * `product_id` - ISBN-10/13 or `{isbn}-{slug}`; anything else returns `None`.
///
/// # Errors
///
/// Returns an error when the details call misses and the fallback search fails.
pub async fn catalog_detail(
    base_url: &str,
    product_id: &str,
) -> bookclerk_storefront::Result<Option<CatalogHit>> {
    let Some(isbn) = isbn_or_slug(product_id).and_then(isbn_digits) else {
        return Ok(None);
    };
    let http = HttpClient::new();
    if let Some(details) = libro_details_logged(&http, base_url, &isbn).await {
        if let Some(hit) = details.audiobook {
            return Ok(Some(hit.decode_html_entities()));
        }
    }
    Ok(libro_isbn_hit(&http, base_url, &isbn)
        .await?
        .map(CatalogHit::decode_html_entities))
}

/// Related candidates for Discover.
///
/// A Libro.fm seed with an ISBN gets the app's related titles from
/// `explore/audiobook_details`. Otherwise, or when that returns none, a
/// Libro.fm seed gets more titles by its first author and a seed from
/// another store gets a title-and-author search. The seed itself is never
/// returned.
///
/// # Arguments
///
/// * `base_url` - Libro.fm API origin.
/// * `seed` - Title the candidates relate to.
/// * `limit` - Most candidates to return.
///
/// # Errors
///
/// Returns an error when the fallback catalog search fails.
pub async fn expand_candidates(
    base_url: &str,
    seed: &ExpandSeed,
    limit: usize,
) -> bookclerk_storefront::Result<Vec<CatalogHit>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let seed_isbn = seed
        .isbn
        .as_deref()
        .and_then(isbn_digits)
        .or_else(|| isbn_digits(seed.product_id.trim()));
    let from_libro = seed.source.eq_ignore_ascii_case("libro");
    let http = HttpClient::new();
    if let (true, Some(isbn)) = (from_libro, seed_isbn.as_deref()) {
        if let Some(details) = libro_details_logged(&http, base_url, isbn).await {
            let origin = format!("libro related to “{}”", seed.title);
            let related: Vec<CatalogHit> = details
                .related
                .into_iter()
                .filter(|h| h.isbn().as_deref() != Some(isbn))
                .take(limit)
                .map(|mut h| {
                    h.origin.clone_from(&origin);
                    h.decode_html_entities()
                })
                .collect();
            if !related.is_empty() {
                return Ok(related);
            }
        }
    }
    let (query, origin) = match primary_author(seed.authors.as_deref()) {
        Some(author) if from_libro => (author.to_string(), format!("libro more by {author}")),
        Some(author) => (
            format!("{} {author}", seed.title.trim()),
            format!("libro catalog search (“{}”)", seed.title),
        ),
        None if from_libro => return Ok(Vec::new()),
        None => (
            seed.title.trim().to_string(),
            format!("libro catalog search (“{}”)", seed.title),
        ),
    };
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    // One extra row so dropping the seed still leaves `limit` candidates.
    let hits = libro_search_hits(&http, base_url, &query, limit.saturating_add(1), 1).await?;
    Ok(hits
        .into_iter()
        .filter(|h| seed_isbn.is_none() || h.isbn() != seed_isbn)
        .take(limit)
        .map(|mut h| {
            h.origin.clone_from(&origin);
            h.decode_html_entities()
        })
        .collect())
}

/// Resolve a Libro.fm product link for a title, by ISBN first and then by
/// title and author.
///
/// With `opts.with_price`, the list price comes from
/// `explore/audiobook_details` (`purchase_info.price`, USD). An edition that
/// details report as not for sale is never offered, priced or not: the title
/// match then moves on to the next search row.
///
/// # Arguments
///
/// * `base_url` - Libro.fm API origin.
/// * `opts` - ISBN, product id, title and authors to match.
///
/// # Errors
///
/// Currently never: a failed ISBN lookup or title search is logged and
/// treated as no match. The `Result` keeps the `ContentSource` signature.
pub async fn purchase_hint(
    base_url: &str,
    opts: &PurchaseHintOpts,
) -> bookclerk_storefront::Result<Option<SourcePurchaseHint>> {
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
    let http = HttpClient::new();

    // Only treat product_id as an ISBN when it looks like one, never an
    // Audible ASIN leaked from a cross-store call. An ISBN alone is not
    // membership: Audible exclusives often carry an ISBN Libro.fm does not
    // sell, so the catalog must return that exact ISBN.
    let isbn = opts
        .isbn
        .as_deref()
        .and_then(isbn_or_slug)
        .or_else(|| opts.product_id.as_deref().and_then(isbn_or_slug))
        .and_then(isbn_digits);
    // An ISBN the details call reports as not for sale; the title search
    // must not offer that same edition.
    let mut not_for_sale: Option<String> = None;
    if let Some(isbn) = isbn {
        if let Some(details) = libro_details_logged(&http, base_url, &isbn).await {
            match (details.audiobook, details.for_sale) {
                (Some(hit), true) => {
                    return Ok(Some(hint_from_hit(hit, title, opts.with_price)));
                }
                (_, false) => not_for_sale = Some(comparable_isbn(&isbn)),
                (None, true) => {}
            }
        } else {
            match libro_isbn_hit(&http, base_url, &isbn).await {
                Ok(Some(hit)) => return Ok(Some(hint_from_hit(hit, title, false))),
                Ok(None) => {}
                Err(err) => tracing::debug!(%isbn, error = %err, "libro ISBN search failed"),
            }
        }
        tracing::debug!(%isbn, "libro ISBN not for sale; trying title search");
    }

    let Some(t) = title else {
        return Ok(None);
    };
    let q = match author {
        Some(a) => format!("{t} {a}"),
        None => t.to_string(),
    };
    let hits = match libro_search_hits(&http, base_url, &q, TITLE_HINT_ROWS, 1).await {
        Ok(hits) => hits,
        Err(err) => {
            tracing::debug!(error = %err, "libro title search failed");
            return Ok(None);
        }
    };
    // Each candidate's own details decide whether it is for sale, whether
    // or not the caller wants a price; an unavailable edition yields to the
    // next row. A candidate whose details miss is offered as it is.
    for hit in hits {
        let Some(isbn) = hit.isbn() else {
            return Ok(Some(hint_from_hit(hit, None, opts.with_price)));
        };
        if not_for_sale.as_deref() == Some(comparable_isbn(&isbn).as_str()) {
            continue;
        }
        match libro_details_logged(&http, base_url, &isbn).await {
            Some(details) if !details.for_sale => {
                tracing::debug!(%isbn, "libro title match not for sale; trying the next");
            }
            Some(LibroDetails {
                audiobook: Some(full),
                ..
            }) => return Ok(Some(hint_from_hit(full, None, opts.with_price))),
            Some(_) | None => return Ok(Some(hint_from_hit(hit, None, opts.with_price))),
        }
    }
    Ok(None)
}

/// Search rows the title fallback of [`purchase_hint`] reads, so an edition
/// that is not for sale can yield to the next match.
const TITLE_HINT_ROWS: usize = 5;

/// Builds a purchase hint pointing at the hit's storefront page, with its
/// price when `with_price` is set and the hit carries one.
fn hint_from_hit(hit: CatalogHit, title: Option<&str>, with_price: bool) -> SourcePurchaseHint {
    let url = hit
        .url
        .clone()
        .or_else(|| Some(format!("{STOREFRONT_ORIGIN}/audiobooks/{}", hit.product_id)));
    let mut hint = SourcePurchaseHint {
        title: title
            .map(str::to_string)
            .or_else(|| Some(hit.title.clone()).filter(|s| !s.is_empty())),
        product_id: hit.product_id,
        url,
        ..Default::default()
    };
    if with_price && hit.price_cents.is_some() {
        hint.price_cents = hit.price_cents;
        hint.price_label.clone_from(&hit.price_label);
        hint.list_price_cents = hit.price_cents;
        hint.list_price_label = hit.price_label;
        hint.currency = hit.currency;
    }
    hint
}

/// One title from `explore/audiobook_details`.
#[derive(Debug, Default)]
pub struct LibroDetails {
    /// The title itself, with its list price; `None` when the payload had no
    /// usable audiobook object.
    pub audiobook: Option<CatalogHit>,
    /// Related titles the app shows under it (lean rows, like search).
    pub related: Vec<CatalogHit>,
    /// Whether Libro.fm sells the title (`purchase_info.for_sale`); `true`
    /// when the payload does not say.
    pub for_sale: bool,
}

/// `explore/audiobook_details` envelope.
#[derive(Debug, Deserialize)]
struct LibroDetailsResponse {
    /// Details payload; absent on an empty body.
    #[serde(default)]
    data: Option<LibroDetailsData>,
}

/// Primary audiobook, related titles and purchase facts.
#[derive(Debug, Deserialize)]
struct LibroDetailsData {
    /// Raw primary audiobook object for [`parse_libro_book`].
    #[serde(default)]
    audiobook: Option<Value>,
    /// Raw related-title objects.
    #[serde(default)]
    related_audiobooks: Vec<Value>,
    /// Purchase facts; `price` is the list price as a decimal string in USD.
    #[serde(default)]
    purchase_info: Option<Value>,
}

/// Parses one `explore/audiobook_details` body.
///
/// Sans-IO so the unit tests drive the same code as the network path. The
/// list price (`purchase_info.price`) is copied onto the audiobook hit.
///
/// # Errors
///
/// Returns an error when `body` is not JSON shaped like the details envelope.
pub fn parse_details(body: &[u8]) -> bookclerk_storefront::Result<LibroDetails> {
    let parsed: LibroDetailsResponse = serde_json::from_slice(body)
        .map_err(|e| bookclerk_storefront::SourceError::api(format!("libro details parse: {e}")))?;
    let Some(data) = parsed.data else {
        return Ok(LibroDetails {
            for_sale: true,
            ..Default::default()
        });
    };
    let purchase = data.purchase_info.as_ref();
    let for_sale = purchase
        .and_then(|p| p.get("for_sale"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let audiobook = data
        .audiobook
        .as_ref()
        .and_then(parse_libro_book)
        .map(|mut h| {
            h.origin = String::from("libro details");
            let price = purchase.and_then(|p| p.get("price")).and_then(usd_cents);
            if let Some(cents) = price.filter(|c| *c > 0) {
                h.price_cents = Some(cents);
                h.currency = Some(String::from("USD"));
                h.price_label = Some(format_money_label(cents, "USD"));
            }
            h
        });
    let related = data
        .related_audiobooks
        .iter()
        .filter_map(parse_libro_book)
        .collect();
    Ok(LibroDetails {
        audiobook,
        related,
        for_sale,
    })
}

/// Fetches `explore/audiobook_details/{isbn}`; `Ok(None)` on `404`.
///
/// # Errors
///
/// Returns an error on transport failure, another non-success status, or an
/// unparseable body.
async fn libro_details(
    http: &HttpClient,
    base_url: &str,
    isbn: &str,
) -> bookclerk_storefront::Result<Option<LibroDetails>> {
    let path = format!("{}explore/audiobook_details/{isbn}", api_prefix());
    let resp = catalog_get(http, base_url, &path)
        .send()
        .await
        .map_err(|e| bookclerk_storefront::SourceError::api(e.to_string()))?;
    if resp.status().as_u16() == 404 {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(bookclerk_storefront::SourceError::api(format!(
            "libro audiobook details HTTP {}",
            resp.status()
        )));
    }
    let body = resp
        .bytes()
        .await
        .map_err(|e| bookclerk_storefront::SourceError::api(e.to_string()))?;
    parse_details(&body).map(Some)
}

/// [`libro_details`] with failures logged and treated as a miss.
async fn libro_details_logged(
    http: &HttpClient,
    base_url: &str,
    isbn: &str,
) -> Option<LibroDetails> {
    match libro_details(http, base_url, isbn).await {
        Ok(details) => details,
        Err(err) => {
            tracing::debug!(isbn, error = %err, "libro audiobook details failed");
            None
        }
    }
}

/// Catalog search for `isbn`, keeping only the row with exactly that ISBN.
async fn libro_isbn_hit(
    http: &HttpClient,
    base_url: &str,
    isbn: &str,
) -> bookclerk_storefront::Result<Option<CatalogHit>> {
    let hits = libro_search_hits(http, base_url, isbn, 24, 1).await?;
    Ok(hits
        .into_iter()
        .find(|h| h.isbn().as_deref().map(comparable_isbn) == Some(comparable_isbn(isbn)))
        .map(|mut h| {
            h.origin = String::from("libro catalog");
            h
        }))
}

/// Explore search envelope (`audiobook_collection`).
#[derive(Debug, Deserialize)]
struct LibroExploreSearch {
    /// Paged audiobook list; absent when the query missed.
    #[serde(default)]
    audiobook_collection: Option<LibroCollection>,
}

/// Explore collection wrapper around raw audiobook JSON objects.
#[derive(Debug, Deserialize)]
struct LibroCollection {
    /// Raw audiobook objects; each is parsed independently so one bad row cannot fail the page.
    #[serde(default)]
    audiobooks: Vec<Value>,
}

/// One page of the app's catalog search, at most `limit` hits.
///
/// # Errors
///
/// Returns an error on transport failure, non-success HTTP, or a body that is
/// not the search envelope.
async fn libro_search_hits(
    http: &HttpClient,
    base_url: &str,
    query: &str,
    limit: usize,
    page: u32,
) -> bookclerk_storefront::Result<Vec<CatalogHit>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let path = format!(
        "{}?page={}&q={}",
        explore_search_path(),
        page.max(1),
        urlencode_minimal(query)
    );
    let resp = catalog_get(http, base_url, &path)
        .send()
        .await
        .map_err(|e| bookclerk_storefront::SourceError::api(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(bookclerk_storefront::SourceError::api(format!(
            "libro catalog search HTTP {}",
            resp.status()
        )));
    }
    let body = resp
        .bytes()
        .await
        .map_err(|e| bookclerk_storefront::SourceError::api(e.to_string()))?;
    parse_search_page(&body, limit)
}

/// Parses one catalog search response body into at most `limit` hits.
///
/// Sans-IO so the replay test and fuzz target drive the same code as the
/// network path.
///
/// # Errors
///
/// Returns an error when `body` is not JSON shaped like the search envelope.
pub fn parse_search_page(body: &[u8], limit: usize) -> bookclerk_storefront::Result<Vec<CatalogHit>> {
    let parsed: LibroExploreSearch = serde_json::from_slice(body)
        .map_err(|e| bookclerk_storefront::SourceError::api(format!("libro search parse: {e}")))?;
    Ok(parsed
        .audiobook_collection
        .map(|c| c.audiobooks)
        .unwrap_or_default()
        .iter()
        .filter_map(parse_libro_book)
        .take(limit)
        .collect())
}

/// An ISBN in the form [`CatalogHit::isbn`] gives (an ISBN-10 becomes its
/// ISBN-13), so a requested ISBN compares equal to the same book's search
/// row however either side wrote it; a value that is not a valid ISBN is
/// compared as given.
fn comparable_isbn(isbn: &str) -> String {
    records::normalize_identifier(IdentifierScheme::Isbn, isbn).unwrap_or_else(|| isbn.to_string())
}

/// Digits of an ISBN or ISBN-leading slug (`9781…-title` → `9781…`), hyphens removed.
fn isbn_digits(raw: &str) -> Option<String> {
    let s = raw.trim();
    if let Some((head, _)) = s.split_once('-') {
        if is_isbn_digits(head) {
            return Some(head.to_ascii_uppercase());
        }
    }
    let compact: String = s.chars().filter(|c| *c != '-').collect();
    is_isbn_digits(&compact).then(|| compact.to_ascii_uppercase())
}

/// Makes a scheme-relative URL (`//covers.libro.fm/…`, as the app API sends
/// covers) absolute over HTTPS; other values pass through.
fn absolute_url(raw: &str) -> String {
    match raw.strip_prefix("//") {
        Some(rest) => format!("https://{rest}"),
        None => raw.to_string(),
    }
}

/// Maps a Libro catalog API object to a [`CatalogHit`]; `None` without ISBN and title.
fn parse_libro_book(v: &Value) -> Option<CatalogHit> {
    let isbn = v
        .get("isbn")
        .and_then(|x| {
            x.as_str()
                .map(str::to_string)
                .or_else(|| x.as_i64().map(|n| n.to_string()))
                .or_else(|| x.as_u64().map(|n| n.to_string()))
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    let title = v
        .get("title")
        .or_else(|| v.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    let mut credits = records::credits_from_names(
        first_person_names(&[v.get("authors"), v.get("author")]),
        CreditRole::Author,
    );
    credits.extend(records::credits_from_names(
        first_person_names(&[
            v.get("narrators"),
            v.get("readBy"),
            v.get("audiobook_info")
                .and_then(|info| info.get("narrators")),
        ]),
        CreditRole::Narrator,
    ));
    let series = v.get("series").and_then(|s| {
        s.as_str()
            .map(str::to_string)
            .or_else(|| s.get("name").and_then(Value::as_str).map(str::to_string))
    });
    let series_index = series_num_to_index(v.get("series_num")).or_else(|| {
        // HTML-derived related payloads sometimes use `series_number`.
        series_num_to_index(v.get("series_number"))
    });
    let cover_url = v
        .get("cover_url")
        .or_else(|| v.get("coverUrl"))
        .or_else(|| v.get("image_url"))
        .or_else(|| v.get("cover"))
        .or_else(|| v.get("image"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(absolute_url);
    let subtitle = v
        .get("subtitle")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let description = v
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let publisher = parse_named_string(v.get("publisher"));
    let published_at = v
        .get("publication_date")
        .or_else(|| v.get("datePublished"))
        .or_else(|| v.get("published_at"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let categories = parse_genres(v.get("genres")).or_else(|| parse_genres(v.get("genre")));
    let language = v
        .get("language")
        .or_else(|| v.get("inLanguage"))
        .or_else(|| {
            v.get("audiobook_info")
                .and_then(|i| i.get("audio_language"))
        })
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let length_minutes = length_minutes_from_value(v);
    let slug = v
        .get("slug")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let product_id = slug.unwrap_or(isbn.as_str()).to_string();
    let url = v
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| Some(format!("{STOREFRONT_ORIGIN}/audiobooks/{product_id}")));
    let is_abridged = parse_abridged_flag(v.get("abridged"));
    Some(CatalogHit {
        product_id,
        identifiers: records::identifier(IdentifierScheme::Isbn, &isbn, None)
            .into_iter()
            .collect(),
        title,
        credits,
        series_entries: records::series_entries(series.as_deref(), series_index.as_deref()),
        cover_url,
        url,
        origin: String::from("libro"),
        subtitle,
        description,
        publisher,
        length_minutes,
        published_at,
        categories,
        language,
        is_abridged,
        ..Default::default()
    })
}

/// Interprets bool, 0/1, or `abridged`/`unabridged` strings as an abridged flag.
fn parse_abridged_flag(v: Option<&Value>) -> Option<bool> {
    match v? {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_i64().map(|i| i != 0),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "abridged" => Some(true),
            "false" | "0" | "no" | "unabridged" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Joins author/narrator names from a string, `{ name }`, or array of either.
fn parse_person_names(v: Option<&Value>) -> Vec<String> {
    let Some(v) = v else {
        return Vec::new();
    };
    if let Some(s) = v.as_str() {
        return records::split_display_names(s);
    }
    if let Some(arr) = v.as_array() {
        return arr
            .iter()
            .filter_map(|x| {
                x.as_str()
                    .or_else(|| x.get("name").and_then(Value::as_str))
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            })
            .collect();
    }
    v.get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| vec![s.to_string()])
        .unwrap_or_default()
}

/// Names from the first of `fields` that lists any.
fn first_person_names(fields: &[Option<&Value>]) -> Vec<String> {
    fields
        .iter()
        .map(|field| parse_person_names(*field))
        .find(|names| !names.is_empty())
        .unwrap_or_default()
}

/// Reads a display string from a JSON string or `{ "name": … }` object.
fn parse_named_string(v: Option<&Value>) -> Option<String> {
    let v = v?;
    v.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            v.get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
}

/// Joins genre names from a string or array of strings/`{ name }` objects.
fn parse_genres(v: Option<&Value>) -> Option<String> {
    let v = v?;
    if let Some(s) = v.as_str() {
        let t = s.trim();
        return (!t.is_empty()).then(|| t.to_string());
    }
    let arr = v.as_array()?;
    let names: Vec<&str> = arr
        .iter()
        .filter_map(|x| {
            x.as_str()
                .or_else(|| x.get("name").and_then(Value::as_str))
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .collect();
    if names.is_empty() {
        None
    } else {
        Some(names.join(", "))
    }
}

/// Runtime in whole minutes from `length_minutes`, duration seconds, or ISO-8601 `PT…`.
fn length_minutes_from_value(v: &Value) -> Option<i64> {
    if let Some(mins) = v
        .get("length_minutes")
        .and_then(Value::as_i64)
        .filter(|&n| n > 0)
    {
        return Some(mins);
    }
    let info = v.get("audiobook_info");
    if let Some(secs) = info
        .and_then(|i| i.get("duration"))
        .and_then(|d| d.as_u64().or_else(|| d.as_i64().map(|n| n as u64)))
        .filter(|&n| n > 0)
    {
        return Some((secs / 60) as i64);
    }
    if let Some(secs) = v
        .get("duration")
        .and_then(|d| d.as_u64().or_else(|| d.as_i64().map(|n| n as u64)))
        .filter(|&n| n > 0)
    {
        // Library API uses seconds; ISO-8601 strings are handled below.
        if secs > 1000 {
            return Some((secs / 60) as i64);
        }
    }
    v.get("duration")
        .and_then(Value::as_str)
        .and_then(parse_iso8601_duration_minutes)
}

/// Parse schema.org durations like `PT11H19M21S` → whole minutes.
fn parse_iso8601_duration_minutes(raw: &str) -> Option<i64> {
    let s = raw.trim();
    let s = s.strip_prefix("PT").or_else(|| s.strip_prefix("pt"))?;
    let mut hours = 0i64;
    let mut minutes = 0i64;
    let mut seconds = 0i64;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let n: i64 = num.parse().ok()?;
        num.clear();
        match c {
            'H' | 'h' => hours = n,
            'M' | 'm' => minutes = n,
            'S' | 's' => seconds = n,
            _ => return None,
        }
    }
    if !num.is_empty() {
        return None;
    }
    let total_secs = hours * 3600 + minutes * 60 + seconds;
    (total_secs > 0).then_some(total_secs / 60)
}

/// US cents for a list price the app API sends as a decimal string such as
/// `"33.59"` (or as a JSON number); `None` for anything else, including
/// negative amounts, more than two decimals and values that overflow.
fn usd_cents(price: &Value) -> Option<i64> {
    let text = match price {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let (whole, frac) = text.split_once('.').unwrap_or((text.as_str(), ""));
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty() || !digits(whole) || frac.len() > 2 || !digits(frac) {
        return None;
    }
    let frac_cents: i64 = match frac.len() {
        0 => 0,
        1 => frac.parse::<i64>().ok()?.checked_mul(10)?,
        _ => frac.parse().ok()?,
    };
    whole
        .parse::<i64>()
        .ok()?
        .checked_mul(100)?
        .checked_add(frac_cents)
}

/// ISBN-10/13, or an ISBN-leading storefront slug (`9781…-ashes-of-man`).
fn isbn_or_slug(raw: &str) -> Option<&str> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Some((head, rest)) = s.split_once('-') {
        if is_isbn_digits(head) && !rest.is_empty() {
            return Some(s);
        }
    }
    let compact: String = s.chars().filter(|c| *c != '-').collect();
    if is_isbn_digits(&compact) {
        Some(s)
    } else {
        None
    }
}

/// True for ISBN-10 (digits plus optional trailing X) or ISBN-13 digit strings.
fn is_isbn_digits(s: &str) -> bool {
    let b = s.as_bytes();
    match b.len() {
        13 => b.iter().all(|c| c.is_ascii_digit()),
        10 => {
            b[..9].iter().all(|c| c.is_ascii_digit())
                && (b[9].is_ascii_digit() || b[9].eq_ignore_ascii_case(&b'X'))
        }
        _ => false,
    }
}

/// Stringifies a series position from int, float, or non-empty string.
fn series_num_to_index(v: Option<&Value>) -> Option<String> {
    let v = v?;
    if let Some(n) = v.as_i64() {
        return Some(n.to_string());
    }
    if let Some(n) = v.as_u64() {
        return Some(n.to_string());
    }
    if let Some(n) = v.as_f64() {
        if n.fract() == 0.0 {
            return Some((n as i64).to_string());
        }
        return Some(n.to_string());
    }
    v.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
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

/// First author token split on comma, semicolon, or ampersand.
fn primary_author(authors: Option<&str>) -> Option<&str> {
    authors?
        .split([',', ';', '&'])
        .map(str::trim)
        .find(|s| !s.is_empty())
}

/// application/x-www-form-urlencoded encoding (spaces as `+`) for explore query strings.
fn urlencode_minimal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
            b' ' => out.push('+'),
            _ => {
                use std::fmt::Write;
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn abridged_flags_accept_bools_numbers_and_words() {
        assert_eq!(parse_abridged_flag(Some(&json!(true))), Some(true));
        assert_eq!(parse_abridged_flag(Some(&json!(0))), Some(false));
        assert_eq!(parse_abridged_flag(Some(&json!(2))), Some(true));
        assert_eq!(parse_abridged_flag(Some(&json!(" Abridged "))), Some(true));
        assert_eq!(parse_abridged_flag(Some(&json!("unabridged"))), Some(false));
        assert_eq!(parse_abridged_flag(Some(&json!("maybe"))), None);
        assert_eq!(parse_abridged_flag(Some(&json!(1.5))), None);
        assert_eq!(parse_abridged_flag(Some(&json!([]))), None);
        assert_eq!(parse_abridged_flag(None), None);
    }

    #[test]
    fn usd_cents_reads_only_plain_decimal_prices() {
        assert_eq!(usd_cents(&json!("33.59")), Some(3359));
        assert_eq!(usd_cents(&json!(" 9.5 ")), Some(950));
        assert_eq!(usd_cents(&json!("12")), Some(1200));
        assert_eq!(usd_cents(&json!(14.99)), Some(1499));
        assert_eq!(usd_cents(&json!(0)), Some(0));
        for bad in [
            json!("-1.00"),
            json!("1.234"),
            json!(".99"),
            json!("1e3"),
            json!("$9.99"),
            json!("99999999999999999999"),
            json!(1e300),
            json!(true),
            json!(null),
        ] {
            assert_eq!(usd_cents(&bad), None, "{bad}");
        }
    }

    #[test]
    fn row_prices_are_ignored_outside_purchase_info() {
        let body = json!({"data": {
            "audiobook": {"isbn": "1", "title": "T", "price": "5.00", "offers": {"lowPrice": 3}},
            "related_audiobooks": [{"isbn": "2", "title": "U", "price": "4.00"}]
        }});
        let details = parse_details(body.to_string().as_bytes()).unwrap();
        assert_eq!(details.audiobook.unwrap().price_cents, None);
        assert_eq!(details.related[0].price_cents, None);
    }

    #[test]
    fn catalog_calls_follow_the_library_path_version() {
        // A LIBRARY_PATH reshaped by a probe bump must not silently pin the
        // catalog calls to the fallback version.
        assert!(LIBRARY_PATH.ends_with("library"), "{LIBRARY_PATH}");
        assert_eq!(
            explore_search_path(),
            format!("{}explore/search", LIBRARY_PATH.trim_end_matches("library"))
        );
    }

    #[test]
    fn isbn_or_slug_accepts_isbn_and_slug() {
        assert_eq!(isbn_or_slug("9781980053446"), Some("9781980053446"));
        assert_eq!(
            isbn_or_slug("9781980053446-ashes-of-man"),
            Some("9781980053446-ashes-of-man")
        );
        assert!(isbn_or_slug("B09ABCDEFG").is_none());
        assert!(isbn_or_slug("chirp-uuid-here").is_none());
    }

    #[test]
    fn parse_libro_related_object() {
        let v = json!({
            "isbn": "9781234567890",
            "title": "Next Title",
            "authors": [{"name": "Ada Author"}],
            "series": {"name": "Test Series"},
            "series_num": 3
        });
        let c = parse_libro_book(&v).unwrap();
        assert_eq!(c.isbn().as_deref(), Some("9781234567890"));
        assert_eq!(c.authors().as_deref(), Some("Ada Author"));
        assert_eq!(c.series().as_deref(), Some("Test Series"));
        assert_eq!(c.series_index().as_deref(), Some("3"));
    }

    #[test]
    fn parse_libro_explore_audiobook_rich() {
        let v = json!({
            "isbn": 9781446462164u64,
            "title": "Caligula",
            "authors": ["Douglas Jackson"],
            "subtitle": "A Novel",
            "publisher": "Random House",
            "publication_date": "2011-03-03",
            "description": "<p>Ancient Rome.</p>",
            "cover_url": "https://covers.libro.fm/9781446462164_400.jpg",
            "genres": [{"name": "Historical Fiction"}],
            "audiobook_info": {
                "narrators": ["Russell Boulter"],
                "duration": 40761
            },
            "series": "Rufus",
            "series_num": "1"
        });
        let c = parse_libro_book(&v).unwrap();
        assert_eq!(c.isbn().as_deref(), Some("9781446462164"));
        assert_eq!(c.narrators().as_deref(), Some("Russell Boulter"));
        assert_eq!(c.length_minutes, Some(679));
        assert_eq!(c.publisher.as_deref(), Some("Random House"));
        assert_eq!(c.categories.as_deref(), Some("Historical Fiction"));
        assert!(c.description.as_deref().unwrap().contains("Ancient Rome"));
        assert_eq!(c.series().as_deref(), Some("Rufus"));
        assert_eq!(c.series_index().as_deref(), Some("1"));
    }

    #[test]
    fn parse_iso8601_duration() {
        assert_eq!(parse_iso8601_duration_minutes("PT11H19M21S"), Some(679));
        assert_eq!(parse_iso8601_duration_minutes("PT90M"), Some(90));
        assert_eq!(parse_iso8601_duration_minutes("PT1H"), Some(60));
    }

    #[test]
    fn money_label_formatting() {
        assert_eq!(format_money_label(299, "USD"), "$2.99");
        assert_eq!(format_money_label(0, "USD"), "FREE");
        assert_eq!(format_money_label(1250, "GBP"), "£12.50");
    }

    #[test]
    fn search_page_maps_app_api_rows() {
        let body = json!({
            "user_info": {"signed_in": false},
            "audiobook_collection": {
                "page": 1,
                "total_pages": 3,
                "audiobooks": [
                    {
                        "title": "Dune",
                        "isbn": 9781427201447u64,
                        "authors": ["Frank Herbert"],
                        "cover_url": "//covers.libro.fm/9781427201447_1120.jpg",
                        "catalog_info": {"bookseller_pick": true},
                        "audiobook_info": {"narrators": ["Scott Brick", "Simon Vance"]}
                    },
                    {"title": "No ISBN"},
                    {"title": "Second", "isbn": "9780000000002", "authors": ["B"]}
                ]
            }
        })
        .to_string();
        let hits = parse_search_page(body.as_bytes(), 10).unwrap();
        assert_eq!(hits.len(), 2);
        let dune = &hits[0];
        assert_eq!(dune.isbn().as_deref(), Some("9781427201447"));
        assert_eq!(dune.title, "Dune");
        assert_eq!(dune.authors().as_deref(), Some("Frank Herbert"));
        assert_eq!(
            dune.narrators().as_deref(),
            Some("Scott Brick, Simon Vance")
        );
        assert_eq!(
            dune.cover_url.as_deref(),
            Some("https://covers.libro.fm/9781427201447_1120.jpg")
        );
        assert_eq!(
            dune.url.as_deref(),
            Some("https://libro.fm/audiobooks/9781427201447")
        );
        assert_eq!(parse_search_page(body.as_bytes(), 1).unwrap().len(), 1);
    }

    #[test]
    fn search_page_tolerates_a_missing_collection_and_rejects_non_json() {
        assert!(parse_search_page(br#"{"user_info":{}}"#, 5)
            .unwrap()
            .is_empty());
        assert!(parse_search_page(b"<html>403 Forbidden</html>", 5).is_err());
    }

    #[test]
    fn isbn_digits_strips_slugs_and_hyphens() {
        assert_eq!(
            isbn_digits("9781980053446-ashes-of-man").as_deref(),
            Some("9781980053446")
        );
        assert_eq!(
            isbn_digits("978-1-4272-0144-7").as_deref(),
            Some("9781427201447")
        );
        assert_eq!(isbn_digits("030774970x").as_deref(), Some("030774970X"));
        assert!(isbn_digits("B09ABCDEFG").is_none());
    }

    #[test]
    fn absolute_url_upgrades_scheme_relative_covers() {
        assert_eq!(
            absolute_url("//covers.libro.fm/x.jpg"),
            "https://covers.libro.fm/x.jpg"
        );
        assert_eq!(absolute_url("https://a.example/x"), "https://a.example/x");
    }

    #[test]
    fn api_prefix_follows_the_library_path() {
        assert!(LIBRARY_PATH.starts_with(api_prefix()));
        assert!(explore_search_path().starts_with("/api/v"));
        assert!(explore_search_path().ends_with("/explore/search"));
        assert!(CATALOG_USER_AGENT.starts_with("bookclerk/"));
    }

    /// A trimmed live `explore/audiobook_details` body (Circe, 2026-11-06).
    fn circe_details() -> serde_json::Value {
        json!({
            "user_info": {"signed_in": false},
            "data": {
                "audiobook": {
                    "title": "Circe",
                    "isbn": 9781478975311u64,
                    "description": "<p>A bold reimagining.</p>",
                    "abridged": false,
                    "series": null,
                    "series_num": null,
                    "genres": [
                        {"id": 4, "name": "Classics", "html_name": "classics"},
                        {"id": 5, "name": "Fiction", "html_name": "fiction-literature"}
                    ],
                    "audiobook_info": {
                        "narrators": ["Perdita Weeks"],
                        "duration": 43705,
                        "audio_language": "eng"
                    },
                    "authors": ["Madeline Miller"],
                    "cover_url": "//covers.libro.fm/9781478975311_1120.jpg",
                    "publisher": "Hachette Audio",
                    "publication_date": "2018-04-10T00:00:00.000Z"
                },
                "related_audiobooks": [
                    {"title": "The Song of Achilles", "isbn": 9780062115577u64, "authors": ["Madeline Miller"]},
                    {"title": "No ISBN"}
                ],
                "purchase_info": {"owned": false, "price": "33.59", "for_sale": true}
            }
        })
    }

    #[test]
    fn details_map_every_field_and_the_list_price() {
        let body = circe_details().to_string();
        let d = parse_details(body.as_bytes()).unwrap();
        assert!(d.for_sale);
        let c = d.audiobook.unwrap();
        assert_eq!(c.title, "Circe");
        assert_eq!(c.publisher.as_deref(), Some("Hachette Audio"));
        assert_eq!(c.categories.as_deref(), Some("Classics, Fiction"));
        assert_eq!(c.length_minutes, Some(728));
        assert_eq!(c.is_abridged, Some(false));
        assert_eq!(c.language.as_deref(), Some("eng"));
        assert_eq!(c.narrators().as_deref(), Some("Perdita Weeks"));
        assert_eq!(c.price_cents, Some(3359));
        assert_eq!(c.price_label.as_deref(), Some("$33.59"));
        assert_eq!(c.currency.as_deref(), Some("USD"));
        assert_eq!(c.origin, "libro details");
        assert_eq!(d.related.len(), 1);
        assert_eq!(d.related[0].title, "The Song of Achilles");
    }

    #[test]
    fn details_tolerate_missing_parts_and_reject_non_json() {
        let empty = parse_details(br#"{"user_info":{}}"#).unwrap();
        assert!(empty.audiobook.is_none() && empty.related.is_empty() && empty.for_sale);
        let not_for_sale = json!({"data": {"purchase_info": {"for_sale": false, "price": "0"}}});
        assert!(
            !parse_details(not_for_sale.to_string().as_bytes())
                .unwrap()
                .for_sale
        );
        assert!(parse_details(b"").is_err());
    }
}
