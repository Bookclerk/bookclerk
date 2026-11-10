//! Conversions between host storefront types and the typed Cap'n Proto plugin
//! ABI ([`bookclerk_plugin_abi`]).
//!
//! # Audience
//!
//! The plugin host (`ExternalSource`) and every native storefront guest. Both
//! sides speak the same generated structs, so the mapping lives here once
//! instead of being copied into each plugin crate.
//!
//! Wire optionality follows the ABI convention: empty text / zero numbers mean
//! "absent" and project to `Option::None` on this side.

use std::path::PathBuf;

use bookclerk_plugin_abi as abi;

use crate::types::{
    CatalogHit, CatalogSearchField, CatalogSearchOpts, CatalogSearchSort, ExpandSeed, LoginOptions,
    PlainAudioPart, PlainFetch, PurchaseHintOpts, SourceAccount, SourcePurchaseHint,
};

/// `limit` sent for the host's `ContentSource::list_deals` when the caller omits one.
pub const DEFAULT_LIST_DEALS_LIMIT: u32 = 20;

/// Host-side registry sort key for guests that leave `sortKey` at zero.
pub const DEFAULT_EXTERNAL_SORT_KEY: u32 = 200;

/// Saturating `usize` → `u32` for wire counters.
fn count_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Saturating `u32` → `usize` for wire counters.
fn count_usize(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

impl From<SourceAccount> for abi::SourceAccount {
    fn from(account: SourceAccount) -> Self {
        Self {
            account_id: account.account_id,
            source: account.source,
            marketplace: account.marketplace,
            label: account.label,
            scan_enabled: account.scan_enabled,
        }
    }
}

impl From<abi::SourceAccount> for SourceAccount {
    fn from(account: abi::SourceAccount) -> Self {
        Self {
            account_id: account.account_id,
            source: account.source,
            marketplace: account.marketplace,
            label: account.label,
            scan_enabled: account.scan_enabled,
        }
    }
}

impl From<PlainAudioPart> for abi::PlainPart {
    fn from(part: PlainAudioPart) -> Self {
        Self {
            path: part.path.display().to_string(),
            title: part.title,
            duration_ms: part.duration_ms,
        }
    }
}

impl From<abi::PlainPart> for PlainAudioPart {
    fn from(part: abi::PlainPart) -> Self {
        Self {
            path: PathBuf::from(part.path),
            title: part.title,
            duration_ms: part.duration_ms,
        }
    }
}

impl From<PlainFetch> for abi::PlainFetch {
    fn from(plain: PlainFetch) -> Self {
        Self {
            parts: plain.parts.into_iter().map(Into::into).collect(),
            m4b_path: plain.m4b_path.map(|p| p.display().to_string()),
            cover_path: plain.cover_path.map(|p| p.display().to_string()),
            chapters: plain
                .chapters
                .into_iter()
                .map(|(title, start_ms)| abi::ChapterMarker { title, start_ms })
                .collect(),
            pdf_url: plain.pdf_url,
        }
    }
}

impl From<abi::PlainFetch> for PlainFetch {
    fn from(plain: abi::PlainFetch) -> Self {
        Self {
            parts: plain.parts.into_iter().map(Into::into).collect(),
            m4b_path: plain.m4b_path.map(PathBuf::from),
            cover_path: plain.cover_path.map(PathBuf::from),
            chapters: plain
                .chapters
                .into_iter()
                .map(|marker| (marker.title, marker.start_ms))
                .collect(),
            pdf_url: plain.pdf_url,
        }
    }
}

/// Tri-state abridgement flag ↔ the host's `Option<bool>`.
#[must_use]
pub fn abridgement_from_flag(is_abridged: Option<bool>) -> abi::Abridgement {
    match is_abridged {
        None => abi::Abridgement::Unknown,
        Some(false) => abi::Abridgement::Unabridged,
        Some(true) => abi::Abridgement::Abridged,
    }
}

/// Host `Option<bool>` view of the wire tri-state abridgement flag.
#[must_use]
pub fn abridgement_to_flag(abridgement: abi::Abridgement) -> Option<bool> {
    match abridgement {
        abi::Abridgement::Unknown => None,
        abi::Abridgement::Unabridged => Some(false),
        abi::Abridgement::Abridged => Some(true),
    }
}

impl From<CatalogHit> for abi::CatalogHit {
    fn from(hit: CatalogHit) -> Self {
        Self {
            product_id: hit.product_id,
            title: hit.title,
            url: hit.url,
            cover_url: hit.cover_url,
            origin: hit.origin,
            subtitle: hit.subtitle,
            description: hit.description,
            publisher: hit.publisher,
            length_minutes: hit.length_minutes,
            published_at: hit.published_at,
            categories: hit.categories,
            language: hit.language,
            price_cents: hit.price_cents,
            currency: hit.currency,
            price_label: hit.price_label,
            rating_overall: hit.rating_overall,
            rating_count: hit.rating_count,
            abridgement: abridgement_from_flag(hit.is_abridged),
            work_kind: hit.work_kind,
            expression_kind: hit.expression_kind,
            media_kind: hit.media_kind,
            identifiers: hit.identifiers,
            credits: hit.credits,
            series_entries: hit.series_entries,
            parent_product_id: hit.parent_product_id,
        }
    }
}

impl From<abi::CatalogHit> for CatalogHit {
    fn from(hit: abi::CatalogHit) -> Self {
        Self {
            product_id: hit.product_id,
            title: hit.title,
            url: hit.url,
            cover_url: hit.cover_url,
            origin: hit.origin,
            subtitle: hit.subtitle,
            description: hit.description,
            publisher: hit.publisher,
            length_minutes: hit.length_minutes,
            published_at: hit.published_at,
            categories: hit.categories,
            language: hit.language,
            price_cents: hit.price_cents,
            currency: hit.currency,
            price_label: hit.price_label,
            rating_overall: hit.rating_overall,
            rating_count: hit.rating_count,
            is_abridged: abridgement_to_flag(hit.abridgement),
            work_kind: hit.work_kind,
            expression_kind: hit.expression_kind,
            media_kind: hit.media_kind,
            identifiers: hit.identifiers,
            credits: hit.credits,
            series_entries: hit.series_entries,
            parent_product_id: hit.parent_product_id,
        }
    }
}

impl From<SourcePurchaseHint> for abi::PurchaseHint {
    fn from(hint: SourcePurchaseHint) -> Self {
        Self {
            product_id: hint.product_id,
            title: hint.title,
            url: hint.url,
            price_cents: hint.price_cents,
            currency: hint.currency,
            price_label: hint.price_label,
            list_price_cents: hint.list_price_cents,
            list_price_label: hint.list_price_label,
            member_price_cents: hint.member_price_cents,
            member_price_label: hint.member_price_label,
        }
    }
}

impl From<abi::PurchaseHint> for SourcePurchaseHint {
    fn from(hint: abi::PurchaseHint) -> Self {
        Self {
            product_id: hint.product_id,
            title: hint.title,
            url: hint.url,
            price_cents: hint.price_cents,
            currency: hint.currency,
            price_label: hint.price_label,
            list_price_cents: hint.list_price_cents,
            list_price_label: hint.list_price_label,
            member_price_cents: hint.member_price_cents,
            member_price_label: hint.member_price_label,
        }
    }
}

impl From<CatalogSearchSort> for abi::CatalogSort {
    /// Host-only re-rank modes (`price`, `length`) have no upstream sort; they
    /// travel as `relevance` and the host re-ranks the merged result.
    fn from(sort: CatalogSearchSort) -> Self {
        match sort {
            CatalogSearchSort::Relevance | CatalogSearchSort::Price | CatalogSearchSort::Length => {
                Self::Relevance
            }
            CatalogSearchSort::Popularity => Self::Popularity,
            CatalogSearchSort::Rating => Self::Rating,
            CatalogSearchSort::Title => Self::Title,
            CatalogSearchSort::Author => Self::Author,
        }
    }
}

impl From<abi::CatalogSort> for CatalogSearchSort {
    fn from(sort: abi::CatalogSort) -> Self {
        match sort {
            abi::CatalogSort::Relevance => Self::Relevance,
            abi::CatalogSort::Popularity => Self::Popularity,
            abi::CatalogSort::Rating => Self::Rating,
            abi::CatalogSort::Title => Self::Title,
            abi::CatalogSort::Author => Self::Author,
        }
    }
}

/// Wire facet for an optional host search field (`None` → `any`).
#[must_use]
pub fn catalog_field_to_abi(field: Option<CatalogSearchField>) -> abi::CatalogField {
    match field {
        None => abi::CatalogField::Any,
        Some(CatalogSearchField::Author) => abi::CatalogField::Author,
        Some(CatalogSearchField::Narrator) => abi::CatalogField::Narrator,
        Some(CatalogSearchField::Series) => abi::CatalogField::Series,
        Some(CatalogSearchField::Genre) => abi::CatalogField::Genre,
    }
}

/// Host search field for a wire facet (`any` → `None`).
#[must_use]
pub fn catalog_field_from_abi(field: abi::CatalogField) -> Option<CatalogSearchField> {
    match field {
        abi::CatalogField::Any => None,
        abi::CatalogField::Author => Some(CatalogSearchField::Author),
        abi::CatalogField::Narrator => Some(CatalogSearchField::Narrator),
        abi::CatalogField::Series => Some(CatalogSearchField::Series),
        abi::CatalogField::Genre => Some(CatalogSearchField::Genre),
    }
}

impl From<&CatalogSearchOpts> for abi::SearchCatalogParams {
    fn from(opts: &CatalogSearchOpts) -> Self {
        Self {
            query: opts.query.clone(),
            region: opts.region.clone(),
            limit: count_u32(opts.limit),
            page: opts.page.max(1),
            sort: opts.sort.into(),
            field: catalog_field_to_abi(opts.field),
            language: opts.language.clone(),
        }
    }
}

impl From<abi::SearchCatalogParams> for CatalogSearchOpts {
    fn from(params: abi::SearchCatalogParams) -> Self {
        Self {
            query: params.query,
            region: params.region,
            limit: count_usize(params.limit),
            page: params.page.max(1),
            sort: params.sort.into(),
            field: catalog_field_from_abi(params.field),
            language: params.language,
        }
    }
}

/// Builds `expandCandidates` params from a seed plus the result cap.
#[must_use]
pub fn expand_candidates_params(seed: &ExpandSeed, limit: usize) -> abi::ExpandCandidatesParams {
    abi::ExpandCandidatesParams {
        source: seed.source.clone(),
        product_id: seed.product_id.clone(),
        title: seed.title.clone(),
        authors: seed.authors.clone(),
        narrators: seed.narrators.clone(),
        series: seed.series.clone(),
        series_asin: seed.series_asin.clone(),
        asin: seed.asin.clone(),
        isbn: seed.isbn.clone(),
        region: seed.region.clone(),
        limit: count_u32(limit),
    }
}

/// Splits `expandCandidates` params into the host seed and result cap.
#[must_use]
pub fn expand_seed_from_params(params: abi::ExpandCandidatesParams) -> (ExpandSeed, usize) {
    let limit = count_usize(params.limit);
    (
        ExpandSeed {
            source: params.source,
            product_id: params.product_id,
            title: params.title,
            authors: params.authors,
            narrators: params.narrators,
            series: params.series,
            series_asin: params.series_asin,
            asin: params.asin,
            isbn: params.isbn,
            region: params.region,
        },
        limit,
    )
}

impl From<&PurchaseHintOpts> for abi::PurchaseHintParams {
    fn from(opts: &PurchaseHintOpts) -> Self {
        Self {
            product_id: opts.product_id.clone(),
            title: opts.title.clone(),
            authors: opts.authors.clone(),
            asin: opts.asin.clone(),
            isbn: opts.isbn.clone(),
            region: opts.region.clone(),
            with_price: opts.with_price,
        }
    }
}

impl From<abi::PurchaseHintParams> for PurchaseHintOpts {
    fn from(params: abi::PurchaseHintParams) -> Self {
        Self {
            product_id: params.product_id,
            title: params.title,
            authors: params.authors,
            asin: params.asin,
            isbn: params.isbn,
            region: params.region,
            with_price: params.with_price,
        }
    }
}

/// Builds guest login params. `plugin_data_dir` is the jail-granted data
/// directory; the host fills `callback_ipc` / `callback_public_base` after it
/// starts the OAuth callback proxy.
#[must_use]
pub fn login_params(plugin_data_dir: String, opts: LoginOptions) -> abi::LoginParams {
    abi::LoginParams {
        plugin_data_dir,
        marketplace: opts.marketplace,
        label: opts.label,
        email: opts.email,
        password: opts.password,
        force: opts.force,
        callback_bind: opts.callback_bind,
        callback_ipc: None,
        callback_public_base: None,
        external: opts.external,
        response_url: opts.response_url,
        show_qr: opts.show_qr,
        timeout_secs: opts.timeout_secs,
        extra: abi::ExtensibleConfig::json(&opts.extra),
    }
}

impl From<abi::LoginParams> for LoginOptions {
    /// Guest view of host login params; the callback IPC / public base are
    /// transport knobs consumed by the guest's OAuth server, not options.
    fn from(params: abi::LoginParams) -> Self {
        Self {
            marketplace: params.marketplace,
            label: params.label,
            email: params.email,
            password: params.password,
            force: params.force,
            callback_bind: params.callback_bind,
            external: params.external,
            response_url: params.response_url,
            show_qr: params.show_qr,
            timeout_secs: params.timeout_secs,
            extra: params.extra.json_value().unwrap_or(serde_json::Value::Null),
        }
    }
}

/// RFC 3339 wire timestamp as UTC; `None` when it does not parse.
#[must_use]
pub fn parse_rfc3339(raw: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(raw.trim())
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

/// RFC 3339 wire form of a host timestamp, for `ScanBook` date fields.
///
/// Sub-second precision is kept when the timestamp has it, so a round
/// trip through the wire form returns the same instant.
#[must_use]
pub fn rfc3339(at: chrono::DateTime<chrono::Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
}

/// Prefix of the page-number scan cursor storefronts with numbered pages use.
const PAGE_CURSOR_PREFIX: &str = "page:";

/// Store page a scan page-number cursor names: page 1 for the empty cursor a
/// pass starts with, otherwise the page [`page_cursor`] wrote.
///
/// # Errors
///
/// Returns an `invalid_cursor` [`abi::PluginError`] when the cursor is not
/// one [`page_cursor`] wrote (a page below 2 included), so the host restarts
/// the pass.
pub fn page_from_cursor(cursor: &str) -> Result<u32, abi::PluginError> {
    if cursor.is_empty() {
        return Ok(1);
    }
    cursor
        .strip_prefix(PAGE_CURSOR_PREFIX)
        .filter(|digits| !digits.starts_with('0') && digits.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|digits| digits.parse::<u32>().ok())
        .filter(|page| *page >= 2)
        .ok_or_else(|| abi::PluginError::invalid_cursor("not a page cursor this plugin wrote"))
}

/// Cursor naming store page `page`, read back by [`page_from_cursor`].
#[must_use]
pub fn page_cursor(page: u32) -> String {
    format!("{PAGE_CURSOR_PREFIX}{page}")
}

/// The pass's credential document, checked against the account it scans.
///
/// # Errors
///
/// Returns [`crate::SourceError::Auth`] when the host sent no credential,
/// a credential for another account, or a blob that is not JSON.
pub fn scan_credential(params: &abi::ScanParams) -> crate::Result<serde_json::Value> {
    if params.account_id.is_empty() || params.credential.account_id != params.account_id {
        return Err(crate::SourceError::Auth(format!(
            "no credentials for account `{}` from host; run login first",
            params.account_id
        )));
    }
    credentials_from_bytes(&params.credential.credentials)
}

/// One page of a full pass: the pass is complete once no page follows.
///
/// Storefronts without a delta API list every title on every pass, so the
/// last page of a pass always says `complete` and leaves `syncToken` empty.
#[must_use]
pub fn full_pass_page(
    books: Vec<abi::ScanBook>,
    next_cursor: Option<String>,
    store_pages: u32,
) -> abi::ScanPage {
    let next_cursor = next_cursor.unwrap_or_default();
    abi::ScanPage {
        books,
        removed: Vec::new(),
        complete: next_cursor.is_empty(),
        next_cursor,
        sync_token: String::new(),
        store_pages,
    }
}

/// Encodes a credential document as the opaque bytes the ABI carries.
///
/// # Errors
///
/// Returns when `credentials` cannot be serialized.
pub fn credentials_to_bytes(credentials: &serde_json::Value) -> crate::Result<Vec<u8>> {
    serde_json::to_vec(credentials).map_err(|err| crate::SourceError::api(err.to_string()))
}

/// Decodes an opaque credential blob into the JSON document guests store.
///
/// # Errors
///
/// Returns [`crate::SourceError::Auth`] when the bytes are not JSON.
pub fn credentials_from_bytes(bytes: &[u8]) -> crate::Result<serde_json::Value> {
    serde_json::from_slice(bytes)
        .map_err(|err| crate::SourceError::Auth(format!("invalid credential blob: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_account_roundtrips_through_wire() {
        let account = SourceAccount {
            account_id: "acct-1".into(),
            source: "libro".into(),
            marketplace: "us".into(),
            label: Some("Main".into()),
            scan_enabled: true,
        };
        let back = SourceAccount::from(abi::SourceAccount::from(account.clone()));
        assert_eq!(format!("{back:?}"), format!("{account:?}"));
    }

    #[test]
    fn purchase_hint_roundtrips_and_decodes_entities() {
        let hint = SourcePurchaseHint {
            product_id: "p-9".into(),
            title: Some("Tom &amp; Jerry".into()),
            url: Some("https://example.test/p-9".into()),
            price_cents: Some(1499),
            currency: Some("USD".into()),
            price_label: Some("&#36;14.99".into()),
            list_price_cents: Some(1999),
            list_price_label: Some("&#36;19.99".into()),
            member_price_cents: Some(999),
            member_price_label: Some("&#36;9.99".into()),
        };
        let back = SourcePurchaseHint::from(abi::PurchaseHint::from(hint.clone()));
        assert_eq!(format!("{back:?}"), format!("{hint:?}"));

        let decoded = back.decode_html_entities();
        assert_eq!(decoded.title.as_deref(), Some("Tom & Jerry"));
        assert_eq!(decoded.price_label.as_deref(), Some("$14.99"));
        assert_eq!(decoded.list_price_label.as_deref(), Some("$19.99"));
        assert_eq!(decoded.member_price_label.as_deref(), Some("$9.99"));
    }

    #[test]
    fn catalog_sort_and_field_map_both_ways() {
        for sort in [
            abi::CatalogSort::Relevance,
            abi::CatalogSort::Popularity,
            abi::CatalogSort::Rating,
            abi::CatalogSort::Title,
            abi::CatalogSort::Author,
        ] {
            assert_eq!(abi::CatalogSort::from(CatalogSearchSort::from(sort)), sort);
        }
        for field in [
            abi::CatalogField::Any,
            abi::CatalogField::Author,
            abi::CatalogField::Narrator,
            abi::CatalogField::Series,
            abi::CatalogField::Genre,
        ] {
            assert_eq!(catalog_field_to_abi(catalog_field_from_abi(field)), field);
        }
    }

    #[test]
    fn expand_params_roundtrip_seed_and_limit() {
        let seed = ExpandSeed {
            source: "chirp".into(),
            product_id: "42".into(),
            title: "Dune".into(),
            authors: Some("Frank Herbert".into()),
            narrators: Some("Scott Brick".into()),
            series: Some("Dune".into()),
            series_asin: Some("B000".into()),
            asin: Some("B001".into()),
            isbn: Some("9780000000000".into()),
            region: "us".into(),
        };
        let (back, limit) = expand_seed_from_params(expand_candidates_params(&seed, 7));
        assert_eq!(limit, 7);
        assert_eq!(format!("{back:?}"), format!("{seed:?}"));
    }

    #[test]
    fn purchase_hint_params_roundtrip() {
        let opts = PurchaseHintOpts {
            product_id: Some("p-1".into()),
            title: Some("Dune".into()),
            authors: Some("Frank Herbert".into()),
            asin: Some("B001".into()),
            isbn: Some("9780000000000".into()),
            region: "uk".into(),
            with_price: true,
        };
        let back = PurchaseHintOpts::from(abi::PurchaseHintParams::from(&opts));
        assert_eq!(format!("{back:?}"), format!("{opts:?}"));
    }

    #[test]
    fn plain_fetch_roundtrips_through_wire() {
        let plain = PlainFetch {
            parts: vec![PlainAudioPart {
                path: PathBuf::from("/tmp/a.m4a"),
                title: Some("Part 1".into()),
                duration_ms: Some(1234),
            }],
            m4b_path: Some(PathBuf::from("/tmp/book.m4b")),
            cover_path: None,
            chapters: vec![("Ch 1".into(), 0), ("Ch 2".into(), 5000)],
            pdf_url: Some("https://cdn.example/book.pdf".into()),
        };
        let wire: abi::PlainFetch = plain.clone().into();
        assert_eq!(wire.chapters[1].start_ms, 5000);
        let back: PlainFetch = wire.into();
        assert_eq!(back.chapters, plain.chapters);
        assert_eq!(back.m4b_path, plain.m4b_path);
        assert_eq!(back.parts[0].path, plain.parts[0].path);
        assert_eq!(back.pdf_url, plain.pdf_url);
    }

    #[test]
    fn abridgement_is_tri_state() {
        for flag in [None, Some(true), Some(false)] {
            assert_eq!(abridgement_to_flag(abridgement_from_flag(flag)), flag);
        }
        let hit = CatalogHit {
            product_id: "p".into(),
            title: "T".into(),
            is_abridged: Some(false),
            ..CatalogHit::default()
        };
        let wire: abi::CatalogHit = hit.into();
        assert_eq!(wire.abridgement, abi::Abridgement::Unabridged);
    }

    #[test]
    fn search_params_map_host_only_sorts_to_relevance() {
        let opts = CatalogSearchOpts {
            query: "dune".into(),
            sort: CatalogSearchSort::Price,
            field: Some(CatalogSearchField::Narrator),
            limit: 25,
            page: 0,
            ..CatalogSearchOpts::default()
        };
        let params = abi::SearchCatalogParams::from(&opts);
        assert_eq!(params.sort, abi::CatalogSort::Relevance);
        assert_eq!(params.field, abi::CatalogField::Narrator);
        assert_eq!(params.page, 1);
        let back = CatalogSearchOpts::from(params);
        assert_eq!(back.limit, 25);
        assert_eq!(back.field, Some(CatalogSearchField::Narrator));
    }

    #[test]
    fn page_cursors_round_trip_and_refuse_foreign_text() {
        assert_eq!(page_from_cursor(""), Ok(1));
        for page in [2, 3, 1000, u32::MAX] {
            assert_eq!(page_from_cursor(&page_cursor(page)), Ok(page));
        }
        for bad in [
            "page:1",
            "page:0",
            "page:02",
            "page:",
            "page:x",
            "2",
            "page:-3",
            "page:4294967296",
        ] {
            let err = page_from_cursor(bad).expect_err(bad);
            assert_eq!(err.code, abi::PluginErrorCode::InvalidCursor, "{bad}");
        }
    }

    #[test]
    fn scan_credential_must_name_the_pass_account() {
        let mut params = abi::ScanParams {
            account_id: "a".into(),
            credential: abi::AccountCredential {
                account_id: "a".into(),
                credentials: br#"{"token":"t"}"#.to_vec(),
            },
            ..abi::ScanParams::default()
        };
        assert_eq!(scan_credential(&params).expect("credential")["token"], "t");
        params.credential.account_id = "b".into();
        assert!(scan_credential(&params).is_err());
        params.account_id = String::new();
        params.credential.account_id = String::new();
        assert!(scan_credential(&params).is_err());
    }

    #[test]
    fn full_pass_pages_complete_on_the_last_page() {
        let middle = full_pass_page(Vec::new(), Some(page_cursor(2)), 1);
        assert!(!middle.complete);
        assert_eq!(middle.next_cursor, "page:2");
        let last = full_pass_page(Vec::new(), None, 1);
        assert!(last.complete);
        assert!(last.next_cursor.is_empty() && last.sync_token.is_empty());
    }

    #[test]
    fn rfc3339_keeps_sub_second_precision() {
        let at = parse_rfc3339("2023-01-01T12:34:56.789Z").expect("valid timestamp");
        assert_eq!(rfc3339(at), "2023-01-01T12:34:56.789Z");
        let whole = parse_rfc3339("2023-01-01T12:34:56Z").expect("valid timestamp");
        assert_eq!(rfc3339(whole), "2023-01-01T12:34:56Z");
    }

    #[test]
    fn catalog_hit_round_trips_structured_records() {
        use crate::records::{self, CreditRole, IdentifierScheme, MediaKind};
        let hit = CatalogHit {
            product_id: "p".into(),
            title: "T".into(),
            credits: records::credits_from_names(["A"], CreditRole::Author),
            identifiers: records::identifier(IdentifierScheme::Isbn, "978", None)
                .into_iter()
                .collect(),
            series_entries: records::series_entries(Some("S"), Some("1")),
            media_kind: MediaKind::Ebook,
            parent_product_id: Some("show".into()),
            ..CatalogHit::default()
        };
        let back = CatalogHit::from(abi::CatalogHit::from(hit.clone()));
        assert_eq!(back.authors().as_deref(), Some("A"));
        assert_eq!(back.narrators(), None);
        assert_eq!(back.isbn().as_deref(), Some("978"));
        assert_eq!(back.asin(), None);
        assert_eq!(back.series().as_deref(), Some("S"));
        assert_eq!(back.series_index().as_deref(), Some("1"));
        assert_eq!(back.media_kind, MediaKind::Ebook);
        assert_eq!(back.parent_product_id, hit.parent_product_id);
    }

    #[test]
    fn login_params_wrap_extra_as_json_config() {
        let opts = LoginOptions {
            marketplace: "uk".into(),
            extra: serde_json::json!({ "ascii_qr": true }),
            ..LoginOptions::default()
        };
        let params = login_params("/tmp/data".into(), opts);
        assert_eq!(params.extra.media_type, abi::JSON_MEDIA_TYPE);
        let back = LoginOptions::from(params);
        assert_eq!(back.marketplace, "uk");
        assert_eq!(back.extra["ascii_qr"], true);
    }

    #[test]
    fn credentials_roundtrip_as_json() {
        let bytes = credentials_to_bytes(&serde_json::json!({ "token": "t" })).unwrap();
        assert_eq!(credentials_from_bytes(&bytes).unwrap()["token"], "t");
        credentials_from_bytes(b"not json").expect_err("must reject non-JSON blobs");
    }
}
