//! Audible brand intro/outro (pre/post-roll) trim helpers.

use serde_json::Value;

use crate::mp4::TrimRange;

/// Brand intro/outro durations from Audible `chapter_info`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BrandDurations {
    pub intro_ms: u64,
    pub outro_ms: u64,
}

impl BrandDurations {
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.intro_ms == 0 && self.outro_ms == 0
    }
}

/// Read brand intro/outro durations from chapter_info JSON.
///
/// Accepts both snake_case (`brand_intro_duration_ms`) and camelCase
/// (`brandIntroDurationMs`) — Audible's live API uses camelCase.
#[must_use]
pub fn brand_durations_from_chapter_info(info: &Value) -> BrandDurations {
    BrandDurations {
        intro_ms: json_u64_keys(info, &["brand_intro_duration_ms", "brandIntroDurationMs"]),
        outro_ms: json_u64_keys(info, &["brand_outro_duration_ms", "brandOutroDurationMs"]),
    }
}

/// Read `runtime_length_ms` / `runtimeLengthMs` from chapter_info JSON.
#[must_use]
pub fn runtime_length_ms_from_chapter_info(info: &Value) -> Option<u64> {
    json_u64_opt_keys(info, &["runtime_length_ms", "runtimeLengthMs"])
}

fn json_u64_keys(info: &Value, keys: &[&str]) -> u64 {
    json_u64_opt_keys(info, keys).unwrap_or(0)
}

fn json_u64_opt_keys(info: &Value, keys: &[&str]) -> Option<u64> {
    for key in keys {
        if let Some(v) = info.get(*key).and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_i64().map(|n| n.max(0) as u64))
                .or_else(|| v.as_f64().map(|n| n.max(0.0) as u64))
        }) {
            return Some(v);
        }
    }
    None
}

/// Build a media trim window that drops Audible branding audio.
///
/// Classic Libation: shift the first chapter start by `intro_ms` and shorten the
/// last chapter by `outro_ms`, then remux only the remaining media. We express
/// the same idea as an absolute `[start_ms, end_ms)` window on the full file.
///
/// When `outro_ms > 0` but `runtime_length_ms` is missing, returns `None` so we
/// do not silently leave branding on the tail while trimming only the intro.
/// Callers should supply runtime (from chapter_info or a media probe) first.
#[must_use]
pub fn brand_trim_range(
    brand: BrandDurations,
    runtime_length_ms: Option<u64>,
) -> Option<TrimRange> {
    if brand.is_empty() {
        return None;
    }
    if brand.outro_ms > 0 && runtime_length_ms.is_none() {
        tracing::warn!(
            intro_ms = brand.intro_ms,
            outro_ms = brand.outro_ms,
            "brand_outro_duration_ms set but runtime_length_ms missing — \
             skipping brand trim to avoid leaving outro branding in the file"
        );
        return None;
    }
    let end_ms = runtime_length_ms.map(|runtime| runtime.saturating_sub(brand.outro_ms));
    Some(TrimRange {
        start_ms: brand.intro_ms,
        end_ms,
    })
}

/// Adjust flat chapter start offsets after brand trim so cues/metadata stay aligned.
///
/// Drops chapters that fall entirely inside the stripped intro, rebases survivors
/// so the first kept chapter starts at 0, and shortens the implied end by `outro_ms`.
#[must_use]
pub fn rebase_chapters_after_brand_trim(
    chapters: &[(String, u64)],
    brand: BrandDurations,
    runtime_length_ms: Option<u64>,
) -> Vec<(String, u64)> {
    if brand.is_empty() {
        return chapters.to_vec();
    }
    let end_ms = runtime_length_ms
        .map(|r| r.saturating_sub(brand.outro_ms))
        .unwrap_or(u64::MAX);
    let mut out = Vec::new();
    for (title, start) in chapters {
        if *start < brand.intro_ms {
            // Keep a chapter that starts before intro only if it extends past intro
            // (classic keeps opening credits but shifts its start). Represent that
            // as a chapter at 0.
            if out.is_empty() {
                out.push((title.clone(), 0));
            }
            continue;
        }
        if *start >= end_ms {
            break;
        }
        out.push((title.clone(), start.saturating_sub(brand.intro_ms)));
    }
    if out.is_empty() {
        // Fall back to a single chapter covering the trimmed media.
        out.push(("Chapter 1".into(), 0));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_brand_fields() {
        let info = serde_json::json!({
            "brand_intro_duration_ms": 4025,
            "brand_outro_duration_ms": 3500,
            "runtime_length_ms": 3600000
        });
        let brand = brand_durations_from_chapter_info(&info);
        assert_eq!(brand.intro_ms, 4025);
        assert_eq!(brand.outro_ms, 3500);
        assert_eq!(runtime_length_ms_from_chapter_info(&info), Some(3_600_000));
        let trim = brand_trim_range(brand, Some(3_600_000)).unwrap();
        assert_eq!(trim.start_ms, 4025);
        assert_eq!(trim.end_ms, Some(3_600_000 - 3500));
    }

    #[test]
    fn parses_camel_case_brand_fields() {
        let info = serde_json::json!({
            "brandIntroDurationMs": 1904,
            "brandOutroDurationMs": 4969,
            "runtimeLengthMs": 58_213_821
        });
        let brand = brand_durations_from_chapter_info(&info);
        assert_eq!(brand.intro_ms, 1904);
        assert_eq!(brand.outro_ms, 4969);
        assert_eq!(runtime_length_ms_from_chapter_info(&info), Some(58_213_821));
    }

    #[test]
    fn skips_trim_when_outro_needs_runtime() {
        let brand = BrandDurations {
            intro_ms: 4_000,
            outro_ms: 3_500,
        };
        assert!(brand_trim_range(brand, None).is_none());
        let intro_only = BrandDurations {
            intro_ms: 4_000,
            outro_ms: 0,
        };
        let trim = brand_trim_range(intro_only, None).unwrap();
        assert_eq!(trim.start_ms, 4_000);
        assert_eq!(trim.end_ms, None);
    }

    #[test]
    fn rebases_chapter_list() {
        let chapters = vec![
            ("Opening Credits".into(), 0u64),
            ("Chapter 1".into(), 10_000),
            ("Chapter 2".into(), 100_000),
            ("End Credits".into(), 3_590_000),
        ];
        let brand = BrandDurations {
            intro_ms: 4_000,
            outro_ms: 5_000,
        };
        let out = rebase_chapters_after_brand_trim(&chapters, brand, Some(3_600_000));
        assert_eq!(out[0].0, "Opening Credits");
        assert_eq!(out[0].1, 0);
        assert_eq!(out[1].0, "Chapter 1");
        assert_eq!(out[1].1, 6_000);
        assert!(out
            .iter()
            .all(|(t, s)| t != "End Credits" || *s < 3_595_000));
    }
}
