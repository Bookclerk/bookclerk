//! Runtime setting overrides (classic `acquire -o Key=value`).
//!
//! # Audience
//!
//! Host CLI / daemon operators applying dotted or classic PascalCase keys onto
//! a loaded [`crate::Config`] without hand-editing TOML.

use std::path::{Path, PathBuf};

use crate::extras::{classic_key_aliases, FileTimestampMode, PathSanitizationMode};
use crate::pipeline_opts::{ChapterJsonMode, OutputFormat};
use crate::{BadBookAction, Config, Result};

/// Apply classic-style `-o Setting=value` overrides onto `config`.
///
/// Keys may be classic PascalCase (`UseWidevine`) or dotted TOML paths
/// (`output.widevine`). Plugin settings are not `config.toml` keys; they are
/// written on the plugin instance (`bookclerk plugins config set`).
pub fn apply_setting_overrides(config: &mut Config, pairs: &[(&str, &str)]) {
    for (key, value) in pairs {
        let dotted = classic_key_aliases().get(*key).copied().unwrap_or(key);
        apply_dotted_override(config, dotted, value);
    }
}

/// Load config, apply `pairs`, and write the result back to the resolved config path.
///
/// # Arguments
///
/// * `files_dir` - Optional `BOOKCLERK_FILES_DIR` override.
/// * `config_path` - Optional explicit `config.toml` path.
/// * `pairs` - `(key, value)` overrides (classic or dotted).
///
/// # Returns
///
/// The updated [`Config`] after a successful write.
///
/// # Errors
///
/// Propagates load, validation, and write failures as [`crate::ConfigError`].
pub fn apply_config_updates(
    files_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
    pairs: &[(&str, &str)],
) -> Result<Config> {
    let mut cfg = Config::load(files_dir, config_path.clone())?;
    apply_setting_overrides(&mut cfg, pairs);
    let path = cfg.paths().config_file.clone();
    cfg.write_toml_file(&path)?;
    Ok(cfg)
}

/// Load `path`, apply `pairs`, and write the updated TOML back to the same path.
///
/// # Arguments
///
/// * `path` - Existing `config.toml` to mutate in place.
/// * `pairs` - `(key, value)` overrides (classic or dotted).
///
/// # Errors
///
/// Propagates read, parse, and write failures as [`crate::ConfigError`].
pub fn apply_config_updates_from_path(path: &Path, pairs: &[(&str, &str)]) -> Result<Config> {
    let mut cfg = Config::from_toml_file(path)?;
    apply_setting_overrides(&mut cfg, pairs);
    cfg.write_toml_file(path)?;
    Ok(cfg)
}

