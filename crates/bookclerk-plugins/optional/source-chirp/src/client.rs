//! GraphQL client for Chirp's Mockingjay Android API.

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{ChirpError, Result};

/// Android Mockingjay GraphQL endpoint.
pub const DEFAULT_GRAPHQL_URL: &str = "https://api.chirpbooks.com/api/graphql";

/// Android Mockingjay User-Agent expected by Chirp's GraphQL edge.
const USER_AGENT_VALUE: &str = "okhttp/4.12.0 Chirp/Bookclerk";

/// `signIn` mutation that returns user id, API token, optional web token, and email.
const SIGN_IN: &str = r#"
mutation signIn($email: String!, $password: String!) {
  signIn(email: $email, password: $password) {
    user { id token webToken email }
  }
}
"#;

/// Paginated `currentUserAudiobooks` query (title A–Z, Chirp-audio capable).
const LIBRARY_PAGE: &str = r#"
query AndroidCurrentUserAudiobooks($page: Int!, $pageSize: Int!) {
  currentUserAudiobooks(
    page: $page
    pageSize: $pageSize
    sort: TITLE_A_Z
    clientCapabilities: [CHIRP_AUDIO]
  ) {
    id
    archived
    playable
    audiobook {
      id
      displayTitle
      subTitle
      displayAuthors
      displayNarrators
      durationMs
      abridged
      publisher
      releasedOn
      coverUrl: optimizedCoverUrl(format: "f_jpg", quality: "q_auto:eco", sizePixels: 700)
      description
    }
  }
}
"#;

/// Single-title query including track media URLs and chapter/part offsets.
const SINGLE_AUDIOBOOK: &str = r#"
query AndroidSingleAudiobook($id: ID!) {
  audiobook(id: $id, clientCapabilities: [CHIRP_AUDIO]) {
    id
    displayTitle
    coverUrl: optimizedCoverUrl(format: "f_jpg", quality: "q_auto:eco", sizePixels: 700)
    tracks {
      id
      mediaUrl
      chapterNumber
      partNumber
      durationMs
      offsetFromBookStartMs
      displayName
    }
    displayAuthors
    displayNarrators
    durationMs
    abridged
    publisher
    releasedOn
    subTitle
  }
}
"#;

// Catalog queries below also select `language`, `abridged`, and `promotedTags`
// (Chirp genre chips) so Discover can filter language and show genres.

/// Catalog search query used by Discover (language, abridged, genre chips, series).
const CATALOG_SEARCH: &str = r#"
query BookclerkCatalogSearch($query: String!, $page: Int!, $pageSize: Int!) {
  audiobooks(query: $query, page: $page, pageSize: $pageSize) {
    totalCount
    objects {
      ... on Audiobook {
        id
        displayTitle
        subTitle
        displayAuthors
        displayNarrators
        url
        coverUrl: optimizedCoverUrl(format: "f_jpg", quality: "q_auto:eco", sizePixels: 200)
        durationMs
        description
        publisher
        releasedOn
        language
        abridged
        promotedTags { displayName }
        seriesAudiobook {
          number
          displayNumber
          series { id name slug }
        }
      }
    }
  }
}
"#;

/// Related-titles query plus the seed title's series membership.
const RELATED_AUDIOBOOKS: &str = r#"
query BookclerkRelatedAudiobooks($id: ID!) {
  audiobook(id: $id) {
    id
    displayTitle
    displayAuthors
    displayNarrators
    relatedAudiobooks {
      id
      displayTitle
      displayAuthors
      displayNarrators
      url
      coverUrl: optimizedCoverUrl(format: "f_jpg", quality: "q_auto:eco", sizePixels: 200)
      language
      abridged
      promotedTags { displayName }
      seriesAudiobook {
        number
        displayNumber
        series { id name slug }
      }
    }
    seriesAudiobook {
      number
      displayNumber
      series { id name slug }
    }
  }
}
"#;

/// Series-by-slug query listing promotable audiobooks in that series.
const SERIES_AUDIOBOOKS: &str = r#"
query BookclerkSeriesAudiobooks($slug: String!) {
  series(slug: $slug) {
    id
    name
    slug
    paginatedAudiobooks(hideUnpromotable: true) {
      totalCount
      objects {
        ... on Audiobook {
          id
          displayTitle
          displayAuthors
          displayNarrators
          url
          coverUrl: optimizedCoverUrl(format: "f_jpg", quality: "q_auto:eco", sizePixels: 200)
          language
          abridged
          promotedTags { displayName }
          seriesAudiobook {
            number
            displayNumber
            series { id name slug }
          }
        }
      }
    }
  }
}
"#;

