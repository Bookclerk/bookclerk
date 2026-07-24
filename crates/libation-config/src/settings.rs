//! Application settings loaded from TOML with environment overrides.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Result};
use crate::extras::{FileTimestampMode, LameConfig, PathSanitizationMode, ReplacementRule};
use crate::naming_profile::{NamingProfile, ResolvedNamingTemplates};
use crate::path_limits::DEFAULT_MAX_FILENAME_LENGTH;
use crate::paths::{resolve_config_path, resolve_files_dir, Paths};
use crate::pipeline_opts::{ChapterJsonMode, GraphicAudioAccess, IngestConfig, OutputFormat};
use crate::plugins::{IntegrationsConfig, SourcesConfig};

/// Top-level Libation configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Config {
    /// Filesystem layout (resolved at load time; not always present in TOML).
    #[serde(skip)]
    pub paths: Option<Paths>,

    pub library: LibraryConfig,
    pub download: DownloadConfig,
    pub storage: StorageConfig,
    pub daemon: DaemonConfig,
    pub auth: AuthConfig,
    /// Content-source plugins (`[sources.audible]`, `[sources.graphicaudio]`, …).
    pub sources: SourcesConfig,
    /// Optional third-party integrations (`[integrations.*]`). Not diagnostics.
    pub integrations: IntegrationsConfig,
    /// Opt-in crash / error-burst report upload (`[diagnostics]`).
    #[serde(default)]
    pub diagnostics: DiagnosticsConfig,
}

/// Auth-file encryption settings (OAuth tokens under `Accounts/`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AuthConfig {
    /// Path to a file containing the auth-file passphrase (Docker/systemd secret).
    /// Prefer this or `LIBATION_AUTH_PASSWORD_FILE` over putting the secret in TOML.
    /// If the path is set but the file is missing, Libation creates it with a
    /// strong random secret — point it at a secrets volume, not `Accounts/`.
    pub password_file: Option<PathBuf>,
    /// Allow writing unencrypted `.auth` files when no passphrase is configured.
    /// Default `false` — OAuth tokens should be encrypted at rest.
    pub allow_plaintext: bool,
}

/// Library / scan related settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct LibraryConfig {
    /// Automatically liberate newly scanned titles (daemon).
    pub auto_liberate: bool,
    /// Scan interval in minutes for `libationd` (0 = disabled).
    pub scan_interval_minutes: u64,
    /// Import podcast episodes during scan (`ImportEpisodes`).
    pub import_episodes: bool,
    /// Import Audible Plus titles during scan (`ImportPlusTitles`).
    pub import_plus_titles: bool,
    /// Liberate podcast episodes (`DownloadEpisodes`; distinct from `ImportEpisodes`).
    pub download_episodes: bool,
    /// Save podcast episodes into the parent show's folder (`SavePodcastsToParentFolder`).
    pub save_podcasts_to_parent_folder: bool,
    /// After scan, enrich non-Audible rows (e.g. Libro.fm) from public Audible/Audnexus metadata.
    pub enrich_from_audible: bool,
    /// Minimum match confidence (0–100) to accept an Audible ASIN enrichment.
    /// Uses AudioBookshelf-style duration/title/author scoring (default 90).
    pub enrich_min_confidence: u8,
    /// When matching storage to the library, relocate matched audio (and
    /// accompanying sidecars) onto the configured naming-profile layout.
    /// Default false — match in place without moving files.
    pub fix_storage_layout: bool,
}

impl Default for LibraryConfig {
    fn default() -> Self {
        Self {
            auto_liberate: false,
            scan_interval_minutes: 60,
            import_episodes: true,
            import_plus_titles: false,
            download_episodes: true,
            save_podcasts_to_parent_folder: false,
            enrich_from_audible: true,
            enrich_min_confidence: 90,
            fix_storage_layout: false,
        }
    }
}

