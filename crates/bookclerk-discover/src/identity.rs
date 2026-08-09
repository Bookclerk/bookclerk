//! Canonical identity for discovery candidates / recommendations.
//!
//! Prefer bibliographic ids (ISBN, then ASIN), then soft title+author matching
//! so the same book from multiple storefronts collapses to one card.

use bookclerk_enrich::{
    canonicalize_isbn, clean_author_for_compares, clean_title_for_compares, levenshtein_similarity,
};
use bookclerk_library::GlobalQueueEntry;

use crate::candidates::StorefrontCandidate;
use crate::recommend::Recommendation;

/// One storefront edition of a work (for multi-store purchase links).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq, Hash)]
pub struct StoreEdition {
    pub source: String,
    pub product_id: String,
}

impl StoreEdition {
    #[must_use]
    pub fn new(source: impl Into<String>, product_id: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            product_id: product_id.into(),
        }
    }
}

/// Hard bibliographic key: canonical ISBN first, then ASIN.
///
/// Note: ISBN is not published by every storefront (Chirp / GraphicAudio /
/// Audible public search often omit it). Soft title+author matching then applies.
#[must_use]
pub fn hard_work_key(asin: Option<&str>, isbn: Option<&str>) -> Option<String> {
    if let Some(isbn) = isbn.map(canonicalize_isbn).filter(|s| !s.is_empty()) {
        return Some(format!("isbn:{isbn}"));
    }
    let asin = asin.map(str::trim).filter(|s| !s.is_empty())?;
    Some(format!("asin:{}", asin.to_ascii_uppercase()))
}

/// Bibliographic identity slice used for merge decisions.
#[derive(Debug, Clone, Copy)]
pub struct WorkIdentity<'a> {
    pub asin: Option<&'a str>,
    pub isbn: Option<&'a str>,
    pub title: &'a str,
    pub authors: Option<&'a str>,
    pub series: Option<&'a str>,
    pub series_index: Option<&'a str>,
}

impl<'a> WorkIdentity<'a> {
    #[must_use]
    pub fn new(
        asin: Option<&'a str>,
        isbn: Option<&'a str>,
        title: &'a str,
        authors: Option<&'a str>,
    ) -> Self {
        Self {
            asin,
            isbn,
            title,
            authors,
            series: None,
            series_index: None,
        }
    }

    #[must_use]
    pub fn with_series(mut self, series: Option<&'a str>) -> Self {
        self.series = series;
        self
    }

    #[must_use]
    pub fn with_series_index(mut self, series_index: Option<&'a str>) -> Self {
        self.series_index = series_index;
        self
    }
}

/// Whether two bibliographic identities refer to the same work.
#[must_use]
pub fn identities_match(a: WorkIdentity<'_>, b: WorkIdentity<'_>) -> bool {
    let isbn_a = a.isbn.map(canonicalize_isbn).filter(|s| !s.is_empty());
    let isbn_b = b.isbn.map(canonicalize_isbn).filter(|s| !s.is_empty());
    // When both sides publish an ISBN, that decides — never soft-merge across
    // distinct ISBNs (series volumes often share author + stripped title).
    if let (Some(ia), Some(ib)) = (&isbn_a, &isbn_b) {
        return ia == ib;
    }

    let asin_a = a.asin.map(str::trim).filter(|s| !s.is_empty());
    let asin_b = b.asin.map(str::trim).filter(|s| !s.is_empty());
    if let (Some(aa), Some(ab)) = (asin_a, asin_b) {
        return aa.eq_ignore_ascii_case(ab);
    }

    // Distinct series positions never soft-merge when they name the *same*
    // series (Infinity Blade #1 vs #2). Crossover novels often carry two
    // series labels with different indexes (Beaumont #19 vs Brady #14) — those
    // must still merge on title+author.
    if series_indices_conflict(a.series_index, b.series_index)
        && series_names_compatible(a.series, b.series)
    {
        return false;
    }

    // Bare "Infinity Blade" vs "Infinity Blade: Redemption" shares a stripped
    // base title — only allow when both sides agree on series index (e.g. #1
    // with "…: Awakening"). Without indexes, keep them separate.
    if bare_vs_volume_title_ambiguity(a.title, b.title) {
        return series_indices_agree(a.series_index, b.series_index)
            && works_match_base(a.title, a.authors, b.title, b.authors);
    }

    // asin↔isbn (or missing hard id): soft title+author, with subtitle guards.
    works_match(a.title, a.authors, b.title, b.authors)
}

/// Soft key from cleaned title + primary author (exact string match only).
///
/// Keeps subtitles so series volumes (`Infinity Blade: Awakening` vs
/// `…: Redemption`) do not share a HashMap bucket when hard ids are absent.
/// Soft [`identities_match`] still joins edition variants (e.g. `A Novel`).
#[must_use]
pub fn soft_work_key(title: &str, authors: Option<&str>) -> String {
    let t = clean_title_for_compares(title, true);
    let a = primary_author_cleaned(authors).unwrap_or_default();
    format!("soft:{t}|{a}")
}

