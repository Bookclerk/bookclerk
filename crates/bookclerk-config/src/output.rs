//! Acquired-audio output packaging and destination plugins.
//!
//! ```toml
//! [output]
//! format = "enriched_m4b"
//! naming_profile = "audiobookshelf"
//!
//! [output.local]
//! enabled = true
//! root = "/data/Audiobooks"
//!
//! [output.s3]
//! enabled = false
//! bucket = "my-library"
//! region = "us-east-1"
//! ```
//!
//! Each destination plugin has its own `enabled` flag; multiple may be on at
//! once (acquired files are written to every enabled destination).
//!
//! S3 credentials come from `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`
//! (optional `AWS_SESSION_TOKEN`) when both are set; otherwise the AWS SDK
//! default provider chain is used (shared `~/.aws/credentials` / config, SSO,
//! instance/task roles — same as AWS CLI).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Result};
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

/// Serde default for [`OutputLocalConfig::enabled`] when the TOML key is omitted.
fn default_true() -> bool {
    true
}

/// Acquire packaging + destination backends (`[output]`).
///
/// Store-specific ingest knobs live under [`crate::SourcesConfig`] / `[sources.*]`.
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
    /// Local filesystem destination plugin (`[output.local]`).
    pub local: OutputLocalConfig,
    /// S3 / MinIO destination plugin (`[output.s3]`).
    pub s3: OutputS3Config,
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
            local: OutputLocalConfig::default(),
            s3: OutputS3Config::default(),
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

    /// Enabled destination plugins (may be more than one).
    #[must_use]
    pub fn enabled_backends(&self) -> Vec<OutputBackendKind> {
        let mut out = Vec::new();
        if self.local.enabled {
            out.push(OutputBackendKind::Local);
        }
        if self.s3.enabled {
            out.push(OutputBackendKind::S3);
        }
        out
    }

    /// True when at least one destination plugin is enabled.
    #[must_use]
    pub fn has_enabled_destination(&self) -> bool {
        !self.enabled_backends().is_empty()
    }

    /// Prefix used for path-length budgeting when generating storage keys.
    ///
    /// When S3 is among the enabled destinations, its prefix is used (stricter
    /// full-key budget). Otherwise the local prefix is used.
    #[must_use]
    pub fn path_limit_prefix(&self) -> String {
        if self.s3.enabled {
            normalize_storage_prefix(self.s3.prefix.trim())
        } else {
            normalize_storage_prefix(self.local.prefix.trim())
        }
    }

    /// Whether any enabled destination is S3 (for path sanitization / key budget).
    #[must_use]
    pub fn is_s3(&self) -> bool {
        self.s3.enabled
    }

    /// Human-readable list of enabled destination ids (`local`, `s3`, …).
    #[must_use]
    pub fn enabled_backend_names(&self) -> Vec<&'static str> {
        self.enabled_backends()
            .into_iter()
            .map(|kind| match kind {
                OutputBackendKind::Local => "local",
                OutputBackendKind::S3 => "s3",
            })
            .collect()
    }

    /// Validate that at least one destination is enabled and S3 has a bucket when used.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ConfigError::Invalid`] when no destination is enabled or
    /// S3 is enabled without a bucket.
    pub fn validate_destinations(&self) -> Result<()> {
        if !self.has_enabled_destination() {
            return Err(ConfigError::Invalid(
                "enable at least one of [output.local] or [output.s3]".into(),
            ));
        }
        if self.s3.enabled && self.s3.bucket.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "output.s3.enabled=true requires output.s3.bucket".into(),
            ));
        }
        Ok(())
    }

    /// Per-destination naming overrides for `kind`.
    #[must_use]
    pub fn naming_for(&self, kind: OutputBackendKind) -> &DestinationNaming {
        match kind {
            OutputBackendKind::Local => &self.local.naming,
            OutputBackendKind::S3 => &self.s3.naming,
        }
    }

    /// Resolved templates for `kind` (destination overrides → global → profile).
    #[must_use]
    pub fn resolve_naming_for(&self, kind: OutputBackendKind) -> ResolvedNamingTemplates {
        self.naming_for(kind).resolve_over(self)
    }

    /// Preferred destination for the library `storage_key` mirror (local if enabled).
    #[must_use]
    pub fn primary_backend(&self) -> Option<OutputBackendKind> {
        if self.local.enabled {
            Some(OutputBackendKind::Local)
        } else if self.s3.enabled {
            Some(OutputBackendKind::S3)
        } else {
            None
        }
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
    /// Filesystem destination under `[output.local]` (default primary).
    #[default]
    Local,
    /// Object-store destination under `[output.s3]`.
    S3,
}

