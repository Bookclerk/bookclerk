//! Extended Bookclerk classic settings (chapter split, LAME, timestamps, etc.).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Character replacement rule (`ReplacementCharacters` in classic Settings.json).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplacementRule {
    /// Substring to search for in each path segment / tag value.
    pub find: String,
    /// Replacement text (may be empty to strip `find`).
    pub replace: String,
}

/// How to sanitize storage path segments when [`crate::OutputConfig::replacement_characters`]
/// is empty (explicit rules always win).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PathSanitizationMode {
    /// Pick a profile from the storage backend and host OS.
    ///
    /// * `output.s3.enabled = true` → [`s3_replacement_characters`]
    /// * Windows host + local storage → [`windows_replacement_characters`]
    /// * otherwise → [`posix_replacement_characters`]
    #[default]
    Auto,
    /// NTFS / classic Libation Windows map (`: * ? " < > | / \`).
    Windows,
    /// Path-separator only (`/` `\`) — suitable for Linux/macOS local disks.
    Posix,
    /// AWS S3 “characters to avoid” plus path separators.
    S3,
    /// Disable platform-specific sanitization; path separators (`/` `\`) are
    /// still stripped so metadata cannot inject extra directories.
    None,
}

/// LAME encoder tuning (classic `Lame*` settings).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct LameConfig {
    /// `LameTarget`: `quality`, `bitrate`, `constant`, etc.
    pub target: String,
    /// `LameVBRQuality` (0–9, lower = better).
    pub vbr_quality: u8,
    /// `LameBitrate` in kbps when target is bitrate/constant.
    pub bitrate_kbps: u32,
    /// `LameMode`: `default`, `mono`, `stereo`.
    pub mode: String,
    /// `LameDownsampleMono`.
    pub downsample_mono: bool,
    /// `LameConstantBitrate`.
    pub constant_bitrate: bool,
}

impl Default for LameConfig {
    fn default() -> Self {
        Self {
            target: String::from("quality"),
            vbr_quality: 2,
            bitrate_kbps: 128,
            mode: String::from("default"),
            downsample_mono: false,
            constant_bitrate: false,
        }
    }
}

/// Which timestamp to apply after acquire (`CreationTime` / `LastWriteTime`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum FileTimestampMode {
    /// Use the wall-clock time at acquire completion (default).
    #[default]
    Now,
    /// Use the account purchase / added-to-library timestamp when known.
    Purchased,
    /// Use the publisher release date when known.
    Published,
}

fn rules_from_pairs(pairs: &[(&str, &str)]) -> Vec<ReplacementRule> {
    pairs
        .iter()
        .map(|(find, replace)| ReplacementRule {
            find: (*find).into(),
            replace: (*replace).into(),
        })
        .collect()
}

/// Path-separator sanitization for POSIX local filesystems.
///
/// Keeps characters such as `:` that are legal on Linux/macOS but reserved on
/// Windows. `/` and `\` are still replaced so tag values cannot inject path
/// segments.
#[must_use]
pub fn posix_replacement_characters() -> Vec<ReplacementRule> {
    rules_from_pairs(&[("/", "_"), ("\\", "_")])
}

/// NTFS-safe replacement map (classic Libation Windows defaults subset).
#[must_use]
pub fn windows_replacement_characters() -> Vec<ReplacementRule> {
    rules_from_pairs(&[
        (":", "_"),
        ("*", "_"),
        ("?", "_"),
        ("\"", "'"),
        ("<", "("),
        (">", ")"),
        ("|", "_"),
        ("/", "_"),
        ("\\", "_"),
    ])
}

/// Characters AWS recommends avoiding in S3 object keys, plus path separators.
///
/// See [Object key naming guidelines](https://docs.aws.amazon.com/AmazonS3/latest/userguide/object-keys.html).
/// Characters that only need URL encoding (e.g. `:`) are left intact.
#[must_use]
pub fn s3_replacement_characters() -> Vec<ReplacementRule> {
    rules_from_pairs(&[
        ("\\", "_"),
        ("{", "_"),
        ("}", "_"),
        ("^", "_"),
        ("%", "_"),
        ("`", "_"),
        ("\"", "'"),
        ("<", "("),
        (">", ")"),
        ("[", "("),
        ("]", ")"),
        ("#", "_"),
        ("|", "_"),
        ("~", "_"),
        ("/", "_"),
    ])
}

