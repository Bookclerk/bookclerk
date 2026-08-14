//! AudioBookshelf-style metadata match confidence scoring.
//!
//! Port of `BookFinder.calculateMatchConfidence` from audiobookshelf
//! (`server/finders/BookFinder.js`), including title/author cleaning and
//! Levenshtein similarity, plus Libro-aware ISBN / narrator signals.

use unicode_normalization::UnicodeNormalization;

/// Weights used by AudioBookshelf (duration dominates).
const W_DURATION: f64 = 0.7;
/// Title similarity weight when narrator is unused (0.2; duration remains 0.7).
const W_TITLE: f64 = 0.2;
/// Author similarity weight when narrator is unused (0.1).
const W_AUTHOR: f64 = 0.1;

/// When both query and candidate have narrators, fold a light narrator weight in
/// (Libro often has this; ABS scoring does not).
const W_DURATION_WITH_NARRATOR: f64 = 0.65;
/// Title similarity weight when both sides have a narrator (0.18).
const W_TITLE_WITH_NARRATOR: f64 = 0.18;
/// Author similarity weight when both sides have a narrator (0.09).
const W_AUTHOR_WITH_NARRATOR: f64 = 0.09;
/// Narrator similarity weight folded in when both query and candidate name a narrator (0.08).
const W_NARRATOR: f64 = 0.08;

/// On exact ISBN match, close this fraction of the remaining gap to 1.0.
///
/// Not a hard accept: multiple Audible ASINs can share one ISBN (abridged vs
/// unabridged, marketplace variants, etc.), so duration/title/author still matter.
const ISBN_MATCH_GAP_CLOSE: f64 = 0.55;

/// Owned-title (e.g. Libro) metadata used as the match query.
#[derive(Debug, Clone, Default)]
pub struct MatchQuery<'a> {
    /// Display title as shown on the storefront or library card.
    pub title: &'a str,
    /// Optional subtitle when the catalog distinguishes it from the title.
    pub subtitle: Option<&'a str>,
    /// Primary author name used for fuzzy matching.
    pub author: Option<&'a str>,
    /// Primary narrator name used for fuzzy matching.
    pub narrator: Option<&'a str>,
    /// Canonical ISBN-13 (or ISBN-10 normalized) when published.
    pub isbn: Option<&'a str>,
    /// Candidate runtime in minutes for scoring against the seed.
    pub duration_minutes: Option<f64>,
}

/// Candidate metadata used for confidence scoring.
#[derive(Debug, Clone)]
pub struct ScoreInput<'a> {
    /// Display title as shown on the storefront or library card.
    pub title: &'a str,
    /// Optional subtitle when the catalog distinguishes it from the title.
    pub subtitle: Option<&'a str>,
    /// Primary author name used for fuzzy matching.
    pub author: Option<&'a str>,
    /// Primary narrator name used for fuzzy matching.
    pub narrator: Option<&'a str>,
    /// Canonical ISBN-13 (or ISBN-10 normalized) when published.
    pub isbn: Option<&'a str>,
    /// Runtime in minutes (Audible / Audnexus `runtimeLengthMin`).
    pub duration_minutes: Option<f64>,
}