/// Author-by-slug query returning the storefront's summary audiobook list.
const AUTHOR_SUMMARY: &str = r#"
query BookclerkAuthorSummary($slug: String!) {
  author(slug: $slug) {
    id
    name
    slug
    summaryAudiobooks {
      id
      displayTitle
      displayAuthors
      displayNarrators
      url
      coverUrl: optimizedCoverUrl(format: "f_jpg", quality: "q_auto:eco", sizePixels: 200)
      language
      abridged
      promotedTags { displayName }
      seriesAudiobook {
        number
        displayNumber
        series { id name slug }
      }
    }
  }
}
"#;

/// Typeahead query returning matching audiobooks and authors for a search term.
const TYPEAHEAD: &str = r#"
query BookclerkTypeahead($searchTerm: String!) {
  typeahead(searchTerm: $searchTerm) {
    audiobooks {
      id
      displayTitle
      displayAuthors
      displayNarrators
      url
      coverUrl: optimizedCoverUrl(format: "f_jpg", quality: "q_auto:eco", sizePixels: 200)
      language
      abridged
      promotedTags { displayName }
    }
    authors { id name slug }
  }
}
"#;

/// Storefront top-deals query used by Discover deal shelves.
const TOP_DEALS: &str = r#"
query BookclerkTopDeals($count: Int!) {
  topDealsAudiobooks(count: $count) {
    id
    displayTitle
    displayAuthors
    displayNarrators
    url
    coverUrl: optimizedCoverUrl(format: "f_jpg", quality: "q_auto:eco", sizePixels: 200)
    language
    abridged
    promotedTags { displayName }
    seriesAudiobook {
      number
      displayNumber
      series { id name slug }
    }
  }
}
"#;

/// Storefront free-deals query used by Discover deal shelves.
const FREE_DEALS: &str = r#"
query BookclerkFreeDeals {
  freeDeals {
    id
    displayTitle
    displayAuthors
    displayNarrators
    url
    coverUrl: optimizedCoverUrl(format: "f_jpg", quality: "q_auto:eco", sizePixels: 200)
    language
    abridged
    promotedTags { displayName }
    seriesAudiobook {
      number
      displayNumber
      series { id name slug }
    }
  }
}
"#;

/// Live `currentProduct` pricing query (discount, listing price, purchase URL).
const AUDIOBOOK_PRICING: &str = r#"
query BookclerkAudiobookPricing($id: ID!) {
  audiobook(id: $id) {
    id
    url
    currentProduct {
      id
      discountPrice
      discountedPriceCents
      listingPrice
      isFreeListing
      hotDeal
      purchaseUrl
      salableInCurrentCountry
      savingsPercent
      showListingPrice
    }
  }
}
"#;

/// Authenticated Chirp GraphQL helper.
#[derive(Debug, Clone)]
pub struct ChirpClient {
    /// Shared HTTP client used for GraphQL and binary downloads.
    http: reqwest::Client,
    /// Mockingjay GraphQL endpoint (defaults to [`DEFAULT_GRAPHQL_URL`]).
    graphql_url: String,
    /// Bearer token from `signIn`; `None` until login or `with_token`.
    access_token: Option<String>,
}

impl Default for ChirpClient {
    fn default() -> Self {
        Self::new(DEFAULT_GRAPHQL_URL)
    }
}