/// Alias for [`windows_replacement_characters`] (classic default name).
#[must_use]
pub fn default_replacement_characters() -> Vec<ReplacementRule> {
    windows_replacement_characters()
}

/// Resolve the character-replacement map used when sanitising storage path segments.
///
/// Explicit non-empty `replacement_characters` always wins. Otherwise
/// `path_sanitization` (with `storage_is_s3` for [`PathSanitizationMode::Auto`])
/// selects a profile.
#[must_use]
pub fn resolve_replacement_characters(
    explicit: &[ReplacementRule],
    mode: PathSanitizationMode,
    storage_is_s3: bool,
) -> Vec<ReplacementRule> {
    if !explicit.is_empty() {
        return explicit.to_vec();
    }
    match mode {
        // Separators only — same floor as Posix — so `none` cannot inject paths.
        PathSanitizationMode::None | PathSanitizationMode::Posix => posix_replacement_characters(),
        PathSanitizationMode::Windows => windows_replacement_characters(),
        PathSanitizationMode::S3 => s3_replacement_characters(),
        PathSanitizationMode::Auto => {
            if storage_is_s3 {
                s3_replacement_characters()
            } else if cfg!(windows) {
                windows_replacement_characters()
            } else {
                posix_replacement_characters()
            }
        }
    }
}

/// Sentinel placed in reconcile path *patterns* where a sanitizable character
/// appeared in metadata. Matches any single character in an on-disk key.
pub const RECONCILE_WILDCARD: char = '\u{E000}';

/// Replacement rules that turn every known sanitizable character into
/// [`RECONCILE_WILDCARD`].
///
/// Used when searching for existing liberations: creation still follows the
/// active profile, but reconcile must match files produced under Windows,
/// POSIX, or S3 rules (or custom `replacement_characters`).
#[must_use]
pub fn reconciliation_wildcard_rules(explicit: &[ReplacementRule]) -> Vec<ReplacementRule> {
    use std::collections::BTreeSet;

    let mut finds = BTreeSet::new();
    for rule in windows_replacement_characters()
        .into_iter()
        .chain(posix_replacement_characters())
        .chain(s3_replacement_characters())
        .chain(explicit.iter().cloned())
    {
        if !rule.find.is_empty() {
            finds.insert(rule.find);
        }
    }
    // Longer finds first so a multi-char explicit rule is not partially eaten.
    let mut finds: Vec<String> = finds.into_iter().collect();
    finds.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
    finds
        .into_iter()
        .map(|find| ReplacementRule {
            find,
            replace: RECONCILE_WILDCARD.to_string(),
        })
        .collect()
}

/// True when `key` matches a reconcile `pattern` built with
/// [`reconciliation_wildcard_rules`] (wildcard = any single character).
#[must_use]
pub fn key_matches_reconcile_pattern(pattern: &str, key: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let key_chars: Vec<char> = key.chars().collect();
    if pat.len() != key_chars.len() {
        return false;
    }
    pat.iter()
        .zip(key_chars.iter())
        .all(|(p, k)| *p == RECONCILE_WILDCARD || p == k)
}

/// Parse classic `ReplacementCharacters` JSON.
///
/// Supports both the classic Libation `Settings.json` shape
/// (`{ "Replacement": [ { "CharacterToReplace": ":", "ReplacementString": "_" }, … ] }`)
/// and a flat map shape (`{ ":": "_", … }`). An empty or unrecognised value falls
/// back to [`windows_replacement_characters`] (classic migrate parity).
pub fn parse_replacement_characters(value: &serde_json::Value) -> Vec<ReplacementRule> {
    // Classic form: an object wrapping a `Replacement` array, or a bare array.
    let array = value
        .get("Replacement")
        .and_then(serde_json::Value::as_array)
        .or_else(|| value.as_array());
    if let Some(items) = array {
        let rules: Vec<ReplacementRule> =
            items.iter().filter_map(parse_replacement_entry).collect();
        return if rules.is_empty() {
            windows_replacement_characters()
        } else {
            rules
        };
    }

    // Flat map form: { find: replace, … }.
    let Some(map) = value.as_object() else {
        return windows_replacement_characters();
    };
    if map.is_empty() {
        return windows_replacement_characters();
    }
    map.iter()
        .map(|(k, v)| ReplacementRule {
            find: k.clone(),
            replace: v.as_str().unwrap_or("_").to_string(),
        })
        .collect()
}

