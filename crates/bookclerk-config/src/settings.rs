//! Application settings loaded from TOML with environment overrides.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Result};
use crate::naming_profile::NamingProfile;
use crate::output::OutputConfig;
use crate::paths::{resolve_config_path, resolve_files_dir, Paths};
use crate::pipeline_opts::{ChapterJsonMode, OutputFormat};
use crate::plugins::{IntegrationsConfig, SourcesConfig};

/// Top-level Bookclerk configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Config {
    /// Filesystem layout (resolved at load time; not always present in TOML).
    #[serde(skip)]
    pub paths: Option<Paths>,

    pub library: LibraryConfig,
    /// Library database backend plugin (`[database]`).
    #[serde(default)]
    pub database: crate::database::DatabaseConfig,
    pub output: OutputConfig,
    pub daemon: DaemonConfig,
    pub auth: AuthConfig,
    /// Content-source plugins (`[sources.audible]`, `[sources.graphicaudio]`, …).
    pub sources: SourcesConfig,
    /// Optional third-party integrations (`[integrations.*]`). Not diagnostics.
    pub integrations: IntegrationsConfig,
    /// Discovery / recommendations / request queue (`[discovery]`).
    #[serde(default)]
    pub discovery: DiscoveryConfig,
    /// Opt-in crash / error-burst report upload (`[diagnostics]`).
    #[serde(default)]
    pub diagnostics: DiagnosticsConfig,
    /// Media decode / encode worker pool (`[media]`).
    #[serde(default)]
    pub media: crate::MediaConfig,
    /// How external plugin guests are run (`[plugins]`).
    #[serde(default)]
    pub plugins: crate::PluginsConfig,
}

/// Auth encryption settings (`[auth]` section).
///
/// Bookclerk always encrypts credentials via the process DEK (`master.key`).
/// Set `BOOKCLERK_AUTH_PASSWORD` (preferred) or `[auth].password` to wrap the
/// DEK with a passphrase at rest (`BCK1` → `BCK2`). A later password can be
/// applied via CLI (`config master-key wrap` / `config set auth.password`) or
/// daemon config reload — no GUI in this surface.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AuthConfig {
    /// Optional passphrase wrapping `master.key`. Prefer `BOOKCLERK_AUTH_PASSWORD`
    /// env (wins when both are set). Registered for log redaction on load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

/// Library / scan related settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct LibraryConfig {
    /// Automatically acquire newly scanned titles (daemon).
    pub auto_acquire: bool,
    /// Scan interval in minutes for `bookclerkd` (0 = disabled).
    pub scan_interval_minutes: u64,
    /// Import podcast episodes during scan (`ImportEpisodes`).
    pub import_episodes: bool,
    /// Import Audible Plus titles during scan (`ImportPlusTitles`).
    pub import_plus_titles: bool,
    /// Acquire podcast episodes (`DownloadEpisodes`; distinct from `ImportEpisodes`).
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

/// Discovery / recommendations / embeddings settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DiscoveryConfig {
    /// Run local embeddings for similarity scoring of storefront candidates.
    pub embeddings_enabled: bool,
    /// Model id (`all-minilm-l6-v2-q` preferred; runtime may fall back to `local-hash-v1`).
    pub embedding_model: String,
    /// ONNX intra-op threads (keep at 1 on small VPSes).
    pub embed_intra_threads: usize,
    /// Fill metadata gaps via Open Library (low-volume, cached; not for bulk).
    pub openlibrary_enabled: bool,
    /// Contact email for Open Library User-Agent (API guidelines).
    pub openlibrary_contact_email: Option<String>,
    /// Max Open Library HTTP calls per enrich run.
    pub openlibrary_max_requests_per_run: usize,
    /// Reserved for a future WorldCat provider (requires API key).
    pub worldcat_enabled: bool,
    /// Expand recommendations from storefront catalogs (unowned titles).
    pub storefront_candidates: bool,
    /// Max local taste seeds used for storefront expansion.
    pub storefront_seed_limit: usize,
    /// Cap remote storefront HTTP calls per recommend run.
    pub storefront_max_remote_calls: usize,
    /// When true, drop GraphicAudio Magento series-set SKUs from candidates.
    /// Default false — series sets are included.
    pub exclude_graphicaudio_series_sets: bool,
    /// How often `bookclerkd` syncs ABS listening progress (0 = disabled).
    pub listen_sync_interval_minutes: u64,
    /// Default recommendation list size.
    pub recommend_limit: usize,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            embeddings_enabled: true,
            embedding_model: String::from("all-minilm-l6-v2-q"),
            embed_intra_threads: 1,
            openlibrary_enabled: true,
            openlibrary_contact_email: None,
            openlibrary_max_requests_per_run: 25,
            worldcat_enabled: false,
            storefront_candidates: true,
            storefront_seed_limit: 8,
            storefront_max_remote_calls: 32,
            exclude_graphicaudio_series_sets: false,
            listen_sync_interval_minutes: 60,
            recommend_limit: 20,
        }
    }
}