/// Download / audio format preferences (Libation parity).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DownloadConfig {
    /// Legacy Audible license quality (`high`/`normal`). Prefer [`Self::ingest`].
    pub quality: AudioQuality,
    /// Legacy container preference (`m4b`/`mp3`). Prefer [`Self::output`].
    pub format: DownloadFormat,
    /// Post-download output formatting. When unset, derived from [`Self::format`] +
    /// [`Self::split_files_by_chapter`]. Default effective value: enriched M4B.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputFormat>,
    /// Ingest quality (global + per-source). Default: highest available.
    pub ingest: IngestConfig,
    /// Prefer Widevine/CENC (also enables Adrm→Widevine fallback; auto-provisions L3 CDM).
    pub widevine: bool,
    /// Prefer xHE-AAC on the Widevine path when offered.
    pub xhe_aac: bool,
    /// Optional local Widevine `.wvd` path (absolute or relative to `LIBATION_FILES_DIR`).
    /// When unset, liberate auto-provisions an L3 CDM via [`widevine_cdm_provider`].
    pub widevine_cdm: Option<PathBuf>,
    /// Remote L3 CDM provider. `None` uses classic Libation AudibleCdm; empty/`off` disables auto-fetch.
    pub widevine_cdm_provider: Option<String>,
    /// Named path-template preset (`audiobookshelf` default, or `classic`).
    /// Per-field `folder_template` / `file_template` / `chapter_file_template`
    /// overrides win when set.
    pub naming_profile: NamingProfile,
    /// Classic Libation `FolderTemplate` override (e.g. `<author>/<title>`).
    /// When unset, uses [`Self::naming_profile`].
    pub folder_template: Option<String>,
    /// Classic Libation `FileTemplate` without extension (e.g. `<asin>` or `<title>`).
    /// When unset, uses [`Self::naming_profile`].
    pub file_template: Option<String>,
    /// Save cover JPEG alongside audio (`DownloadCoverArt`).
    /// Default on; cover is also embedded when [`Self::fixup_metadata`] is true.
    pub download_cover: bool,
    /// Download companion PDF when available (classic separate PDF liberator).
    /// Default on.
    pub download_pdf: bool,
    /// Write a `.cue` sidecar from API chapters (`CreateCueSheet`; classic default off).
    pub create_cue: bool,
    /// Embed tags, cover, and chapters natively (`AllowLibationFixup`; classic default on).
    pub fixup_metadata: bool,
    /// Chapter JSON sidecars: `off` | `flat` | `tree` | `both` (default off).
    pub chapter_json: ChapterJsonMode,
    /// Deprecated: use [`Self::chapter_json`]. Kept for migration/overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save_chapter_json: Option<bool>,
    /// Persist raw catalog API JSON (`metadata.json`; classic `SaveMetadataToFile`).
    pub save_metadata_json: bool,
    /// Cover image size for download/embed (`500`, `1215`, or `native`).
    pub cover_size: String,
    /// Preferred Audible chapter API layout when fetching (`tree` or `flat`).
    pub chapter_layout: String,
    /// Re-download when liberated media already exists (`OverwriteExisting`).
    pub overwrite_existing: bool,
    /// Scratch directory for in-progress downloads (`InProgress`); relative to files_dir.
    pub in_progress: Option<PathBuf>,
    /// Action when a title fails to liberate (`BadBook`).
    pub bad_book_action: BadBookAction,
    /// Legacy split flag; prefer `output = "split_mp3_by_chapter"`.
    pub split_files_by_chapter: bool,
    /// Max MP3 part size in MiB when output is `split_mp3_by_size`.
    pub split_mp3_max_mb: u32,
    /// Template for split chapter filenames (`ChapterFileTemplate`).
    /// When unset, uses [`Self::naming_profile`].
    pub chapter_file_template: Option<String>,
    /// Template for chapter titles in metadata (`ChapterTitleTemplate`).
    pub chapter_title_template: Option<String>,
    /// Minimum chapter file duration in minutes (`MinimumFileDuration`).
    pub minimum_file_duration_minutes: u32,
    pub combine_nested_chapter_titles: bool,
    pub merge_opening_and_end_credits: bool,
    pub strip_unabridged: bool,
    pub strip_audible_brand_audio: bool,
    /// Download clips/bookmarks sidecar (`DownloadClipsBookmarks`).
    pub download_clips_bookmarks: bool,
    /// Keep encrypted download in storage (`RetainAaxFile`).
    pub retain_aax_file: bool,
    /// Download speed cap in KB/s (`DownloadSpeedLimit`; 0 = unlimited).
    pub download_speed_limit_kbps: u32,
    pub lame: LameConfig,
    /// Resample/downsample when sample rate exceeds this Hz (`MaxSampleRate`).
    pub max_sample_rate: Option<u32>,
    pub creation_time: FileTimestampMode,
    pub last_write_time: FileTimestampMode,
    /// Path sanitization profile when [`Self::replacement_characters`] is empty.
    pub path_sanitization: PathSanitizationMode,
    /// Explicit classic `ReplacementCharacters` map. When non-empty, overrides
    /// [`Self::path_sanitization`].
    pub replacement_characters: Vec<ReplacementRule>,
    /// Max length per path segment (classic `LongPath.MaxFilenameLength`).
    /// Default 255; set to `0` to disable truncation.
    pub max_filename_length: u32,
}

impl DownloadConfig {
    /// Resolved output format (explicit `output`, else legacy `format`/`split_*`).
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
}

/// How to handle liberate failures (`BadBook` setting).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum BadBookAction {
    #[default]
    Ask,
    Abort,
    Retry,
    Ignore,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            quality: AudioQuality::High,
            format: DownloadFormat::M4b,
            output: None,
            ingest: IngestConfig::default(),
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
            in_progress: None,
            bad_book_action: BadBookAction::Ask,
            split_files_by_chapter: false,
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
            // Empty → resolve via path_sanitization + storage.backend at use time.
            replacement_characters: Vec::new(),
            max_filename_length: DEFAULT_MAX_FILENAME_LENGTH as u32,
        }
    }
}

impl DownloadConfig {
    /// Resolve folder / file / chapter-file templates from
    /// [`Self::naming_profile`] with per-field overrides.
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

/// Audible download quality.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum AudioQuality {
    #[default]
    High,
    Normal,
}

/// Container / codec preference for liberated files.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum DownloadFormat {
    #[default]
    M4b,
    Mp3,
}

/// Pluggable storage backend configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct StorageConfig {
    pub backend: StorageBackendKind,
    /// Key prefix under the local root or S3 bucket (e.g. `library/`).
    ///
    /// Library `storage_key` values stay relative to this prefix (the backend
    /// prepends it). When empty and [`Self::backend`] is S3, falls back to the
    /// legacy [`StorageS3Config::prefix`].
    pub prefix: String,
    pub local: StorageLocalConfig,
    pub s3: StorageS3Config,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            backend: StorageBackendKind::Local,
            prefix: String::new(),
            local: StorageLocalConfig::default(),
            s3: StorageS3Config::default(),
        }
    }
}

