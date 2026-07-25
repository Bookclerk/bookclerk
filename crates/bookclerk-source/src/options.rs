//! Shared download / acquire options (source-agnostic packaging + naming).

use std::path::PathBuf;

use bookclerk_config::{
    resolve_replacement_characters, AudioQuality, ChapterJsonMode, Config, FileTimestampMode,
    LameConfig, NamingProfile, OutputBackendKind, OutputConfig, OutputFormat, PathLimits,
    PathSanitizationMode, ReplacementRule, ResolvedNamingTemplates,
};
use serde::{Deserialize, Serialize};

/// Options for acquire / fetch packaging and naming.
///
/// Store-specific ingest knobs live on each [`crate::ContentSource`] instance
/// (parsed from `[sources.<id>]` at registration). `quality` remains here so
/// Audible’s download helpers can overlay the plugin bitrate for a single fetch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DownloadOptions {
    /// Optional per-fetch audio quality overlay (set by plugins that need it).
    pub quality: AudioQuality,
    pub format: OutputFormat,
    pub widevine: bool,
    pub xhe_aac: bool,
    pub widevine_cdm: Option<PathBuf>,
    /// Remote L3 CDM provider URL (`None` = classic Libation AudibleCdm; empty/`off` = disable).
    pub widevine_cdm_provider: Option<String>,
    /// Path-template profile; per-field template overrides win when set.
    pub naming_profile: NamingProfile,
    pub folder_template: Option<String>,
    pub file_template: Option<String>,
    pub download_cover: bool,
    pub download_pdf: bool,
    pub create_cue: bool,
    pub fixup_metadata: bool,
    pub chapter_json: ChapterJsonMode,
    pub save_metadata_json: bool,
    pub cover_size: String,
    pub chapter_layout: String,
    pub overwrite_existing: bool,
    pub split_files_by_chapter: bool,
    pub split_mp3_max_mb: u32,
    pub chapter_file_template: Option<String>,
    pub chapter_title_template: Option<String>,
    pub minimum_file_duration_minutes: u32,
    pub combine_nested_chapter_titles: bool,
    pub merge_opening_and_end_credits: bool,
    pub strip_unabridged: bool,
    pub strip_audible_brand_audio: bool,
    pub download_clips_bookmarks: bool,
    pub retain_aax_file: bool,
    pub download_speed_limit_kbps: u32,
    pub lame: LameConfig,
    pub max_sample_rate: Option<u32>,
    pub creation_time: FileTimestampMode,
    pub last_write_time: FileTimestampMode,
    pub replacement_characters: Vec<ReplacementRule>,
    /// Filesystem / object-store path length limits for storage keys.
    pub path_limits: PathLimits,
    /// Save podcast episodes under the parent show folder.
    pub save_podcasts_to_parent_folder: bool,
}

fn path_sanitization_is_windows(mode: PathSanitizationMode, storage_is_s3: bool) -> bool {
    match mode {
        PathSanitizationMode::Windows => true,
        PathSanitizationMode::Auto => !storage_is_s3 && cfg!(windows),
        _ => false,
    }
}

impl From<&OutputConfig> for DownloadOptions {
    fn from(cfg: &OutputConfig) -> Self {
        Self {
            quality: AudioQuality::High,
            format: cfg.effective_format(),
            widevine: cfg.widevine,
            xhe_aac: cfg.xhe_aac,
            widevine_cdm: cfg.widevine_cdm.clone(),
            widevine_cdm_provider: cfg.widevine_cdm_provider.clone(),
            naming_profile: cfg.naming_profile,
            folder_template: cfg.folder_template.clone(),
            file_template: cfg.file_template.clone(),
            download_cover: cfg.download_cover,
            download_pdf: cfg.download_pdf,
            create_cue: cfg.create_cue,
            fixup_metadata: cfg.fixup_metadata,
            chapter_json: cfg.effective_chapter_json(),
            save_metadata_json: cfg.save_metadata_json,
            cover_size: cfg.cover_size.clone(),
            chapter_layout: cfg.chapter_layout.clone(),
            overwrite_existing: cfg.overwrite_existing,
            split_files_by_chapter: cfg.effective_format().wants_split_by_chapter(),
            split_mp3_max_mb: cfg.split_mp3_max_mb,
            chapter_file_template: cfg.chapter_file_template.clone(),
            chapter_title_template: cfg.chapter_title_template.clone(),
            minimum_file_duration_minutes: cfg.minimum_file_duration_minutes,
            combine_nested_chapter_titles: cfg.combine_nested_chapter_titles,
            merge_opening_and_end_credits: cfg.merge_opening_and_end_credits,
            strip_unabridged: cfg.strip_unabridged,
            strip_audible_brand_audio: cfg.strip_audible_brand_audio,
            download_clips_bookmarks: cfg.download_clips_bookmarks,
            retain_aax_file: cfg.retain_aax_file,
            download_speed_limit_kbps: cfg.download_speed_limit_kbps,
            lame: cfg.lame.clone(),
            max_sample_rate: cfg.max_sample_rate,
            creation_time: cfg.creation_time,
            last_write_time: cfg.last_write_time,
            replacement_characters: resolve_replacement_characters(
                &cfg.replacement_characters,
                cfg.path_sanitization,
                false,
            ),
            path_limits: PathLimits::resolve(
                cfg.max_filename_length,
                false,
                "",
                path_sanitization_is_windows(cfg.path_sanitization, false),
            ),
            save_podcasts_to_parent_folder: false,
        }
    }
}

