//! Splitting a store's display string of contributor names.

/// Separator between names in a flat display string, and between a name and
/// its suffix (`Martin Luther King, Jr.`).
pub const NAME_SEPARATOR: &str = ", ";

/// Name suffixes that belong to the name before them when a display string
/// is split on commas (`Martin Luther King, Jr.`).
const NAME_SUFFIXES: &[&str] = &[
    "jr", "jr.", "sr", "sr.", "ii", "iii", "iv", "phd", "ph.d.", "md", "m.d.", "esq", "esq.",
];

/// Splits a display string of contributor names into one name per entry.
///
/// Splits on `,`, `;`, `&` and the word `and`, and keeps a name suffix such
/// as `Jr.` with the name before it. The caller decodes HTML entities first.
///
/// # Arguments
///
/// * `display` - Names as one string (`"Jane Doe, John Roe & Ann Poe"`).
///
/// # Returns
///
/// The trimmed, non-empty names in the order they appear.
#[must_use]
pub fn split_display_names(display: &str) -> Vec<String> {
    // Any run of whitespace (tabs, newlines, no-break spaces) reads as one
    // space, so the word `and` splits however the store spaced it.
    let display = display.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut names: Vec<String> = Vec::new();
    for part in display.split([',', ';', '&']) {
        for piece in split_on_and(part) {
            let piece = piece.trim();
            if piece.is_empty() {
                continue;
            }
            let is_suffix = NAME_SUFFIXES.contains(&piece.to_ascii_lowercase().as_str());
            match names.last_mut() {
                Some(previous) if is_suffix => {
                    previous.push_str(NAME_SEPARATOR);
                    previous.push_str(piece);
                }
                _ => names.push(piece.to_string()),
            }
        }
    }
    names
}

/// Splits on the standalone word `and`, matched without regard to case.
fn split_on_and(part: &str) -> Vec<&str> {
    let lower = part.to_ascii_lowercase();
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut search = 0;
    while let Some(found) = lower[search..].find(" and ") {
        let at = search + found;
        pieces.push(&part[start..at]);
        start = at + " and ".len();
        search = start;
    }
    pieces.push(&part[start..]);
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_lists_and_keeps_suffixes() {
        assert_eq!(
            split_display_names("Jane Doe, John Roe & Ann Poe and  Bo Li; Martin King, Jr."),
            vec![
                "Jane Doe",
                "John Roe",
                "Ann Poe",
                "Bo Li",
                "Martin King, Jr."
            ]
        );
        assert!(split_display_names(" , ").is_empty());
    }
}