impl ChirpClient {
    /// Constructs a new instance with default or provided parameters.
    #[must_use]
    pub fn new(graphql_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            graphql_url: graphql_url.into(),
            access_token: None,
        }
    }

    /// With HTTP.
    #[must_use]
    pub fn with_http(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    /// With token.
    #[must_use]
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.access_token = Some(token.into());
        self
    }

    /// Access token.
    #[must_use]
    pub fn access_token(&self) -> Option<&str> {
        self.access_token.as_deref()
    }

    /// Graphql URL.
    #[must_use]
    pub fn graphql_url(&self) -> &str {
        &self.graphql_url
    }

    /// JSON GraphQL headers plus an optional `Authorization: Bearer` token.
    fn headers(&self, with_auth: bool) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            reqwest::header::ACCEPT,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
        if with_auth {
            let token = self
                .access_token
                .as_deref()
                .ok_or_else(|| ChirpError::auth("not logged in"))?;
            let value = format!("Bearer {token}");
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&value)
                    .map_err(|e| ChirpError::auth(format!("invalid token header: {e}")))?,
            );
        }
        Ok(headers)
    }

    /// Posts a GraphQL operation and returns the `data` object; HTTP/GraphQL errors fail.
    async fn graphql(
        &self,
        operation_name: &str,
        query: &str,
        variables: Value,
        with_auth: bool,
    ) -> Result<Value> {
        let body = json!({
            "operationName": operation_name,
            "query": query,
            "variables": variables,
        });
        let resp = self
            .http
            .post(&self.graphql_url)
            .headers(self.headers(with_auth)?)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(ChirpError::api(format!(
                "GraphQL HTTP {status}: {}",
                truncate(&text, 300)
            )));
        }
        let parsed: Value = serde_json::from_str(&text)?;
        if let Some(errors) = parsed.get("errors").and_then(|e| e.as_array()) {
            if !errors.is_empty() {
                let msg = errors
                    .iter()
                    .filter_map(|e| e.get("message").and_then(|m| m.as_str()))
                    .collect::<Vec<_>>()
                    .join("; ");
                // Some operations return data:null with errors (e.g. bad password).
                if parsed
                    .get("data")
                    .and_then(|d| d.as_object())
                    .is_none_or(|o| o.values().all(|v| v.is_null()))
                {
                    return Err(ChirpError::api(msg));
                }
                tracing::debug!(%msg, "GraphQL returned errors with partial data");
            }
        }
        Ok(parsed)
    }

    /// GraphQL `signIn` mutation.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn login(&mut self, email: &str, password: &str) -> Result<SignInUser> {
        let parsed = self
            .graphql(
                "signIn",
                SIGN_IN,
                json!({ "email": email, "password": password }),
                false,
            )
            .await
            .map_err(|e| match e {
                ChirpError::Api(m) if m.to_ascii_lowercase().contains("invalid") => {
                    ChirpError::auth(m)
                }
                other => other,
            })?;
        let user = parsed
            .pointer("/data/signIn/user")
            .cloned()
            .ok_or_else(|| ChirpError::auth("signIn response missing user"))?;
        let user: SignInUser = serde_json::from_value(user)?;
        if user.token.is_empty() {
            return Err(ChirpError::auth("signIn response missing token"));
        }
        self.access_token = Some(user.token.clone());
        Ok(user)
    }

    /// Paginated owned library (`currentUserAudiobooks`).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn library_page(&self, page: u32, page_size: u32) -> Result<Vec<LibraryItem>> {
        let parsed = self
            .graphql(
                "AndroidCurrentUserAudiobooks",
                LIBRARY_PAGE,
                json!({ "page": page, "pageSize": page_size }),
                true,
            )
            .await?;
        let items = parsed
            .pointer("/data/currentUserAudiobooks")
            .cloned()
            .unwrap_or(Value::Array(Vec::new()));
        Ok(serde_json::from_value(items)?)
    }

    /// Full audiobook with track `mediaUrl`s.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn audiobook(&self, id: &str) -> Result<Audiobook> {
        let parsed = self
            .graphql(
                "AndroidSingleAudiobook",
                SINGLE_AUDIOBOOK,
                json!({ "id": id }),
                true,
            )
            .await?;
        let book = parsed
            .pointer("/data/audiobook")
            .cloned()
            .ok_or_else(|| ChirpError::api(format!("audiobook {id} not found")))?;
        if book.is_null() {
            return Err(ChirpError::api(format!("audiobook {id} not found")));
        }
        Ok(serde_json::from_value(book)?)
    }

    /// Live deal / list pricing for a Chirp audiobook (public GraphQL).
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn audiobook_pricing(&self, id: &str) -> Result<Option<ChirpProductPricing>> {
        let id = id.trim();
        if id.is_empty() {
            return Ok(None);
        }
        let parsed = self
            .graphql(
                "BookclerkAudiobookPricing",
                AUDIOBOOK_PRICING,
                json!({ "id": id }),
                false,
            )
            .await?;
        let Some(product) = parsed.pointer("/data/audiobook/currentProduct") else {
            return Ok(None);
        };
        if product.is_null() {
            return Ok(None);
        }
        Ok(serde_json::from_value(product.clone()).ok())
    }

    /// Catalog search (`audiobooks(query:)`) — no auth required.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn search_catalog(
        &self,
        query: &str,
        page: u32,
        page_size: u32,
    ) -> Result<Vec<CatalogAudiobook>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let parsed = self
            .graphql(
                "BookclerkCatalogSearch",
                CATALOG_SEARCH,
                json!({ "query": query, "page": page, "pageSize": page_size }),
                false,
            )
            .await?;
        Ok(parse_paginated_audiobooks(
            parsed.pointer("/data/audiobooks/objects"),
        ))
    }

    /// Related titles for a Chirp audiobook id — no auth required.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn related_audiobooks(&self, id: &str) -> Result<RelatedCatalog> {
        let parsed = self
            .graphql(
                "BookclerkRelatedAudiobooks",
                RELATED_AUDIOBOOKS,
                json!({ "id": id }),
                false,
            )
            .await?;
        let related =
            parse_paginated_audiobooks(parsed.pointer("/data/audiobook/relatedAudiobooks"));
        let series = parsed
            .pointer("/data/audiobook/seriesAudiobook/series")
            .and_then(|v| serde_json::from_value::<CatalogSeries>(v.clone()).ok());
        Ok(RelatedCatalog { related, series })
    }

    /// Series catalog by slug (`mistborn-audiobooks`, …) — no auth required.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn series_catalog(&self, slug: &str) -> Result<Option<SeriesCatalog>> {
        let slug = slug.trim();
        if slug.is_empty() {
            return Ok(None);
        }
        let parsed = self
            .graphql(
                "BookclerkSeriesAudiobooks",
                SERIES_AUDIOBOOKS,
                json!({ "slug": slug }),
                false,
            )
            .await?;
        let Some(series_v) = parsed.pointer("/data/series") else {
            return Ok(None);
        };
        if series_v.is_null() {
            return Ok(None);
        }
        let series: CatalogSeries = serde_json::from_value(series_v.clone())?;
        let audiobooks =
            parse_paginated_audiobooks(parsed.pointer("/data/series/paginatedAudiobooks/objects"));
        Ok(Some(SeriesCatalog { series, audiobooks }))
    }

    /// Author summary titles by slug — no auth required.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn author_summary(&self, slug: &str) -> Result<Option<AuthorCatalog>> {
        let slug = slug.trim();
        if slug.is_empty() {
            return Ok(None);
        }
        let parsed = self
            .graphql(
                "BookclerkAuthorSummary",
                AUTHOR_SUMMARY,
                json!({ "slug": slug }),
                false,
            )
            .await?;
        let Some(author_v) = parsed.pointer("/data/author") else {
            return Ok(None);
        };
        if author_v.is_null() {
            return Ok(None);
        }
        let author: CatalogAuthor = serde_json::from_value(author_v.clone())?;
        let audiobooks =
            parse_paginated_audiobooks(parsed.pointer("/data/author/summaryAudiobooks"));
        Ok(Some(AuthorCatalog { author, audiobooks }))
    }

    /// Typeahead for author slug resolution and quick title hits — no auth required.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn typeahead(&self, search_term: &str) -> Result<TypeaheadCatalog> {
        let search_term = search_term.trim();
        if search_term.is_empty() {
            return Ok(TypeaheadCatalog::default());
        }
        let parsed = self
            .graphql(
                "BookclerkTypeahead",
                TYPEAHEAD,
                json!({ "searchTerm": search_term }),
                false,
            )
            .await?;
        let audiobooks = parse_paginated_audiobooks(parsed.pointer("/data/typeahead/audiobooks"));
        let authors = parsed
            .pointer("/data/typeahead/authors")
            .and_then(|v| serde_json::from_value::<Vec<CatalogAuthor>>(v.clone()).ok())
            .unwrap_or_default();
        Ok(TypeaheadCatalog {
            audiobooks,
            authors,
        })
    }

    /// Resolve a Chirp author slug via typeahead (exact name match only).
    ///
    /// Deliberately no substring fallback — short needles like `"ann"` would
    /// otherwise resolve to unrelated authors and pollute candidate expansion.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn resolve_author_slug(&self, author_name: &str) -> Result<Option<String>> {
        let tip = self.typeahead(author_name).await?;
        let want = author_name.trim();
        if want.is_empty() {
            return Ok(None);
        }
        Ok(tip
            .authors
            .into_iter()
            .find(|a| a.name.eq_ignore_ascii_case(want))
            .map(|a| a.slug))
    }

    /// Try common Chirp series slug forms derived from a series title.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn resolve_series_catalog(&self, series_name: &str) -> Result<Option<SeriesCatalog>> {
        for slug in chirp_slug_candidates(series_name) {
            match self.series_catalog(&slug).await {
                Ok(Some(catalog)) => return Ok(Some(catalog)),
                Ok(None) => continue,
                Err(err) => {
                    // Missing series returns GraphQL errors; treat as miss.
                    tracing::debug!(slug, error = %err, "chirp series slug miss");
                }
            }
        }
        Ok(None)
    }

    /// Current Chirp top deals — no auth required.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn top_deals(&self, count: u32) -> Result<Vec<CatalogAudiobook>> {
        let count = count.clamp(1, 40);
        let parsed = self
            .graphql(
                "BookclerkTopDeals",
                TOP_DEALS,
                json!({ "count": count }),
                false,
            )
            .await?;
        Ok(parse_paginated_audiobooks(
            parsed.pointer("/data/topDealsAudiobooks"),
        ))
    }

    /// Current Chirp free deals — no auth required.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn free_deals(&self) -> Result<Vec<CatalogAudiobook>> {
        let parsed = self
            .graphql("BookclerkFreeDeals", FREE_DEALS, json!({}), false)
            .await?;
        Ok(parse_paginated_audiobooks(
            parsed.pointer("/data/freeDeals"),
        ))
    }

    /// Download bytes from an absolute media URL.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn download_bytes(&self, url: &str) -> Result<bytes::Bytes> {
        let resp = self
            .http
            .get(url)
            .header(USER_AGENT, USER_AGENT_VALUE)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(ChirpError::download(format!(
                "download failed ({status}) for {url}"
            )));
        }
        Ok(resp.bytes().await?)
    }
}