/// Local filesystem destination (`[output.local]`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct OutputLocalConfig {
    /// When true, write acquired media under [`Self::root`] (default true).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Root directory for acquired audiobooks.
    pub root: PathBuf,
    /// Optional key prefix under [`Self::root`] (trailing slash optional).
    pub prefix: String,
    /// Optional naming overrides for this destination only.
    #[serde(flatten)]
    pub naming: DestinationNaming,
}

impl Default for OutputLocalConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            root: PathBuf::from("Audiobooks"),
            prefix: String::new(),
            naming: DestinationNaming::default(),
        }
    }
}

/// S3 / MinIO destination (`[output.s3]`).
///
/// Credentials resolve in order:
/// 1. `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` env (optional
///    `AWS_SESSION_TOKEN`) — process env override (not written to the DB unless
///    you run `bookclerk config s3-credentials set`)
/// 2. `encrypted_secrets` row (`kind=s3`, `account_type=operator`,
///    `account_id=default`, `name=default`) — fail closed if the row is
///    encrypted and `BOOKCLERK_AUTH_PASSWORD` cannot unlock it
/// 3. AWS SDK default provider chain (`~/.aws/credentials`, SSO, EC2/ECS/EKS
///    roles, …)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct OutputS3Config {
    /// When true, write acquired media to the configured S3/MinIO bucket (default false).
    pub enabled: bool,
    /// Destination bucket name (required when [`Self::enabled`] is true).
    pub bucket: String,
    /// Key prefix under the bucket (trailing slash optional; default `library/`).
    pub prefix: String,
    /// AWS region id (default `us-east-1`); also used by MinIO path-style clients.
    pub region: String,
    /// Optional custom endpoint (MinIO, LocalStack, etc.).
    pub endpoint: Option<String>,
    /// Force path-style addressing (typical for MinIO).
    pub force_path_style: bool,
    /// Optional naming overrides for this destination only.
    #[serde(flatten)]
    pub naming: DestinationNaming,
}

impl Default for OutputS3Config {
    fn default() -> Self {
        Self {
            enabled: false,
            bucket: String::new(),
            prefix: String::from("library/"),
            region: String::from("us-east-1"),
            endpoint: None,
            force_path_style: false,
            naming: DestinationNaming::default(),
        }
    }
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

    #[test]
    fn destination_naming_flattens_onto_destination_tables() {
        let cfg: OutputConfig = toml::from_str(
            r#"
            [local]
            enabled = true
            folder_template = "<local folder>"

            [s3]
            enabled = true
            bucket = "books"
            file_template = "<asin>"
            chapter_file_template = "<ch#>"
            "#,
        )
        .unwrap();

        assert_eq!(
            cfg.naming_for(OutputBackendKind::Local)
                .folder_template
                .as_deref(),
            Some("<local folder>")
        );
        assert_eq!(
            cfg.naming_for(OutputBackendKind::S3)
                .file_template
                .as_deref(),
            Some("<asin>")
        );
        assert_eq!(
            cfg.naming_for(OutputBackendKind::S3)
                .chapter_file_template
                .as_deref(),
            Some("<ch#>")
        );
    }
}
