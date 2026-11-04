//! Title and author cleaning for compares.
//!
//! Port of `cleanTitleForCompares` and `cleanAuthorForCompares` from
//! audiobookshelf (`server/finders/BookFinder.js`), moved here from
//! `bookclerk-enrich`'s `match_score` so the resolver's work matcher and the
//! enrichment score clean text the same way.
//!
//! Because it derives from audiobookshelf, this module is licensed under
//! GPL-3.0-only rather than the workspace's AGPL-3.0-or-later.

use unicode_normalization::UnicodeNormalization;

/// True when the title already embeds a subtitle after `: ` or ` - `.
///
/// # Arguments
///
/// * `title` - Display title.
///
/// # Returns
///
/// True when a subtitle separator is present.
#[must_use]
pub fn has_subtitle(title: &str) -> bool {
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

/// Cleans a title for compares (AudioBookshelf `cleanTitleForCompares`).
///
/// The caller decodes HTML entities first; this function treats `&amp;`
/// as literal text.
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
pub fn clean_title(title: &str, keep_subtitle: bool) -> String {
    if title.is_empty() {
        return String::new();
    }
    let title = strip_redundant_spaces(title);
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

/// Cleans an author name for compares (AudioBookshelf `cleanAuthorForCompares`).
///
/// The caller decodes HTML entities first.
///
/// # Arguments
///
/// * `author` - String `author` for this call.
///
/// # Returns
///
/// String result for this operation.
#[must_use]
pub fn clean_author(author: &str) -> String {
    if author.is_empty() {
        return String::new();
    }
    let author = strip_redundant_spaces(author);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn author_ascii_transforms_preserve_utf8() {
        assert_eq!(separate_initials("josé a.b. author"), "josé a. b. author");
        assert_eq!(strip_et_al("José Author et al."), "José Author");
        assert_eq!(strip_et_al("Søren Kierkegaard et al."), "Søren Kierkegaard");
    }

    #[test]
    fn cleans_title_parentheses_and_subtitle() {
        assert_eq!(clean_title("Dune (Unabridged): Book One", false), "dune");
        assert_eq!(
            clean_title("Dune (Unabridged): Book One", true),
            "dune : book one"
        );
        assert_eq!(clean_title("Ender's Game", true), "enders game");
        assert_eq!(clean_title("Les Misérables", true), "les miserables");
        assert_eq!(clean_title("", true), "");
    }

    #[test]
    fn cleans_author_initials_and_et_al() {
        assert_eq!(clean_author("J.R.R. Tolkien"), "j. r. r. tolkien");
        assert_eq!(clean_author("George R. R. Martin"), "george martin");
        assert_eq!(clean_author("Jane Doe et al."), "jane doe");
        assert_eq!(clean_author("Émile  Zola"), "emile zola");
        assert_eq!(clean_author(""), "");
    }
}