/// Best stable map key for a recommendation / candidate.
#[must_use]
pub fn work_map_key(
    asin: Option<&str>,
    isbn: Option<&str>,
    title: &str,
    authors: Option<&str>,
    source: Option<&str>,
    product_id: Option<&str>,
) -> String {
    if let Some(k) = hard_work_key(asin, isbn) {
        return k;
    }
    if !title.trim().is_empty() {
        let soft = soft_work_key(title, authors);
        // Avoid collapsing unrelated empties into one bucket.
        if soft != "soft:|" {
            return soft;
        }
    }
    match (source, product_id) {
        (Some(s), Some(p)) if !s.is_empty() && !p.is_empty() => format!("{s}:{p}"),
        (_, Some(p)) if !p.is_empty() => format!("product:{p}"),
        _ => format!("title:{}", title.trim().to_ascii_lowercase()),
    }
}

#[must_use]
pub fn candidate_map_key(c: &StorefrontCandidate) -> String {
    work_map_key(
        c.asin.as_deref(),
        c.isbn.as_deref(),
        &c.title,
        c.authors.as_deref(),
        Some(c.source.as_str()),
        Some(c.product_id.as_str()),
    )
}

#[must_use]
pub fn recommendation_map_key(r: &Recommendation) -> String {
    work_map_key(
        r.asin.as_deref(),
        r.isbn.as_deref(),
        &r.title,
        r.authors.as_deref(),
        r.candidate_source.as_deref(),
        r.candidate_product_id.as_deref(),
    )
}

/// Whether two titles should be treated as the same work.
#[must_use]
pub fn works_match(
    title_a: &str,
    authors_a: Option<&str>,
    title_b: &str,
    authors_b: Option<&str>,
) -> bool {
    // "Infinity Blade: Awakening" vs "Infinity Blade: Redemption" share a base
    // title after subtitle stripping — reject when both sides have distinct,
    // non-generic subtitles. Bare vs volume is handled in [`identities_match`]
    // (needs series_index); refuse here so direct `works_match` callers stay safe.
    if distinguishing_subtitles_conflict(title_a, title_b)
        || bare_vs_volume_title_ambiguity(title_a, title_b)
    {
        return false;
    }

    works_match_base(title_a, authors_a, title_b, authors_b)
}

/// Title+author soft match without subtitle-volume guards (caller already gated).
fn works_match_base(
    title_a: &str,
    authors_a: Option<&str>,
    title_b: &str,
    authors_b: Option<&str>,
) -> bool {
    let ta = clean_title_for_compares(title_a, false);
    let tb = clean_title_for_compares(title_b, false);
    if ta.is_empty() || tb.is_empty() {
        return false;
    }
    let title_sim = levenshtein_similarity(&ta, &tb);
    let contained = title_contains_other(&ta, &tb);
    let authors_a = cleaned_author_list(authors_a);
    let authors_b = cleaned_author_list(authors_b);
    match (authors_a.is_empty(), authors_b.is_empty()) {
        (false, false) => {
            if !authors_match_lists(&authors_a, &authors_b) {
                return false;
            }
            // Exact / near-exact titles, or short/long variants ("Hail Mary" /
            // "Project Hail Mary") when the author is the same.
            title_sim >= 0.90 || contained
        }
        // Both authors missing: near-exact titles only (no loose containment —
        // Magento/Chirp first hits often share a short token with the query).
        (true, true) => title_sim >= 0.96,
        // One side missing author (common for GraphicAudio catalog hits): require
        // near-exact title equality — do not soft-merge on substring containment.
        _ => title_sim >= 0.96,
    }
}

/// Subtitle after `: ` or ` - `, when present.
fn subtitle_portion(title: &str) -> Option<&str> {
    for sep in [": ", " - "] {
        if let Some((_, right)) = title.split_once(sep) {
            let t = right.trim();
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    None
}

fn is_generic_subtitle(cleaned: &str) -> bool {
    matches!(
        cleaned,
        "a novel"
            | "an novel"
            | "the novel"
            | "a memoir"
            | "a thriller"
            | "a romance"
            | "a mystery"
            | "unabridged"
            | "abridged"
    )
}

/// Meaningful (non-generic) subtitle, if any.
fn distinguishing_subtitle(title: &str) -> Option<String> {
    let raw = subtitle_portion(title)?;
    let cleaned = clean_title_for_compares(raw, true);
    if cleaned.is_empty() || is_generic_subtitle(&cleaned) {
        None
    } else {
        Some(cleaned)
    }
}

/// True when both titles carry different meaningful subtitles (series volumes).
fn distinguishing_subtitles_conflict(title_a: &str, title_b: &str) -> bool {
    let (Some(ca), Some(cb)) = (
        distinguishing_subtitle(title_a),
        distinguishing_subtitle(title_b),
    ) else {
        return false;
    };
    ca != cb && levenshtein_similarity(&ca, &cb) < 0.90
}

/// Bare series name vs a volume with a distinguishing subtitle (same base).
fn bare_vs_volume_title_ambiguity(title_a: &str, title_b: &str) -> bool {
    match (
        distinguishing_subtitle(title_a),
        distinguishing_subtitle(title_b),
    ) {
        (Some(_), None) | (None, Some(_)) => {
            let ta = clean_title_for_compares(title_a, false);
            let tb = clean_title_for_compares(title_b, false);
            !ta.is_empty() && ta == tb
        }
        _ => false,
    }
}

/// Parse Audible/Chirp-style series indexes (`"1"`, `"1.5"`, `"Book 2"`, `"02"`).
#[must_use]
pub fn parse_series_index(raw: Option<&str>) -> Option<f64> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(n) = raw.parse::<f64>() {
        return Some(n);
    }
    // Pull the first number-like token.
    let mut num = String::new();
    let mut seen_digit = false;
    let mut seen_dot = false;
    for c in raw.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            seen_digit = true;
        } else if c == '.' && seen_digit && !seen_dot {
            num.push(c);
            seen_dot = true;
        } else if seen_digit {
            break;
        }
    }
    if num.is_empty() || num == "." {
        None
    } else {
        num.parse().ok()
    }
}

