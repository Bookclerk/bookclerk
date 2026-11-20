//! Language codes: one spelling for the many ways stores write a language.
//!
//! Stores name the same language differently (`en`, `en-US`, `eng`,
//! `English`), and the resolver compares expressions by language, so every
//! listing's language goes through [`normalize_language`] before it is
//! compared or stored. Discover's language filters use the same function
//! (re-exported by `bookclerk-storefront`).

/// Normalize a language tag or display name to a primary ISO-ish code.
///
/// Accepts BCP-47 (`en-US`), bare codes (`en`), ISO 639-2 codes in either
/// their terminology or bibliographic (MARC) form (`fra` / `fre`, as
/// Libro.fm sends them), and common Audible display names (`english`).
/// Returns `None` for empty / unrecognized input.
#[must_use]
pub fn normalize_language(raw: &str) -> Option<String> {
    let s = raw.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    // Take primary subtag before `-` / `_`.
    let primary = s.split(['-', '_']).next().unwrap_or(s.as_str()).trim();
    if primary.is_empty() {
        return None;
    }
    let code = match primary {
        "english" | "eng" => "en",
        "spanish" | "español" | "espanol" | "spa" => "es",
        "french" | "français" | "francais" | "fra" | "fre" => "fr",
        "german" | "deutsch" | "deu" | "ger" => "de",
        "italian" | "italiano" | "ita" => "it",
        "portuguese" | "português" | "portugues" | "por" => "pt",
        "japanese" | "日本語" | "jpn" => "ja",
        "chinese"
        | "中文"
        | "chi"
        | "zho"
        | "mandarin"
        | "cantonese"
        | "chinese_simplified"
        | "chinese_traditional"
        | "simplified_chinese"
        | "traditional_chinese" => "zh",
        "korean" | "한국어" | "kor" => "ko",
        "dutch" | "nederlands" | "nld" | "dut" => "nl",
        "swedish" | "svenska" | "swe" => "sv",
        "danish" | "dansk" | "dan" => "da",
        "norwegian" | "norsk" | "nor" => "no",
        "finnish" | "suomi" | "fin" => "fi",
        "polish" | "polski" | "pol" => "pl",
        "russian" | "русский" | "rus" => "ru",
        "arabic" | "العربية" | "ara" => "ar",
        "hindi" | "हिन्दी" | "hin" => "hi",
        "turkish" | "türkçe" | "turkce" | "tur" => "tr",
        other if other.len() == 2 || other.len() == 3 => other,
        _ => return None,
    };
    Some(code.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_spellings_of_one_language_agree() {
        for raw in ["en", "EN", "en-US", "en_GB", "eng", "English", " english "] {
            assert_eq!(normalize_language(raw).as_deref(), Some("en"), "{raw}");
        }
        assert_eq!(normalize_language("fre").as_deref(), Some("fr"));
        assert_eq!(normalize_language("fra").as_deref(), Some("fr"));
    }

    #[test]
    fn every_named_language_maps_to_its_code() {
        let cases = [
            ("Español", "es"),
            ("Deutsch", "de"),
            ("Italiano", "it"),
            ("Português", "pt"),
            ("Japanese", "ja"),
            ("chinese_simplified", "zh"),
            ("Korean", "ko"),
            ("Nederlands", "nl"),
            ("Svenska", "sv"),
            ("Dansk", "da"),
            ("Norsk", "no"),
            ("Suomi", "fi"),
            ("Polski", "pl"),
            ("Russian", "ru"),
            ("Arabic", "ar"),
            ("Hindi", "hi"),
            ("Turkish", "tr"),
        ];
        for (raw, code) in cases {
            assert_eq!(normalize_language(raw).as_deref(), Some(code), "{raw}");
        }
    }

    #[test]
    fn unknown_or_blank_is_none() {
        assert_eq!(normalize_language("-US"), None);
        assert_eq!(normalize_language(""), None);
        assert_eq!(normalize_language("   "), None);
        assert_eq!(normalize_language("klingonese"), None);
        assert_eq!(normalize_language("tlh").as_deref(), Some("tlh"));
    }
}
