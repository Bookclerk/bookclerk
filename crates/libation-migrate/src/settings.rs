//! Import classic `Settings.json` into libation-rs [`Config`].

use std::path::Path;

use libation_config::{
    parse_replacement_characters, AudioQuality, BadBookAction, Config, DownloadFormat,
    FileTimestampMode, StorageBackendKind,
};
use serde_json::Value;

use crate::error::{MigrateError, Result};

/// Load Settings.json as a JSON object.
pub fn load_settings_json(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path).map_err(|err| {
        MigrateError::Settings(format!("failed to read {}: {err}", path.display()))
    })?;
    serde_json::from_str(&text)
        .map_err(|err| MigrateError::Settings(format!("invalid Settings.json: {err}")))
}

/// Apply classic Settings.json keys onto `config`.
pub fn apply_settings_json(config: &mut Config, settings: &Value) {
    if let Some(books) = string_at(settings, "Books") {
        config.storage.backend = StorageBackendKind::Local;
        config.storage.local.root = Path::new(books).to_path_buf();
    }

    if let Some(quality) = string_at(settings, "FileDownloadQuality") {
        config.download.quality = match quality.to_ascii_lowercase().as_str() {
            "normal" => AudioQuality::Normal,
            _ => AudioQuality::High,
        };
    }

    if let Some(lossy) = bool_at(settings, "DecryptToLossy") {
        config.download.format = if lossy {
            DownloadFormat::Mp3
        } else {
            DownloadFormat::M4b
        };
    }

    if let Some(wv) = bool_at(settings, "UseWidevine") {
        config.download.widevine = wv;
    }
    if let Some(xhe) = bool_at(settings, "Request_xHE_AAC") {
        config.download.xhe_aac = xhe;
    }

    if let Some(folder) = string_at(settings, "FolderTemplate") {
        config.download.folder_template = Some(folder.to_string());
    }
    if let Some(file) = string_at(settings, "FileTemplate") {
        config.download.file_template = Some(file.to_string());
    }

    if let Some(v) = bool_at(settings, "DownloadCoverArt") {
        config.download.download_cover = v;
    }
    if let Some(v) = bool_at(settings, "CreateCueSheet") {
        config.download.create_cue = v;
    }
    if let Some(v) = bool_at(settings, "AllowLibationFixup") {
        config.download.fixup_metadata = v;
    }
    if let Some(v) = bool_at(settings, "SaveMetadataToFile") {
        config.download.save_metadata_json = v;
    }

    // AutoDownloadEpisodes is poorly named upstream: it means auto-download
    // after scan (books), not podcasts-only.
    if let Some(auto) = bool_at(settings, "AutoDownloadEpisodes") {
        config.library.auto_liberate = auto;
    }

    if let Some(auto_scan) = bool_at(settings, "AutoScan") {
        // Classic GUI scans about every 5 minutes when enabled.
        config.library.scan_interval_minutes = if auto_scan { 5 } else { 0 };
    }

    if let Some(v) = bool_at(settings, "OverwriteExisting") {
        config.download.overwrite_existing = v;
    }
    if let Some(v) = bool_at(settings, "ImportEpisodes") {
        config.library.import_episodes = v;
    }
    if let Some(v) = bool_at(settings, "ImportPlusTitles") {
        config.library.import_plus_titles = v;
    }
    if let Some(dir) = string_at(settings, "InProgress") {
        config.download.in_progress = Some(Path::new(dir).to_path_buf());
    }
    if let Some(bad) = string_at(settings, "BadBook") {
        config.download.bad_book_action = match bad {
            "Abort" => BadBookAction::Abort,
            "Retry" => BadBookAction::Retry,
            "Ignore" => BadBookAction::Ignore,
            _ => BadBookAction::Ask,
        };
    }
    if let Some(v) = bool_at(settings, "DownloadEpisodes") {
        config.library.download_episodes = v;
    }
    if let Some(v) = bool_at(settings, "SavePodcastsToParentFolder") {
        config.library.save_podcasts_to_parent_folder = v;
    }
    if let Some(v) = bool_at(settings, "SplitFilesByChapter") {
        config.download.split_files_by_chapter = v;
    }
    if let Some(v) = string_at(settings, "ChapterFileTemplate") {
        config.download.chapter_file_template = Some(v.to_string());
    }
    if let Some(v) = string_at(settings, "ChapterTitleTemplate") {
        config.download.chapter_title_template = Some(v.to_string());
    }
    if let Some(v) = settings.get("MinimumFileDuration").and_then(Value::as_i64) {
        config.download.minimum_file_duration_minutes = v as u32;
    }
    if let Some(v) = bool_at(settings, "CombineNestedChapterTitles") {
        config.download.combine_nested_chapter_titles = v;
    }
    if let Some(v) = bool_at(settings, "MergeOpeningAndEndCredits") {
        config.download.merge_opening_and_end_credits = v;
    }
    if let Some(v) = bool_at(settings, "StripUnabridged") {
        config.download.strip_unabridged = v;
    }
    if let Some(v) = bool_at(settings, "StripAudibleBrandAudio") {
        config.download.strip_audible_brand_audio = v;
    }
    if let Some(v) = bool_at(settings, "DownloadClipsBookmarks") {
        config.download.download_clips_bookmarks = v;
    }
    if let Some(v) = bool_at(settings, "RetainAaxFile") {
        config.download.retain_aax_file = v;
    }
    if let Some(v) = settings.get("DownloadSpeedLimit").and_then(Value::as_i64) {
        config.download.download_speed_limit_kbps = v as u32;
    }
    if let Some(v) = string_at(settings, "LameTarget") {
        config.download.lame.target = v.to_string();
    }
    if let Some(v) = settings.get("LameVBRQuality").and_then(Value::as_i64) {
        config.download.lame.vbr_quality = v as u8;
    }
    if let Some(v) = settings.get("LameBitrate").and_then(Value::as_i64) {
        config.download.lame.bitrate_kbps = v as u32;
    }
    if let Some(v) = string_at(settings, "LameMode") {
        config.download.lame.mode = v.to_string();
    }
    if let Some(v) = bool_at(settings, "LameDownsampleMono") {
        config.download.lame.downsample_mono = v;
    }
    if let Some(v) = bool_at(settings, "LameConstantBitrate") {
        config.download.lame.constant_bitrate = v;
    }
    if let Some(v) = settings.get("MaxSampleRate").and_then(Value::as_i64) {
        config.download.max_sample_rate = Some(v as u32);
    }
    if let Some(v) = string_at(settings, "CreationTime") {
        config.download.creation_time = parse_timestamp_mode(v);
    }
    if let Some(v) = string_at(settings, "LastWriteTime") {
        config.download.last_write_time = parse_timestamp_mode(v);
    }
    if let Some(v) = settings.get("ReplacementCharacters") {
        config.download.replacement_characters = parse_replacement_characters(v);
    }
}