/// Deserializes a paginated `objects` array into catalog audiobooks; missing JSON yields empty.
fn parse_paginated_audiobooks(value: Option<&Value>) -> Vec<CatalogAudiobook> {
    let Some(value) = value else {
        return Vec::new();
    };
    serde_json::from_value::<Vec<CatalogAudiobook>>(value.clone()).unwrap_or_default()
}

/// Slug guesses for Chirp `series(slug:)` / `author(slug:)`.
#[must_use]
pub fn chirp_slug_candidates(name: &str) -> Vec<String> {
    let base = slugify(name);
    if base.is_empty() {
        return Vec::new();
    }
    let mut out = vec![
        format!("{base}-audiobooks"),
        base.clone(),
        format!("the-{base}-audiobooks"),
        format!("the-{base}"),
    ];
    out.sort();
    out.dedup();
    out
}

/// Lowercases ASCII alphanumerics and collapses other runs to `-` for Chirp slug guesses.
fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Truncates `s` to `max` bytes for error snippets (assumes ASCII GraphQL text).
fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}

/// User object from `signIn`.
#[derive(Debug, Clone, Deserialize)]
pub struct SignInUser {
    /// Chirp user id returned by `signIn`.
    pub id: String,
    /// API bearer token used for authenticated GraphQL.
    pub token: String,
    #[serde(default, rename = "webToken")]
    /// Optional web-session token (wire `webToken`); unused by the Android client.
    pub web_token: Option<String>,
    /// Account email from `signIn`.
    pub email: String,
}

