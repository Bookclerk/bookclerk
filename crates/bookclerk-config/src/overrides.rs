//! Runtime setting overrides (classic `acquire -o Key=value`).

use std::path::PathBuf;

use crate::extras::{classic_key_aliases, FileTimestampMode, PathSanitizationMode};
use crate::pipeline_opts::{ChapterJsonMode, OutputFormat};
use crate::{BadBookAction, Config};

/// Apply classic-style `-o Setting=value` overrides onto `config`.
///
/// Keys may be classic PascalCase (`FileDownloadQuality`) or dotted TOML paths
/// (`sources.audible.bitrate`).
pub fn apply_setting_overrides(config: &mut Config, pairs: &[(&str, &str)]) {
    for (key, value) in pairs {
        let dotted = classic_key_aliases().get(*key).copied().unwrap_or(key);
        apply_dotted_override(config, dotted, value);
    }
}

fn apply_dotted_override(config: &mut Config, key: &str, value: &str) {
    let v = value.trim();
    match key {
        "output.local.enabled" => {
            config.output.local.enabled = parse_bool(v).unwrap_or(false);
        }
        "output.local.root" => {
            config.output.local.root = PathBuf::from(v);
            config.output.local.enabled = true;
        }
        "output.local.prefix" => config.output.local.prefix = v.to_string(),
        "output.s3.enabled" => {
            config.output.s3.enabled = parse_bool(v).unwrap_or(false);
        }
        "output.s3.bucket" => config.output.s3.bucket = v.to_string(),
        "output.s3.prefix" => config.output.s3.prefix = v.to_string(),
        "output.s3.region" => config.output.s3.region = v.to_string(),
        "output.s3.endpoint" => config.output.s3.endpoint = Some(v.to_string()),
        "output.s3.force_path_style" => {
            config.output.s3.force_path_style = parse_bool(v).unwrap_or(false);
        }
        "sources.audible.bitrate" | "sources.audible.quality" => {
            // Classic FileDownloadQuality maps onto Audible store bitrate.
            let bitrate = match v.to_ascii_lowercase().as_str() {
                "normal" => "normal",
                _ => "high",
            };
            config.sources.set_string("audible", "bitrate", bitrate);
        }
        "output.format" => {
            if let Some(format) = OutputFormat::parse(v) {
                config.output.format = format;
            } else {
                config.output.format = if parse_bool(v).unwrap_or(false)
                    || v.eq_ignore_ascii_case("mp3")
                    || v.eq_ignore_ascii_case("lossy")
                {
                    OutputFormat::SingleMp3
                } else {
                    OutputFormat::EnrichedM4b
                };
            }
        }
        "graphicaudio.access" => {
            let _ = config
                .sources
                .apply_dotted_override("graphicaudio.access", v);
        }
        "output.widevine" => config.output.widevine = parse_bool(v).unwrap_or(false),
        "output.xhe_aac" => config.output.xhe_aac = parse_bool(v).unwrap_or(false),
        "output.widevine_cdm" => config.output.widevine_cdm = Some(PathBuf::from(v)),
        "output.widevine_cdm_provider" => {
            config.output.widevine_cdm_provider = Some(v.to_string());
        }
        "auth.password_file" => {
            config.auth.password_file = Some(PathBuf::from(v));
        }
        "auth.allow_plaintext" => {
            config.auth.allow_plaintext = parse_bool(v).unwrap_or(false);
        }
        "output.naming_profile" => {
            if let Some(profile) = crate::NamingProfile::parse(v) {
                config.output.naming_profile = profile;
            }
        }
        "output.folder_template" => config.output.folder_template = Some(v.to_string()),
        "output.file_template" => config.output.file_template = Some(v.to_string()),
        "output.path_sanitization" => {
            config.output.path_sanitization = match v.to_ascii_lowercase().as_str() {
                "windows" | "win" | "ntfs" => PathSanitizationMode::Windows,
                "posix" | "unix" | "linux" | "macos" | "mac" => PathSanitizationMode::Posix,
                "s3" | "object" | "object_storage" => PathSanitizationMode::S3,
                "none" | "off" | "disabled" => PathSanitizationMode::None,
                _ => PathSanitizationMode::Auto,
            };
        }
        "output.max_filename_length" => {
            if let Ok(n) = v.parse() {
                config.output.max_filename_length = n;
            }
        }
        "output.download_cover" => {
            config.output.download_cover = parse_bool(v).unwrap_or(false);
        }
        "output.download_pdf" => config.output.download_pdf = parse_bool(v).unwrap_or(false),
        "output.create_cue" => config.output.create_cue = parse_bool(v).unwrap_or(false),
        "output.fixup_metadata" => {
            config.output.fixup_metadata = parse_bool(v).unwrap_or(false);
        }
        "output.save_chapter_json" | "output.chapter_json" => {
            if let Some(mode) = ChapterJsonMode::parse(v) {
                config.output.chapter_json = mode;
            } else if let Some(b) = parse_bool(v) {
                config.output.save_chapter_json = Some(b);
                config.output.chapter_json = if b {
                    ChapterJsonMode::Tree
                } else {
                    ChapterJsonMode::Off
                };
            }
        }
        "output.save_metadata_json" => {
            config.output.save_metadata_json = parse_bool(v).unwrap_or(false);
        }
        "output.overwrite_existing" => {
            config.output.overwrite_existing = parse_bool(v).unwrap_or(false);
        }
        "output.multi_destination" => {
            config.output.multi_destination = match v.trim().to_ascii_lowercase().as_str() {
                "refetch_missing" | "refetch-missing" => {
                    crate::MultiDestinationMode::RefetchMissing
                }
                "refetch_all" | "refetch-all" => crate::MultiDestinationMode::RefetchAll,
                _ => crate::MultiDestinationMode::SyncMissing,
            };
        }
        "output.in_progress" => config.output.in_progress = Some(PathBuf::from(v)),
        "output.bad_book_action" => {
            config.output.bad_book_action = match v {
                "Abort" => BadBookAction::Abort,
                "Retry" => BadBookAction::Retry,
                "Ignore" => BadBookAction::Ignore,
                _ => BadBookAction::Ask,
            };
        }
        "output.split_files_by_chapter" => {
            if parse_bool(v).unwrap_or(false) {
                config.output.format = OutputFormat::SplitMp3ByChapter;
            }
        }
        "output.split_mp3_max_mb" => {
            if let Ok(n) = v.parse() {
                config.output.split_mp3_max_mb = n;
            }
        }
        "output.chapter_file_template" => {
            config.output.chapter_file_template = Some(v.to_string());
        }
        "output.chapter_title_template" => {
            config.output.chapter_title_template = Some(v.to_string());
        }
        "output.minimum_file_duration_minutes" => {
            if let Ok(n) = v.parse() {
                config.output.minimum_file_duration_minutes = n;
            }
        }
        "output.combine_nested_chapter_titles" => {
            config.output.combine_nested_chapter_titles = parse_bool(v).unwrap_or(false);
        }
        "output.merge_opening_and_end_credits" => {
            config.output.merge_opening_and_end_credits = parse_bool(v).unwrap_or(false);
        }
        "output.strip_unabridged" => {
            config.output.strip_unabridged = parse_bool(v).unwrap_or(false);
        }
        "output.strip_audible_brand_audio" => {
            config.output.strip_audible_brand_audio = parse_bool(v).unwrap_or(false);
        }
        "output.download_clips_bookmarks" => {
            config.output.download_clips_bookmarks = parse_bool(v).unwrap_or(false);
        }
        "output.retain_aax_file" => {
            config.output.retain_aax_file = parse_bool(v).unwrap_or(false);
        }
        "output.download_speed_limit_kbps" => {
            if let Ok(n) = v.parse() {
                config.output.download_speed_limit_kbps = n;
            }
        }
        "output.lame.target" => config.output.lame.target = v.to_string(),
        "output.lame.vbr_quality" => {
            if let Ok(n) = v.parse() {
                config.output.lame.vbr_quality = n;
            }
        }
        "output.lame.bitrate_kbps" => {
            if let Ok(n) = v.parse() {
                config.output.lame.bitrate_kbps = n;
            }
        }
        "output.lame.mode" => config.output.lame.mode = v.to_string(),
        "output.lame.downsample_mono" => {
            config.output.lame.downsample_mono = parse_bool(v).unwrap_or(false);
        }
        "output.lame.constant_bitrate" => {
            config.output.lame.constant_bitrate = parse_bool(v).unwrap_or(false);
        }
        "output.max_sample_rate" => {
            config.output.max_sample_rate = v.parse().ok();
        }
        "output.creation_time" => {
            config.output.creation_time = parse_timestamp_mode(v);
        }
        "output.last_write_time" => {
            config.output.last_write_time = parse_timestamp_mode(v);
        }
        "output.cover_size" => config.output.cover_size = v.to_string(),
        "output.chapter_layout" => config.output.chapter_layout = v.to_string(),
        "library.auto_acquire" => config.library.auto_acquire = parse_bool(v).unwrap_or(false),
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
        "library.enrich_from_audible" => {
            config.library.enrich_from_audible = parse_bool(v).unwrap_or(true);
        }
        "library.enrich_min_confidence" => {
            if let Ok(n) = v.parse::<u8>() {
                config.library.enrich_min_confidence = n.min(100);
            }
        }
        "library.fix_storage_layout" => {
            config.library.fix_storage_layout = parse_bool(v).unwrap_or(false);
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
        other if let Some(rest) = other.strip_prefix("sources.") => {
            if !config.sources.apply_dotted_override(rest, v) {
                tracing::warn!(key, value = v, "unknown sources override; ignoring");
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
        assert_eq!(cfg.sources.get_string("audible", "bitrate"), Some("normal"));
    }

    #[test]
    fn dotted_override_sets_widevine() {
        let mut cfg = Config::default();
        apply_setting_overrides(&mut cfg, &[("output.widevine", "true")]);
        assert!(cfg.output.widevine);
    }

    #[test]
    fn enrich_from_audible_defaults_true_and_override() {
        let cfg = Config::default();
        assert!(cfg.library.enrich_from_audible);
        assert_eq!(cfg.library.enrich_min_confidence, 90);
        let mut cfg = Config::default();
        apply_setting_overrides(&mut cfg, &[("library.enrich_from_audible", "false")]);
        assert!(!cfg.library.enrich_from_audible);
        apply_setting_overrides(&mut cfg, &[("library.enrich_min_confidence", "85")]);
        assert_eq!(cfg.library.enrich_min_confidence, 85);
    }
}