impl Default for LibraryConfig {
    fn default() -> Self {
        Self {
            auto_acquire: false,
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

/// Audible download quality.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum AudioQuality {
    #[default]
    High,
    Normal,
}

/// Daemon / HTTP control plane settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DaemonConfig {
    /// Bind addresses for the HTTP control plane (string or array in TOML).
    ///
    /// Default binds both IPv4 and IPv6 loopback on port 8787.
    pub listen: crate::ListenAddrs,
    /// Emit JSON logs on stderr when true (journald sink is always structured).
    pub json_logs: bool,
    /// When true, `bookclerkd` starts an in-process system tray in a graphical
    /// session (opens the web UI in the system browser).
    ///
    /// Headless hosts (no `DISPLAY`/`WAYLAND_DISPLAY`, no session bus) skip the
    /// tray. Override with `BOOKCLERK_NO_TRAY=1` or `BOOKCLERK_DAEMON_TRAY=0`.
    pub tray: bool,
    /// Operator authentication for the HTTP API / GUI.
    pub auth: DaemonAuthConfig,
    /// Peer addresses trusted to set `X-Forwarded-For` / `Forwarded` (CIDR or IP).
    ///
    /// Empty (default): always use the direct TCP peer — safe behind no proxy.
    /// When the peer matches an entry, login throttling uses the leftmost
    /// `X-Forwarded-For` client address (typical single reverse-proxy layout).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trusted_proxies: Vec<String>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            listen: crate::ListenAddrs::default(),
            json_logs: true,
            tray: true,
            auth: DaemonAuthConfig::default(),
            trusted_proxies: Vec::new(),
        }
    }
}

/// Operator token / session settings for the daemon HTTP API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DaemonAuthConfig {
    /// When true, `/api/*` (and legacy control routes) require Bearer or session cookie.
    pub enabled: bool,
    /// Browser session lifetime in hours after `POST /api/auth/login`.
    pub session_ttl_hours: u64,
    /// Failed `POST /api/auth/login` attempts (per client IP) before a lockout.
    pub login_max_failures: u32,
    /// How long a client stays locked out after exceeding [`Self::login_max_failures`].
    pub login_lockout_secs: u64,
}

impl Default for DaemonAuthConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            session_ttl_hours: 12,
            login_max_failures: 5,
            login_lockout_secs: 60,
        }
    }
}

