//! Minimal port of the `NameParser` (Python `nameparser` / C# `NameParser`) logic
//! sufficient for Bookclerk's `{T}{F}{M}{L}{S}` contributor formatting.
//!
//! This is not a full port of every edge case in `nameparser`; it handles the
//! common Western name shapes (first/middle/last with compound last-name
//! prefixes, leading titles and trailing suffixes) that Bookclerk relies on.

/// Lower-cased last-name prefixes / conjunction particles that attach to the
/// following word(s) to form a compound surname (e.g. `de Mesquita`,
/// `Van Doren`, `Bon Jovi`).
const PREFIXES: &[&str] = &[
    "abu", "bin", "bon", "da", "dal", "de", "del", "dela", "della", "delle", "delli", "dello",
    "der", "di", "do", "dos", "du", "ibn", "la", "le", "san", "santa", "st", "ste", "van", "vel",
    "von", "den", "ter", "ten",
];

/// Lower-cased (punctuation-stripped) name suffixes.
const SUFFIXES: &[&str] = &[
    "jr", "sr", "ii", "iii", "iv", "v", "phd", "md", "esq", "esquire", "ret", "jd", "dds", "dvm",
];

/// Lower-cased (punctuation-stripped) leading titles.
const TITLES: &[&str] = &[
    "dr",
    "mr",
    "mrs",
    "ms",
    "miss",
    "sir",
    "dame",
    "professor",
    "prof",
    "col",
    "lt",
    "gen",
    "sgt",
    "capt",
    "rev",
    "hon",
    "pres",
    "gov",
];

#[derive(Debug, Clone, Default)]
pub(crate) struct HumanName {
    pub title: String,
    pub first: String,
    pub middle: String,
    pub last: String,
    pub suffix: String,
}

fn strip_punct(word: &str) -> String {
    word.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Mirror of `ContributorDto.RemoveSuffix`.
fn remove_suffix_marker(name: &str) -> String {
    let name = name.replace('\u{2019}', "'").replace(" - Ret.", ", Ret.");
    match name.find(" - ") {
        Some(idx) if idx > 0 => name[..idx].trim().to_string(),
        _ => name.trim().to_string(),
    }
}

impl HumanName {
    pub fn parse(raw: &str) -> Self {
        let cleaned = remove_suffix_marker(raw);

        // Comma handling: "Last, First Middle" or "First Last, Suffix".
        let (mut lead, comma_tail): (String, Option<String>) = match cleaned.split_once(',') {
            Some((head, tail)) => (head.trim().to_string(), Some(tail.trim().to_string())),
            None => (cleaned.clone(), None),
        };

        let mut result = HumanName::default();
        let mut suffix_parts: Vec<String> = Vec::new();

        // If there's a comma and the tail is a suffix, treat as suffix; otherwise
        // it's "Last, First" ordering.
        if let Some(tail) = comma_tail {
            let tail_tokens: Vec<&str> = tail.split_whitespace().collect();
            let all_suffix = !tail_tokens.is_empty()
                && tail_tokens
                    .iter()
                    .all(|t| SUFFIXES.contains(&strip_punct(t).as_str()));
            if all_suffix {
                for t in tail_tokens {
                    suffix_parts.push(t.trim_end_matches('.').to_string());
                }
            } else {
                // "Last, First Middle" -> reorder into "First Middle Last"
                lead = format!("{tail} {lead}");
            }
        }

        let mut tokens: Vec<String> = lead.split_whitespace().map(str::to_string).collect();

        // Leading titles.
        while tokens.len() > 1 {
            let t = strip_punct(&tokens[0]);
            if TITLES.contains(&t.as_str()) {
                if !result.title.is_empty() {
                    result.title.push(' ');
                }
                result.title.push_str(&tokens.remove(0));
            } else {
                break;
            }
        }

        // Trailing suffixes.
        while tokens.len() > 1 {
            let t = strip_punct(tokens.last().unwrap());
            if SUFFIXES.contains(&t.as_str()) {
                let tok = tokens.pop().unwrap();
                suffix_parts.insert(0, tok.trim_end_matches('.').to_string());
            } else {
                break;
            }
        }
        if !suffix_parts.is_empty() {
            result.suffix = suffix_parts.join(" ");
        }

        if tokens.is_empty() {
            return result;
        }
        if tokens.len() == 1 {
            // Single name parses as first name (Prefer.FirstOverPrefix).
            result.first = tokens.remove(0);
            return result;
        }

        // Determine the last-name run: final token plus any preceding prefix
        // particles, but never consuming the first token.
        let mut last_start = tokens.len() - 1;
        while last_start > 1 {
            let candidate = strip_punct(&tokens[last_start - 1]);
            if PREFIXES.contains(&candidate.as_str()) {
                last_start -= 1;
            } else {
                break;
            }
        }

        result.first = tokens[0].clone();
        result.last = tokens[last_start..].join(" ");
        if last_start > 1 {
            result.middle = tokens[1..last_start].join(" ");
        }

        result
    }
}
