//! Length limits for the host keys acquire builds from naming templates.
//!
//! Classic Libation enforces [`DEFAULT_MAX_FILENAME_LENGTH`] (255) per path
//! component via `LongPath.MaxFilenameLength`, and a host key is at most
//! [`MAX_HOST_KEY_BYTES`] (1024) UTF-8 bytes including the destination's
//! prefix, which every file system and S3-compatible store accepts. Acquire
//! shortens long titles and series to fit, keeping the call number, so a
//! destination's storage plugin rarely has to shorten a key itself when it
//! translates it.

use serde::{Deserialize, Serialize};

/// Classic Libation `LongPath.MaxFilenameLength` — per path component.
pub const DEFAULT_MAX_FILENAME_LENGTH: usize = 255;

/// Largest host key, in UTF-8 bytes, including the destination's prefix:
/// the S3 object-key limit, which file systems also accept.
pub const MAX_HOST_KEY_BYTES: usize = 1024;

/// How to measure path component length when truncating.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PathLengthMeasure {
    /// Unicode scalar count (classic Libation on Windows).
    Chars,
    /// UTF-8 byte count (classic Libation on Unix; always used for S3 keys).
    #[default]
    Utf8Bytes,
}

/// Limits applied when hardening acquired storage keys.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct PathLimits {
    /// Max length per folder / file-stem segment. `0` disables per-segment caps.
    pub max_filename_length: usize,
    /// Max UTF-8 bytes for the full relative storage key (`folder/…/file.ext`).
    /// `0` disables. Host keys use [`MAX_HOST_KEY_BYTES`] minus the prefix length.
    pub max_storage_key_bytes: usize,
    /// Whether segment truncation counts Unicode scalars or UTF-8 bytes.
    pub measure: PathLengthMeasure,
}

impl Default for PathLimits {
    fn default() -> Self {
        Self {
            max_filename_length: DEFAULT_MAX_FILENAME_LENGTH,
            max_storage_key_bytes: 0,
            measure: PathLengthMeasure::Utf8Bytes,
        }
    }
}

impl PathLimits {
    /// Host-key limits for a destination whose keys sit under `prefix`:
    /// segments of at most `max_filename_length` UTF-8 bytes, and a whole
    /// key of [`MAX_HOST_KEY_BYTES`] minus the normalized `prefix` length
    /// (its trailing `/` included when the prefix is non-empty).
    ///
    /// Bytes are counted for every destination: a name that fits in bytes
    /// fits in characters and UTF-16 units too.
    #[must_use]
    pub fn for_host_keys(max_filename_length: u32, prefix: &str) -> Self {
        let prefix_len = crate::output::normalize_storage_prefix(prefix).len();
        Self {
            max_filename_length: usize::try_from(max_filename_length)
                .unwrap_or(DEFAULT_MAX_FILENAME_LENGTH),
            max_storage_key_bytes: MAX_HOST_KEY_BYTES.saturating_sub(prefix_len),
            measure: PathLengthMeasure::Utf8Bytes,
        }
    }
}

/// Measure `s` according to `measure`.
#[must_use]
pub fn path_len(s: &str, measure: PathLengthMeasure) -> usize {
    match measure {
        PathLengthMeasure::Chars => s.chars().count(),
        PathLengthMeasure::Utf8Bytes => s.len(),
    }
}

/// Truncate `s` so its measured length is ≤ `limit`. Never splits a UTF-8 char.
#[must_use]
pub fn truncate_path_component(s: &str, limit: usize, measure: PathLengthMeasure) -> String {
    if limit == 0 || path_len(s, measure) <= limit {
        return s.to_string();
    }
    match measure {
        PathLengthMeasure::Chars => s.chars().take(limit).collect(),
        PathLengthMeasure::Utf8Bytes => {
            let mut end = limit.min(s.len());
            while end > 0 && !s.is_char_boundary(end) {
                end -= 1;
            }
            s[..end].to_string()
        }
    }
}

/// Truncate a filename stem so `stem + "." + ext` fits in `max_filename_length`.
///
/// When the stem ends with ` [id]` (ASIN/ISBN bracket form used by naming
/// profiles), the id suffix is preserved and the title prefix is shortened.
#[must_use]
pub fn truncate_filename_stem(
    stem: &str,
    ext: &str,
    max_filename_length: usize,
    measure: PathLengthMeasure,
) -> String {
    if max_filename_length == 0 {
        return stem.to_string();
    }
    let ext = ext.trim_start_matches('.');
    let reserved = if ext.is_empty() {
        0
    } else {
        path_len(ext, measure).saturating_add(1) // '.' + ext
    };
    let stem_limit = max_filename_length.saturating_sub(reserved);
    if stem_limit == 0 {
        return String::new();
    }
    if path_len(stem, measure) <= stem_limit {
        return stem.to_string();
    }

    // Prefer keeping a trailing " [ID]" uniqueness suffix.
    if let Some((prefix, suffix)) = split_bracket_id_suffix(stem) {
        let suffix_len = path_len(suffix, measure);
        if suffix_len < stem_limit {
            let prefix_limit = stem_limit - suffix_len;
            let truncated_prefix = truncate_path_component(prefix, prefix_limit, measure)
                .trim_end()
                .to_string();
            return format!("{truncated_prefix}{suffix}");
        }
    }

    truncate_path_component(stem, stem_limit, measure)
}