impl From<&Config> for DownloadOptions {
    fn from(cfg: &Config) -> Self {
        let primary = cfg
            .output
            .primary_backend()
            .unwrap_or(OutputBackendKind::Local);
        Self::for_output_backend(cfg, primary)
    }
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self::from(&OutputConfig::default())
    }
}

impl DownloadOptions {
    /// Build acquire options stamped for one output destination.
    ///
    /// Packaging knobs come from global `[output]`; naming templates prefer
    /// that destination's overrides (`[output.local]` / `[output.s3]`).
    #[must_use]
    pub fn for_output_backend(cfg: &Config, kind: OutputBackendKind) -> Self {
        let storage_is_s3 = matches!(kind, OutputBackendKind::S3);
        let naming = cfg.output.naming_for(kind);
        let mut opts = Self::from(&cfg.output);
        opts.save_podcasts_to_parent_folder = cfg.library.save_podcasts_to_parent_folder;
        opts.naming_profile = naming.effective_profile(&cfg.output);
        opts.folder_template = naming.effective_folder_template(&cfg.output);
        opts.file_template = naming.effective_file_template(&cfg.output);
        opts.chapter_file_template = naming.effective_chapter_file_template(&cfg.output);
        let prefix = match kind {
            OutputBackendKind::Local => {
                bookclerk_config::normalize_storage_prefix(cfg.output.local.prefix.trim())
            }
            OutputBackendKind::S3 => {
                bookclerk_config::normalize_storage_prefix(cfg.output.s3.prefix.trim())
            }
        };
        opts.replacement_characters = resolve_replacement_characters(
            &cfg.output.replacement_characters,
            cfg.output.path_sanitization,
            storage_is_s3,
        );
        opts.path_limits = PathLimits::resolve(
            cfg.output.max_filename_length,
            storage_is_s3,
            &prefix,
            path_sanitization_is_windows(cfg.output.path_sanitization, storage_is_s3),
        );
        opts
    }

    /// Resolved post-processing format.
    #[must_use]
    pub fn effective_output(&self) -> OutputFormat {
        self.format
    }

    #[must_use]
    pub fn wants_mp3(&self) -> bool {
        self.effective_output().wants_mp3()
    }

    #[must_use]
    pub fn wants_split_by_chapter(&self) -> bool {
        self.effective_output().wants_split_by_chapter() || self.split_files_by_chapter
    }

    #[must_use]
    pub fn is_noop_output(&self) -> bool {
        self.effective_output().is_noop()
    }

    #[must_use]
    pub fn wants_opus(&self) -> bool {
        self.effective_output().wants_opus()
    }

    #[must_use]
    pub fn wants_split_by_size(&self) -> bool {
        self.effective_output().wants_split_by_size()
    }

    #[must_use]
    pub fn chapter_json_flat(&self) -> bool {
        self.chapter_json.wants_flat()
    }

    #[must_use]
    pub fn chapter_json_tree(&self) -> bool {
        self.chapter_json.wants_tree()
    }

    #[must_use]
    pub fn wants_chapter_json(&self) -> bool {
        self.chapter_json.wants_any()
    }

    /// Resolve folder / file / chapter-file templates from the naming profile
    /// with per-field overrides.
    #[must_use]
    pub fn naming_templates(&self) -> ResolvedNamingTemplates {
        ResolvedNamingTemplates::resolve(
            self.naming_profile,
            self.folder_template.as_deref(),
            self.file_template.as_deref(),
            self.chapter_file_template.as_deref(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_config::Config;

    #[test]
    fn for_output_backend_applies_destination_naming_overrides() {
        let mut cfg = Config::default();
        cfg.output.naming_profile = NamingProfile::Classic;
        cfg.output.folder_template = Some("<global folder>".into());
        cfg.output.file_template = Some("<global file>".into());
        cfg.output.chapter_file_template = Some("<global chapter>".into());
        cfg.output.local.naming.file_template = Some("<local file>".into());
        cfg.output.s3.enabled = true;
        cfg.output.s3.bucket = "books".into();
        cfg.output.s3.naming.naming_profile = Some(NamingProfile::Audiobookshelf);
        cfg.output.s3.naming.folder_template = Some("<s3 folder>".into());
        cfg.output.s3.naming.chapter_file_template = Some("<s3 chapter>".into());

        let local = DownloadOptions::for_output_backend(&cfg, OutputBackendKind::Local);
        let s3 = DownloadOptions::for_output_backend(&cfg, OutputBackendKind::S3);

        assert_eq!(local.naming_profile, NamingProfile::Classic);
        assert_eq!(local.folder_template.as_deref(), Some("<global folder>"));
        assert_eq!(local.file_template.as_deref(), Some("<local file>"));
        assert_eq!(
            local.chapter_file_template.as_deref(),
            Some("<global chapter>")
        );
        assert_eq!(s3.naming_profile, NamingProfile::Audiobookshelf);
        assert_eq!(s3.folder_template.as_deref(), Some("<s3 folder>"));
        assert_eq!(s3.file_template.as_deref(), Some("<global file>"));
        assert_eq!(s3.chapter_file_template.as_deref(), Some("<s3 chapter>"));
    }
}