fn parse_timestamp_mode(v: &str) -> FileTimestampMode {
    match v.to_ascii_lowercase().as_str() {
        "purchased" | "dateadded" => FileTimestampMode::Purchased,
        "published" | "releasedate" => FileTimestampMode::Published,
        _ => FileTimestampMode::Now,
    }
}

fn string_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn bool_at(value: &Value, key: &str) -> Option<bool> {
    value.get(key).and_then(Value::as_bool)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_core_settings() {
        let settings = serde_json::json!({
            "Books": "/data/Audiobooks",
            "FileDownloadQuality": "Normal",
            "DecryptToLossy": true,
            "UseWidevine": true,
            "Request_xHE_AAC": true,
            "FolderTemplate": "<author>/<title>",
            "FileTemplate": "<title> [<asin>]",
            "DownloadCoverArt": true,
            "CreateCueSheet": true,
            "AllowLibationFixup": false,
            "SaveMetadataToFile": false,
            "AutoDownloadEpisodes": true,
            "AutoScan": true
        });
        let mut cfg = Config::default();
        apply_settings_json(&mut cfg, &settings);
        assert_eq!(cfg.storage.local.root, Path::new("/data/Audiobooks"));
        assert_eq!(cfg.download.quality, AudioQuality::Normal);
        assert_eq!(cfg.download.format, DownloadFormat::Mp3);
        assert!(cfg.download.widevine);
        assert!(cfg.download.xhe_aac);
        assert!(cfg.download.download_cover);
        assert!(cfg.download.create_cue);
        assert!(!cfg.download.fixup_metadata);
        assert!(!cfg.download.save_metadata_json);
        assert_eq!(
            cfg.download.folder_template.as_deref(),
            Some("<author>/<title>")
        );
        assert_eq!(
            cfg.download.file_template.as_deref(),
            Some("<title> [<asin>]")
        );
        assert!(cfg.library.auto_liberate);
        assert_eq!(cfg.library.scan_interval_minutes, 5);
    }
}
