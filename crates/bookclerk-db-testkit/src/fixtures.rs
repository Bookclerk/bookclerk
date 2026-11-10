//! Realistic row contents sized against a capability advertisement.
//!
//! Short ASCII test rows never reach a cap. These helpers build text the
//! way real catalogs do (multibyte descriptions, wide JSON documents) and
//! size it from the advertisement, so a test that passes under
//! [`crate::Profile::Minimum`] has exercised the limits.

use bookclerk_plugin_abi::DbCapabilities;

/// One sentence of mixed-script text, 86 bytes of UTF-8 in 60 characters.
const SAMPLE: &str = "Ängström läste Ñandú-böcker 書籍 about le rêve 📚 Ω ";

/// Multibyte text of at most `max_bytes` UTF-8 bytes, cut on a character
/// boundary.
///
/// Repeats a mixed-script sentence so byte and character counts differ:
/// a cap counted in characters instead of bytes lets this through.
#[must_use]
pub fn multibyte_text(max_bytes: usize) -> String {
    let mut out = String::with_capacity(max_bytes);
    for ch in SAMPLE.chars().cycle() {
        if out.len() + ch.len_utf8() > max_bytes {
            break;
        }
        out.push(ch);
    }
    out
}

/// A description that fits one cell with `headroom` bytes to spare under
/// `caps.max_cell_bytes`.
#[must_use]
pub fn description_near_cell_cap(caps: &DbCapabilities, headroom: usize) -> String {
    let cap = usize::try_from(caps.max_cell_bytes).unwrap_or(usize::MAX);
    multibyte_text(cap.saturating_sub(headroom))
}

/// A description that is one byte over `caps.max_cell_bytes`.
///
/// Padded with ASCII, so the length is exact.
#[must_use]
pub fn description_over_cell_cap(caps: &DbCapabilities) -> String {
    let cap = usize::try_from(caps.max_cell_bytes).unwrap_or(usize::MAX);
    let mut out = multibyte_text(cap.saturating_sub(16));
    while out.len() <= cap {
        out.push('x');
    }
    out
}

/// A JSON object of about `bytes` bytes keyed by `key`: a title, a
/// multibyte description and ASCII padding to the size.
///
/// `key` and the title are serialized as JSON strings, so a key holding a
/// quote or a backslash still produces a valid document with that key.
#[must_use]
pub fn wide_json(key: &str, bytes: usize) -> String {
    let head = format!(
        "{{\"key\":{},\"title\":{},\"description\":\"",
        json_string(key),
        json_string(&key.to_uppercase())
    );
    let tail = "\",\"pad\":\"\"}";
    let budget = bytes.saturating_sub(head.len() + tail.len());
    let description = multibyte_text(budget / 2);
    let pad = "x".repeat(budget.saturating_sub(description.len()));
    format!("{head}{description}\",\"pad\":\"{pad}\"}}")
}

/// `text` as a quoted, escaped JSON string.
fn json_string(text: &str) -> String {
    // A string always serializes; the fallback keeps the function total
    // without an `expect`.
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string())
}

