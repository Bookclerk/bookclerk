//! Application settings loaded from TOML with environment overrides.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Result};
use crate::extras::{FileTimestampMode, LameConfig, PathSanitizationMode, ReplacementRule};
use crate::paths::{resolve_config_path, resolve_files_dir, Paths};

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
        }
    }
}

/// Download / audio format preferences (Libation parity).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DownloadConfig {
    pub quality: AudioQuality,
    pub format: DownloadFormat,
    /// Prefer Widevine/CENC (also enables Adrm→Widevine fallback; auto-provisions L3 CDM).
    pub widevine: bool,
    /// Prefer xHE-AAC on the Widevine path when offered.
    pub xhe_aac: bool,
    /// Optional local Widevine `.wvd` path (absolute or relative to `LIBATION_FILES_DIR`).
    /// When unset, liberate auto-provisions an L3 CDM via [`widevine_cdm_provider`].
    pub widevine_cdm: Option<PathBuf>,
    /// Remote L3 CDM provider. `None` uses classic Libation AudibleCdm; empty/`off` disables auto-fetch.
    pub widevine_cdm_provider: Option<String>,
    /// Classic Libation `FolderTemplate` (e.g. `<author>/<title>`).
    pub folder_template: Option<String>,
    /// Classic Libation `FileTemplate` without extension (e.g. `<asin>` or `<title> [<asin>]`).
    pub file_template: Option<String>,
    /// Save cover JPEG alongside audio (`DownloadCoverArt`; classic default off).
    pub download_cover: bool,
    /// Download companion PDF when available (classic separate PDF liberator).
    pub download_pdf: bool,
    /// Write a `.cue` sidecar from API chapters (`CreateCueSheet`; classic default off).
    pub create_cue: bool,
    /// Embed tags, cover, and chapters natively (`AllowLibationFixup`; classic default on).
    pub fixup_metadata: bool,
    /// Persist API chapter JSON (`chapters.<layout>.json`).
    pub save_chapter_json: bool,
    /// Persist raw catalog API JSON (`metadata.json`; classic `SaveMetadataToFile`).
    pub save_metadata_json: bool,
    /// Cover image size for download/embed (`500`, `1215`, or `native`).
    pub cover_size: String,
    /// Chapter layout for API/metadata (`tree` or `flat`).
    pub chapter_layout: String,
    /// Re-download when liberated media already exists (`OverwriteExisting`).
    pub overwrite_existing: bool,
    /// Scratch directory for in-progress downloads (`InProgress`); relative to files_dir.
    pub in_progress: Option<PathBuf>,
    /// Action when a title fails to liberate (`BadBook`).
    pub bad_book_action: BadBookAction,
    /// Split liberated audio into one file per chapter (`SplitFilesByChapter`).
    pub split_files_by_chapter: bool,
    /// Template for split chapter filenames (`ChapterFileTemplate`).
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
            widevine: false,
            xhe_aac: false,
            widevine_cdm: None,
            widevine_cdm_provider: None,
            folder_template: None,
            file_template: None,
            download_cover: false,
            download_pdf: true,
            create_cue: false,
            fixup_metadata: true,
            save_chapter_json: true,
            save_metadata_json: false,
            cover_size: String::from("500"),
            chapter_layout: String::from("tree"),
            overwrite_existing: false,
            in_progress: None,
            bad_book_action: BadBookAction::Ask,
            split_files_by_chapter: false,
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
        }
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
    pub local: StorageLocalConfig,
    pub s3: StorageS3Config,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            backend: StorageBackendKind::Local,
            local: StorageLocalConfig::default(),
            s3: StorageS3Config::default(),
        }
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
    /// Emit JSON logs when true.
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
        cfg.warn_unsupported_options();
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
        if let Ok(v) = std::env::var("LIBATION_SCAN_INTERVAL_MINUTES") {
            if let Ok(n) = v.parse() {
                self.library.scan_interval_minutes = n;
            }
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
            self.download.save_chapter_json =
                parse_bool(&v).unwrap_or(self.download.save_chapter_json);
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
        Ok(())
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
}
