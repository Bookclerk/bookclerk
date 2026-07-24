//! Shared download / liberate options (source-agnostic).

use std::path::PathBuf;

use libation_config::{
    resolve_replacement_characters, AudioQuality, ChapterJsonMode, Config, DownloadConfig,
    DownloadFormat, FileTimestampMode, IngestConfig, IngestQuality, LameConfig, OutputFormat,
    ReplacementRule, StorageBackendKind,
};
use serde::{Deserialize, Serialize};

/// Options for liberate / fetch across content sources.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DownloadOptions {
    pub quality: AudioQuality,
    pub format: DownloadFormat,
    /// Explicit output format when set; else derived from [`Self::format`] /
    /// [`Self::split_files_by_chapter`].
    pub output: Option<OutputFormat>,
    pub ingest: IngestConfig,
    pub widevine: bool,
    pub xhe_aac: bool,
    pub widevine_cdm: Option<PathBuf>,
    /// Remote L3 CDM provider URL (`None` = classic Libation AudibleCdm; empty/`off` = disable).
    pub widevine_cdm_provider: Option<String>,
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
    /// Save podcast episodes under the parent show folder.
    pub save_podcasts_to_parent_folder: bool,
}

impl DownloadOptions {
    /// Resolved post-processing format.
    #[must_use]
    pub fn effective_output(&self) -> OutputFormat {
        if let Some(output) = self.output {
            return output;
        }
        match (self.format, self.split_files_by_chapter) {
            (DownloadFormat::Mp3, true) => OutputFormat::SplitMp3ByChapter,
            (DownloadFormat::Mp3, false) => OutputFormat::SingleMp3,
            (DownloadFormat::M4b, _) => OutputFormat::EnrichedM4b,
        }
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

    /// Ingest quality for a content source id.
    #[must_use]
    pub fn ingest_quality(&self, source: &str) -> IngestQuality {
        self.ingest.quality_for(source)
    }
}

impl From<&DownloadConfig> for DownloadOptions {
    fn from(cfg: &DownloadConfig) -> Self {
        Self {
            quality: cfg.ingest.quality_for("audible").as_audible(),
            format: cfg.format,
            output: cfg.output.or(Some(cfg.effective_output())),
            ingest: cfg.ingest.clone(),
            widevine: cfg.widevine,
            xhe_aac: cfg.xhe_aac,
            widevine_cdm: cfg.widevine_cdm.clone(),
            widevine_cdm_provider: cfg.widevine_cdm_provider.clone(),
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
            split_files_by_chapter: cfg.split_files_by_chapter
                || matches!(cfg.effective_output(), OutputFormat::SplitMp3ByChapter),
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
            save_podcasts_to_parent_folder: false,
        }
    }
}

impl From<&Config> for DownloadOptions {
    fn from(cfg: &Config) -> Self {
        let mut opts = Self::from(&cfg.download);
        opts.save_podcasts_to_parent_folder = cfg.library.save_podcasts_to_parent_folder;
        opts.replacement_characters = resolve_replacement_characters(
            &cfg.download.replacement_characters,
            cfg.download.path_sanitization,
            cfg.storage.backend == StorageBackendKind::S3,
        );
        // Plugin-table ingest overrides win over [download.ingest.sources].
        if let Some(q) = cfg.sources.ingest_override("audible") {
            opts.ingest.sources.audible = Some(q);
        }
        if let Some(q) = cfg.sources.ingest_override("libro") {
            opts.ingest.sources.libro = Some(q);
        }
        if let Some(q) = cfg.sources.ingest_override("chirp") {
            opts.ingest.sources.chirp = Some(q);
        }
        if let Some(q) = cfg.sources.ingest_override("graphicaudio") {
            opts.ingest.sources.graphicaudio = Some(q);
        }
        opts
    }
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self::from(&DownloadConfig::default())
    }
}