/// One `currentUserAudiobooks` row.
#[derive(Debug, Clone, Deserialize)]
pub struct LibraryItem {
    /// Identifier.
    pub id: String,
    /// Archived.
    #[serde(default)]
    pub archived: Option<bool>,
    /// Playable.
    #[serde(default)]
    pub playable: Option<bool>,
    /// Audiobook.
    pub audiobook: Option<Audiobook>,
}

/// Chirp audiobook metadata (+ optional tracks).
#[derive(Debug, Clone, Deserialize)]
pub struct Audiobook {
    /// Identifier.
    pub id: String,
    /// Display title.
    #[serde(default, rename = "displayTitle")]
    pub display_title: Option<String>,
    /// Sub title.
    #[serde(default, rename = "subTitle")]
    pub sub_title: Option<String>,
    /// Display authors.
    #[serde(default, rename = "displayAuthors")]
    pub display_authors: Option<String>,
    /// Display narrators.
    #[serde(default, rename = "displayNarrators")]
    pub display_narrators: Option<String>,
    /// Duration ms.
    #[serde(default, rename = "durationMs")]
    pub duration_ms: Option<u64>,
    /// Abridged.
    #[serde(default)]
    pub abridged: Option<bool>,
    /// Publisher.
    #[serde(default)]
    pub publisher: Option<String>,
    /// Released on.
    #[serde(default, rename = "releasedOn")]
    pub released_on: Option<String>,
    /// Cover URL.
    #[serde(default, rename = "coverUrl")]
    pub cover_url: Option<String>,
    /// Description.
    #[serde(default)]
    pub description: Option<String>,
    /// Tracks.
    #[serde(default)]
    pub tracks: Vec<Track>,
}

