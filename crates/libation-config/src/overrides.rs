//! Runtime setting overrides (classic `liberate -o Key=value`).

use std::path::PathBuf;

use crate::extras::{classic_key_aliases, FileTimestampMode, PathSanitizationMode};
use crate::settings::{AudioQuality, BadBookAction, Config, DownloadFormat};

/// Apply classic-style `-o Setting=value` overrides onto `config`.
///
/// Keys may be classic PascalCase (`FileDownloadQuality`) or dotted TOML paths
/// (`download.quality`).
pub fn apply_setting_overrides(config: &mut Config, pairs: &[(&str, &str)]) {
    for (key, value) in pairs {
        let dotted = classic_key_aliases().get(*key).copied().unwrap_or(key);
        apply_dotted_override(config, dotted, value);
    }
}

fn apply_dotted_override(config: &mut Config, key: &str, value: &str) {
    let v = value.trim();
    match key {
        "storage.local.root" => config.storage.local.root = PathBuf::from(v),
        "download.quality" => {
            config.download.quality = match v.to_ascii_lowercase().as_str() {
                "normal" => AudioQuality::Normal,
                _ => AudioQuality::High,
            };
        }
        "download.format" => {
            config.download.format = if parse_bool(v).unwrap_or(false)
                || v.eq_ignore_ascii_case("mp3")
                || v.eq_ignore_ascii_case("lossy")
            {
                DownloadFormat::Mp3
            } else {
                DownloadFormat::M4b
            };
        }
        "download.widevine" => config.download.widevine = parse_bool(v).unwrap_or(false),
        "download.xhe_aac" => config.download.xhe_aac = parse_bool(v).unwrap_or(false),
        "download.widevine_cdm" => config.download.widevine_cdm = Some(PathBuf::from(v)),
        "download.widevine_cdm_provider" => {
            config.download.widevine_cdm_provider = Some(v.to_string());
        }
        "auth.password_file" => {
            config.auth.password_file = Some(PathBuf::from(v));
        }
        "auth.allow_plaintext" => {
            config.auth.allow_plaintext = parse_bool(v).unwrap_or(false);
        }
        "download.folder_template" => config.download.folder_template = Some(v.to_string()),
        "download.file_template" => config.download.file_template = Some(v.to_string()),
        "download.path_sanitization" => {
            config.download.path_sanitization = match v.to_ascii_lowercase().as_str() {
                "windows" | "win" | "ntfs" => PathSanitizationMode::Windows,
                "posix" | "unix" | "linux" | "macos" | "mac" => PathSanitizationMode::Posix,
                "s3" | "object" | "object_storage" => PathSanitizationMode::S3,
                "none" | "off" | "disabled" => PathSanitizationMode::None,
                _ => PathSanitizationMode::Auto,
            };
        }
        "download.download_cover" => {
            config.download.download_cover = parse_bool(v).unwrap_or(false);
        }
        "download.download_pdf" => config.download.download_pdf = parse_bool(v).unwrap_or(false),
        "download.create_cue" => config.download.create_cue = parse_bool(v).unwrap_or(false),
        "download.fixup_metadata" => {
            config.download.fixup_metadata = parse_bool(v).unwrap_or(false);
        }
        "download.save_chapter_json" => {
            config.download.save_chapter_json = parse_bool(v).unwrap_or(false);
        }
        "download.save_metadata_json" => {
            config.download.save_metadata_json = parse_bool(v).unwrap_or(false);
        }
        "download.overwrite_existing" => {
            config.download.overwrite_existing = parse_bool(v).unwrap_or(false);
        }
        "download.in_progress" => config.download.in_progress = Some(PathBuf::from(v)),
        "download.bad_book_action" => {
            config.download.bad_book_action = match v {
                "Abort" => BadBookAction::Abort,
                "Retry" => BadBookAction::Retry,
                "Ignore" => BadBookAction::Ignore,
                _ => BadBookAction::Ask,
            };
        }
        "download.split_files_by_chapter" => {
            config.download.split_files_by_chapter = parse_bool(v).unwrap_or(false);
        }
        "download.chapter_file_template" => {
            config.download.chapter_file_template = Some(v.to_string());
        }
        "download.chapter_title_template" => {
            config.download.chapter_title_template = Some(v.to_string());
        }
        "download.minimum_file_duration_minutes" => {
            if let Ok(n) = v.parse() {
                config.download.minimum_file_duration_minutes = n;
            }
        }
        "download.combine_nested_chapter_titles" => {
            config.download.combine_nested_chapter_titles = parse_bool(v).unwrap_or(false);
        }
        "download.merge_opening_and_end_credits" => {
            config.download.merge_opening_and_end_credits = parse_bool(v).unwrap_or(false);
        }
        "download.strip_unabridged" => {
            config.download.strip_unabridged = parse_bool(v).unwrap_or(false);
        }
        "download.strip_audible_brand_audio" => {
            config.download.strip_audible_brand_audio = parse_bool(v).unwrap_or(false);
        }
        "download.download_clips_bookmarks" => {
            config.download.download_clips_bookmarks = parse_bool(v).unwrap_or(false);
        }
        "download.retain_aax_file" => {
            config.download.retain_aax_file = parse_bool(v).unwrap_or(false);
        }
        "download.download_speed_limit_kbps" => {
            if let Ok(n) = v.parse() {
                config.download.download_speed_limit_kbps = n;
            }
        }
        "download.lame.target" => config.download.lame.target = v.to_string(),
        "download.lame.vbr_quality" => {
            if let Ok(n) = v.parse() {
                config.download.lame.vbr_quality = n;
            }
        }
        "download.lame.bitrate_kbps" => {
            if let Ok(n) = v.parse() {
                config.download.lame.bitrate_kbps = n;
            }
        }
        "download.lame.mode" => config.download.lame.mode = v.to_string(),
        "download.lame.downsample_mono" => {
            config.download.lame.downsample_mono = parse_bool(v).unwrap_or(false);
        }
        "download.lame.constant_bitrate" => {
            config.download.lame.constant_bitrate = parse_bool(v).unwrap_or(false);
        }
        "download.max_sample_rate" => {
            config.download.max_sample_rate = v.parse().ok();
        }
        "download.creation_time" => {
            config.download.creation_time = parse_timestamp_mode(v);
        }
        "download.last_write_time" => {
            config.download.last_write_time = parse_timestamp_mode(v);
        }
        "download.cover_size" => config.download.cover_size = v.to_string(),
        "download.chapter_layout" => config.download.chapter_layout = v.to_string(),
        "library.auto_liberate" => config.library.auto_liberate = parse_bool(v).unwrap_or(false),
        "library.import_episodes" => {
            config.library.import_episodes = parse_bool(v).unwrap_or(true);
        }
        "library.import_plus_titles" => {
            config.library.import_plus_titles = parse_bool(v).unwrap_or(false);
        }
        "library.download_episodes" => {
            config.library.download_episodes = parse_bool(v).unwrap_or(true);
        }
        "library.save_podcasts_to_parent_folder" => {
            config.library.save_podcasts_to_parent_folder = parse_bool(v).unwrap_or(false);
        }
        "library.enrich_libro_from_audible" => {
            config.library.enrich_libro_from_audible = parse_bool(v).unwrap_or(true);
        }
        "library.scan_interval_minutes" => {
            if v.eq_ignore_ascii_case("true") {
                config.library.scan_interval_minutes = 5;
            } else if v.eq_ignore_ascii_case("false") {
                config.library.scan_interval_minutes = 0;
            } else if let Ok(n) = v.parse() {
                config.library.scan_interval_minutes = n;
            }
        }
        _ => tracing::warn!(key, value = v, "unknown setting override; ignoring"),
    }
}

fn parse_timestamp_mode(v: &str) -> FileTimestampMode {
    match v.to_ascii_lowercase().as_str() {
        "purchased" | "dateadded" => FileTimestampMode::Purchased,
        "published" | "releasedate" => FileTimestampMode::Published,
        _ => FileTimestampMode::Now,
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_key_override_maps_quality() {
        let mut cfg = Config::default();
        apply_setting_overrides(&mut cfg, &[("FileDownloadQuality", "Normal")]);
        assert_eq!(cfg.download.quality, AudioQuality::Normal);
    }

    #[test]
    fn dotted_override_sets_widevine() {
        let mut cfg = Config::default();
        apply_setting_overrides(&mut cfg, &[("download.widevine", "true")]);
        assert!(cfg.download.widevine);
    }

    #[test]
    fn enrich_libro_from_audible_defaults_true_and_override() {
        let cfg = Config::default();
        assert!(cfg.library.enrich_libro_from_audible);
        let mut cfg = Config::default();
        apply_setting_overrides(&mut cfg, &[("library.enrich_libro_from_audible", "false")]);
        assert!(!cfg.library.enrich_libro_from_audible);
    }
}
