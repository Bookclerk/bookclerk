//! Lucene-style query normalization for classic Libation search syntax.

/// Normalize a classic Libation/Lucene query for Tantivy's parser.
///
/// Handles common shortcuts: `-liberated`, `[tag]`, field synonyms.
#[must_use]
pub fn normalize_lucene_query(input: &str) -> String {
    let mut out = input.trim().to_string();
    if out.is_empty() {
        return out;
    }

    // Tag shortcuts: [auto_bio] -> tags:auto_bio
    let mut normalized = String::new();
    let mut chars = out.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '[' {
            let mut tag = String::new();
            while let Some(&next) = chars.peek() {
                chars.next();
                if next == ']' {
                    break;
                }
                tag.push(next);
            }
            if !tag.is_empty() {
                normalized.push_str("tags:");
                normalized.push_str(&tag);
            }
            continue;
        }
        normalized.push(ch);
    }
    out = normalized;

    // Classic boolean shortcuts on liberated/finished fields.
    for (from, to) in [
        ("-liberated", "liberated:false"),
        (" NOT liberated", " liberated:false"),
        (" liberated", " liberated:true"),
        ("-finished", "finished:false"),
        (" finished", " finished:true"),
    ] {
        out = out.replace(from, to);
    }

    // Field synonyms (classic IndexRuleCollection aliases).
    for (from, to) in [
        ("author:", "authors:"),
        ("narrator:", "narrators:"),
        ("asin:", "id:"),
        ("productid:", "id:"),
    ] {
        out = out.replace(from, to);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_brackets_become_field_query() {
        assert_eq!(normalize_lucene_query("[bio]"), "tags:bio");
    }

    #[test]
    fn negated_liberated_shortcut() {
        assert_eq!(normalize_lucene_query("-liberated"), "liberated:false");
    }
}
