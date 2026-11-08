//! Preferred-language ranking helpers for catalog merge.
//!
//! # Audience
//!
//! Host discover / catalog code soft-prioritising hits by language.

/// Normalize a language tag or display name to a primary ISO-ish code.
///
/// Accepts BCP-47 (`en-US`), bare codes (`en`), and common Audible display
/// names (`english`). Returns `None` for empty / unrecognized input.
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
        "french" | "français" | "francais" | "fra" => "fr",
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

/// Preferred language for Discover when the client omits one.
#[must_use]
pub fn default_preferred_language() -> &'static str {
    "en"
}

/// Rank a hit's language relative to the preferred code.
///
/// - `0` — matches preferred
/// - `1` — unknown / missing
/// - `2` — other language
#[must_use]
pub fn language_rank(hit_language: Option<&str>, preferred: &str) -> u8 {
    let pref =
        normalize_language(preferred).unwrap_or_else(|| default_preferred_language().to_string());
    match hit_language.and_then(normalize_language) {
        None => 1,
        Some(code) if code == pref => 0,
        Some(_) => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_bcp47_and_names() {
        assert_eq!(normalize_language("en-US").as_deref(), Some("en"));
        assert_eq!(normalize_language("EN").as_deref(), Some("en"));
        assert_eq!(normalize_language("english").as_deref(), Some("en"));
        assert_eq!(normalize_language("Spanish").as_deref(), Some("es"));
        assert_eq!(normalize_language("  ").as_deref(), None);
        assert_eq!(normalize_language("unknown-lang").as_deref(), None);
    }

    #[test]
    fn rank_preferred_unknown_other() {
        assert_eq!(language_rank(Some("english"), "en"), 0);
        assert_eq!(language_rank(Some("en-GB"), "en"), 0);
        assert_eq!(language_rank(None, "en"), 1);
        assert_eq!(language_rank(Some(""), "en"), 1);
        assert_eq!(language_rank(Some("spanish"), "en"), 2);
        assert_eq!(language_rank(Some("english"), "es"), 2);
        assert_eq!(language_rank(Some("español"), "es"), 0);
    }
}
