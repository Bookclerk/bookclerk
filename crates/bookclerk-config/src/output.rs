//! Acquired-audio output packaging (`[output]`).
//!
//! ```toml
//! [output]
//! format = "enriched_m4b"
//! naming_profile = "audiobookshelf"
//! # Host-local scratch directory (stays in config.toml).
//! # in_progress = "/var/tmp/bookclerk"
//! ```
//!
//! The shared table is the `core.output` cluster configuration document in
//! the library database. This TOML table (plus `BOOKCLERK_OUTPUT_*`) only
//! seeds that document the first time a host enrolls against a database
//! without one; after that, edit it with `bookclerk config set output.*`,
//! `PATCH /api/settings`, or `PUT /api/config/domains/core.output`. The
//! scratch directory `in_progress` is host-local bootstrap and is read from
//! here on every start.
//!
//! Destinations (`local`, `s3`, …) are plugin instances with their own
//! settings (root, bucket, prefix, naming overrides) and credentials; nothing
//! destination-specific lives in `config.toml`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::extras::{FileTimestampMode, LameConfig, PathSanitizationMode, ReplacementRule};
use crate::naming_profile::{NamingProfile, ResolvedNamingTemplates};
use crate::path_limits::DEFAULT_MAX_FILENAME_LENGTH;
use crate::pipeline_opts::{ChapterJsonMode, OutputFormat};

/// Optional naming overrides for one destination plugin.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DestinationNaming {
    /// Override `[output].naming_profile` for this destination when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub naming_profile: Option<NamingProfile>,
    /// Optional folder-path template override (Libation tag syntax).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_template: Option<String>,
    /// Optional file-stem template override (no extension).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_template: Option<String>,
    /// Optional per-chapter file-stem template when splitting by chapter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chapter_file_template: Option<String>,
}

impl DestinationNaming {
    /// Merge destination overrides over global `[output]` naming.
    #[must_use]
    pub fn resolve_over(&self, global: &OutputConfig) -> ResolvedNamingTemplates {
        let profile = self.naming_profile.unwrap_or(global.naming_profile);
        ResolvedNamingTemplates::resolve(
            profile,
            self.folder_template
                .as_deref()
                .or(global.folder_template.as_deref()),
            self.file_template
                .as_deref()
                .or(global.file_template.as_deref()),
            self.chapter_file_template
                .as_deref()
                .or(global.chapter_file_template.as_deref()),
        )
    }

    /// Profile after destination override (else global).
    #[must_use]
    pub fn effective_profile(&self, global: &OutputConfig) -> NamingProfile {
        self.naming_profile.unwrap_or(global.naming_profile)
    }

    /// Destination folder template, falling back to global `[output]`.
    #[must_use]
    pub fn effective_folder_template(&self, global: &OutputConfig) -> Option<String> {
        self.folder_template
            .clone()
            .or_else(|| global.folder_template.clone())
    }

    /// Destination file-stem template, falling back to global `[output]`.
    #[must_use]
    pub fn effective_file_template(&self, global: &OutputConfig) -> Option<String> {
        self.file_template
            .clone()
            .or_else(|| global.file_template.clone())
    }

    /// Destination chapter-file template, falling back to global `[output]`.
    #[must_use]
    pub fn effective_chapter_file_template(&self, global: &OutputConfig) -> Option<String> {
        self.chapter_file_template
            .clone()
            .or_else(|| global.chapter_file_template.clone())
    }
}