/// Split `stem` into `(prefix, " [id]")` when it ends with a bracket token.
fn split_bracket_id_suffix(stem: &str) -> Option<(&str, &str)> {
    let start = stem.rfind(" [")?;
    let rest = &stem[start..];
    if rest.ends_with(']') && rest.len() > 3 {
        Some((&stem[..start], rest))
    } else {
        None
    }
}

/// Enforce per-segment and full-key limits on a relative storage key.
///
/// `key` uses `/` separators. The final component is treated as a filename
/// (extension preserved). Returns the possibly truncated key.
#[must_use]
pub fn enforce_storage_key_limits(key: &str, limits: PathLimits) -> String {
    if limits.max_filename_length == 0 && limits.max_storage_key_bytes == 0 {
        return key.to_string();
    }

    let mut parts: Vec<String> = key.split('/').map(str::to_string).collect();
    if parts.is_empty() {
        return String::new();
    }

    // Per-segment caps.
    if limits.max_filename_length > 0 {
        let last = parts.len() - 1;
        for (i, part) in parts.iter_mut().enumerate() {
            if i == last {
                if let Some((stem, ext)) = part.rsplit_once('.') {
                    *part = format!(
                        "{}.{}",
                        truncate_filename_stem(
                            stem,
                            ext,
                            limits.max_filename_length,
                            limits.measure
                        ),
                        ext
                    );
                } else {
                    *part =
                        truncate_path_component(part, limits.max_filename_length, limits.measure);
                }
            } else {
                *part = truncate_path_component(part, limits.max_filename_length, limits.measure);
            }
        }
    }

    // Full relative key budget (UTF-8 bytes) — shorten longest folder segments.
    if limits.max_storage_key_bytes > 0 {
        loop {
            parts.retain(|p| !p.is_empty());
            if parts.is_empty() {
                break;
            }
            let key = parts.join("/");
            if key.len() <= limits.max_storage_key_bytes {
                break;
            }
            let last = parts.len() - 1;
            if last == 0 {
                // Only the filename remains — trim its stem to the remaining budget.
                let file = parts[0].clone();
                if let Some((stem, ext)) = file.rsplit_once('.') {
                    let budget = limits.max_storage_key_bytes.saturating_sub(ext.len() + 1);
                    parts[0] = format!(
                        "{}.{}",
                        truncate_path_component(stem, budget, PathLengthMeasure::Utf8Bytes),
                        ext
                    );
                } else {
                    parts[0] = truncate_path_component(
                        &file,
                        limits.max_storage_key_bytes,
                        PathLengthMeasure::Utf8Bytes,
                    );
                }
                break;
            }
            let Some(idx) = parts
                .iter()
                .enumerate()
                .take(last)
                .max_by_key(|(_, p)| p.len())
                .map(|(i, _)| i)
            else {
                break;
            };
            let mut truncated = parts[idx].clone();
            truncated.pop();
            while !truncated.is_empty() && !truncated.is_char_boundary(truncated.len()) {
                truncated.pop();
            }
            if truncated == parts[idx] {
                break;
            }
            parts[idx] = truncated;
        }
    }

    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_utf8_without_splitting_chars() {
        let s = "éééé"; // 4 chars, 8 bytes
        assert_eq!(
            truncate_path_component(s, 4, PathLengthMeasure::Utf8Bytes),
            "éé"
        );
        assert_eq!(
            truncate_path_component(s, 3, PathLengthMeasure::Chars),
            "ééé"
        );
    }

    #[test]
    fn preserves_bracket_id_when_truncating_stem() {
        let stem = format!("{} [B00EXAMPLE1]", "A".repeat(300));
        let out = truncate_filename_stem(&stem, "m4b", 40, PathLengthMeasure::Utf8Bytes);
        assert!(out.ends_with(" [B00EXAMPLE1]"), "{out}");
        assert!(path_len(&format!("{out}.m4b"), PathLengthMeasure::Utf8Bytes) <= 40);
    }

    #[test]
    fn enforce_shortens_long_folder_for_s3_budget() {
        let key = format!("{}/{}.m4b", "a".repeat(200), "b".repeat(200));
        let limits = PathLimits {
            max_filename_length: 255,
            max_storage_key_bytes: 100,
            measure: PathLengthMeasure::Utf8Bytes,
        };
        let out = enforce_storage_key_limits(&key, limits);
        assert!(out.len() <= 100, "{} > 100 ({out})", out.len());
        assert!(out.ends_with(".m4b"));
    }

    #[test]
    fn host_key_budget_normalizes_prefix_trailing_slash() {
        let with_slash = PathLimits::for_host_keys(255, "library/");
        let without = PathLimits::for_host_keys(255, "library");
        assert_eq!(
            with_slash.max_storage_key_bytes,
            without.max_storage_key_bytes
        );
        assert_eq!(
            without.max_storage_key_bytes,
            MAX_HOST_KEY_BYTES - "library/".len()
        );
        assert_eq!(
            PathLimits::for_host_keys(80, "").max_storage_key_bytes,
            MAX_HOST_KEY_BYTES
        );
        assert_eq!(PathLimits::for_host_keys(80, "").max_filename_length, 80);
    }
}