/// Compile-time default from `BOOKCLERK_DIAGNOSTICS_COLLECTOR_URL` when running `cargo build`.
fn compile_time_collector_url() -> Option<&'static str> {
    option_env!("BOOKCLERK_DIAGNOSTICS_COLLECTOR_URL")
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Opt-in sharing of recent **redacted** logs on crash or error bursts.
///
/// Defaults to disabled. Operators flip `share_reports` and set `collector_url`
/// (or bake `BOOKCLERK_DIAGNOSTICS_COLLECTOR_URL` at `cargo build` time).
/// The client POSTs to `/submit`; a GitHub Action calls `/report`.
///
/// Bookclerk never manages log-file rotation — use journald / the container
/// runtime (Windows Event Log / macOS unified logging are future work).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DiagnosticsConfig {
    /// When true, share redacted crash/ERROR-burst reports with the collector.
    #[serde(alias = "upload_enabled")]
    pub share_reports: bool,
    /// Worker origin (HTTPS). When empty, uses `BOOKCLERK_DIAGNOSTICS_COLLECTOR_URL`
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
        // Record the path actually used for load so writers (`config set`,
        // `plugins enable`, …) update the same file the user pointed at via
        // `--config` / `BOOKCLERK_CONFIG`, not only `{files_dir}/config.toml`.
        let mut paths = paths;
        paths.config_file = path;
        // Honour `[database.sqlite].path` (and env override) for library.db.
        paths.library_db = cfg.database.sqlite_path(&paths.files_dir);
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

    /// Apply `BOOKCLERK_*` environment overrides.
    pub fn apply_env_overrides(&mut self) {
        if let Ok(v) = std::env::var("BOOKCLERK_DATABASE_PLUGIN") {
            if !v.trim().is_empty() {
                self.database.plugin = v.trim().to_string();
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DATABASE_SQLITE_PATH") {
            if !v.trim().is_empty() {
                self.database.sqlite.path = Some(PathBuf::from(v));
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_D1_ACCOUNT_ID") {
            self.database.d1.account_id = v;
        }
        if let Ok(v) = std::env::var("BOOKCLERK_D1_DATABASE_ID") {
            self.database.d1.database_id = v;
        }
        if let Ok(v) = std::env::var("BOOKCLERK_D1_API_BASE") {
            if !v.trim().is_empty() {
                self.database.d1.api_base = v;
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DATABASE_POSTGRES_URL") {
            if !v.trim().is_empty() {
                self.database.postgres.url = Some(v.trim().to_string());
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DATABASE_POSTGRES_URL_FILE") {
            if !v.trim().is_empty() {
                self.database.postgres.url_file = Some(PathBuf::from(v.trim()));
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_OUTPUT_LOCAL_ENABLED") {
            if let Some(enabled) = parse_bool(&v) {
                self.output.local.enabled = enabled;
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_OUTPUT_S3_ENABLED") {
            if let Some(enabled) = parse_bool(&v) {
                self.output.s3.enabled = enabled;
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_OUTPUT_LOCAL_ROOT") {
            self.output.local.root = PathBuf::from(v);
            self.output.local.enabled = true;
        }
        if let Ok(v) = std::env::var("BOOKCLERK_OUTPUT_LOCAL_PREFIX") {
            self.output.local.prefix = v;
        }
        if let Ok(v) = std::env::var("BOOKCLERK_OUTPUT_S3_BUCKET")
            .or_else(|_| std::env::var("BOOKCLERK_S3_BUCKET"))
        {
            self.output.s3.bucket = v;
        }
        if let Ok(v) = std::env::var("BOOKCLERK_OUTPUT_S3_PREFIX")
            .or_else(|_| std::env::var("BOOKCLERK_S3_PREFIX"))
        {
            self.output.s3.prefix = v;
        }
        if let Ok(v) = std::env::var("BOOKCLERK_OUTPUT_S3_REGION")
            .or_else(|_| std::env::var("BOOKCLERK_S3_REGION"))
        {
            self.output.s3.region = v;
        }
        if let Ok(v) = std::env::var("BOOKCLERK_OUTPUT_S3_ENDPOINT")
            .or_else(|_| std::env::var("BOOKCLERK_S3_ENDPOINT"))
        {
            self.output.s3.endpoint = Some(v);
        }
        if let Ok(v) = std::env::var("BOOKCLERK_OUTPUT_S3_FORCE_PATH_STYLE")
            .or_else(|_| std::env::var("BOOKCLERK_S3_FORCE_PATH_STYLE"))
        {
            self.output.s3.force_path_style =
                parse_bool(&v).unwrap_or(self.output.s3.force_path_style);
        }
        self.media.apply_env_overrides();
        self.plugins.apply_env_overrides();
        if let Ok(v) = std::env::var("BOOKCLERK_DAEMON_LISTEN") {
            match crate::ListenAddrs::parse_list(&v) {
                Ok(addrs) => self.daemon.listen = addrs,
                Err(err) => tracing::warn!(
                    value = %v,
                    error = %err,
                    "ignoring invalid BOOKCLERK_DAEMON_LISTEN"
                ),
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DAEMON_JSON_LOGS") {
            self.daemon.json_logs = parse_bool(&v).unwrap_or(self.daemon.json_logs);
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DAEMON_TRAY") {
            self.daemon.tray = parse_bool(&v).unwrap_or(self.daemon.tray);
        }
        // Explicit kill-switch wins over BOOKCLERK_DAEMON_TRAY / config.toml.
        if std::env::var_os("BOOKCLERK_NO_TRAY").is_some() {
            self.daemon.tray = false;
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DAEMON_AUTH_ENABLED") {
            self.daemon.auth.enabled = parse_bool(&v).unwrap_or(self.daemon.auth.enabled);
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DAEMON_AUTH_SESSION_TTL_HOURS") {
            if let Ok(hours) = v.trim().parse::<u64>() {
                self.daemon.auth.session_ttl_hours = hours.max(1);
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DAEMON_AUTH_LOGIN_MAX_FAILURES") {
            if let Ok(n) = v.trim().parse::<u32>() {
                self.daemon.auth.login_max_failures = n.max(1);
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DAEMON_AUTH_LOGIN_LOCKOUT_SECS") {
            if let Ok(secs) = v.trim().parse::<u64>() {
                self.daemon.auth.login_lockout_secs = secs.max(1);
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DAEMON_TRUSTED_PROXIES") {
            let parsed: Vec<String> = v
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            if !parsed.is_empty() {
                self.daemon.trusted_proxies = parsed;
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DIAGNOSTICS_SHARE_REPORTS")
            .or_else(|_| std::env::var("BOOKCLERK_DIAGNOSTICS_UPLOAD_ENABLED"))
        {
            self.diagnostics.share_reports =
                parse_bool(&v).unwrap_or(self.diagnostics.share_reports);
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DIAGNOSTICS_COLLECTOR_URL")
            .or_else(|_| std::env::var("BOOKCLERK_DIAGNOSTICS_UPLOAD_URL"))
        {
            self.diagnostics.collector_url = v;
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DIAGNOSTICS_UPLOAD_ON_CRASH") {
            self.diagnostics.upload_on_crash =
                parse_bool(&v).unwrap_or(self.diagnostics.upload_on_crash);
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DIAGNOSTICS_UPLOAD_ON_ERROR_BURST") {
            self.diagnostics.upload_on_error_burst =
                parse_bool(&v).unwrap_or(self.diagnostics.upload_on_error_burst);
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DIAGNOSTICS_UPLOAD_ON_WARN_BURST") {
            self.diagnostics.upload_on_warn_burst =
                parse_bool(&v).unwrap_or(self.diagnostics.upload_on_warn_burst);
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DIAGNOSTICS_ERROR_BURST_THRESHOLD") {
            if let Ok(n) = v.parse() {
                self.diagnostics.error_burst_threshold = n;
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DIAGNOSTICS_ERROR_BURST_WINDOW_SECS") {
            if let Ok(n) = v.parse() {
                self.diagnostics.error_burst_window_secs = n;
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DIAGNOSTICS_WARN_BURST_THRESHOLD") {
            if let Ok(n) = v.parse() {
                self.diagnostics.warn_burst_threshold = n;
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DIAGNOSTICS_WARN_BURST_WINDOW_SECS") {
            if let Ok(n) = v.parse() {
                self.diagnostics.warn_burst_window_secs = n;
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DIAGNOSTICS_RING_BUFFER_CAPACITY") {
            if let Ok(n) = v.parse() {
                self.diagnostics.ring_buffer_capacity = n;
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_AUTO_ACQUIRE") {
            self.library.auto_acquire = parse_bool(&v).unwrap_or(self.library.auto_acquire);
        }
        if let Ok(v) = std::env::var("BOOKCLERK_ENRICH_FROM_AUDIBLE") {
            self.library.enrich_from_audible =
                parse_bool(&v).unwrap_or(self.library.enrich_from_audible);
        }
        if let Ok(v) = std::env::var("BOOKCLERK_ENRICH_MIN_CONFIDENCE") {
            if let Ok(n) = v.parse::<u8>() {
                self.library.enrich_min_confidence = n.min(100);
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DISCOVERY_EMBEDDINGS_ENABLED") {
            self.discovery.embeddings_enabled =
                parse_bool(&v).unwrap_or(self.discovery.embeddings_enabled);
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DISCOVERY_EMBEDDING_MODEL") {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                self.discovery.embedding_model = trimmed.to_string();
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DISCOVERY_EMBED_INTRA_THREADS") {
            if let Ok(n) = v.parse::<usize>() {
                self.discovery.embed_intra_threads = n.max(1);
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DISCOVERY_OPENLIBRARY_ENABLED") {
            self.discovery.openlibrary_enabled =
                parse_bool(&v).unwrap_or(self.discovery.openlibrary_enabled);
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DISCOVERY_OPENLIBRARY_CONTACT_EMAIL") {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                self.discovery.openlibrary_contact_email = Some(trimmed.to_string());
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DISCOVERY_STOREFRONT_CANDIDATES") {
            self.discovery.storefront_candidates =
                parse_bool(&v).unwrap_or(self.discovery.storefront_candidates);
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DISCOVERY_EXCLUDE_GRAPHICAUDIO_SERIES_SETS") {
            self.discovery.exclude_graphicaudio_series_sets =
                parse_bool(&v).unwrap_or(self.discovery.exclude_graphicaudio_series_sets);
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DISCOVERY_LISTEN_SYNC_INTERVAL_MINUTES") {
            if let Ok(n) = v.parse::<u64>() {
                self.discovery.listen_sync_interval_minutes = n;
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DISCOVERY_RECOMMEND_LIMIT") {
            if let Ok(n) = v.parse::<usize>() {
                self.discovery.recommend_limit = n.max(1);
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_FIX_STORAGE_LAYOUT") {
            self.library.fix_storage_layout =
                parse_bool(&v).unwrap_or(self.library.fix_storage_layout);
        }
        if let Ok(v) = std::env::var("BOOKCLERK_SCAN_INTERVAL_MINUTES") {
            if let Ok(n) = v.parse() {
                self.library.scan_interval_minutes = n;
            }
        }
        if let Ok(v) =
            std::env::var("BOOKCLERK_GA_ACCESS").or_else(|_| std::env::var("BOOKCLERK_GA_FETCH"))
        {
            let trimmed = v.trim();
            if !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("auto") {
                self.sources.set_string("graphicaudio", "access", trimmed);
            }
        }
        // Generic `BOOKCLERK_SOURCE_<ID>_ENABLED` for any source/plugin id
        // (`AUDIBLE` → `audible`, `MY_STORE` → `my_store`, …).
        apply_source_enabled_env_overrides(&mut self.sources, std::env::vars());
        if let Ok(v) = std::env::var("BOOKCLERK_INTEGRATIONS_PUBLIC_ORIGIN") {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                self.integrations.public_origin = Some(trimmed.to_string());
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_ABS_BASE_URL") {
            self.integrations.set_audiobookshelf_string("base_url", v);
        }
        if let Ok(v) = std::env::var("BOOKCLERK_ABS_API_KEY") {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                self.integrations
                    .set_audiobookshelf_string("api_key", trimmed);
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_ABS_LIBRARY_ID") {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                self.integrations
                    .set_audiobookshelf_string("library_id", trimmed);
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_ABS_ENABLED") {
            if let Some(b) = parse_bool(&v) {
                self.integrations.set_audiobookshelf_bool("enabled", b);
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_ABS_WATCH_USERS") {
            if let Some(b) = parse_bool(&v) {
                self.integrations.set_audiobookshelf_bool("watch_users", b);
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_ABS_NOTIFY_SCAN_ON_ACQUIRE") {
            if let Some(b) = parse_bool(&v) {
                self.integrations
                    .set_audiobookshelf_bool("notify_scan_on_acquire", b);
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_ABS_ALLOW_CREDENTIAL_LOGIN") {
            if let Some(b) = parse_bool(&v) {
                self.integrations
                    .set_audiobookshelf_bool("allow_credential_login", b);
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_OUTPUT_WIDEVINE")
            .or_else(|_| std::env::var("BOOKCLERK_WIDEVINE"))
        {
            self.output.widevine = parse_bool(&v).unwrap_or(self.output.widevine);
        }
        if let Ok(v) = std::env::var("BOOKCLERK_OUTPUT_XHE_AAC")
            .or_else(|_| std::env::var("BOOKCLERK_XHE_AAC"))
        {
            self.output.xhe_aac = parse_bool(&v).unwrap_or(self.output.xhe_aac);
        }
        if let Ok(v) = std::env::var("BOOKCLERK_OUTPUT_WIDEVINE_CDM")
            .or_else(|_| std::env::var("BOOKCLERK_WIDEVINE_CDM"))
        {
            self.output.widevine_cdm = Some(PathBuf::from(v));
        }
        if let Ok(v) = std::env::var("BOOKCLERK_OUTPUT_WIDEVINE_CDM_PROVIDER")
            .or_else(|_| std::env::var("BOOKCLERK_WIDEVINE_CDM_PROVIDER"))
        {
            self.output.widevine_cdm_provider = Some(v);
        }
        if let Ok(v) = std::env::var("BOOKCLERK_OUTPUT_NAMING_PROFILE")
            .or_else(|_| std::env::var("BOOKCLERK_NAMING_PROFILE"))
        {
            if let Some(profile) = NamingProfile::parse(&v) {
                self.output.naming_profile = profile;
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_OUTPUT_FOLDER_TEMPLATE")
            .or_else(|_| std::env::var("BOOKCLERK_FOLDER_TEMPLATE"))
        {
            self.output.folder_template = Some(v);
        }
        if let Ok(v) = std::env::var("BOOKCLERK_OUTPUT_FILE_TEMPLATE")
            .or_else(|_| std::env::var("BOOKCLERK_FILE_TEMPLATE"))
        {
            self.output.file_template = Some(v);
        }
        if let Ok(v) = std::env::var("BOOKCLERK_OUTPUT_DOWNLOAD_COVER")
            .or_else(|_| std::env::var("BOOKCLERK_DOWNLOAD_COVER"))
        {
            self.output.download_cover = parse_bool(&v).unwrap_or(self.output.download_cover);
        }
        if let Ok(v) = std::env::var("BOOKCLERK_OUTPUT_DOWNLOAD_PDF")
            .or_else(|_| std::env::var("BOOKCLERK_DOWNLOAD_PDF"))
        {
            self.output.download_pdf = parse_bool(&v).unwrap_or(self.output.download_pdf);
        }
        if let Ok(v) = std::env::var("BOOKCLERK_OUTPUT_CREATE_CUE")
            .or_else(|_| std::env::var("BOOKCLERK_CREATE_CUE"))
        {
            self.output.create_cue = parse_bool(&v).unwrap_or(self.output.create_cue);
        }
        if let Ok(v) = std::env::var("BOOKCLERK_OUTPUT_FIXUP_METADATA")
            .or_else(|_| std::env::var("BOOKCLERK_FIXUP_METADATA"))
        {
            self.output.fixup_metadata = parse_bool(&v).unwrap_or(self.output.fixup_metadata);
        }
        if let Ok(v) = std::env::var("BOOKCLERK_OUTPUT_SAVE_CHAPTER_JSON")
            .or_else(|_| std::env::var("BOOKCLERK_SAVE_CHAPTER_JSON"))
        {
            if let Some(mode) = ChapterJsonMode::parse(&v) {
                self.output.chapter_json = mode;
            } else if let Some(b) = parse_bool(&v) {
                self.output.save_chapter_json = Some(b);
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_OUTPUT_CHAPTER_JSON")
            .or_else(|_| std::env::var("BOOKCLERK_CHAPTER_JSON"))
        {
            if let Some(mode) = ChapterJsonMode::parse(&v) {
                self.output.chapter_json = mode;
            }
        }
        if let Ok(v) =
            std::env::var("BOOKCLERK_OUTPUT_FORMAT").or_else(|_| std::env::var("BOOKCLERK_OUTPUT"))
        {
            if let Some(output) = OutputFormat::parse(&v) {
                self.output.format = output;
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_AUDIBLE_BITRATE") {
            let bitrate = match v.trim().to_ascii_lowercase().as_str() {
                "normal" => "normal",
                _ => "high",
            };
            self.sources.set_string("audible", "bitrate", bitrate);
        }
        if let Ok(v) = std::env::var("BOOKCLERK_LIBRO_CONTAINER") {
            if !v.trim().is_empty() {
                self.sources
                    .set_string("libro", "container", v.trim().to_ascii_lowercase());
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_GA_BITRATE") {
            if !v.trim().is_empty() {
                self.sources
                    .set_string("graphicaudio", "bitrate", v.trim().to_ascii_lowercase());
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_GA_CONTAINER") {
            if !v.trim().is_empty() {
                self.sources
                    .set_string("graphicaudio", "container", v.trim().to_ascii_lowercase());
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_OUTPUT_COVER_SIZE")
            .or_else(|_| std::env::var("BOOKCLERK_COVER_SIZE"))
        {
            if !v.trim().is_empty() {
                self.output.cover_size = v;
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_OUTPUT_CHAPTER_LAYOUT")
            .or_else(|_| std::env::var("BOOKCLERK_CHAPTER_LAYOUT"))
        {
            if !v.trim().is_empty() {
                self.output.chapter_layout = v;
            }
        }
    }

    /// Soft validation of cross-field constraints.
    pub fn validate(&self) -> Result<()> {
        self.database.validate()?;
        self.output.validate_destinations()?;
        if self.diagnostics.share_reports && self.diagnostics.effective_collector_url().is_empty() {
            return Err(ConfigError::Invalid(
                "diagnostics.share_reports=true requires diagnostics.collector_url, \
                 BOOKCLERK_DIAGNOSTICS_COLLECTOR_URL at runtime, or the same variable \
                 set when running cargo build"
                    .into(),
            ));
        }
        // Reserved names that must not appear as opaque plugin tables.
        if self.integrations.plugins.contains_key("diagnostics") {
            return Err(ConfigError::Invalid(
                "unknown field `diagnostics` under [integrations] — use top-level [diagnostics]"
                    .into(),
            ));
        }
        for reserved in [
            "claim_ticket_ttl_hours",
            "public_origin",
            "portal_session_ttl_hours",
        ] {
            if self.integrations.plugins.contains_key(reserved) {
                return Err(ConfigError::Invalid(format!(
                    "reserved integrations field `{reserved}` must not be a plugin table"
                )));
            }
        }
        Ok(())
    }

    /// Resolve the passphrase that wraps `master.key`.
    ///
    /// Prefers `BOOKCLERK_AUTH_PASSWORD` over `[auth].password`.
    #[must_use]
    pub fn auth_password(&self) -> Option<String> {
        if let Ok(v) = std::env::var("BOOKCLERK_AUTH_PASSWORD") {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        self.auth
            .password
            .as_ref()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
    }

    /// Register config/env secrets for exact-value log redaction.
    pub fn register_known_secrets(&self) {
        crate::redact::register_secrets_from_env();
        if let Some(pw) = self.auth_password() {
            crate::redact::register_secret(&pw);
        }
        if let Some(key) = self.integrations.audiobookshelf().api_key {
            let trimmed = key.trim();
            if !trimmed.is_empty() {
                crate::redact::register_secret(trimmed);
            }
        }
        for env_key in [
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "BOOKCLERK_AWS_ACCESS_KEY_ID",
            "BOOKCLERK_AWS_SECRET_ACCESS_KEY",
            "BOOKCLERK_AWS_SESSION_TOKEN",
        ] {
            if let Ok(v) = std::env::var(env_key) {
                let trimmed = v.trim();
                if !trimmed.is_empty() {
                    crate::redact::register_secret(trimmed);
                }
            }
        }
        for env_key in ["BOOKCLERK_D1_API_TOKEN", "CLOUDFLARE_API_TOKEN"] {
            if let Ok(v) = std::env::var(env_key) {
                let trimmed = v.trim();
                if !trimmed.is_empty() {
                    crate::redact::register_secret(trimmed);
                }
            }
        }
        // Postgres URL contains credentials — register as a secret for redaction.
        if let Some(url) = &self.database.postgres.url {
            let trimmed = url.trim();
            if !trimmed.is_empty() {
                crate::redact::register_secret(trimmed);
            }
        }
        if let Ok(v) = std::env::var("BOOKCLERK_DATABASE_POSTGRES_URL") {
            let trimmed = v.trim().to_string();
            if !trimmed.is_empty() {
                crate::redact::register_secret(&trimmed);
            }
        }
        if let Some(path) = &self.database.postgres.url_file {
            if let Ok(raw) = std::fs::read_to_string(path) {
                let trimmed = raw.trim().to_string();
                if !trimmed.is_empty() {
                    crate::redact::register_secret(&trimmed);
                }
            }
        }
    }

    /// Resolve relative `output.local.root` under `files_dir`.
    ///
    /// Keeps absolute roots (Docker `/data/Audiobooks`, classic migrate paths)
    /// unchanged. Relative defaults like `Audiobooks` become
    /// `{BOOKCLERK_FILES_DIR}/Audiobooks` so systemd/Docker cwd does not matter.
    pub fn resolve_relative_paths(&mut self) {
        let Some(paths) = &self.paths else {
            return;
        };
        if self.output.local.root.is_relative() {
            self.output.local.root = paths.files_dir.join(&self.output.local.root);
        }
        if let Some(cdm) = &self.output.widevine_cdm {
            if cdm.is_relative() {
                self.output.widevine_cdm = Some(paths.files_dir.join(cdm));
            }
        }
        if let Some(scratch) = &self.output.in_progress {
            if scratch.is_relative() {
                self.output.in_progress = Some(paths.files_dir.join(scratch));
            }
        }
        if let Some(db_path) = &self.database.sqlite.path {
            if db_path.is_relative() {
                self.database.sqlite.path = Some(paths.files_dir.join(db_path));
            }
        }
        if let Some(url_file) = &self.database.postgres.url_file {
            if url_file.is_relative() {
                self.database.postgres.url_file = Some(paths.files_dir.join(url_file));
            }
        }
    }

    /// Warn / note about Widevine / auth encryption setup.
    pub fn warn_unsupported_options(&self) {
        if self.output.widevine && self.output.widevine_cdm.is_none() {
            tracing::info!(
                "output.widevine=true — L3 CDM auto-provisions via AudibleCdm on first Widevine \
                 acquire (Android auth from `bookclerk auth login`), stored in encrypted_secrets. \
                 Optional BYO: output.widevine_cdm / {{files_dir}}/widevine.wvd \
                 (or set output.widevine_cdm_provider=off)"
            );
        }
        if self.auth_password().is_none() {
            tracing::warn!(
                "no auth password set — master.key may be BCK1 (unwrapped DEK). \
                 Set BOOKCLERK_AUTH_PASSWORD or [auth].password, then \
                 `bookclerk config master-key wrap` (or reload bookclerkd)."
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
        self.output
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

/// Apply `BOOKCLERK_SOURCE_<ID>_ENABLED` overrides from an env-like key/value
/// iterator. `<ID>` is lowercased as the source plugin id.
fn apply_source_enabled_env_overrides<I, K, V>(sources: &mut crate::SourcesConfig, vars: I)
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    const PREFIX: &str = "BOOKCLERK_SOURCE_";
    const SUFFIX: &str = "_ENABLED";
    for (key, value) in vars {
        let key = key.as_ref();
        let Some(rest) = key.strip_prefix(PREFIX) else {
            continue;
        };
        let Some(id_part) = rest.strip_suffix(SUFFIX) else {
            continue;
        };
        if id_part.is_empty() {
            continue;
        }
        if let Some(enabled) = parse_bool(value.as_ref()) {
            sources.set_enabled(&id_part.to_ascii_lowercase(), enabled);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize_storage_prefix;

    #[test]
    fn parse_example_toml() {
        let text = r#"
[library]
auto_acquire = true
scan_interval_minutes = 30

[output]
format = "enriched_m4b"
widevine = true
xhe_aac = false

[sources.audible]
bitrate = "high"

[output.local]
enabled = true
root = "/data/audiobooks"

[output.s3]
enabled = false
bucket = "my-audiobooks"
prefix = "library/"
region = "us-east-1"

[daemon]
listen = "0.0.0.0:8787"
json_logs = true
"#;
        let cfg = Config::from_toml_str(text, "test").unwrap();
        assert!(cfg.library.auto_acquire);
        assert_eq!(cfg.library.scan_interval_minutes, 30);
        assert_eq!(cfg.output.enabled_backend_names(), vec!["local"]);
        assert_eq!(cfg.output.local.root, PathBuf::from("/data/audiobooks"));
        assert_eq!(cfg.daemon.listen.as_slice(), ["0.0.0.0:8787"]);
        assert!(!cfg.diagnostics.share_reports);
    }

    #[test]
    fn output_path_limit_prefix_and_multi_enabled() {
        let mut cfg = Config::default();
        assert_eq!(cfg.output.path_limit_prefix(), "");
        assert_eq!(cfg.output.enabled_backend_names(), vec!["local"]);

        cfg.output.local.prefix = "books".into();
        assert_eq!(cfg.output.path_limit_prefix(), "books/");

        // Both destinations may be enabled; S3 prefix wins for key budgeting.
        cfg.output.s3.enabled = true;
        cfg.output.s3.bucket = "b".into();
        cfg.output.s3.prefix = "library/".into();
        assert_eq!(cfg.output.enabled_backend_names(), vec!["local", "s3"]);
        assert_eq!(cfg.output.path_limit_prefix(), "library/");
        assert!(cfg.validate().is_ok());

        cfg.output.local.enabled = false;
        cfg.output.s3.prefix = "s3-books".into();
        assert_eq!(cfg.output.path_limit_prefix(), "s3-books/");
        assert_eq!(cfg.output.enabled_backend_names(), vec!["s3"]);
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
            cfg.sources.get_string("graphicaudio", "access"),
            Some("zip")
        );
        // Missing plugin tables default to enabled.
        assert!(Config::default().sources.is_enabled("audible"));
        assert!(Config::default().sources.is_enabled("graphicaudio"));
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
        let cfg = Config::from_toml_str(text, "test").unwrap();
        let err = cfg.validate().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("diagnostics"),
            "expected reserved diagnostics error, got: {msg}"
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
bitrate = "lo"
"#;
        let cfg = Config::from_toml_str(text, "test").unwrap();
        assert!(!cfg.sources.is_enabled("chirp"));
        assert!(cfg.sources.is_enabled("audible"));
        assert_eq!(
            cfg.sources.get_string("graphicaudio", "access"),
            Some("device")
        );
        assert_eq!(
            cfg.sources.get_string("graphicaudio", "bitrate"),
            Some("lo")
        );
    }

    #[test]
    fn sources_native_knobs() {
        let text = r#"
[sources.audible]
bitrate = "normal"

[sources.libro]
container = "zip"

[sources.graphicaudio]
access = "zip"
bitrate = "lo"
container = "mp3"
"#;
        let cfg = Config::from_toml_str(text, "test").unwrap();
        assert_eq!(cfg.sources.get_string("audible", "bitrate"), Some("normal"));
        assert_eq!(cfg.sources.get_string("libro", "container"), Some("zip"));
        assert_eq!(
            cfg.sources.get_string("graphicaudio", "access"),
            Some("zip")
        );
        assert_eq!(
            cfg.sources.get_string("graphicaudio", "bitrate"),
            Some("lo")
        );
        assert_eq!(
            cfg.sources.get_string("graphicaudio", "container"),
            Some("mp3")
        );
    }

    #[test]
    fn sources_partial_table_keeps_enabled_true() {
        // Only setting source-specific knobs must not flip enabled→false.
        let text = r#"
[sources.graphicaudio]
access = "zip"

[sources.libro]
enabled = true
"#;
        let cfg = Config::from_toml_str(text, "test").unwrap();
        assert!(cfg.sources.is_enabled("graphicaudio"));
        assert!(cfg.sources.is_enabled("libro"));
        assert!(cfg.sources.is_enabled("audible"));
        assert!(cfg.sources.is_enabled("chirp"));
    }

    #[test]
    fn source_enabled_env_overrides_any_plugin_id() {
        let mut cfg = Config::default();
        apply_source_enabled_env_overrides(
            &mut cfg.sources,
            [
                ("BOOKCLERK_SOURCE_CHIRP_ENABLED", "0"),
                ("BOOKCLERK_SOURCE_ECHO_ENABLED", "false"),
                ("BOOKCLERK_SOURCE_MY_STORE_ENABLED", "1"),
                ("BOOKCLERK_SOURCE_GRAPHICAUDIO_ENABLED", "off"),
                ("UNRELATED", "0"),
                ("BOOKCLERK_SOURCE_NOPE", "0"),
                ("BOOKCLERK_SOURCE__ENABLED", "0"),
            ],
        );
        assert!(!cfg.sources.is_enabled("chirp"));
        assert!(!cfg.sources.is_enabled("echo"));
        assert!(cfg.sources.is_enabled("my_store"));
        assert!(!cfg.sources.is_enabled("graphicaudio"));
        // Untouched ids still default enabled.
        assert!(cfg.sources.is_enabled("audible"));
        assert!(cfg.sources.is_enabled("libro"));
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
        cfg.output.local.enabled = false;
        cfg.output.s3.enabled = true;
        assert!(cfg.validate().is_err());
        cfg.output.s3.bucket = "books".into();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn default_widevine_is_off() {
        assert!(!Config::default().output.widevine);
    }

    #[test]
    fn relative_storage_root_resolves_under_files_dir() {
        let mut cfg = Config {
            paths: Some(Paths::from_files_dir(PathBuf::from("/var/lib/bookclerk"))),
            output: OutputConfig {
                local: crate::OutputLocalConfig {
                    root: PathBuf::from("Audiobooks"),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        cfg.resolve_relative_paths();
        assert_eq!(
            cfg.output.local.root,
            PathBuf::from("/var/lib/bookclerk/Audiobooks")
        );
    }

    #[test]
    fn absolute_storage_root_unchanged() {
        let mut cfg = Config {
            paths: Some(Paths::from_files_dir(PathBuf::from("/var/lib/bookclerk"))),
            output: OutputConfig {
                local: crate::OutputLocalConfig {
                    root: PathBuf::from("/data/Audiobooks"),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };
        cfg.resolve_relative_paths();
        assert_eq!(cfg.output.local.root, PathBuf::from("/data/Audiobooks"));
    }

    #[test]
    fn write_toml_roundtrip() {
        let mut cfg = Config::default();
        cfg.library.auto_acquire = true;
        cfg.output.local.root = PathBuf::from("/data/books");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        cfg.write_toml_file(&path).unwrap();
        let loaded = Config::from_toml_file(&path).unwrap();
        assert!(loaded.library.auto_acquire);
        assert_eq!(loaded.output.local.root, PathBuf::from("/data/books"));
    }

    #[test]
    fn load_records_explicit_config_path() {
        let dir = tempfile::tempdir().unwrap();
        let files = dir.path().join("files");
        std::fs::create_dir_all(&files).unwrap();
        let custom = dir.path().join("custom.toml");
        std::fs::write(&custom, "library.auto_acquire = true\n").unwrap();
        let cfg = Config::load(Some(files), Some(custom.clone())).unwrap();
        assert_eq!(cfg.paths().config_file, custom);
        assert!(cfg.library.auto_acquire);
    }

    #[test]
    fn chapter_json_defaults_off_and_maps_legacy_bool() {
        let cfg = OutputConfig::default();
        assert_eq!(cfg.effective_chapter_json(), ChapterJsonMode::Off);
        assert_eq!(cfg.effective_format(), OutputFormat::EnrichedM4b);

        let legacy = OutputConfig {
            save_chapter_json: Some(true),
            chapter_layout: "flat".into(),
            ..Default::default()
        };
        assert_eq!(legacy.effective_chapter_json(), ChapterJsonMode::Flat);

        let explicit = OutputConfig {
            chapter_json: ChapterJsonMode::Both,
            save_chapter_json: Some(false),
            ..Default::default()
        };
        assert_eq!(explicit.effective_chapter_json(), ChapterJsonMode::Both);
    }

    #[test]
    fn output_format_is_direct() {
        let mut cfg = OutputConfig {
            format: OutputFormat::SingleMp3,
            ..Default::default()
        };
        assert_eq!(cfg.effective_format(), OutputFormat::SingleMp3);
        cfg.format = OutputFormat::None;
        assert_eq!(cfg.effective_format(), OutputFormat::None);
    }
}