/// Calculate match confidence in `[0.0, 1.0]`.
///
/// Base score follows AudioBookshelf (duration / title / author). When Libro
/// (or other) query metadata is richer:
/// - **Narrator** (both sides present): small extra weight in the blend
/// - **Exact ISBN** (normalized digits): boost by closing
///   [`ISBN_MATCH_GAP_CLOSE`] of the remaining gap to 1.0 (not a forced accept)
///
/// When the query title is an ASIN, returns `1.0`.
///
/// # Arguments
///
/// * `book` - Library book row to update.
/// * `query` - Query vector or free-text search string.
///
/// # Returns
///
/// `f64` result.
#[must_use]
pub fn calculate_match_confidence(book: &ScoreInput<'_>, query: &MatchQuery<'_>) -> f64 {
    let query_title = composed_title(query.title, query.subtitle);
    let title_is_asin = is_valid_asin(&query_title.to_ascii_uppercase());
    if title_is_asin {
        return 1.0;
    }

    let duration_score = match (query.duration_minutes, book.duration_minutes) {
        (Some(lib_mins), Some(book_mins)) => {
            let duration_diff = (book_mins - lib_mins).abs();
            if duration_diff <= 1.0 {
                1.0
            } else if duration_diff <= 5.0 {
                1.1 - 0.1 * duration_diff
            } else if duration_diff <= 10.0 {
                1.2 - 0.12 * duration_diff
            } else {
                0.0
            }
        }
        _ => 0.1,
    };

    let title_query_has_subtitle = has_subtitle(&query_title) || query.subtitle.is_some();
    let title_score = title_similarity(&query_title, book, title_query_has_subtitle);
    let author_score = people_similarity(
        query.author,
        book.author,
        /*empty_query_neutral=*/ true,
    );

    let query_has_narrator = query
        .narrator
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some();
    let book_has_narrator = book
        .narrator
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some();

    let mut confidence = if query_has_narrator && book_has_narrator {
        let narrator_score = people_similarity(
            query.narrator,
            book.narrator,
            /*empty_query_neutral=*/ false,
        );
        W_DURATION_WITH_NARRATOR * duration_score
            + W_TITLE_WITH_NARRATOR * title_score
            + W_AUTHOR_WITH_NARRATOR * author_score
            + W_NARRATOR * narrator_score
    } else {
        W_DURATION * duration_score + W_TITLE * title_score + W_AUTHOR * author_score
    };

    if isbn_exact_match(query.isbn, book.isbn) {
        // Pull toward 1.0 without ignoring other signals (multi-ASIN ISBN risk).
        confidence += (1.0 - confidence) * ISBN_MATCH_GAP_CLOSE;
    }

    confidence.clamp(0.0, 1.0)
}

/// Digits-only ISBN for exact matching (strips hyphens / spaces / `ISBN` prefix).
///
/// # Arguments
///
/// * `raw` - String `raw` for this call.
///
/// # Returns
///
/// String result for this operation.
#[must_use]
pub fn normalize_isbn(raw: &str) -> String {
    let trimmed = raw.trim();
    let without_prefix = trimmed
        .strip_prefix("ISBN-13:")
        .or_else(|| trimmed.strip_prefix("ISBN-10:"))
        .or_else(|| trimmed.strip_prefix("ISBN:"))
        .or_else(|| trimmed.strip_prefix("isbn:"))
        .unwrap_or(trimmed)
        .trim();
    without_prefix
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == 'X' || *c == 'x')
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Normalize then convert ISBN-10 → ISBN-13 (`978…`) when possible.
///
/// ISBN is **not** available from every storefront (Chirp / GraphicAudio / Audible
/// public catalog often omit it). When present, prefer this canonical form so
/// 10- and 13-digit variants of the same book share one key.
///
/// # Arguments
///
/// * `raw` - String `raw` for this call.
///
/// # Returns
///
/// String result for this operation.
#[must_use]
pub fn canonicalize_isbn(raw: &str) -> String {
    let n = normalize_isbn(raw);
    if n.len() == 13 && (n.starts_with("978") || n.starts_with("979")) {
        return n;
    }
    if n.len() == 10 {
        let core = &n[..9];
        if !core.chars().all(|c| c.is_ascii_digit()) {
            return n;
        }
        let mut body = String::from("978");
        body.push_str(core);
        let mut sum = 0u32;
        for (i, c) in body.chars().enumerate() {
            let d = c.to_digit(10).unwrap_or(0);
            sum += if i % 2 == 0 { d } else { d * 3 };
        }
        let check = (10 - (sum % 10)) % 10;
        body.push(char::from_digit(check, 10).unwrap_or('0'));
        return body;
    }
    n
}

/// Returns true when both sides normalize to the same ISBN-13 (or matching ISBN-10).
///
/// # Arguments
///
/// * `query_isbn` - String `query_isbn` for this call.
/// * `book_isbn` - String `book_isbn` for this call.
///
/// # Returns
///
/// `true` when the predicate holds.
#[must_use]
pub fn isbn_exact_match(query_isbn: Option<&str>, book_isbn: Option<&str>) -> bool {
    let Some(q) = query_isbn.map(canonicalize_isbn).filter(|s| !s.is_empty()) else {
        return false;
    };
    let Some(b) = book_isbn.map(canonicalize_isbn).filter(|s| !s.is_empty()) else {
        return false;
    };
    q == b
}