impl StorageConfig {
    /// Effective object-key prefix for the active backend (normalized, trailing `/`).
    ///
    /// Prefers [`Self::prefix`]. When that is empty and the backend is S3, uses
    /// [`StorageS3Config::prefix`] so existing S3-only configs keep working.
    #[must_use]
    pub fn effective_prefix(&self) -> String {
        let primary = self.prefix.trim();
        if !primary.is_empty() {
            return normalize_storage_prefix(primary);
        }
        if self.backend == StorageBackendKind::S3 {
            return normalize_storage_prefix(self.s3.prefix.trim());
        }
        String::new()
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

/// Which storage implementation to use.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum StorageBackendKind {
    #[default]
    Local,
    S3,
}

/// Local filesystem storage root.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct StorageLocalConfig {
    /// Root directory for liberated audiobooks.
    pub root: PathBuf,
}

impl Default for StorageLocalConfig {
    fn default() -> Self {
        Self {
            root: PathBuf::from("Audiobooks"),
        }
    }
}

/// S3 / MinIO storage settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct StorageS3Config {
    pub bucket: String,
    /// Legacy S3-only key prefix. Prefer [`StorageConfig::prefix`]; used when
    /// that shared prefix is empty.
    pub prefix: String,
    pub region: String,
    /// Optional custom endpoint (MinIO, LocalStack, etc.).
    pub endpoint: Option<String>,
    /// Force path-style addressing (typical for MinIO).
    pub force_path_style: bool,
}

impl Default for StorageS3Config {
    fn default() -> Self {
        Self {
            bucket: String::new(),
            prefix: String::from("library/"),
            region: String::from("us-east-1"),
            endpoint: None,
            force_path_style: false,
        }
    }
}

/// Daemon / HTTP control plane settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DaemonConfig {
    /// Bind address for the HTTP control plane.
    pub listen: String,
    /// Emit JSON logs on stderr when true (journald sink is always structured).
    pub json_logs: bool,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            listen: String::from("127.0.0.1:8787"),
            json_logs: true,
        }
    }
}

/// Compile-time default from `LIBATION_DIAGNOSTICS_COLLECTOR_URL` when running `cargo build`.
fn compile_time_collector_url() -> Option<&'static str> {
    option_env!("LIBATION_DIAGNOSTICS_COLLECTOR_URL")
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Opt-in sharing of recent **redacted** logs on crash or error bursts.
///
/// Defaults to disabled. Operators flip `share_reports` and set `collector_url`
/// (or bake `LIBATION_DIAGNOSTICS_COLLECTOR_URL` at `cargo build` time).
/// The client POSTs to `/submit`; a GitHub Action calls `/report`.
///
/// Libation never manages log-file rotation — use journald / the container
/// runtime (Windows Event Log / macOS unified logging are future work).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DiagnosticsConfig {
    /// When true, share redacted crash/ERROR-burst reports with the collector.
    #[serde(alias = "upload_enabled")]
    pub share_reports: bool,
    /// Worker origin (HTTPS). When empty, uses `LIBATION_DIAGNOSTICS_COLLECTOR_URL`
    /// from config/runtime env, else the value baked in at `cargo build` time.
    #[serde(alias = "upload_url")]
    pub collector_url: String,
    /// Upload the ring buffer from the panic hook.
    pub upload_on_crash: bool,
    /// Upload when ERROR volume crosses the burst threshold.
    pub upload_on_error_burst: bool,
    /// Upload when WARN volume crosses the warn-burst threshold.
    pub upload_on_warn_burst: bool,
    /// Number of ERROR events inside the window that trigger an upload.
    pub error_burst_threshold: u32,
    /// Sliding window length for error-burst detection, in seconds.
    pub error_burst_window_secs: u64,
    /// Number of WARN events inside the window that trigger an upload.
    pub warn_burst_threshold: u32,
    /// Sliding window length for warn-burst detection, in seconds.
    pub warn_burst_window_secs: u64,
    /// Max redacted events retained for upload (all levels through TRACE).
    pub ring_buffer_capacity: u32,
}

impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self {
            share_reports: false,
            collector_url: String::new(),
            upload_on_crash: true,
            upload_on_error_burst: true,
            upload_on_warn_burst: true,
            error_burst_threshold: 10,
            error_burst_window_secs: 60,
            warn_burst_threshold: 20,
            warn_burst_window_secs: 60,
            ring_buffer_capacity: 200,
        }
    }
}

impl DiagnosticsConfig {
    /// Resolved Worker origin: config/env `collector_url`, else baked deploy URL.
    #[must_use]
    pub fn effective_collector_url(&self) -> String {
        let explicit = self.collector_url.trim();
        if !explicit.is_empty() {
            return explicit.to_string();
        }
        compile_time_collector_url()
            .map(str::to_string)
            .unwrap_or_default()
    }

    /// HTTPS endpoint clients POST redacted JSON to (`…/submit`).
    #[must_use]
    pub fn effective_submit_url(&self) -> String {
        let base = self
            .effective_collector_url()
            .trim_end_matches('/')
            .to_string();
        if base.is_empty() {
            return String::new();
        }
        if base.to_ascii_lowercase().ends_with("/submit") {
            base
        } else {
            format!("{base}/submit")
        }
    }