/// Applies one dotted `key=value` override onto [`Config`]; unknown keys are logged and ignored.
fn apply_dotted_override(config: &mut Config, key: &str, value: &str) {
    let v = value.trim();
    match key {
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
        "output.widevine" => config.output.widevine = parse_bool(v).unwrap_or(false),
        "output.xhe_aac" => config.output.xhe_aac = parse_bool(v).unwrap_or(false),
        "output.widevine_cdm" => config.output.widevine_cdm = Some(PathBuf::from(v)),
        "output.widevine_cdm_provider" => {
            config.output.widevine_cdm_provider = Some(v.to_string());
        }
        "auth.allow_plaintext" => {
            tracing::warn!(key, "auth.allow_plaintext is no longer supported; Bookclerk always encrypts credentials via the master key (master.key)");
        }
        "auth.password" => {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                config.auth.password = None;
            } else {
                crate::redact::register_secret(trimmed);
                config.auth.password = Some(trimmed.to_string());
            }
        }
        "auth.password_file" => {
            tracing::warn!(
                key,
                "auth.password_file is no longer supported; use BOOKCLERK_AUTH_PASSWORD \
                 or [auth].password (then `bookclerk config master-key wrap` / daemon reload)"
            );
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
        "database.plugin" => {
            config.database.plugin = v.trim().to_string();
        }
        "database.sqlite.path" => {
            let trimmed = v.trim();
            config.database.sqlite.path = if trimmed.is_empty() {
                None
            } else {
                Some(PathBuf::from(trimmed))
            };
        }
        "database.d1.account_id" => {
            config.database.d1.account_id = v.trim().to_string();
        }
        "database.d1.database_id" => {
            config.database.d1.database_id = v.trim().to_string();
        }
        "database.d1.api_base" => {
            config.database.d1.api_base = v.trim().to_string();
        }
        "database.postgres.url" => {
            let trimmed = v.trim();
            config.database.postgres.url = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            };
        }
        "database.postgres.url_file" => {
            let trimmed = v.trim();
            config.database.postgres.url_file = if trimmed.is_empty() {
                None
            } else {
                Some(PathBuf::from(trimmed))
            };
        }
        "daemon.listen" => match crate::ListenAddrs::parse_list(v) {
            Ok(addrs) => config.daemon.listen = addrs,
            Err(err) => {
                tracing::warn!(key, value = v, error = %err, "invalid daemon.listen; ignoring")
            }
        },
        "daemon.auth.enabled" => {
            if let Some(b) = parse_bool(v) {
                config.daemon.auth.enabled = b;
            }
        }
        "daemon.auth.require_second_factor" => {
            if let Some(b) = parse_bool(v) {
                config.daemon.auth.require_second_factor = b;
            }
        }
        "daemon.auth.tray_handoff_ttl_secs" => {
            if let Ok(secs) = v.trim().parse::<u64>() {
                config.daemon.auth.tray_handoff_ttl_secs = secs.clamp(
                    crate::TRAY_HANDOFF_TTL_SECS_MIN,
                    crate::TRAY_HANDOFF_TTL_SECS_MAX,
                );
            }
        }
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
        "jobs.max_pending" => {
            if let Ok(n) = v.parse::<u32>() {
                config.jobs.max_pending = n.max(1);
            }
        }
        "jobs.lease_seconds" => {
            if let Ok(n) = v.parse::<u64>() {
                config.jobs.lease_seconds = n.max(5);
            }
        }
        "jobs.max_attempts" => {
            if let Ok(n) = v.parse::<u32>() {
                config.jobs.max_attempts = n.max(1);
            }
        }
        "jobs.retention_days" => {
            if let Ok(n) = v.parse::<u64>() {
                config.jobs.retention_days = n.max(1);
            }
        }
        "jobs.temp_quota_bytes" => {
            if let Ok(n) = v.parse::<u64>() {
                config.jobs.temp_quota_bytes = n;
            }
        }
        "jobs.concurrency.network" => {
            if let Ok(n) = v.parse::<u32>() {
                config.jobs.concurrency.network = n.max(1);
            }
        }
        "events.retention_days" => {
            if let Ok(n) = v.parse::<u64>() {
                config.events.retention_days = n.max(1);
            }
        }
        "events.dead_letter_retention_days" => {
            if let Ok(n) = v.parse::<u64>() {
                config.events.dead_letter_retention_days = n.max(1);
            }
        }
        "events.concurrency" => {
            if let Ok(n) = v.parse::<u32>() {
                config.events.concurrency = n.max(1);
            }
        }
        "discovery.embeddings_enabled" => {
            config.discovery.embeddings_enabled = parse_bool(v).unwrap_or(true);
        }
        "discovery.embedding_model" => {
            config.discovery.embedding_model = v.to_string();
        }
        "discovery.embed_intra_threads" => {
            if let Ok(n) = v.parse::<usize>() {
                config.discovery.embed_intra_threads = n.max(1);
            }
        }
        "discovery.openlibrary_enabled" => {
            config.discovery.openlibrary_enabled = parse_bool(v).unwrap_or(true);
        }
        "discovery.listen_sync_interval_minutes" => {
            if let Ok(n) = v.parse() {
                config.discovery.listen_sync_interval_minutes = n;
            }
        }
        "discovery.recommend_limit" => {
            if let Ok(n) = v.parse::<usize>() {
                config.discovery.recommend_limit = n.max(1);
            }
        }
        "integrations.public_origin" => {
            config.integrations.public_origin = (!v.is_empty()).then(|| v.to_string());
        }
        "integrations.claim_ticket_ttl_hours" => {
            if let Ok(n) = v.parse::<u64>() {
                config.integrations.claim_ticket_ttl_hours = n.max(1);
            }
        }
        "integrations.portal_session_ttl_hours" => {
            if let Ok(n) = v.parse::<u64>() {
                config.integrations.portal_session_ttl_hours = n.max(1);
            }
        }
        "plugins.isolation" => {
            if let Some(isolation) = crate::Isolation::parse(v) {
                config.plugins.isolation = isolation;
            }
        }
        "media.isolation" => {
            if let Some(isolation) = crate::Isolation::parse(v) {
                config.media.isolation = isolation;
            }
        }
        "plugins.jail.memory_mib" => {
            if v.is_empty() {
                config.plugins.jail.memory_mib = None;
            } else if let Ok(n) = v.parse::<u64>() {
                config.plugins.jail.memory_mib = Some(n);
            }
        }
        "plugins.jail.cpu_rate_percent" | "plugins.jail.cpu_cores" => {
            if v.is_empty() {
                // Empty resets to the platform default (80% of one core).
                config.plugins.jail.cpu_rate_percent = Some(80);
            } else if v.contains('.') {
                if let Ok(cores) = v.parse::<f64>() {
                    let pct = ((cores * 100.0).round() as u32)
                        .clamp(1, crate::plugins::host_cpu_rate_max());
                    config.plugins.jail.cpu_rate_percent = Some(pct);
                }
            } else if let Ok(n) = v.parse::<u32>() {
                config.plugins.jail.cpu_rate_percent =
                    Some(n.clamp(1, crate::plugins::host_cpu_rate_max()));
            }
        }
        "plugins.jail.extra_processes" => {
            if v.is_empty() {
                config.plugins.jail.extra_processes = Some(2);
            } else if let Ok(n) = v.parse::<u32>() {
                config.plugins.jail.extra_processes = Some(n.min(62));
            }
        }
        _ => tracing::warn!(key, value = v, "unknown setting override; ignoring"),
    }
}