/// Joins title and subtitle with `: ` unless the subtitle is already contained in the title.
fn composed_title(title: &str, subtitle: Option<&str>) -> String {
    let title = title.trim();
    let Some(sub) = subtitle.map(str::trim).filter(|s| !s.is_empty()) else {
        return title.to_string();
    };
    if title.is_empty() {
        return sub.to_string();
    }
    if title
        .to_ascii_lowercase()
        .contains(&sub.to_ascii_lowercase())
    {
        title.to_string()
    } else {
        format!("{title}: {sub}")
    }
}

/// Best Levenshtein score of a cleaned query name against a candidate or its comma-separated parts.
fn people_similarity(
    query: Option<&str>,
    candidate: Option<&str>,
    empty_query_neutral: bool,
) -> f64 {
    let norm_query = clean_author_for_compares(query.unwrap_or(""));
    if norm_query.is_empty() {
        return if empty_query_neutral { 1.0 } else { 0.0 };
    }
    let norm_candidate = clean_author_for_compares(candidate.unwrap_or(""));
    if norm_candidate.is_empty() {
        return 0.0;
    }
    let parts: Vec<String> = norm_candidate
        .split(',')
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|p| !p.is_empty())
        .collect();
    if parts.is_empty() {
        return 0.0;
    }
    let mut max_part_score = levenshtein_similarity(&norm_query, &norm_candidate);
    if parts.len() > 1 || norm_candidate.contains(',') {
        for part in &parts {
            max_part_score = max_part_score.max(levenshtein_similarity(&norm_query, part));
        }
    }
    max_part_score
}