/// Live Chirp storefront pricing (`audiobook.currentProduct`).
#[derive(Debug, Clone, Deserialize)]
pub struct ChirpProductPricing {
    /// Identifier.
    #[serde(default, deserialize_with = "deserialize_id_string_opt")]
    pub id: Option<String>,
    /// Discount price.
    #[serde(default, rename = "discountPrice")]
    pub discount_price: String,
    /// Discounted price cents.
    #[serde(default, rename = "discountedPriceCents")]
    pub discounted_price_cents: Option<i64>,
    /// Listing price.
    #[serde(default, rename = "listingPrice")]
    pub listing_price: Option<String>,
    /// Is free listing.
    #[serde(default, rename = "isFreeListing")]
    pub is_free_listing: bool,
    /// Hot deal.
    #[serde(default, rename = "hotDeal")]
    pub hot_deal: bool,
    /// Purchase URL.
    #[serde(default, rename = "purchaseUrl")]
    pub purchase_url: Option<String>,
    /// Salable in current country.
    #[serde(default, rename = "salableInCurrentCountry")]
    pub salable_in_current_country: Option<bool>,
}

/// Accepts a JSON string, number, or other value as an optional id string.
fn deserialize_id_string_opt<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(match value {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(other) => Some(other.to_string()),
    })
}

/// Catalog-oriented audiobook (search / related / series).
#[derive(Debug, Clone, Deserialize)]
pub struct CatalogAudiobook {
    /// Identifier.
    pub id: String,
    /// Display title.
    #[serde(default, rename = "displayTitle")]
    pub display_title: Option<String>,
    /// Display authors.
    #[serde(default, rename = "displayAuthors")]
    pub display_authors: Option<String>,
    /// Display narrators.
    #[serde(default, rename = "displayNarrators")]
    pub display_narrators: Option<String>,
    /// URL.
    #[serde(default)]
    pub url: Option<String>,
    /// Cover URL.
    #[serde(default, rename = "coverUrl")]
    pub cover_url: Option<String>,
    /// Series audiobook.
    #[serde(default, rename = "seriesAudiobook")]
    pub series_audiobook: Option<SeriesAudiobookRef>,
    /// Sub title.
    #[serde(default, rename = "subTitle")]
    pub sub_title: Option<String>,
    /// Description.
    #[serde(default)]
    pub description: Option<String>,
    /// Publisher.
    #[serde(default)]
    pub publisher: Option<String>,
    /// Duration ms.
    #[serde(default, rename = "durationMs")]
    pub duration_ms: Option<u64>,
    /// Released on.
    #[serde(default, rename = "releasedOn")]
    pub released_on: Option<String>,
    /// Chirp display language (`English`, `Spanish`, …).
    #[serde(default)]
    pub language: Option<String>,
    /// Abridged.
    #[serde(default)]
    pub abridged: Option<bool>,
    /// Storefront genre chips (`Thrillers`, `Crime Fiction & Mysteries`, …).
    #[serde(default, rename = "promotedTags")]
    pub promoted_tags: Vec<CatalogTag>,
}