fn series_indices_conflict(a: Option<&str>, b: Option<&str>) -> bool {
    match (parse_series_index(a), parse_series_index(b)) {
        (Some(x), Some(y)) => (x - y).abs() > 0.001,
        _ => false,
    }
}

fn series_indices_agree(a: Option<&str>, b: Option<&str>) -> bool {
    match (parse_series_index(a), parse_series_index(b)) {
        (Some(x), Some(y)) => (x - y).abs() <= 0.001,
        _ => false,
    }
}

/// True when both sides name the same series (or either side omits a name).
///
/// Missing names stay compatible so index-only guards still apply. Distinct
/// series labels (crossover editions) are *not* compatible — index mismatches
/// between them must not block a title+author soft merge.
fn series_names_compatible(a: Option<&str>, b: Option<&str>) -> bool {
    let a = a.map(str::trim).filter(|s| !s.is_empty());
    let b = b.map(str::trim).filter(|s| !s.is_empty());
    match (a, b) {
        (None, _) | (_, None) => true,
        (Some(sa), Some(sb)) => {
            let ca = clean_title_for_compares(sa, true);
            let cb = clean_title_for_compares(sb, true);
            if ca.is_empty() || cb.is_empty() {
                return true;
            }
            ca == cb || levenshtein_similarity(&ca, &cb) >= 0.85 || title_contains_other(&ca, &cb)
        }
    }
}

/// True when the shorter cleaned title is a whole-word subset of the longer one.
fn title_contains_other(a: &str, b: &str) -> bool {
    let (shorter, longer) = if a.chars().count() <= b.chars().count() {
        (a, b)
    } else {
        (b, a)
    };
    if shorter.chars().count() < 4 {
        return false;
    }
    if longer == shorter
        || longer.starts_with(&format!("{shorter} "))
        || longer.ends_with(&format!(" {shorter}"))
    {
        return true;
    }
    longer.contains(&format!(" {shorter} "))
}

fn primary_author_cleaned(authors: Option<&str>) -> Option<String> {
    cleaned_author_list(authors).into_iter().next()
}

/// Split a storefront author string into cleaned people (order preserved).
///
/// Strips role suffixes (`Name - editor`, `Name (foreword)`) so Audible's
/// contributor roles do not block set / subset compares.
fn cleaned_author_list(authors: Option<&str>) -> Vec<String> {
    let Some(raw) = authors.map(str::trim).filter(|s| !s.is_empty()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for part in raw.split([',', ';', '&', '/']) {
        let part = strip_author_role_suffix(part.trim());
        if part.is_empty() {
            continue;
        }
        let cleaned = clean_author_for_compares(part);
        if cleaned.is_empty() {
            continue;
        }
        if !out.iter().any(|e| e == &cleaned) {
            out.push(cleaned);
        }
    }
    out
}

fn strip_author_role_suffix(name: &str) -> &str {
    let name = name.trim();
    if let Some((left, _)) = name.split_once(" - ") {
        let left = left.trim();
        if !left.is_empty() {
            return left;
        }
    }
    if let Some((left, _)) = name.split_once(" – ") {
        let left = left.trim();
        if !left.is_empty() {
            return left;
        }
    }
    if let Some((left, rest)) = name.split_once(" (") {
        if rest.contains(')') {
            let left = left.trim();
            if !left.is_empty() {
                return left;
            }
        }
    }
    name
}

/// Order-independent author match: equal sets, subset (editor/foreword extras),
/// or fuzzy primary-author equality.
fn authors_match_lists(a: &[String], b: &[String]) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    use std::collections::HashSet;
    let set_a: HashSet<&str> = a.iter().map(String::as_str).collect();
    let set_b: HashSet<&str> = b.iter().map(String::as_str).collect();
    if set_a == set_b {
        return true;
    }
    // "Travis Langley" ⊆ "Travis Langley; Kyle Maddock - foreword"
    if set_a.is_subset(&set_b) || set_b.is_subset(&set_a) {
        return true;
    }
    let pa = a[0].as_str();
    let pb = b[0].as_str();
    pa == pb || levenshtein_similarity(pa, pb) >= 0.85
}