/// Acquire packaging (`[output]`).
///
/// Destination plugins (`local`, `s3`, …) declare their own `[[settings]]`
/// (root, bucket, prefix, per-destination naming overrides) and are
/// configured on their plugin instances; nothing destination-specific lives
/// in `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct OutputConfig {
    /// Post-acquire packaging format (default: enriched M4B).
    pub format: OutputFormat,
    /// Prefer Widevine/CENC (also enables Adrm→Widevine fallback; auto-provisions L3 CDM).
    pub widevine: bool,
    /// Prefer xHE-AAC on the Widevine path when offered.
    pub xhe_aac: bool,
    /// Optional local Widevine `.wvd` path (absolute or relative to `BOOKCLERK_FILES_DIR`).
    pub widevine_cdm: Option<PathBuf>,
    /// Remote L3 CDM provider. `None` uses classic Libation AudibleCdm; empty/`off` disables.
    pub widevine_cdm_provider: Option<String>,
    /// Named path-template preset (`audiobookshelf` default, or `classic`).
    pub naming_profile: NamingProfile,
    /// Optional folder-path template override; `None` uses the naming profile default.
    pub folder_template: Option<String>,
    /// Optional file-stem template override; `None` uses the naming profile default.
    pub file_template: Option<String>,
    /// Save cover JPEG alongside audio.
    pub download_cover: bool,
    /// Download companion PDF when available.
    pub download_pdf: bool,
    /// Write a `.cue` sidecar from API chapters.
    pub create_cue: bool,
    /// Embed tags, cover, and chapters natively.
    pub fixup_metadata: bool,
    /// Chapter JSON sidecars: `off` | `flat` | `tree` | `both`.
    pub chapter_json: ChapterJsonMode,
    /// Deprecated: use [`Self::chapter_json`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save_chapter_json: Option<bool>,
    /// Persist raw catalog API JSON (`metadata.json`).
    pub save_metadata_json: bool,
    /// Cover image size for download/embed (`500`, `1215`, or `native`).
    pub cover_size: String,
    /// Preferred Audible chapter API layout when fetching (`tree` or `flat`).
    pub chapter_layout: String,
    /// Re-acquire when media already exists at the destination.
    pub overwrite_existing: bool,
    /// When multiple destinations are enabled and only some already have the
    /// title: copy from a present dest, re-fetch into missing only, or re-fetch
    /// into every destination.
    pub multi_destination: MultiDestinationMode,
    /// Scratch directory for in-progress work; relative to files_dir.
    pub in_progress: Option<PathBuf>,
    /// Operator action when a title fails to acquire (`ask` / `abort` / `retry` / `ignore`).
    pub bad_book_action: BadBookAction,
    /// Max MP3 part size in MiB when format is `split_mp3_by_size`.
    pub split_mp3_max_mb: u32,
    /// Optional per-chapter file-stem template; `None` uses the naming profile default.
    pub chapter_file_template: Option<String>,
    /// Optional embedded chapter-title template when rewriting chapter names.
    pub chapter_title_template: Option<String>,
    /// Drop chapter splits shorter than this many minutes (`0` = keep all).
    pub minimum_file_duration_minutes: u32,
    /// When true, flatten nested chapter titles into a single path segment.
    pub combine_nested_chapter_titles: bool,
    /// When true, merge opening/end credit chapters into adjacent content chapters.
    pub merge_opening_and_end_credits: bool,
    /// When true, strip an "Unabridged" suffix from titles used in naming.
    pub strip_unabridged: bool,
    /// When true, trim Audible brand intro/outro audio from the remux window.
    pub strip_audible_brand_audio: bool,
    /// When true, download Audible clips/bookmarks sidecars when the store offers them.
    pub download_clips_bookmarks: bool,
    /// Keep encrypted download in storage (`RetainAaxFile`).
    pub retain_aax_file: bool,
    /// Fetch speed cap in KB/s (`0` = unlimited).
    pub download_speed_limit_kbps: u32,
    /// LAME encoder knobs used when packaging to MP3.
    pub lame: LameConfig,
    /// Optional ceiling for output sample rate in Hz (`None` = leave source rate).
    pub max_sample_rate: Option<u32>,
    /// How to set the file creation / birth timestamp after acquire.
    pub creation_time: FileTimestampMode,
    /// How to set the file last-write / mtime after acquire.
    pub last_write_time: FileTimestampMode,
    /// Auto / Windows / POSIX / S3 sanitisation profile when replacement rules are empty.
    pub path_sanitization: PathSanitizationMode,
    /// Explicit find/replace rules for path segments (wins over [`Self::path_sanitization`]).
    pub replacement_characters: Vec<ReplacementRule>,
    /// Max length per path segment. Default 255; `0` disables truncation.
    pub max_filename_length: u32,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            format: OutputFormat::default(),
            widevine: false,
            xhe_aac: false,
            widevine_cdm: None,
            widevine_cdm_provider: None,
            naming_profile: NamingProfile::default(),
            folder_template: None,
            file_template: None,
            download_cover: true,
            download_pdf: true,
            create_cue: false,
            fixup_metadata: true,
            chapter_json: ChapterJsonMode::Off,
            save_chapter_json: None,
            save_metadata_json: false,
            cover_size: String::from("500"),
            chapter_layout: String::from("tree"),
            overwrite_existing: false,
            multi_destination: MultiDestinationMode::default(),
            in_progress: None,
            bad_book_action: BadBookAction::Ask,
            split_mp3_max_mb: 200,
            chapter_file_template: None,
            chapter_title_template: None,
            minimum_file_duration_minutes: 0,
            combine_nested_chapter_titles: false,
            merge_opening_and_end_credits: false,
            strip_unabridged: false,
            strip_audible_brand_audio: false,
            download_clips_bookmarks: false,
            retain_aax_file: false,
            download_speed_limit_kbps: 0,
            lame: LameConfig::default(),
            max_sample_rate: None,
            creation_time: FileTimestampMode::Now,
            last_write_time: FileTimestampMode::Now,
            path_sanitization: PathSanitizationMode::Auto,
            replacement_characters: Vec::new(),
            max_filename_length: DEFAULT_MAX_FILENAME_LENGTH as u32,
        }
    }
}

