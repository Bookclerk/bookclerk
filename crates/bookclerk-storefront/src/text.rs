//! Text normalization for storefront / catalog metadata.

use std::borrow::Cow;

/// Decode HTML entities in storefront metadata (`Memory&#39;s Blade` → `Memory's Blade`).
///
/// Libro.fm (and occasionally other catalogs) emit entity-encoded titles and
/// descriptions. Decode before display, persistence, and soft identity matching.
///
/// Fast path: strings without `&` are returned borrowed (no allocation).
#[must_use]
pub fn decode_html_entities_cow(raw: &str) -> Cow<'_, str> {
    if !raw.contains('&') {
        Cow::Borrowed(raw)
    } else {
        Cow::Owned(html_escape::decode_html_entities(raw).into_owned())
    }
}

/// Owned decode (allocates even when unchanged — prefer [`decode_html_entities_cow`]
/// or [`decode_html_entities_in_place`] on hot paths).
#[must_use]
pub fn decode_html_entities(raw: &str) -> String {
    decode_html_entities_cow(raw).into_owned()
}

/// Decode in place when the value contains `&`; otherwise leave untouched.
pub fn decode_html_entities_in_place(value: &mut String) {
    if value.contains('&') {
        *value = html_escape::decode_html_entities(value).into_owned();
    }
}

/// Decode an optional string in place.
pub fn decode_html_entities_opt_in_place(value: &mut Option<String>) {
    if let Some(s) = value.as_mut() {
        decode_html_entities_in_place(s);
    }
}

/// Decode when present; leave `None` unchanged.
#[must_use]
pub fn decode_html_entities_opt(raw: Option<&str>) -> Option<String> {
    raw.map(|s| decode_html_entities_cow(s).into_owned())
}

/// True when any human-readable field may contain an HTML entity.
#[must_use]
pub fn str_maybe_html_entity(raw: &str) -> bool {
    raw.contains('&')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_libro_apostrophe_entity() {
        assert_eq!(decode_html_entities("Memory&#39;s Blade"), "Memory's Blade");
        assert_eq!(
            decode_html_entities("Memory&apos;s Blade"),
            "Memory's Blade"
        );
    }

    #[test]
    fn leaves_plain_text_untouched() {
        assert_eq!(decode_html_entities("Memory's Blade"), "Memory's Blade");
        assert!(matches!(
            decode_html_entities_cow("Memory's Blade"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn decodes_common_named_entities() {
        assert_eq!(
            decode_html_entities("Tom &amp; Jerry: &quot;Chase&quot;"),
            "Tom & Jerry: \"Chase\""
        );
    }

    #[test]
    fn in_place_skips_when_no_ampersand() {
        let mut s = String::from("Memory's Blade");
        let ptr = s.as_ptr();
        decode_html_entities_in_place(&mut s);
        assert_eq!(s, "Memory's Blade");
        assert_eq!(s.as_ptr(), ptr);
    }
}