/// Parse a single classic `Replacement` entry
/// (`{ "CharacterToReplace": ":", "ReplacementString": "_" }`).
fn parse_replacement_entry(entry: &serde_json::Value) -> Option<ReplacementRule> {
    let obj = entry.as_object()?;
    let find = match obj.get("CharacterToReplace") {
        Some(serde_json::Value::String(s)) => s.clone(),
        // A char may also be serialised as a JSON number (code point).
        Some(serde_json::Value::Number(n)) => {
            let code = u32::try_from(n.as_u64()?).ok()?;
            char::from_u32(code)?.to_string()
        }
        _ => return None,
    };
    if find.is_empty() {
        return None;
    }
    let replace = obj
        .get("ReplacementString")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    Some(ReplacementRule { find, replace })
}

/// Apply replacement rules to a string (classic filename sanitization).
#[must_use]
pub fn apply_replacements(input: &str, rules: &[ReplacementRule]) -> String {
    let mut out = input.to_string();
    for rule in rules {
        if !rule.find.is_empty() {
            out = out.replace(&rule.find, &rule.replace);
        }
    }
    out
}

/// Classic Settings.json key → dotted config path for `get-setting` / `-o` overrides.
#[must_use]
pub fn classic_key_aliases() -> HashMap<&'static str, &'static str> {
    HashMap::from([
        ("Books", "output.local.root"),
        ("FileDownloadQuality", "sources.audible.bitrate"),
        ("DecryptToLossy", "output.format"),
        ("UseWidevine", "output.widevine"),
        ("Request_xHE_AAC", "output.xhe_aac"),
        ("NamingProfile", "output.naming_profile"),
        ("FolderTemplate", "output.folder_template"),
        ("FileTemplate", "output.file_template"),
        ("DownloadCoverArt", "output.download_cover"),
        ("CreateCueSheet", "output.create_cue"),
        ("AllowLibationFixup", "output.fixup_metadata"),
        ("SaveMetadataToFile", "output.save_metadata_json"),
        ("AutoDownloadEpisodes", "library.auto_acquire"),
        ("FixStorageLayout", "library.fix_storage_layout"),
        ("AutoScan", "library.scan_interval_minutes"),
        ("OverwriteExisting", "output.overwrite_existing"),
        ("InProgress", "output.in_progress"),
        ("ImportEpisodes", "library.import_episodes"),
        ("ImportPlusTitles", "library.import_plus_titles"),
        ("DownloadEpisodes", "library.download_episodes"),
        (
            "SavePodcastsToParentFolder",
            "library.save_podcasts_to_parent_folder",
        ),
        ("BadBook", "output.bad_book_action"),
        ("SplitFilesByChapter", "output.split_files_by_chapter"),
        ("ChapterFileTemplate", "output.chapter_file_template"),
        ("ChapterTitleTemplate", "output.chapter_title_template"),
        ("ReplacementCharacters", "output.replacement_characters"),
        ("PathSanitization", "output.path_sanitization"),
        ("MaxFilenameLength", "output.max_filename_length"),
        (
            "MinimumFileDuration",
            "output.minimum_file_duration_minutes",
        ),
        (
            "CombineNestedChapterTitles",
            "output.combine_nested_chapter_titles",
        ),
        (
            "MergeOpeningAndEndCredits",
            "output.merge_opening_and_end_credits",
        ),
        ("StripUnabridged", "output.strip_unabridged"),
        ("StripAudibleBrandAudio", "output.strip_audible_brand_audio"),
        ("DownloadClipsBookmarks", "output.download_clips_bookmarks"),
        ("RetainAaxFile", "output.retain_aax_file"),
        ("DownloadSpeedLimit", "output.download_speed_limit_kbps"),
        ("LameTarget", "output.lame.target"),
        ("LameVBRQuality", "output.lame.vbr_quality"),
        ("LameBitrate", "output.lame.bitrate_kbps"),
        ("LameMode", "output.lame.mode"),
        ("LameDownsampleMono", "output.lame.downsample_mono"),
        ("LameConstantBitrate", "output.lame.constant_bitrate"),
        ("MaxSampleRate", "output.max_sample_rate"),
        ("CreationTime", "output.creation_time"),
        ("LastWriteTime", "output.last_write_time"),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_classic_replacement_array() {
        let json = serde_json::json!({
            "Replacement": [
                { "CharacterToReplace": ":", "ReplacementString": "_", "Description": "colon" },
                { "CharacterToReplace": "*", "ReplacementString": "", "Description": "asterisk" },
            ]
        });
        let rules = parse_replacement_characters(&json);
        assert_eq!(
            rules,
            vec![
                ReplacementRule {
                    find: ":".into(),
                    replace: "_".into()
                },
                ReplacementRule {
                    find: "*".into(),
                    replace: "".into()
                },
            ]
        );
    }

    #[test]
    fn parse_bare_replacement_array() {
        let json = serde_json::json!([
            { "CharacterToReplace": "?", "ReplacementString": "_" },
        ]);
        let rules = parse_replacement_characters(&json);
        assert_eq!(
            rules,
            vec![ReplacementRule {
                find: "?".into(),
                replace: "_".into()
            }]
        );
    }

    #[test]
    fn parse_flat_map_replacement() {
        let json = serde_json::json!({ ":": "_", "/": "-" });
        let rules = parse_replacement_characters(&json);
        assert!(rules.contains(&ReplacementRule {
            find: ":".into(),
            replace: "_".into()
        }));
        assert!(rules.contains(&ReplacementRule {
            find: "/".into(),
            replace: "-".into()
        }));
    }

    #[test]
    fn parse_empty_falls_back_to_windows() {
        assert_eq!(
            parse_replacement_characters(&serde_json::json!({})),
            windows_replacement_characters()
        );
        assert_eq!(
            parse_replacement_characters(&serde_json::json!({ "Replacement": [] })),
            windows_replacement_characters()
        );
    }

    #[test]
    fn resolve_explicit_rules_win() {
        let explicit = vec![ReplacementRule {
            find: ":".into(),
            replace: "-".into(),
        }];
        let got = resolve_replacement_characters(&explicit, PathSanitizationMode::Posix, false);
        assert_eq!(got, explicit);
    }

    #[test]
    fn resolve_auto_local_is_posix_on_unix() {
        let got = resolve_replacement_characters(&[], PathSanitizationMode::Auto, false);
        if cfg!(windows) {
            assert_eq!(got, windows_replacement_characters());
        } else {
            assert_eq!(got, posix_replacement_characters());
            assert!(!got.iter().any(|r| r.find == ":"));
        }
    }

    #[test]
    fn resolve_auto_s3_uses_s3_profile() {
        let got = resolve_replacement_characters(&[], PathSanitizationMode::Auto, true);
        assert_eq!(got, s3_replacement_characters());
        assert!(got.iter().any(|r| r.find == "#"));
        assert!(!got.iter().any(|r| r.find == ":"));
    }

    #[test]
    fn posix_keeps_colon_strips_slash() {
        let rules = posix_replacement_characters();
        assert_eq!(
            apply_replacements("Title: Sub/Part", &rules),
            "Title: Sub_Part"
        );
    }

    #[test]
    fn s3_strips_hash_keeps_colon() {
        let rules = s3_replacement_characters();
        assert_eq!(
            apply_replacements("Book #1: Intro", &rules),
            "Book _1: Intro"
        );
    }

    #[test]
    fn none_still_strips_path_separators() {
        let got = resolve_replacement_characters(&[], PathSanitizationMode::None, false);
        assert_eq!(got, posix_replacement_characters());
        assert_eq!(apply_replacements("A/B\\C", &got), "A_B_C");
    }

    #[test]
    fn reconcile_wildcards_match_across_profiles() {
        let rules = reconciliation_wildcard_rules(&[]);
        let pattern = apply_replacements("Hello: World #1", &rules);
        assert!(key_matches_reconcile_pattern(
            &pattern,
            &apply_replacements("Hello: World #1", &posix_replacement_characters())
        ));
        assert!(key_matches_reconcile_pattern(
            &pattern,
            &apply_replacements("Hello: World #1", &windows_replacement_characters())
        ));
        assert!(key_matches_reconcile_pattern(
            &pattern,
            &apply_replacements("Hello: World #1", &s3_replacement_characters())
        ));
        assert!(!key_matches_reconcile_pattern(&pattern, "Hello: World #2"));
    }
}