/// Levenshtein similarity of cleaned titles, optionally keeping subtitle text.
fn title_similarity(title_query: &str, book: &ScoreInput<'_>, keep_subtitle: bool) -> f64 {
    let clean_title = clean_title_for_compares(book.title, keep_subtitle);
    let clean_subtitle = if keep_subtitle {
        book.subtitle
            .filter(|s| !s.is_empty())
            .map(|s| format!(": {s}"))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let norm_book_title = format!("{clean_title}{clean_subtitle}");
    let norm_title_query = clean_title_for_compares(title_query, keep_subtitle);
    levenshtein_similarity(&norm_title_query, &norm_book_title)
}

/// True when the title already embeds a subtitle after `: ` or ` - `.
fn has_subtitle(title: &str) -> bool {
    title.contains(": ") || title.contains(" - ")
}

/// Drops the first `: ` or ` - ` suffix so compares can ignore subtitles.
fn strip_subtitle(title: &str) -> String {
    if let Some((left, _)) = title.split_once(": ") {
        left.trim().to_string()
    } else if let Some((left, _)) = title.split_once(" - ") {
        left.trim().to_string()
    } else {
        title.to_string()
    }
}

/// NFD-decomposes and strips combining marks so accented letters compare as ASCII.
fn replace_accented_chars(s: &str) -> String {
    s.nfd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .collect()
}

/// Collapses Unicode whitespace runs to a single ASCII space.
fn strip_redundant_spaces(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Clean a title for fuzzy compares (AudioBookshelf `cleanTitleForCompares`).
///
/// # Arguments
///
/// * `title` - Display title.
/// * `keep_subtitle` - When true, retain subtitle text for soft compares.
///
/// # Returns
///
/// String result for this operation.
#[must_use]
pub fn clean_title_for_compares(title: &str, keep_subtitle: bool) -> String {
    if title.is_empty() {
        return String::new();
    }
    // Libro (and others) may ship entity-encoded titles; decode before compare.
    let title = bookclerk_library::decode_html_entities_cow(title);
    let title = strip_redundant_spaces(title.as_ref());
    let stripped = if keep_subtitle {
        title
    } else {
        strip_subtitle(&title)
    };
    // Remove parenthetical content (ABS: /\([^)]*\)/g).
    let mut without_parens = String::with_capacity(stripped.len());
    let mut chars = stripped.chars();
    while let Some(c) = chars.next() {
        if c == '(' {
            for nc in chars.by_ref() {
                if nc == ')' {
                    break;
                }
            }
        } else {
            without_parens.push(c);
        }
    }
    let cleaned = strip_redundant_spaces(&without_parens).replace('\'', "");
    replace_accented_chars(&cleaned).to_ascii_lowercase()
}

/// Clean an author string for fuzzy compares (AudioBookshelf `cleanAuthorForCompares`).
///
/// # Arguments
///
/// * `author` - String `author` for this call.
///
/// # Returns
///
/// String result for this operation.
#[must_use]
pub fn clean_author_for_compares(author: &str) -> String {
    if author.is_empty() {
        return String::new();
    }
    let author = bookclerk_library::decode_html_entities_cow(author);
    let author = strip_redundant_spaces(author.as_ref());
    let mut clean = replace_accented_chars(&author).to_ascii_lowercase();
    clean = separate_initials(&clean);
    clean = strip_middle_initials(&clean);
    clean = strip_et_al(&clean);
    clean
}

/// Inserts a space after `x.y` so initials tokenize like AudioBookshelf (`j.k.` → `j. k.`).
fn separate_initials(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() + 4);
    let mut i = 0;
    while i < bytes.len() {
        out.push(bytes[i]);
        if bytes[i].is_ascii_lowercase()
            && i + 2 < bytes.len()
            && bytes[i + 1] == b'.'
            && bytes[i + 2].is_ascii_lowercase()
        {
            out.push(b'.');
            out.push(b' ');
            i += 2;
            continue;
        }
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// Removes middle initials between a surname-like token and a following two-letter word.
fn strip_middle_initials(s: &str) -> String {
    let b = s.as_bytes();
    let is_word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0usize;
    while i < b.len() {
        let mut removed = false;
        if i >= 2 && is_word(b[i - 1]) && is_word(b[i - 2]) {
            // Collect end offsets of each `\s+[a-z]\.?` repetition so we can
            // backtrack like JS when the `\s+\w\w` lookahead fails.
            let mut ends = Vec::new();
            let mut j = i;
            loop {
                let before = j;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j == before || j >= b.len() || !b[j].is_ascii_lowercase() {
                    break;
                }
                j += 1;
                if j < b.len() && b[j] == b'.' {
                    j += 1;
                }
                ends.push(j);
            }
            for &end in ends.iter().rev() {
                let mut k = end;
                let before_ws = k;
                while k < b.len() && b[k].is_ascii_whitespace() {
                    k += 1;
                }
                if k > before_ws && k + 1 < b.len() && is_word(b[k]) && is_word(b[k + 1]) {
                    i = end;
                    removed = true;
                    break;
                }
            }
        }
        if !removed {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// Drops a trailing ` et al` / ` et al.` suffix used on multi-author credits.
fn strip_et_al(s: &str) -> String {
    const SUFFIX: &[u8] = b" et al";
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(SUFFIX) {
            let mut j = i + SUFFIX.len();
            if j < bytes.len() && bytes[j] == b'.' {
                j += 1;
            }
            if j == bytes.len() || bytes[j] == b' ' {
                i = j;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// Levenshtein distance (case-insensitive by default, matching ABS).
///
/// # Arguments
///
/// * `a` - Left-hand vector.
/// * `b` - Right-hand vector.
///
/// # Returns
///
/// `usize` result.
#[must_use]
pub fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.to_lowercase().chars().collect();
    let b: Vec<char> = b.to_lowercase().chars().collect();
    let (m, n) = (a.len(), b.len());
    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr = vec![0; n + 1];
    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

/// Similarity in `[0.0, 1.0]` from Levenshtein distance.
///
/// # Arguments
///
/// * `a` - Left-hand vector.
/// * `b` - Right-hand vector.
///
/// # Returns
///
/// `f64` result.
#[must_use]
pub fn levenshtein_similarity(a: &str, b: &str) -> f64 {
    let distance = levenshtein_distance(a, b);
    let max_len = a.chars().count().max(b.chars().count());
    if max_len == 0 {
        1.0
    } else {
        1.0 - (distance as f64 / max_len as f64)
    }
}

/// True when `s` looks like an Audible ASIN (`^[A-Z0-9]{10}$`, case-insensitive).
///
/// Includes all-numeric catalog ids (e.g. `1094100765`); Audnexus accepts these
/// and older Audible storefront ASINs are often numeric.
///
/// # Arguments
///
/// * `s` - Input string to validate or normalize.
///
/// # Returns
///
/// `true` when the predicate holds.
#[must_use]
pub fn is_valid_asin(s: &str) -> bool {
    let s = s.trim();
    s.len() == 10 && s.chars().all(|c| c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query<'a>(title: &'a str, author: Option<&'a str>, duration: Option<f64>) -> MatchQuery<'a> {
        MatchQuery {
            title,
            author,
            duration_minutes: duration,
            ..Default::default()
        }
    }

    #[test]
    fn exact_title_author_duration_is_high_confidence() {
        let book = ScoreInput {
            title: "Forward the Foundation",
            subtitle: None,
            author: Some("Isaac Asimov"),
            narrator: None,
            isbn: None,
            duration_minutes: Some(970.0),
        };
        let c = calculate_match_confidence(
            &book,
            &query("Forward the Foundation", Some("Isaac Asimov"), Some(970.0)),
        );
        assert!(c >= 0.99, "confidence={c}");
    }

    #[test]
    fn large_duration_gap_lowers_score() {
        let book = ScoreInput {
            title: "Forward the Foundation",
            subtitle: None,
            author: Some("Isaac Asimov"),
            narrator: None,
            isbn: None,
            duration_minutes: Some(970.0),
        };
        let c = calculate_match_confidence(
            &book,
            &query("Forward the Foundation", Some("Isaac Asimov"), Some(200.0)),
        );
        // Duration weight 0.7 → score ≈ 0.3 even with perfect title/author.
        assert!(c < 0.35, "confidence={c}");
        assert!(c >= 0.29, "confidence={c}");
    }

    #[test]
    fn isbn_exact_match_boosts_but_does_not_force_perfect() {
        let book = ScoreInput {
            title: "Forward the Foundation",
            subtitle: None,
            author: Some("Isaac Asimov"),
            narrator: None,
            isbn: Some("978-0-307-97062-6"),
            duration_minutes: Some(970.0),
        };
        let mut q = query("Forward the Foundation", Some("Isaac Asimov"), Some(200.0));
        let without = calculate_match_confidence(&book, &q);
        q.isbn = Some("9780307970626");
        let with = calculate_match_confidence(&book, &q);
        assert!(with > without, "with={with} without={without}");
        assert!(
            with < 1.0,
            "ISBN must not force 1.0 (multi-ASIN risk); got {with}"
        );
        // Closes 55% of the remaining gap.
        let expected = without + (1.0 - without) * ISBN_MATCH_GAP_CLOSE;
        assert!(
            (with - expected).abs() < 1e-9,
            "with={with} expected={expected}"
        );
    }

    #[test]
    fn matching_narrator_raises_score_when_duration_weak() {
        let book = ScoreInput {
            title: "Some Title",
            subtitle: None,
            author: Some("Ann Author"),
            narrator: Some("Larry McKeever"),
            isbn: None,
            duration_minutes: Some(400.0),
        };
        let mut q = query("Some Title", Some("Ann Author"), Some(390.0));
        let without = calculate_match_confidence(&book, &q);
        q.narrator = Some("Larry McKeever");
        let with = calculate_match_confidence(&book, &q);
        assert!(with >= without, "with={with} without={without}");
    }

    #[test]
    fn asin_query_is_perfect() {
        let book = ScoreInput {
            title: "Anything",
            subtitle: None,
            author: None,
            narrator: None,
            isbn: None,
            duration_minutes: None,
        };
        assert_eq!(
            calculate_match_confidence(&book, &query("B005WWT30E", None, None)),
            1.0
        );
    }

    #[test]
    fn cleans_title_parentheses_and_subtitle() {
        assert_eq!(
            clean_title_for_compares("Ender's Game (Ender's Saga)", false),
            "enders game"
        );
        assert_eq!(
            clean_title_for_compares("Cool Book: Coolest Ever", false),
            "cool book"
        );
    }

    #[test]
    fn cleans_author_middle_initials() {
        assert_eq!(
            clean_author_for_compares("John R. R. Tolkien"),
            "john tolkien"
        );
    }

    #[test]
    fn author_ascii_transforms_preserve_utf8() {
        assert_eq!(separate_initials("josé a.b. author"), "josé a. b. author");
        assert_eq!(strip_et_al("José Author et al."), "José Author");
        assert_eq!(strip_et_al("Søren Kierkegaard et al."), "Søren Kierkegaard");
    }

    #[test]
    fn normalize_isbn_strips_hyphens() {
        assert_eq!(normalize_isbn("978-1-234-56789-0"), "9781234567890");
        assert_eq!(normalize_isbn("ISBN: 9781234567890"), "9781234567890");
    }

    #[test]
    fn valid_asin_accepts_alphanumeric_and_numeric_ids() {
        assert!(is_valid_asin("B005WWT30E"));
        assert!(is_valid_asin("1094100765"));
        assert!(!is_valid_asin("444622"));
        assert!(!is_valid_asin("B005WWT30"));
        assert!(!is_valid_asin("B005-WWT30"));
    }
}
