//! Import classic `Settings.json` into bookclerk [`Config`].

use std::path::Path;

use bookclerk_config::{
    parse_replacement_characters, BadBookAction, Config, FileTimestampMode, OutputFormat,
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
        config.output.local.enabled = true;
        config.output.s3.enabled = false;
        config.output.local.root = Path::new(books).to_path_buf();
    }

    if let Some(quality) = string_at(settings, "FileDownloadQuality") {
        let bitrate = match quality.to_ascii_lowercase().as_str() {
            "normal" => "normal",
            _ => "high",
        };
        config.sources.set_string("audible", "bitrate", bitrate);
    }

    if let Some(lossy) = bool_at(settings, "DecryptToLossy") {
        config.output.format = if lossy {
            OutputFormat::SingleMp3
        } else {
            OutputFormat::EnrichedM4b
        };
    }

    if let Some(wv) = bool_at(settings, "UseWidevine") {
        config.output.widevine = wv;
    }
    if let Some(xhe) = bool_at(settings, "Request_xHE_AAC") {
        config.output.xhe_aac = xhe;
    }

    if let Some(folder) = string_at(settings, "FolderTemplate") {
        config.output.folder_template = Some(folder.to_string());
    }
    if let Some(file) = string_at(settings, "FileTemplate") {
        config.output.file_template = Some(file.to_string());
    }

    if let Some(v) = bool_at(settings, "DownloadCoverArt") {
        config.output.download_cover = v;
    }
    if let Some(v) = bool_at(settings, "CreateCueSheet") {
        config.output.create_cue = v;
    }
    if let Some(v) = bool_at(settings, "AllowLibationFixup") {
        config.output.fixup_metadata = v;
    }
    if let Some(v) = bool_at(settings, "SaveMetadataToFile") {
        config.output.save_metadata_json = v;
    }

    // AutoDownloadEpisodes is poorly named upstream: it means auto-download
    // after scan (books), not podcasts-only.
    if let Some(auto) = bool_at(settings, "AutoDownloadEpisodes") {
        config.library.auto_acquire = auto;
    }

    if let Some(auto_scan) = bool_at(settings, "AutoScan") {
        // Classic GUI scans about every 5 minutes when enabled.
        config.library.scan_interval_minutes = if auto_scan { 5 } else { 0 };
    }

    if let Some(v) = bool_at(settings, "OverwriteExisting") {
        config.output.overwrite_existing = v;
    }
    if let Some(v) = bool_at(settings, "ImportEpisodes") {
        config.library.import_episodes = v;
    }
    if let Some(v) = bool_at(settings, "ImportPlusTitles") {
        config.library.import_plus_titles = v;
    }
    if let Some(dir) = string_at(settings, "InProgress") {
        config.output.in_progress = Some(Path::new(dir).to_path_buf());
    }
    if let Some(bad) = string_at(settings, "BadBook") {
        config.output.bad_book_action = match bad {
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
        if v {
            config.output.format = OutputFormat::SplitMp3ByChapter;
        }
    }
    if let Some(v) = string_at(settings, "ChapterFileTemplate") {
        config.output.chapter_file_template = Some(v.to_string());
    }
    if let Some(v) = string_at(settings, "ChapterTitleTemplate") {
        config.output.chapter_title_template = Some(v.to_string());
    }
    if let Some(v) = settings.get("MinimumFileDuration").and_then(Value::as_i64) {
        config.output.minimum_file_duration_minutes = v as u32;
    }
    if let Some(v) = bool_at(settings, "CombineNestedChapterTitles") {
        config.output.combine_nested_chapter_titles = v;
    }
    if let Some(v) = bool_at(settings, "MergeOpeningAndEndCredits") {
        config.output.merge_opening_and_end_credits = v;
    }
    if let Some(v) = bool_at(settings, "StripUnabridged") {
        config.output.strip_unabridged = v;
    }
    if let Some(v) = bool_at(settings, "StripAudibleBrandAudio") {
        config.output.strip_audible_brand_audio = v;
    }
    if let Some(v) = bool_at(settings, "DownloadClipsBookmarks") {
        config.output.download_clips_bookmarks = v;
    }
    if let Some(v) = bool_at(settings, "RetainAaxFile") {
        config.output.retain_aax_file = v;
    }
    if let Some(v) = settings.get("DownloadSpeedLimit").and_then(Value::as_i64) {
        config.output.download_speed_limit_kbps = v as u32;
    }
    if let Some(v) = string_at(settings, "LameTarget") {
        config.output.lame.target = v.to_string();
    }
    if let Some(v) = settings.get("LameVBRQuality").and_then(Value::as_i64) {
        config.output.lame.vbr_quality = v as u8;
    }
    if let Some(v) = settings.get("LameBitrate").and_then(Value::as_i64) {
        config.output.lame.bitrate_kbps = v as u32;
    }
    if let Some(v) = string_at(settings, "LameMode") {
        config.output.lame.mode = v.to_string();
    }
    if let Some(v) = bool_at(settings, "LameDownsampleMono") {
        config.output.lame.downsample_mono = v;
    }
    if let Some(v) = bool_at(settings, "LameConstantBitrate") {
        config.output.lame.constant_bitrate = v;
    }
    if let Some(v) = settings.get("MaxSampleRate").and_then(Value::as_i64) {
        config.output.max_sample_rate = Some(v as u32);
    }
    if let Some(v) = string_at(settings, "CreationTime") {
        config.output.creation_time = parse_timestamp_mode(v);
    }
    if let Some(v) = string_at(settings, "LastWriteTime") {
        config.output.last_write_time = parse_timestamp_mode(v);
    }
    if let Some(v) = settings.get("ReplacementCharacters") {
        config.output.replacement_characters = parse_replacement_characters(v);
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

/// Best-effort reverse of [`apply_settings_json`] for Libation Settings.json export.
#[must_use]
pub fn config_to_settings_json(config: &Config) -> Value {
    use serde_json::json;

    let quality = match config.sources.get_string("audible", "bitrate") {
        Some("normal") => "Normal",
        _ => "High",
    };
    let decrypt_to_lossy = matches!(
        config.output.format,
        OutputFormat::SingleMp3 | OutputFormat::SplitMp3ByChapter
    );
    let bad_book = match config.output.bad_book_action {
        BadBookAction::Abort => "Abort",
        BadBookAction::Retry => "Retry",
        BadBookAction::Ignore => "Ignore",
        BadBookAction::Ask => "Ask",
    };
    let mut map = serde_json::Map::new();
    map.insert(
        "Books".into(),
        json!(config.output.local.root.display().to_string()),
    );
    map.insert("FileDownloadQuality".into(), json!(quality));
    map.insert("DecryptToLossy".into(), json!(decrypt_to_lossy));
    map.insert("UseWidevine".into(), json!(config.output.widevine));
    map.insert("Request_xHE_AAC".into(), json!(config.output.xhe_aac));
    if let Some(folder) = &config.output.folder_template {
        map.insert("FolderTemplate".into(), json!(folder));
    }
    if let Some(file) = &config.output.file_template {
        map.insert("FileTemplate".into(), json!(file));
    }
    map.insert(
        "DownloadCoverArt".into(),
        json!(config.output.download_cover),
    );
    map.insert("CreateCueSheet".into(), json!(config.output.create_cue));
    map.insert(
        "AllowLibationFixup".into(),
        json!(config.output.fixup_metadata),
    );
    map.insert(
        "SaveMetadataToFile".into(),
        json!(config.output.save_metadata_json),
    );
    map.insert(
        "AutoDownloadEpisodes".into(),
        json!(config.library.auto_acquire),
    );
    map.insert(
        "AutoScan".into(),
        json!(config.library.scan_interval_minutes > 0),
    );
    map.insert(
        "OverwriteExisting".into(),
        json!(config.output.overwrite_existing),
    );
    map.insert(
        "ImportEpisodes".into(),
        json!(config.library.import_episodes),
    );
    map.insert(
        "ImportPlusTitles".into(),
        json!(config.library.import_plus_titles),
    );
    if let Some(dir) = &config.output.in_progress {
        map.insert("InProgress".into(), json!(dir.display().to_string()));
    }
    map.insert("BadBook".into(), json!(bad_book));
    map.insert(
        "DownloadEpisodes".into(),
        json!(config.library.download_episodes),
    );
    map.insert(
        "SavePodcastsToParentFolder".into(),
        json!(config.library.save_podcasts_to_parent_folder),
    );
    map.insert(
        "SplitFilesByChapter".into(),
        json!(matches!(
            config.output.format,
            OutputFormat::SplitMp3ByChapter
        )),
    );
    if let Some(v) = &config.output.chapter_file_template {
        map.insert("ChapterFileTemplate".into(), json!(v));
    }
    if let Some(v) = &config.output.chapter_title_template {
        map.insert("ChapterTitleTemplate".into(), json!(v));
    }
    map.insert(
        "MinimumFileDuration".into(),
        json!(config.output.minimum_file_duration_minutes),
    );
    map.insert(
        "CombineNestedChapterTitles".into(),
        json!(config.output.combine_nested_chapter_titles),
    );
    map.insert(
        "MergeOpeningAndEndCredits".into(),
        json!(config.output.merge_opening_and_end_credits),
    );
    map.insert(
        "StripUnabridged".into(),
        json!(config.output.strip_unabridged),
    );
    map.insert(
        "StripAudibleBrandAudio".into(),
        json!(config.output.strip_audible_brand_audio),
    );
    map.insert(
        "DownloadClipsBookmarks".into(),
        json!(config.output.download_clips_bookmarks),
    );
    map.insert("RetainAaxFile".into(), json!(config.output.retain_aax_file));
    map.insert(
        "DownloadSpeedLimit".into(),
        json!(config.output.download_speed_limit_kbps),
    );
    map.insert("LameTarget".into(), json!(config.output.lame.target));
    map.insert(
        "LameVBRQuality".into(),
        json!(config.output.lame.vbr_quality),
    );
    map.insert("LameBitrate".into(), json!(config.output.lame.bitrate_kbps));
    map.insert("LameMode".into(), json!(config.output.lame.mode));
    map.insert(
        "LameDownsampleMono".into(),
        json!(config.output.lame.downsample_mono),
    );
    map.insert(
        "LameConstantBitrate".into(),
        json!(config.output.lame.constant_bitrate),
    );
    if let Some(rate) = config.output.max_sample_rate {
        map.insert("MaxSampleRate".into(), json!(rate));
    }
    map.insert(
        "CreationTime".into(),
        json!(match config.output.creation_time {
            FileTimestampMode::Purchased => "DateAdded",
            FileTimestampMode::Published => "PublishedDate",
            FileTimestampMode::Now => "Now",
        }),
    );
    map.insert(
        "LastWriteTime".into(),
        json!(match config.output.last_write_time {
            FileTimestampMode::Purchased => "DateAdded",
            FileTimestampMode::Published => "PublishedDate",
            FileTimestampMode::Now => "Now",
        }),
    );
    Value::Object(map)
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
        assert_eq!(cfg.output.local.root, Path::new("/data/Audiobooks"));
        assert_eq!(cfg.sources.get_string("audible", "bitrate"), Some("normal"));
        assert_eq!(cfg.output.format, OutputFormat::SingleMp3);
        assert!(cfg.output.widevine);
        assert!(cfg.output.xhe_aac);
        assert!(cfg.output.download_cover);
        assert!(cfg.output.create_cue);
        assert!(!cfg.output.fixup_metadata);
        assert!(!cfg.output.save_metadata_json);
        assert_eq!(
            cfg.output.folder_template.as_deref(),
            Some("<author>/<title>")
        );
        assert_eq!(
            cfg.output.file_template.as_deref(),
            Some("<title> [<asin>]")
        );
        assert!(cfg.library.auto_acquire);
        assert_eq!(cfg.library.scan_interval_minutes, 5);
    }
}