impl OutputConfig {
    /// Packaging format used for acquire (currently the raw [`Self::format`] field).
    #[must_use]
    pub fn effective_format(&self) -> OutputFormat {
        self.format
    }

    /// Resolved chapter JSON sidecar mode (default off).
    #[must_use]
    pub fn effective_chapter_json(&self) -> ChapterJsonMode {
        if let Some(true) = self.save_chapter_json {
            if self.chapter_json == ChapterJsonMode::Off {
                return match self.chapter_layout.to_ascii_lowercase().as_str() {
                    "flat" => ChapterJsonMode::Flat,
                    "both" => ChapterJsonMode::Both,
                    _ => ChapterJsonMode::Tree,
                };
            }
        }
        if let Some(false) = self.save_chapter_json {
            if self.chapter_json == ChapterJsonMode::Off {
                return ChapterJsonMode::Off;
            }
        }
        self.chapter_json
    }

    /// Resolve folder / file / chapter-file templates.
    #[must_use]
    pub fn resolve_naming_templates(&self) -> ResolvedNamingTemplates {
        ResolvedNamingTemplates::resolve(
            self.naming_profile,
            self.folder_template.as_deref(),
            self.file_template.as_deref(),
            self.chapter_file_template.as_deref(),
        )
    }
}

/// Behavior when some output destinations already have a title and others do not.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum MultiDestinationMode {
    /// Copy the existing object from a present destination into missing ones
    /// (no store re-download). Default.
    #[default]
    SyncMissing,
    /// Re-run acquire fetch/encode, but write only to destinations that lack
    /// the title.
    RefetchMissing,
    /// Re-run acquire and write to every enabled destination.
    RefetchAll,
}

/// How to handle acquire failures (`BadBook` setting).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum BadBookAction {
    /// Prompt the operator (interactive CLI / GUI) for how to proceed (default).
    #[default]
    Ask,
    /// Stop the acquire batch on the first hard failure.
    Abort,
    /// Retry the failed title before giving up.
    Retry,
    /// Skip the failed title and continue the batch.
    Ignore,
}

/// Which output destination plugin is active.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum OutputBackendKind {
    /// Filesystem destination (the `local` plugin; default primary).
    #[default]
    Local,
    /// Object-store destination (the `s3` plugin).
    S3,
}

/// Normalize a storage key prefix: empty stays empty; otherwise ensure a trailing `/`.
#[must_use]
pub fn normalize_storage_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim().trim_start_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.ends_with('/') {
        trimmed.to_string()
    } else {
        format!("{trimmed}/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_naming_resolves_over_global_templates() {
        let global = OutputConfig {
            naming_profile: NamingProfile::Classic,
            folder_template: Some("<author>/<title>".into()),
            file_template: Some("<title> global".into()),
            chapter_file_template: Some("global <ch#>".into()),
            ..Default::default()
        };
        let destination = DestinationNaming {
            naming_profile: Some(NamingProfile::Audiobookshelf),
            folder_template: None,
            file_template: Some("<asin>".into()),
            chapter_file_template: None,
        };

        let resolved = destination.resolve_over(&global);

        assert_eq!(resolved.folder, "<author>/<title>");
        assert_eq!(resolved.file, "<asin>");
        assert_eq!(resolved.chapter_file, "global <ch#>");
    }
}