/// Merge storefront metadata into `into`, preferring filled fields / richer text.
pub fn merge_candidate_metadata(into: &mut StorefrontCandidate, from: &StorefrontCandidate) {
    push_edition(
        &mut into.store_editions,
        StoreEdition::new(&from.source, &from.product_id),
    );
    if into.asin.is_none() {
        into.asin = from.asin.clone();
    }
    if into.isbn.is_none() {
        into.isbn = from.isbn.clone().map(|i| {
            let n = canonicalize_isbn(&i);
            if n.is_empty() {
                i
            } else {
                n
            }
        });
    } else if let Some(isbn) = into.isbn.as_mut() {
        let n = canonicalize_isbn(isbn);
        if !n.is_empty() {
            *isbn = n;
        }
    }
    fill_opt_string(&mut into.authors, from.authors.as_deref());
    fill_opt_string(&mut into.narrators, from.narrators.as_deref());
    fill_opt_string(&mut into.series, from.series.as_deref());
    fill_opt_string(&mut into.series_index, from.series_index.as_deref());
    fill_opt_string(&mut into.cover_url, from.cover_url.as_deref());
    fill_opt_string(&mut into.subtitle, from.subtitle.as_deref());
    fill_opt_string(&mut into.description, from.description.as_deref());
    fill_opt_string(&mut into.publisher, from.publisher.as_deref());
    fill_opt_string(&mut into.published_at, from.published_at.as_deref());
    fill_opt_string(&mut into.categories, from.categories.as_deref());
    fill_opt_string(&mut into.language, from.language.as_deref());
    if into.length_minutes.is_none() {
        into.length_minutes = from.length_minutes;
    }
    if into.price_cents.is_none() {
        into.price_cents = from.price_cents;
        into.currency = from.currency.clone();
        into.price_label = from.price_label.clone();
    }
    if into.rating_overall.is_none() {
        into.rating_overall = from.rating_overall;
    }
    // Prefer the rating backed by more votes when both sides have counts.
    match (into.rating_count, from.rating_count) {
        (None, Some(n)) => {
            into.rating_count = Some(n);
            if from.rating_overall.is_some() {
                into.rating_overall = from.rating_overall;
            }
        }
        (Some(a), Some(b)) if b > a => {
            into.rating_count = Some(b);
            if from.rating_overall.is_some() {
                into.rating_overall = from.rating_overall;
            }
        }
        _ => {}
    }
    if into.audible_rank.is_none() {
        into.audible_rank = from.audible_rank;
    }
    if into.is_abridged.is_none() {
        into.is_abridged = from.is_abridged;
    }
    if from.title.len() > into.title.len() && !from.title.is_empty() {
        // Prefer the longer title when both exist (often includes subtitle).
        into.title = from.title.clone();
    }
    if !from.origin.is_empty() && !into.origin.contains(&from.origin) {
        into.origin = format!("{}; {}", into.origin, from.origin);
    }
}

/// Merge a scored recommendation into an existing card.
pub fn merge_recommendation(into: &mut Recommendation, mut from: Recommendation) {
    for ed in std::mem::take(&mut from.store_editions) {
        push_edition(&mut into.store_editions, ed);
    }
    if let (Some(s), Some(p)) = (
        from.candidate_source.as_deref(),
        from.candidate_product_id.as_deref(),
    ) {
        push_edition(&mut into.store_editions, StoreEdition::new(s, p));
    }
    if let (Some(s), Some(p)) = (
        into.candidate_source.as_deref(),
        into.candidate_product_id.as_deref(),
    ) {
        push_edition(&mut into.store_editions, StoreEdition::new(s, p));
    }

    if from.score > into.score {
        into.score = from.score;
        // Keep the higher-scoring store as the primary candidate identity.
        if from.candidate_source.is_some() {
            into.candidate_source = from.candidate_source.clone();
            into.candidate_product_id = from.candidate_product_id.clone();
        }
        if from.title.len() >= into.title.len() {
            into.title = from.title.clone();
        }
    } else {
        // Multi-store agreement is itself a signal.
        into.score += 0.35;
    }

    if into.asin.is_none() {
        into.asin = from.asin.clone();
    }
    if into.isbn.is_none() {
        into.isbn = from.isbn.clone().map(|i| {
            let n = canonicalize_isbn(&i);
            if n.is_empty() {
                i
            } else {
                n
            }
        });
    }
    fill_opt_string(&mut into.authors, from.authors.as_deref());
    fill_opt_string(&mut into.narrators, from.narrators.as_deref());
    fill_opt_string(&mut into.series, from.series.as_deref());
    fill_opt_string(&mut into.series_index, from.series_index.as_deref());
    fill_opt_string(&mut into.seed_categories, from.seed_categories.as_deref());
    fill_opt_string(&mut into.cover_url, from.cover_url.as_deref());
    fill_opt_string(&mut into.subtitle, from.subtitle.as_deref());
    fill_opt_string(&mut into.description, from.description.as_deref());
    fill_opt_string(&mut into.publisher, from.publisher.as_deref());
    fill_opt_string(&mut into.published_at, from.published_at.as_deref());
    fill_opt_string(&mut into.genres, from.genres.as_deref());
    fill_opt_string(&mut into.language, from.language.as_deref());
    if into.length_minutes.is_none() {
        into.length_minutes = from.length_minutes;
    }
    if into.work_id.is_none() {
        into.work_id = from.work_id.clone();
    }
    if from.from_request {
        into.from_request = true;
        if into.request_uuid.is_none() {
            into.request_uuid = from.request_uuid.clone();
        }
    }
    for cat in from.categories {
        push_shelf_category(&mut into.categories, &cat);
    }
    for reason in from.reasons {
        if !into.reasons.iter().any(|r| r == &reason) {
            into.reasons.push(reason);
        }
    }
    for hint in from.purchase_hints {
        if !into.purchase_hints.iter().any(|h| {
            h.source.eq_ignore_ascii_case(&hint.source)
                && h.product_id.eq_ignore_ascii_case(&hint.product_id)
        }) {
            into.purchase_hints.push(hint);
        }
    }
    into.work_key = recommendation_map_key(into);
}