/// Chirp `Tag` used as a promoted genre chip on catalog audiobooks.
#[derive(Debug, Clone, Deserialize)]
pub struct CatalogTag {
    #[serde(default, rename = "displayName")]
    /// Storefront genre-chip label (wire `displayName`).
    pub display_name: Option<String>,
}

impl CatalogAudiobook {
    /// Title.
    #[must_use]
    pub fn title(&self) -> String {
        self.display_title
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.id.clone())
    }

    /// Series name.
    #[must_use]
    pub fn series_name(&self) -> Option<String> {
        self.series_audiobook
            .as_ref()
            .map(|s| s.series.name.clone())
    }
}

/// Series membership on a catalog audiobook.
#[derive(Debug, Clone, Deserialize)]
pub struct SeriesAudiobookRef {
    #[serde(default)]
    /// Numeric series position when Chirp reports one.
    pub number: Option<i64>,
    #[serde(default, rename = "displayNumber")]
    /// Storefront-facing series position (wire `displayNumber`, may be non-integer).
    pub display_number: Option<String>,
    /// Series identity (id, name, slug) this title belongs to.
    pub series: CatalogSeries,
}

/// Chirp series metadata.
#[derive(Debug, Clone, Deserialize)]
pub struct CatalogSeries {
    /// Identifier.
    #[serde(deserialize_with = "deserialize_id_string")]
    pub id: String,
    /// Display or configuration name.
    pub name: String,
    /// Slug.
    pub slug: String,
}

/// Chirp author metadata.
#[derive(Debug, Clone, Deserialize)]
pub struct CatalogAuthor {
    /// Identifier.
    #[serde(deserialize_with = "deserialize_id_string")]
    pub id: String,
    /// Display or configuration name.
    pub name: String,
    /// Slug.
    pub slug: String,
}

/// Related-audiobook expansion result.
#[derive(Debug, Clone)]
pub struct RelatedCatalog {
    /// Related.
    pub related: Vec<CatalogAudiobook>,
    /// Series.
    pub series: Option<CatalogSeries>,
}

/// Series page expansion result.
#[derive(Debug, Clone)]
pub struct SeriesCatalog {
    /// Series.
    pub series: CatalogSeries,
    /// Audiobooks.
    pub audiobooks: Vec<CatalogAudiobook>,
}

/// Author summary expansion result.
#[derive(Debug, Clone)]
pub struct AuthorCatalog {
    /// Author.
    pub author: CatalogAuthor,
    /// Audiobooks.
    pub audiobooks: Vec<CatalogAudiobook>,
}

/// Typeahead hits.
#[derive(Debug, Clone, Default)]
pub struct TypeaheadCatalog {
    /// Audiobooks.
    pub audiobooks: Vec<CatalogAudiobook>,
    /// Authors.
    pub authors: Vec<CatalogAuthor>,
}

/// One downloadable / playable track.
#[derive(Debug, Clone, Deserialize)]
pub struct Track {
    /// Identifier.
    pub id: String,
    /// Media URL.
    #[serde(default, rename = "mediaUrl")]
    pub media_url: Option<String>,
    /// Chapter number.
    #[serde(default, rename = "chapterNumber")]
    pub chapter_number: Option<i64>,
    /// Part number.
    #[serde(default, rename = "partNumber")]
    pub part_number: Option<i64>,
    /// Duration ms.
    #[serde(default, rename = "durationMs")]
    pub duration_ms: Option<u64>,
    /// Offset from book start ms.
    #[serde(default, rename = "offsetFromBookStartMs")]
    pub offset_from_book_start_ms: Option<u64>,
    /// Display name.
    #[serde(default, rename = "displayName")]
    pub display_name: Option<String>,
}

/// Accepts a JSON string, number, or other value as a required id string.
fn deserialize_id_string<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::String(s) => Ok(s),
        Value::Number(n) => Ok(n.to_string()),
        other => Ok(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_candidates_include_audiobooks_suffix() {
        let c = chirp_slug_candidates("Mistborn Saga");
        assert!(c.iter().any(|s| s == "mistborn-saga-audiobooks"));
        assert!(c.iter().any(|s| s == "mistborn-saga"));
    }
}