/// `count` `(key, json)` rows whose JSON together exceeds
/// `caps.max_result_bytes`, so a listing of them all does not fit one
/// inline result, while each row's JSON fits one cell under
/// `caps.max_cell_bytes`.
///
/// Keys are `prefix` followed by a zero-padded index, so they sort in
/// insertion order.
///
/// # Panics
///
/// Panics when `count` rows of at most one cell each cannot exceed the
/// result cap, so a test asking for too few rows fails at the fixture
/// instead of passing on a listing that never reached the cap, and when a
/// generated row does not fit a cell after all.
#[must_use]
pub fn rows_over_result_cap(
    caps: &DbCapabilities,
    prefix: &str,
    count: usize,
) -> Vec<(String, String)> {
    let cap = usize::try_from(caps.max_result_bytes).unwrap_or(usize::MAX);
    let cell_cap = usize::try_from(caps.max_cell_bytes).unwrap_or(usize::MAX);
    let count = count.max(1);
    // Each row carries its share of the cap plus a margin, so the rows
    // together always exceed it, and no row is wider than a cell.
    let per_row = (cap / count + 64).min(cell_cap);
    assert!(
        per_row.saturating_mul(count) > cap,
        "{count} rows of at most {cell_cap} bytes cannot exceed a {cap}-byte result cap"
    );
    (0..count)
        .map(|i| {
            let key = format!("{prefix}{i:05}");
            let json = wide_json(&key, per_row);
            let len = json.len();
            assert!(
                len <= cell_cap,
                "row {key} is {len} bytes, over the {cell_cap}-byte cell cap"
            );
            (key, json)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_plugin_abi::HOST_MIN_CELL_BYTES;

    #[test]
    fn multibyte_text_respects_the_byte_cap_on_a_char_boundary() {
        for cap in [0usize, 1, 2, 3, 85, 86, 87, 1_000] {
            let text = multibyte_text(cap);
            assert!(text.len() <= cap, "{cap}: {}", text.len());
            assert!(
                text.len() + 4 > cap || cap < 4,
                "{cap} left too much unused"
            );
        }
        assert!(multibyte_text(86).chars().count() < 86);
    }

    #[test]
    fn descriptions_bracket_the_cell_cap() {
        let caps = crate::Profile::Minimum.capabilities();
        let cap = usize::try_from(HOST_MIN_CELL_BYTES).unwrap();
        let under = description_near_cell_cap(&caps, 8);
        assert!(under.len() <= cap - 8 && under.len() > cap - 16);
        let over = description_over_cell_cap(&caps);
        assert_eq!(over.len(), cap + 1);
    }

    #[test]
    fn wide_json_is_valid_and_about_the_requested_size() {
        let json = wide_json("k1", 2_048);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["key"], "k1");
        assert!(json.len() <= 2_048 && json.len() > 2_000, "{}", json.len());
    }

    #[test]
    fn wide_json_escapes_the_key_and_the_title() {
        let json = wide_json("quo\"te\\back", 512);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["key"], "quo\"te\\back");
        assert_eq!(value["title"], "QUO\"TE\\BACK");
        assert!(json.len() <= 512, "{}", json.len());
    }

    #[test]
    fn rows_over_result_cap_keep_every_row_inside_a_cell() {
        let caps = crate::Profile::Minimum.capabilities();
        let cell_cap = usize::try_from(caps.max_cell_bytes).unwrap();
        let result_cap = usize::try_from(caps.max_result_bytes).unwrap();
        // Five cells of 1,024 bytes are the fewest that exceed 4,096.
        let rows = rows_over_result_cap(&caps, "r", 5);
        assert!(rows.iter().all(|(_, json)| json.len() <= cell_cap));
        let total: usize = rows.iter().map(|(_, json)| json.len()).sum();
        assert!(total > result_cap, "{total}");
    }

    #[test]
    #[should_panic(expected = "cannot exceed")]
    fn rows_over_result_cap_refuses_a_count_that_cannot_reach_the_cap() {
        let caps = crate::Profile::Minimum.capabilities();
        let _ = rows_over_result_cap(&caps, "r", 1);
    }

    #[test]
    fn rows_over_result_cap_exceed_the_cap_together() {
        for profile in crate::Profile::ALL {
            let caps = profile.capabilities();
            let rows = rows_over_result_cap(&caps, "r", 8);
            let total: usize = rows.iter().map(|(_, json)| json.len()).sum();
            assert!(
                total > usize::try_from(caps.max_result_bytes).unwrap(),
                "{profile:?}"
            );
            for (_, json) in &rows {
                assert!(
                    json.len() <= usize::try_from(caps.max_cell_bytes).unwrap(),
                    "{profile:?}"
                );
            }
        }
    }
}