/// Maps `purchased` / `published` (and aliases) onto [`FileTimestampMode`]; anything else is `Now`.
fn parse_timestamp_mode(v: &str) -> FileTimestampMode {
    match v.to_ascii_lowercase().as_str() {
        "purchased" | "dateadded" => FileTimestampMode::Purchased,
        "published" | "releasedate" => FileTimestampMode::Published,
        _ => FileTimestampMode::Now,
    }
}

/// Parses `1`/`true`/`yes`/`on` or `0`/`false`/`no`/`off`; other strings are `None`.
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
    fn dotted_override_sets_widevine() {
        let mut cfg = Config::default();
        apply_setting_overrides(&mut cfg, &[("output.widevine", "true")]);
        assert!(cfg.output.widevine);
    }

    #[test]
    fn dotted_override_sets_daemon_listen_and_auth() {
        let mut cfg = Config::default();
        apply_setting_overrides(
            &mut cfg,
            &[
                ("daemon.listen", "[::1]:8787"),
                ("daemon.auth.enabled", "false"),
                ("daemon.auth.tray_handoff_ttl_secs", "45"),
            ],
        );
        assert_eq!(cfg.daemon.listen.as_slice(), ["[::1]:8787"]);
        assert!(!cfg.daemon.auth.enabled);
        assert_eq!(cfg.daemon.auth.tray_handoff_ttl_secs, 45);
        apply_setting_overrides(&mut cfg, &[("daemon.auth.tray_handoff_ttl_secs", "10")]);
        assert_eq!(cfg.daemon.auth.tray_handoff_ttl_secs, 30);
        apply_setting_overrides(&mut cfg, &[("daemon.auth.tray_handoff_ttl_secs", "9999")]);
        assert_eq!(cfg.daemon.auth.tray_handoff_ttl_secs, 900);
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

    #[test]
    fn database_connection_overrides_persist() {
        let mut cfg = Config::default();
        apply_setting_overrides(
            &mut cfg,
            &[
                ("database.sqlite.path", "custom.db"),
                ("database.d1.account_id", "acc"),
                ("database.d1.database_id", "dbid"),
                ("database.postgres.url", "postgres://localhost/bookclerk"),
                ("database.plugin", ""),
            ],
        );
        assert_eq!(
            cfg.database.sqlite.path.as_deref(),
            Some(std::path::Path::new("custom.db"))
        );
        assert_eq!(cfg.database.d1.account_id, "acc");
        assert_eq!(cfg.database.d1.database_id, "dbid");
        assert_eq!(
            cfg.database.postgres.url.as_deref(),
            Some("postgres://localhost/bookclerk")
        );
        assert_eq!(cfg.database.plugin, "");
    }
}