pub fn push_edition(editions: &mut Vec<StoreEdition>, edition: StoreEdition) {
    if edition.source.trim().is_empty() || edition.product_id.trim().is_empty() {
        return;
    }
    if editions.iter().any(|e| {
        e.source.eq_ignore_ascii_case(&edition.source)
            && e.product_id.eq_ignore_ascii_case(&edition.product_id)
    }) {
        return;
    }
    // Prefer one edition per source (keep first / primary).
    if editions
        .iter()
        .any(|e| e.source.eq_ignore_ascii_case(&edition.source))
    {
        return;
    }
    editions.push(edition);
}

/// Append a stable shelf-kind tag (`finish_series`, `author`, …) if missing.
pub fn push_shelf_category(categories: &mut Vec<String>, kind: &str) {
    let kind = kind.trim();
    if kind.is_empty() {
        return;
    }
    if categories.iter().any(|c| c.eq_ignore_ascii_case(kind)) {
        return;
    }
    categories.push(kind.to_ascii_lowercase());
}

/// Merge wishlist queue rows that share ISBN/ASIN or soft title+author identity.
///
/// Sums `wish_count`, unions store editions / purchase hints, and prefers
/// richer bibliographic metadata (longer strings / filled optionals). ISBN-keyed
/// `work_key` wins when available.
#[must_use]
pub fn merge_global_queue_entries(entries: Vec<GlobalQueueEntry>) -> Vec<GlobalQueueEntry> {
    let mut merged: Vec<GlobalQueueEntry> = Vec::new();
    for entry in entries {
        if let Some(existing) = merged.iter_mut().find(|e| {
            identities_match(
                WorkIdentity::new(
                    e.asin.as_deref(),
                    e.isbn.as_deref(),
                    &e.title,
                    e.authors.as_deref(),
                )
                .with_series(e.series.as_deref())
                .with_series_index(e.series_index.as_deref()),
                WorkIdentity::new(
                    entry.asin.as_deref(),
                    entry.isbn.as_deref(),
                    &entry.title,
                    entry.authors.as_deref(),
                )
                .with_series(entry.series.as_deref())
                .with_series_index(entry.series_index.as_deref()),
            )
        }) {
            existing.wish_count += entry.wish_count;
            for uuid in entry.sample_uuids {
                if existing.sample_uuids.len() >= 8 {
                    break;
                }
                if !existing.sample_uuids.contains(&uuid) {
                    existing.sample_uuids.push(uuid);
                }
            }
            if entry.first_requested_at < existing.first_requested_at {
                existing.first_requested_at = entry.first_requested_at;
            }
            if entry.last_requested_at > existing.last_requested_at {
                existing.last_requested_at = entry.last_requested_at;
            }
            if entry.title.len() > existing.title.len() && !entry.title.is_empty() {
                existing.title = entry.title;
            }
            fill_opt_string(&mut existing.authors, entry.authors.as_deref());
            fill_opt_string(&mut existing.cover_url, entry.cover_url.as_deref());
            fill_opt_string(&mut existing.description, entry.description.as_deref());
            fill_opt_string(&mut existing.subtitle, entry.subtitle.as_deref());
            fill_opt_string(&mut existing.narrators, entry.narrators.as_deref());
            fill_opt_string(&mut existing.series, entry.series.as_deref());
            fill_opt_string(&mut existing.series_index, entry.series_index.as_deref());
            fill_opt_string(&mut existing.publisher, entry.publisher.as_deref());
            fill_opt_string(&mut existing.genres, entry.genres.as_deref());
            fill_opt_string(&mut existing.language, entry.language.as_deref());
            fill_opt_string(&mut existing.published_at, entry.published_at.as_deref());
            if existing.length_minutes.is_none() {
                existing.length_minutes = entry.length_minutes;
            }
            if existing.asin.is_none() {
                existing.asin = entry.asin;
            }
            if existing.isbn.is_none() {
                existing.isbn = entry.isbn.map(|i| {
                    let n = canonicalize_isbn(&i);
                    if n.is_empty() {
                        i
                    } else {
                        n
                    }
                });
            } else if let Some(isbn) = existing.isbn.as_mut() {
                let n = canonicalize_isbn(isbn);
                if !n.is_empty() {
                    *isbn = n;
                }
            }
            existing.description = bookclerk_library::pick_better_description(
                existing.description.as_deref(),
                entry.description.as_deref(),
            );
            for ed in entry.store_editions {
                push_wishlist_edition(&mut existing.store_editions, ed);
            }
            for hint in entry.purchase_hints {
                push_wishlist_purchase_hint(&mut existing.purchase_hints, hint);
            }
            // Prefer the strongest bibliographic key.
            existing.work_key = work_map_key(
                existing.asin.as_deref(),
                existing.isbn.as_deref(),
                &existing.title,
                existing.authors.as_deref(),
                None,
                None,
            );
        } else {
            let mut entry = entry;
            if let Some(isbn) = entry.isbn.as_mut() {
                let n = canonicalize_isbn(isbn);
                if !n.is_empty() {
                    *isbn = n;
                }
            }
            entry.work_key = work_map_key(
                entry.asin.as_deref(),
                entry.isbn.as_deref(),
                &entry.title,
                entry.authors.as_deref(),
                None,
                None,
            );
            merged.push(entry);
        }
    }
    merged
}