    /// True when report sharing is enabled and a collector URL can be resolved.
    #[must_use]
    pub fn upload_ready(&self) -> bool {
        self.share_reports && !self.effective_collector_url().is_empty()
    }
}

impl Config {
    /// Load config: defaults ← optional TOML file ← environment overrides.
    pub fn load(cli_files_dir: Option<PathBuf>, config_path: Option<PathBuf>) -> Result<Self> {
        let files_dir = resolve_files_dir(cli_files_dir);
        let paths = Paths::from_files_dir(files_dir);
        let path = config_path.unwrap_or_else(|| resolve_config_path(&paths.files_dir));

        let mut cfg = if path.exists() {
            Self::from_toml_file(&path)?
        } else {
            tracing::debug!(?path, "config file not found; using defaults");
            Self::default()
        };

        cfg.apply_env_overrides();
        cfg.paths = Some(paths);
        cfg.resolve_relative_paths();
        cfg.register_known_secrets();
        // Callers should invoke [`Self::warn_unsupported_options`] *after*
        // `init_tracing_with` so startup guidance is not dropped.
        cfg.validate()?;
        Ok(cfg)
    }

    /// Parse a TOML config file.
    pub fn from_toml_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_toml_str(&text, &path.display().to_string())
    }

    /// Parse TOML from a string.
    pub fn from_toml_str(text: &str, origin: &str) -> Result<Self> {
        toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: origin.to_string(),
            source,
        })
    }

    /// Apply `LIBATION_*` environment overrides.
    pub fn apply_env_overrides(&mut self) {
        if let Ok(v) = std::env::var("LIBATION_STORAGE_BACKEND") {
            match v.to_ascii_lowercase().as_str() {
                "local" => self.storage.backend = StorageBackendKind::Local,
                "s3" => self.storage.backend = StorageBackendKind::S3,
                other => tracing::warn!(%other, "unknown LIBATION_STORAGE_BACKEND; ignoring"),
            }
        }
        if let Ok(v) = std::env::var("LIBATION_STORAGE_LOCAL_ROOT") {
            self.storage.local.root = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("LIBATION_STORAGE_PREFIX") {
            self.storage.prefix = v;
        }
        if let Ok(v) = std::env::var("LIBATION_S3_BUCKET") {
            self.storage.s3.bucket = v;
        }
        if let Ok(v) = std::env::var("LIBATION_S3_PREFIX") {
            self.storage.s3.prefix = v;
        }
        if let Ok(v) = std::env::var("LIBATION_S3_REGION") {
            self.storage.s3.region = v;
        }
        if let Ok(v) = std::env::var("LIBATION_S3_ENDPOINT") {
            self.storage.s3.endpoint = Some(v);
        }
        if let Ok(v) = std::env::var("LIBATION_S3_FORCE_PATH_STYLE") {
            self.storage.s3.force_path_style =
                parse_bool(&v).unwrap_or(self.storage.s3.force_path_style);
        }
        if let Ok(v) = std::env::var("LIBATION_DAEMON_LISTEN") {
            self.daemon.listen = v;
        }
        if let Ok(v) = std::env::var("LIBATION_DAEMON_JSON_LOGS") {
            self.daemon.json_logs = parse_bool(&v).unwrap_or(self.daemon.json_logs);
        }
        if let Ok(v) = std::env::var("LIBATION_DIAGNOSTICS_SHARE_REPORTS")
            .or_else(|_| std::env::var("LIBATION_DIAGNOSTICS_UPLOAD_ENABLED"))
        {
            self.diagnostics.share_reports =
                parse_bool(&v).unwrap_or(self.diagnostics.share_reports);
        }
        if let Ok(v) = std::env::var("LIBATION_DIAGNOSTICS_COLLECTOR_URL")
            .or_else(|_| std::env::var("LIBATION_DIAGNOSTICS_UPLOAD_URL"))
        {
            self.diagnostics.collector_url = v;
        }
        if let Ok(v) = std::env::var("LIBATION_DIAGNOSTICS_UPLOAD_ON_CRASH") {
            self.diagnostics.upload_on_crash =
                parse_bool(&v).unwrap_or(self.diagnostics.upload_on_crash);
        }
        if let Ok(v) = std::env::var("LIBATION_DIAGNOSTICS_UPLOAD_ON_ERROR_BURST") {
            self.diagnostics.upload_on_error_burst =
                parse_bool(&v).unwrap_or(self.diagnostics.upload_on_error_burst);
        }
        if let Ok(v) = std::env::var("LIBATION_DIAGNOSTICS_UPLOAD_ON_WARN_BURST") {
            self.diagnostics.upload_on_warn_burst =
                parse_bool(&v).unwrap_or(self.diagnostics.upload_on_warn_burst);
        }
        if let Ok(v) = std::env::var("LIBATION_DIAGNOSTICS_ERROR_BURST_THRESHOLD") {
            if let Ok(n) = v.parse() {
                self.diagnostics.error_burst_threshold = n;
            }
        }
        if let Ok(v) = std::env::var("LIBATION_DIAGNOSTICS_ERROR_BURST_WINDOW_SECS") {
            if let Ok(n) = v.parse() {
                self.diagnostics.error_burst_window_secs = n;
            }
        }
        if let Ok(v) = std::env::var("LIBATION_DIAGNOSTICS_WARN_BURST_THRESHOLD") {
            if let Ok(n) = v.parse() {
                self.diagnostics.warn_burst_threshold = n;
            }
        }
        if let Ok(v) = std::env::var("LIBATION_DIAGNOSTICS_WARN_BURST_WINDOW_SECS") {
            if let Ok(n) = v.parse() {
                self.diagnostics.warn_burst_window_secs = n;
            }
        }
        if let Ok(v) = std::env::var("LIBATION_DIAGNOSTICS_RING_BUFFER_CAPACITY") {
            if let Ok(n) = v.parse() {
                self.diagnostics.ring_buffer_capacity = n;
            }
        }
        if let Ok(v) = std::env::var("LIBATION_AUTO_LIBERATE") {
            self.library.auto_liberate = parse_bool(&v).unwrap_or(self.library.auto_liberate);
        }
        if let Ok(v) = std::env::var("LIBATION_ENRICH_FROM_AUDIBLE") {
            self.library.enrich_from_audible =
                parse_bool(&v).unwrap_or(self.library.enrich_from_audible);
        }
        if let Ok(v) = std::env::var("LIBATION_ENRICH_MIN_CONFIDENCE") {
            if let Ok(n) = v.parse::<u8>() {
                self.library.enrich_min_confidence = n.min(100);
            }
        }
        if let Ok(v) = std::env::var("LIBATION_FIX_STORAGE_LAYOUT") {
            self.library.fix_storage_layout =
                parse_bool(&v).unwrap_or(self.library.fix_storage_layout);
        }
        if let Ok(v) = std::env::var("LIBATION_SCAN_INTERVAL_MINUTES") {
            if let Ok(n) = v.parse() {
                self.library.scan_interval_minutes = n;
            }
        }
        if let Ok(v) =
            std::env::var("LIBATION_GA_ACCESS").or_else(|_| std::env::var("LIBATION_GA_FETCH"))
        {
            if let Some(access) = GraphicAudioAccess::parse(&v) {
                self.sources.graphicaudio.access = access;
            } else if !v.trim().is_empty() && !v.eq_ignore_ascii_case("auto") {
                tracing::warn!(
                    value = %v,
                    "unknown LIBATION_GA_ACCESS / LIBATION_GA_FETCH; expected web|zip|device"
                );
            }
        }
        if let Ok(v) = std::env::var("LIBATION_SOURCE_AUDIBLE_ENABLED") {
            self.sources.audible.enabled = parse_bool(&v).unwrap_or(self.sources.audible.enabled);
        }
        if let Ok(v) = std::env::var("LIBATION_SOURCE_LIBRO_ENABLED") {
            self.sources.libro.enabled = parse_bool(&v).unwrap_or(self.sources.libro.enabled);
        }
        if let Ok(v) = std::env::var("LIBATION_SOURCE_CHIRP_ENABLED") {
            self.sources.chirp.enabled = parse_bool(&v).unwrap_or(self.sources.chirp.enabled);
        }
        if let Ok(v) = std::env::var("LIBATION_SOURCE_GRAPHICAUDIO_ENABLED") {
            self.sources.graphicaudio.enabled =
                parse_bool(&v).unwrap_or(self.sources.graphicaudio.enabled);
        }
        if let Ok(v) = std::env::var("LIBATION_AUTH_PASSWORD_FILE") {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                self.auth.password_file = Some(PathBuf::from(trimmed));
            }
        }
        if let Ok(v) = std::env::var("LIBATION_AUTH_ALLOW_PLAINTEXT") {
            if let Some(b) = parse_bool(&v) {
                self.auth.allow_plaintext = b;
            }
        }
        if let Ok(v) = std::env::var("LIBATION_WIDEVINE") {
            self.download.widevine = parse_bool(&v).unwrap_or(self.download.widevine);
        }
        if let Ok(v) = std::env::var("LIBATION_XHE_AAC") {
            self.download.xhe_aac = parse_bool(&v).unwrap_or(self.download.xhe_aac);
        }
        if let Ok(v) = std::env::var("LIBATION_WIDEVINE_CDM") {
            self.download.widevine_cdm = Some(PathBuf::from(v));
        }
        if let Ok(v) = std::env::var("LIBATION_WIDEVINE_CDM_PROVIDER") {
            self.download.widevine_cdm_provider = Some(v);
        }
        if let Ok(v) = std::env::var("LIBATION_NAMING_PROFILE") {
            if let Some(profile) = NamingProfile::parse(&v) {
                self.download.naming_profile = profile;
            }
        }
        if let Ok(v) = std::env::var("LIBATION_FOLDER_TEMPLATE") {
            self.download.folder_template = Some(v);
        }
        if let Ok(v) = std::env::var("LIBATION_FILE_TEMPLATE") {
            self.download.file_template = Some(v);
        }
        if let Ok(v) = std::env::var("LIBATION_DOWNLOAD_COVER") {
            self.download.download_cover = parse_bool(&v).unwrap_or(self.download.download_cover);
        }
        if let Ok(v) = std::env::var("LIBATION_DOWNLOAD_PDF") {
            self.download.download_pdf = parse_bool(&v).unwrap_or(self.download.download_pdf);
        }
        if let Ok(v) = std::env::var("LIBATION_CREATE_CUE") {
            self.download.create_cue = parse_bool(&v).unwrap_or(self.download.create_cue);
        }
        if let Ok(v) = std::env::var("LIBATION_FIXUP_METADATA") {
            self.download.fixup_metadata = parse_bool(&v).unwrap_or(self.download.fixup_metadata);
        }
        if let Ok(v) = std::env::var("LIBATION_SAVE_CHAPTER_JSON") {
            if let Some(mode) = ChapterJsonMode::parse(&v) {
                self.download.chapter_json = mode;
            } else if let Some(b) = parse_bool(&v) {
                self.download.save_chapter_json = Some(b);
            }
        }
        if let Ok(v) = std::env::var("LIBATION_CHAPTER_JSON") {
            if let Some(mode) = ChapterJsonMode::parse(&v) {
                self.download.chapter_json = mode;
            }
        }
        if let Ok(v) = std::env::var("LIBATION_OUTPUT") {
            if let Some(output) = OutputFormat::parse(&v) {
                self.download.output = Some(output);
            }
        }
        if let Ok(v) = std::env::var("LIBATION_INGEST_QUALITY") {
            if let Some(q) = crate::pipeline_opts::IngestQuality::parse(&v) {
                self.download.ingest.quality = q;
            }
        }
        if let Ok(v) = std::env::var("LIBATION_COVER_SIZE") {
            if !v.trim().is_empty() {
                self.download.cover_size = v;
            }
        }
        if let Ok(v) = std::env::var("LIBATION_CHAPTER_LAYOUT") {
            if !v.trim().is_empty() {
                self.download.chapter_layout = v;
            }
        }
    }

    /// Soft validation of cross-field constraints.
    pub fn validate(&self) -> Result<()> {
        if self.storage.backend == StorageBackendKind::S3 && self.storage.s3.bucket.is_empty() {
            return Err(ConfigError::Invalid(
                "storage.backend=s3 requires storage.s3.bucket".into(),
            ));
        }
        if self.diagnostics.share_reports && self.diagnostics.effective_collector_url().is_empty() {
            return Err(ConfigError::Invalid(
                "diagnostics.share_reports=true requires diagnostics.collector_url, \
                 LIBATION_DIAGNOSTICS_COLLECTOR_URL at runtime, or the same variable \
                 set when running cargo build"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Register config/env secrets for exact-value log redaction.
    pub fn register_known_secrets(&self) {
        crate::redact::register_secrets_from_env();
        if let Some(path) = &self.auth.password_file {
            if let Ok(contents) = std::fs::read_to_string(path) {
                let trimmed = contents.trim();
                if !trimmed.is_empty() {
                    crate::redact::register_secret(trimmed);
                }
            }
        }
    }

    /// Resolve relative `storage.local.root` under `files_dir`.
    ///
    /// Keeps absolute roots (Docker `/data/Audiobooks`, classic migrate paths)
    /// unchanged. Relative defaults like `Audiobooks` become
    /// `{LIBATION_FILES_DIR}/Audiobooks` so systemd/Docker cwd does not matter.
    pub fn resolve_relative_paths(&mut self) {
        let Some(paths) = &self.paths else {
            return;
        };
        if self.storage.local.root.is_relative() {
            self.storage.local.root = paths.files_dir.join(&self.storage.local.root);
        }
        if let Some(cdm) = &self.download.widevine_cdm {
            if cdm.is_relative() {
                self.download.widevine_cdm = Some(paths.files_dir.join(cdm));
            }
        }
        if let Some(scratch) = &self.download.in_progress {
            if scratch.is_relative() {
                self.download.in_progress = Some(paths.files_dir.join(scratch));
            }
        }
    }

    /// Warn / note about Widevine / auth encryption setup.
    pub fn warn_unsupported_options(&self) {
        if self.download.widevine && self.download.widevine_cdm.is_none() {
            tracing::info!(
                "download.widevine=true — L3 CDM auto-provisions via AudibleCdm on first Widevine \
                 liberate (Android auth from `libation auth login`). \
                 Optional BYO: download.widevine_cdm / {{files_dir}}/widevine.wvd / \
                 {{files_dir}}/Accounts/<account>.wvd (or set download.widevine_cdm_provider=off)"
            );
        }
        let has_password_env = std::env::var_os("LIBATION_AUTH_PASSWORD")
            .is_some_and(|v| !v.is_empty())
            || std::env::var_os("LIBATION_AUTH_PASSWORD_FILE").is_some_and(|v| !v.is_empty())
            || self.auth.password_file.is_some();
        if !has_password_env && !self.auth.allow_plaintext {
            tracing::info!(
                "auth encryption: set LIBATION_AUTH_PASSWORD or LIBATION_AUTH_PASSWORD_FILE \
                 (auto-creates a strong random secret at that path if missing — use a secrets \
                 volume, not Accounts/) or [auth].password_file; or set auth.allow_plaintext=true \
                 for unprotected local token files"
            );
        } else if self.auth.allow_plaintext && !has_password_env {
            tracing::warn!(
                "auth.allow_plaintext=true — OAuth tokens may be stored unprotected under Accounts/"
            );
        }
    }

    /// Write the config as TOML (skips resolved `paths`).
    pub fn write_toml_file(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = self.to_toml_string()?;
        std::fs::write(path, text)?;
        Ok(())
    }

    /// Serialize to a TOML string suitable for `config.toml`.
    pub fn to_toml_string(&self) -> Result<String> {
        // Clone without paths so they are not written.
        let mut out = self.clone();
        out.paths = None;
        toml::to_string_pretty(&out).map_err(|source| ConfigError::Invalid(source.to_string()))
    }

    /// Resolved paths (panic only if `load` was not used — callers should use `load`).
    #[must_use]
    pub fn paths(&self) -> &Paths {
        self.paths
            .as_ref()
            .expect("Config.paths populated by Config::load")
    }

    /// Scratch directory for downloads / decrypt (`InProgress` or default cache).
    #[must_use]
    pub fn download_cache_dir(&self) -> PathBuf {
        self.download
            .in_progress
            .clone()
            .unwrap_or_else(|| self.paths().cache_dir.clone())
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
    fn parse_example_toml() {
        let text = r#"
[library]
auto_liberate = true
scan_interval_minutes = 30

[download]
quality = "high"
format = "m4b"
widevine = true
xhe_aac = false

[storage]
backend = "local"

[storage.local]
root = "/data/audiobooks"

[storage.s3]
bucket = "my-audiobooks"
prefix = "library/"
region = "us-east-1"

[daemon]
listen = "0.0.0.0:8787"
json_logs = true
"#;
        let cfg = Config::from_toml_str(text, "test").unwrap();
        assert!(cfg.library.auto_liberate);
        assert_eq!(cfg.library.scan_interval_minutes, 30);
        assert_eq!(cfg.storage.backend, StorageBackendKind::Local);
        assert_eq!(cfg.storage.local.root, PathBuf::from("/data/audiobooks"));
        assert_eq!(cfg.daemon.listen, "0.0.0.0:8787");
        assert!(!cfg.diagnostics.share_reports);
    }

    #[test]
    fn storage_prefix_effective_for_local_and_s3() {
        let mut cfg = Config::default();
        assert_eq!(cfg.storage.effective_prefix(), "");

        cfg.storage.prefix = "books".into();
        assert_eq!(cfg.storage.effective_prefix(), "books/");

        // Shared prefix wins over legacy s3.prefix.
        cfg.storage.backend = StorageBackendKind::S3;
        cfg.storage.s3.prefix = "library/".into();
        assert_eq!(cfg.storage.effective_prefix(), "books/");

        // Empty shared prefix → S3 falls back to legacy s3.prefix.
        cfg.storage.prefix.clear();
        assert_eq!(cfg.storage.effective_prefix(), "library/");

        // Local ignores legacy s3.prefix when shared prefix is empty.
        cfg.storage.backend = StorageBackendKind::Local;
        assert_eq!(cfg.storage.effective_prefix(), "");
    }

    #[test]
    fn normalize_storage_prefix_trims_and_slashes() {
        assert_eq!(normalize_storage_prefix(""), "");
        assert_eq!(normalize_storage_prefix("  "), "");
        assert_eq!(normalize_storage_prefix("library"), "library/");
        assert_eq!(normalize_storage_prefix("library/"), "library/");
        assert_eq!(normalize_storage_prefix("/library"), "library/");
    }

    #[test]
    fn sources_graphicaudio_access_from_toml() {
        let text = r#"
[sources.graphicaudio]
access = "zip"
"#;
        let cfg = Config::from_toml_str(text, "test").unwrap();
        assert_eq!(
            cfg.sources.graphicaudio.access,
            crate::GraphicAudioAccess::Zip
        );
        assert_eq!(
            Config::default().sources.graphicaudio.access,
            crate::GraphicAudioAccess::Web
        );
        assert!(Config::default().sources.audible.enabled);
    }

    #[test]
    fn diagnostics_top_level_table() {
        let text = r#"
[diagnostics]
share_reports = true
collector_url = "https://reports.example"
"#;
        let cfg = Config::from_toml_str(text, "test").unwrap();
        assert!(cfg.diagnostics.share_reports);
        assert_eq!(cfg.diagnostics.collector_url, "https://reports.example");
        // Integrations stay empty / distinct from diagnostics.
        assert_eq!(cfg.integrations, crate::IntegrationsConfig::default());
    }

    #[test]
    fn integrations_diagnostics_is_rejected() {
        let text = r#"
[integrations.diagnostics]
share_reports = true
"#;
        let err = Config::from_toml_str(text, "test").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("diagnostics") || msg.contains("unknown"),
            "expected unknown-field error, got: {msg}"
        );
    }

    #[test]
    fn sources_plugin_enabled_defaults() {
        let text = r#"
[sources.chirp]
enabled = false

[sources.graphicaudio]
enabled = true
access = "device"
ingest = "low"
"#;
        let cfg = Config::from_toml_str(text, "test").unwrap();
        assert!(!cfg.sources.is_enabled("chirp"));
        assert!(cfg.sources.is_enabled("audible"));
        assert_eq!(
            cfg.sources.graphicaudio.access,
            crate::GraphicAudioAccess::Device
        );
        assert_eq!(
            cfg.sources.ingest_override("graphicaudio"),
            Some(crate::IngestQuality::Low)
        );
    }

    #[test]
    fn sources_partial_table_keeps_enabled_true() {
        // Only setting source-specific knobs must not flip enabled→false
        // (bool's Default is false; plugins use default_true).
        let text = r#"
[sources.graphicaudio]
access = "zip"

[sources.libro]
ingest = "high"
"#;
        let cfg = Config::from_toml_str(text, "test").unwrap();
        assert!(cfg.sources.graphicaudio.enabled);
        assert!(cfg.sources.libro.enabled);
        assert!(cfg.sources.audible.enabled);
        assert!(cfg.sources.chirp.enabled);
    }

    #[test]
    fn diagnostics_share_reports_defaults_off() {
        let cfg = Config::default();
        assert!(!cfg.diagnostics.share_reports);
        assert!(cfg.validate().is_ok());
        assert!(cfg.diagnostics.effective_collector_url().is_empty());
    }

    #[test]
    fn diagnostics_compile_time_collector_url_when_config_empty() {
        let baked = compile_time_collector_url();
        let mut cfg = Config::default();
        cfg.diagnostics.share_reports = true;
        match baked {
            Some(url) => {
                assert!(cfg.validate().is_ok());
                assert_eq!(cfg.diagnostics.effective_collector_url(), url);
            }
            None => {
                assert!(cfg.validate().is_err());
            }
        }
    }

    #[test]
    fn diagnostics_share_reports_requires_collector_url() {
        let mut cfg = Config::default();
        cfg.diagnostics.share_reports = true;
        assert!(cfg.validate().is_err());
        cfg.diagnostics.collector_url = "https://reports.example".into();
        assert!(cfg.validate().is_ok());
        assert_eq!(
            cfg.diagnostics.effective_submit_url(),
            "https://reports.example/submit"
        );
    }

    #[test]
    fn diagnostics_submit_url_preserves_explicit_submit_path() {
        let mut cfg = Config::default();
        cfg.diagnostics.collector_url = "https://reports.example/submit".into();
        assert_eq!(
            cfg.diagnostics.effective_submit_url(),
            "https://reports.example/submit"
        );
        cfg.diagnostics.collector_url = "https://reports.example/submit/".into();
        assert_eq!(
            cfg.diagnostics.effective_submit_url(),
            "https://reports.example/submit"
        );
    }

    #[test]
    fn diagnostics_accepts_legacy_upload_enabled_alias() {
        let text = r#"
[diagnostics]
upload_enabled = true
upload_url = "https://example.invalid"
"#;
        let cfg = Config::from_toml_str(text, "test").unwrap();
        assert!(cfg.diagnostics.share_reports);
        assert_eq!(cfg.diagnostics.collector_url, "https://example.invalid");
        assert_eq!(
            cfg.diagnostics.effective_submit_url(),
            "https://example.invalid/submit"
        );
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn s3_requires_bucket() {
        let mut cfg = Config::default();
        cfg.storage.backend = StorageBackendKind::S3;
        assert!(cfg.validate().is_err());
        cfg.storage.s3.bucket = "books".into();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn default_widevine_is_off() {
        assert!(!Config::default().download.widevine);
    }

    #[test]
    fn relative_storage_root_resolves_under_files_dir() {
        let mut cfg = Config {
            paths: Some(Paths::from_files_dir(PathBuf::from("/var/lib/libation"))),
            storage: StorageConfig {
                local: StorageLocalConfig {
                    root: PathBuf::from("Audiobooks"),
                },
                ..Default::default()
            },
            ..Default::default()
        };
        cfg.resolve_relative_paths();
        assert_eq!(
            cfg.storage.local.root,
            PathBuf::from("/var/lib/libation/Audiobooks")
        );
    }

    #[test]
    fn absolute_storage_root_unchanged() {
        let mut cfg = Config {
            paths: Some(Paths::from_files_dir(PathBuf::from("/var/lib/libation"))),
            storage: StorageConfig {
                local: StorageLocalConfig {
                    root: PathBuf::from("/data/Audiobooks"),
                },
                ..Default::default()
            },
            ..Default::default()
        };
        cfg.resolve_relative_paths();
        assert_eq!(cfg.storage.local.root, PathBuf::from("/data/Audiobooks"));
    }

    #[test]
    fn write_toml_roundtrip() {
        let mut cfg = Config::default();
        cfg.library.auto_liberate = true;
        cfg.storage.local.root = PathBuf::from("/data/books");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        cfg.write_toml_file(&path).unwrap();
        let loaded = Config::from_toml_file(&path).unwrap();
        assert!(loaded.library.auto_liberate);
        assert_eq!(loaded.storage.local.root, PathBuf::from("/data/books"));
    }

    #[test]
    fn chapter_json_defaults_off_and_maps_legacy_bool() {
        let cfg = DownloadConfig::default();
        assert_eq!(cfg.effective_chapter_json(), ChapterJsonMode::Off);
        assert_eq!(cfg.effective_output(), OutputFormat::EnrichedM4b);

        let legacy = DownloadConfig {
            save_chapter_json: Some(true),
            chapter_layout: "flat".into(),
            ..Default::default()
        };
        assert_eq!(legacy.effective_chapter_json(), ChapterJsonMode::Flat);

        let explicit = DownloadConfig {
            chapter_json: ChapterJsonMode::Both,
            save_chapter_json: Some(false),
            ..Default::default()
        };
        assert_eq!(explicit.effective_chapter_json(), ChapterJsonMode::Both);
    }

    #[test]
    fn output_derives_from_legacy_format_and_split() {
        let mut cfg = DownloadConfig {
            format: DownloadFormat::Mp3,
            ..Default::default()
        };
        assert_eq!(cfg.effective_output(), OutputFormat::SingleMp3);
        cfg.split_files_by_chapter = true;
        assert_eq!(cfg.effective_output(), OutputFormat::SplitMp3ByChapter);
        cfg.output = Some(OutputFormat::None);
        assert_eq!(cfg.effective_output(), OutputFormat::None);
    }
}