fn push_wishlist_edition(
    editions: &mut Vec<bookclerk_library::WishlistStoreEdition>,
    edition: bookclerk_library::WishlistStoreEdition,
) {
    if edition.source.trim().is_empty() || edition.product_id.trim().is_empty() {
        return;
    }
    if editions.iter().any(|e| {
        e.source.eq_ignore_ascii_case(&edition.source)
            && e.product_id.eq_ignore_ascii_case(&edition.product_id)
    }) {
        return;
    }
    if editions
        .iter()
        .any(|e| e.source.eq_ignore_ascii_case(&edition.source))
    {
        return;
    }
    editions.push(edition);
}

fn push_wishlist_purchase_hint(
    hints: &mut Vec<bookclerk_library::WishlistPurchaseHint>,
    hint: bookclerk_library::WishlistPurchaseHint,
) {
    if hint.source.trim().is_empty() || hint.product_id.trim().is_empty() {
        return;
    }
    if let Some(existing) = hints.iter_mut().find(|h| {
        h.source.eq_ignore_ascii_case(&hint.source)
            && h.product_id.eq_ignore_ascii_case(&hint.product_id)
    }) {
        // Richest-wins for overlapping hint fields.
        fill_opt_string(&mut existing.title, hint.title.as_deref());
        fill_opt_string(&mut existing.url, hint.url.as_deref());
        fill_opt_string(&mut existing.currency, hint.currency.as_deref());
        fill_opt_string(&mut existing.price_label, hint.price_label.as_deref());
        fill_opt_string(
            &mut existing.list_price_label,
            hint.list_price_label.as_deref(),
        );
        fill_opt_string(
            &mut existing.member_price_label,
            hint.member_price_label.as_deref(),
        );
        if existing.price_cents.is_none() {
            existing.price_cents = hint.price_cents;
        }
        if existing.list_price_cents.is_none() {
            existing.list_price_cents = hint.list_price_cents;
        }
        if existing.member_price_cents.is_none() {
            existing.member_price_cents = hint.member_price_cents;
        }
        return;
    }
    hints.push(hint);
}

fn fill_opt_string(slot: &mut Option<String>, incoming: Option<&str>) {
    let Some(v) = incoming.map(str::trim).filter(|s| !s.is_empty()) else {
        return;
    };
    match slot {
        None => *slot = Some(v.to_string()),
        Some(existing) if existing.len() < v.len() => *slot = Some(v.to_string()),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isbn_preferred_over_asin() {
        let key = hard_work_key(Some("B00TEST"), Some("978-1-234-56789-0")).unwrap();
        assert_eq!(key, "isbn:9781234567890");
    }

    #[test]
    fn isbn10_and_isbn13_share_hard_key() {
        // ISBN-10 0-306-40615-2 → ISBN-13 9780306406157
        let a = hard_work_key(None, Some("0-306-40615-2")).unwrap();
        let b = hard_work_key(None, Some("978-0-306-40615-7")).unwrap();
        assert_eq!(a, b);
        assert_eq!(a, "isbn:9780306406157");
    }

    #[test]
    fn asin_and_isbn_soft_merge_when_titles_match() {
        assert!(identities_match(
            WorkIdentity::new(
                Some("B00HAIL"),
                None,
                "Project Hail Mary",
                Some("Andy Weir"),
            ),
            WorkIdentity::new(
                None,
                Some("9781234567890"),
                "Project Hail Mary: A Novel",
                Some("Andy Weir"),
            ),
        ));
    }

    #[test]
    fn short_and_long_titles_merge_same_author() {
        assert!(works_match(
            "Hail Mary",
            Some("Andy Weir"),
            "Project Hail Mary",
            Some("Andy Weir"),
        ));
    }

    #[test]
    fn merge_queue_sums_wish_counts_across_asin_isbn() {
        use bookclerk_library::{WishlistPurchaseHint, WishlistStoreEdition};
        use chrono::Utc;
        let now = Utc::now();
        let merged = merge_global_queue_entries(vec![
            GlobalQueueEntry {
                work_key: "asin:B00HAIL".into(),
                title: "Hail Mary".into(),
                authors: Some("Andy Weir".into()),
                asin: Some("B00HAIL".into()),
                isbn: None,
                cover_url: Some("https://img.example/a.jpg".into()),
                description: Some("Short".into()),
                subtitle: None,
                narrators: Some("Ray Porter".into()),
                series: None,
                series_index: None,
                publisher: None,
                length_minutes: Some(960),
                published_at: None,
                genres: Some("Sci-Fi".into()),
                language: Some("en".into()),
                store_editions: vec![WishlistStoreEdition {
                    source: "audible".into(),
                    product_id: "B00HAIL".into(),
                }],
                purchase_hints: vec![WishlistPurchaseHint {
                    source: "audible".into(),
                    product_id: "B00HAIL".into(),
                    title: Some("Hail Mary".into()),
                    url: Some("https://audible.example/B00HAIL".into()),
                    ..Default::default()
                }],
                wish_count: 1,
                sample_uuids: vec!["a".into()],
                first_requested_at: now,
                last_requested_at: now,
            },
            GlobalQueueEntry {
                work_key: "isbn:9781234567890".into(),
                title: "Project Hail Mary".into(),
                authors: Some("Andy Weir".into()),
                asin: None,
                isbn: Some("9781234567890".into()),
                cover_url: Some("https://img.example/longer-cover-path.jpg".into()),
                description: Some("A much longer description of the novel.".into()),
                subtitle: Some("A Novel".into()),
                narrators: None,
                series: None,
                series_index: None,
                publisher: Some("Ballantine".into()),
                length_minutes: None,
                published_at: Some("2021-05-04".into()),
                genres: None,
                language: None,
                store_editions: vec![WishlistStoreEdition {
                    source: "libro".into(),
                    product_id: "9781234567890".into(),
                }],
                purchase_hints: vec![WishlistPurchaseHint {
                    source: "libro".into(),
                    product_id: "9781234567890".into(),
                    title: Some("Project Hail Mary".into()),
                    url: Some("https://libro.example/9781234567890".into()),
                    price_cents: Some(1499),
                    currency: Some("USD".into()),
                    price_label: Some("$14.99".into()),
                    ..Default::default()
                }],
                wish_count: 2,
                sample_uuids: vec!["b".into()],
                first_requested_at: now,
                last_requested_at: now,
            },
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].wish_count, 3);
        assert!(merged[0].work_key.starts_with("isbn:"));
        assert_eq!(merged[0].title, "Project Hail Mary");
        assert_eq!(
            merged[0].cover_url.as_deref(),
            Some("https://img.example/longer-cover-path.jpg")
        );
        assert_eq!(
            merged[0].description.as_deref(),
            Some("A much longer description of the novel.")
        );
        assert_eq!(merged[0].subtitle.as_deref(), Some("A Novel"));
        assert_eq!(merged[0].narrators.as_deref(), Some("Ray Porter"));
        assert_eq!(merged[0].publisher.as_deref(), Some("Ballantine"));
        assert_eq!(merged[0].length_minutes, Some(960));
        assert_eq!(merged[0].store_editions.len(), 2);
        assert_eq!(merged[0].purchase_hints.len(), 2);
    }

    #[test]
    fn soft_match_same_title_author() {
        assert!(works_match(
            "Project Hail Mary",
            Some("Andy Weir"),
            "Project Hail Mary: A Novel",
            Some("Andy Weir")
        ));
    }

    #[test]
    fn soft_match_decodes_html_entities_in_title() {
        // Libro.fm often returns `Memory&#39;s Blade` while Audible uses a real apostrophe.
        assert!(works_match(
            "Memory's Blade",
            Some("Eric Warren"),
            "Memory&#39;s Blade",
            Some("Eric Warren"),
        ));
        assert!(identities_match(
            WorkIdentity::new(
                Some("B0FAKEASIN"),
                None,
                "Memory's Blade",
                Some("Eric Warren"),
            ),
            WorkIdentity::new(
                None,
                Some("9781664936256"),
                "Memory&#39;s Blade",
                Some("Eric Warren"),
            ),
        ));
    }

    #[test]
    fn soft_match_rejects_series_volume_subtitles() {
        assert!(!works_match(
            "Infinity Blade: Awakening",
            Some("Brandon Sanderson"),
            "Infinity Blade: Redemption",
            Some("Brandon Sanderson"),
        ));
        // Audible often ships the first volume as the bare series name.
        assert!(!works_match(
            "Infinity Blade",
            Some("Brandon Sanderson"),
            "Infinity Blade: Redemption",
            Some("Brandon Sanderson"),
        ));
        // Without series indexes, bare vs volume stays unmerged (safe).
        assert!(!identities_match(
            WorkIdentity::new(
                Some("B07933B798"),
                None,
                "Infinity Blade",
                Some("Brandon Sanderson"),
            ),
            WorkIdentity::new(
                None,
                Some("9781501992032"),
                "Infinity Blade: Redemption",
                Some("Brandon Sanderson"),
            ),
        ));
        // #1 bare Audible title may join #1 "…: Awakening" from another store.
        assert!(identities_match(
            WorkIdentity::new(
                Some("B07933B798"),
                None,
                "Infinity Blade",
                Some("Brandon Sanderson"),
            )
            .with_series_index(Some("1")),
            WorkIdentity::new(
                None,
                Some("9781501992049"),
                "Infinity Blade: Awakening",
                Some("Brandon Sanderson"),
            )
            .with_series_index(Some("1")),
        ));
        // #1 must not absorb #2 Redemption even with soft title containment.
        assert!(!identities_match(
            WorkIdentity::new(
                Some("B07933B798"),
                None,
                "Infinity Blade",
                Some("Brandon Sanderson"),
            )
            .with_series(Some("Infinity Blade"))
            .with_series_index(Some("1")),
            WorkIdentity::new(
                None,
                Some("9781501992032"),
                "Infinity Blade: Redemption",
                Some("Brandon Sanderson"),
            )
            .with_series(Some("Infinity Blade"))
            .with_series_index(Some("2")),
        ));
        // Crossover novels: same title/author, different series labels+indexes
        // (Audible Beaumont #19 vs Chirp/Libro Brady #14) must still merge.
        assert!(identities_match(
            WorkIdentity::new(
                Some("B002V1M2XE"),
                None,
                "Fire and Ice",
                Some("J. A. Jance"),
            )
            .with_series(Some("J. P. Beaumont"))
            .with_series_index(Some("19")),
            WorkIdentity::new(
                None,
                Some("9780061776670"),
                "Fire and Ice",
                Some("J. A. Jance"),
            )
            .with_series(Some("Joanna Brady Mysteries"))
            .with_series_index(Some("14")),
        ));
        assert!(!identities_match(
            WorkIdentity::new(
                Some("B07933B798"),
                Some("9781501992049"),
                "Infinity Blade: Awakening",
                Some("Brandon Sanderson"),
            ),
            WorkIdentity::new(
                Some("B0798QRTTK"),
                Some("9781501992032"),
                "Infinity Blade: Redemption",
                Some("Brandon Sanderson"),
            ),
        ));
        assert_ne!(
            soft_work_key("Infinity Blade: Awakening", Some("Brandon Sanderson")),
            soft_work_key("Infinity Blade: Redemption", Some("Brandon Sanderson")),
        );
    }

    #[test]
    fn soft_match_rejects_different_author() {
        assert!(!works_match(
            "Project Hail Mary",
            Some("Andy Weir"),
            "Project Hail Mary",
            Some("Someone Else")
        ));
    }

    #[test]
    fn soft_match_authors_order_independent() {
        // Chirp/Libro often emit the same co-authors in opposite order as one string.
        assert!(works_match(
            "Game of Thrones and Philosophy",
            Some("William Irwin & Henry Jacoby"),
            "Game of Thrones and Philosophy",
            Some("Henry Jacoby & William Irwin"),
        ));
        assert!(identities_match(
            WorkIdentity::new(
                None,
                Some("9781118160930"),
                "Game of Thrones and Philosophy",
                Some("William Irwin; Henry Jacoby"),
            ),
            WorkIdentity::new(
                None,
                None,
                "Game of Thrones and Philosophy",
                Some("Henry Jacoby & William Irwin"),
            ),
        ));
    }

    #[test]
    fn soft_match_author_role_suffix_and_subset() {
        // Audible lists editor + foreword; Libro may only list the editor.
        assert!(works_match(
            "Game of Thrones Psychology",
            Some("Travis Langley - editor, Kyle Maddock - foreword"),
            "Game of Thrones Psychology",
            Some("Travis Langley"),
        ));
        // Distinct primary authors with no shared set still refuse (sparse metadata).
        assert!(!works_match(
            "Game of Thrones Psychology",
            Some("Travis Langley"),
            "Game of Thrones Psychology",
            Some("Kyle Maddock"),
        ));
    }

    #[test]
    fn soft_match_rejects_authorless_loose_containment() {
        // GraphicAudio / Magento often return authorless hits; a shared token must
        // not collapse unrelated titles onto a Ruocchio wishlist item.
        assert!(!works_match(
            "Ashes of Man",
            Some("Christopher Ruocchio"),
            "Red Rising Saga 1: Red Rising 1 of 2",
            None
        ));
        assert!(!works_match(
            "Ashes of Man",
            Some("Christopher Ruocchio"),
            "The Sun Eater Saga (Series Set)",
            None
        ));
    }

    #[test]
    fn soft_match_allows_authorless_near_exact_title() {
        assert!(works_match(
            "Ashes of Man",
            Some("Christopher Ruocchio"),
            "Ashes of Man",
            None
        ));
    }

    #[test]
    fn merge_keeps_editions_per_source() {
        let mut editions = vec![StoreEdition::new("audible", "B00A")];
        push_edition(&mut editions, StoreEdition::new("libro", "9781"));
        push_edition(&mut editions, StoreEdition::new("audible", "B00B"));
        assert_eq!(editions.len(), 2);
        assert_eq!(editions[1].source, "libro");
    }
}
