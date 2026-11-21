//! Host configuration, files-dir layout, and logging for Bookclerk.
//!
//! # Audience
//!
//! Host binaries (`bookclerk`, `bookclerkd`) and in-process host crates — not
//! guest plugins. Parses `config.toml`, applies env overrides, resolves
//! `BOOKCLERK_FILES_DIR`, and installs tracing / diagnostics.
//!
//! Product narrative: `docs/server/configuration.md`, `docs/server/diagnostics.md`. Style:
//! `docs/contributing/code-documentation.md`.

// Pre-existing sites, grandfathered until fixed: https://github.com/Bookclerk/bookclerk/issues/284
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::expect_used
)]

mod atomic_replace;
mod cache_settings;
mod cookie_flags;
mod database;
mod desktop;
mod diagnostics;
/// [`ConfigError`] and the crate [`Result`] alias.
mod error;
mod events;
mod extras;
mod install_id;
mod intake;
mod isolation;
mod jobs;
mod journal;
mod listen;
mod logging;
mod media;
mod naming_profile;
mod oidc_broker;
mod operator_auth;
mod output;
mod output_copy;
mod overrides;
mod path_limits;
mod paths;
mod pipeline_opts;
mod placement;
mod platform;
mod plugins;
mod redact;
mod report_history;
mod report_lookup;
mod settings;
mod uploads;

pub use atomic_replace::replace_file;
pub use cache_settings::{
    parse_cache_setting, CacheSettingError, CacheSettingKey, DEFAULT_CACHE_BUDGET_BYTES,
    DEFAULT_CACHE_UNVIEWED_DAYS, MAX_CACHE_BUDGET_BYTES, MAX_CACHE_UNVIEWED_DAYS,
    MIN_CACHE_BUDGET_BYTES,
};
pub use cookie_flags::{cookie_secure_suffix, oidc_transaction_cookie_flags, session_cookie_flags};
pub use database::{
    resolve_d1_api_token, resolve_postgres_url, DatabaseConfig, DatabaseD1Config,
    DatabasePluginKind, DatabasePostgresConfig, DatabaseSqliteConfig,
};
pub use desktop::graphical_session_available;
pub use diagnostics::{
    global as diagnostics_global, BufferedEvent, DiagnosticsHandle, ProcessRole, UploadPayload,
    SLOW_TARGET,
};
pub use error::{ConfigError, Result};
pub use events::EventsConfig;
pub use extras::{
    apply_replacements, classic_key_aliases, default_replacement_characters,
    key_matches_reconcile_pattern, parse_replacement_characters, posix_replacement_characters,
    reconciliation_wildcard_rules, resolve_replacement_characters, s3_replacement_characters,
    windows_replacement_characters, FileTimestampMode, LameConfig, PathSanitizationMode,
    ReplacementRule, RECONCILE_WILDCARD,
};
pub use install_id::{
    checkin_due, record_checkin, send_checkin, CheckIn, InstallId, CHECKIN_INTERVAL_MS,
};
pub use intake::{
    glob_matches, parse_intake_settings, AfterIngest, IntakeAttribution, IntakeSettings,
    IntakeSettingsError, IntakeSettingsRequest, BUILTIN_IGNORE_PATTERNS,
    DEFAULT_SCAN_INTERVAL_SECONDS, DEFAULT_SETTLE_SECONDS, FAILED_FOLDER, IMPORTED_FOLDER,
    MAX_IGNORE_PATTERNS, MAX_IGNORE_PATTERN_BYTES, MAX_INTAKE_CANONICAL_BYTES,
    MAX_INTAKE_SETTINGS_BYTES, MAX_PRUNE_IMPORTED_DAYS, MAX_SCAN_INTERVAL_SECONDS,
    MAX_SETTLE_SECONDS, MIN_SCAN_INTERVAL_SECONDS, MIN_SETTLE_SECONDS,
};
pub use isolation::Isolation;
pub use jobs::{JobsConcurrencyConfig, JobsConfig};
pub use journal::{journald_available, os_log_available, OsLogFacility, OsLogLayer};
pub use listen::ListenAddrs;
pub use logging::{
    init_tracing, init_tracing_with, ring_filter, strip_ansi_escapes, LogFormat, LoggingHandle,
    TracingOptions, RING_DEPENDENCY_LEVEL, RING_TRACE_TARGETS,
};
pub use media::MediaConfig;
pub use naming_profile::{NamingProfile, NamingProfileTemplates, ResolvedNamingTemplates};
pub use oidc_broker::{
    allowlist_permits, email_domain_allowed, oidc_apple_private_key_env_key,
    oidc_client_secret_env_key, oidc_secret_store_name, resolve_mapped_role, OidcBrokerConfig,
    OidcProviderConfig, OidcProvisionMode,
};
pub use operator_auth::{
    generate_operator_token, read_operator_token_env, validate_operator_token,
    ResolveOperatorTokenEnv,
};
pub use output::{
    normalize_storage_prefix, BadBookAction, DestinationId, DestinationNaming, MissingFileAction,
    MultiDestinationMode, OutputConfig,
};
pub use output_copy::{
    copies_to_make, mp3_cbr_kbps, validate_copies, CopyError, CopyFormat, CopyQuality,
    OpusSettings, OutputCopy, MAX_EXTRA_COPIES, MAX_QUALITY_BYTES, MP3_CBR_KBPS, OPUS_KBPS,
    OPUS_MAIN_KBPS,
};
pub use overrides::{
    apply_config_updates, apply_config_updates_from_path, apply_setting_overrides,
};
pub use path_limits::{
    enforce_storage_key_limits, path_len, truncate_filename_stem, truncate_path_component,
    PathLengthMeasure, PathLimits, DEFAULT_MAX_FILENAME_LENGTH, MAX_HOST_KEY_BYTES,
};
pub use paths::{
    resolve_config_path, resolve_files_dir, Paths, BOOKCLERK_CONFIG_ENV, BOOKCLERK_FILES_DIR_ENV,
};
pub use pipeline_opts::{ChapterJsonMode, OutputFormat};
pub use placement::{
    parse_destination_filter, DestinationFilter, FilterEffect, FilterError, FilterField,
    FilterRule, PlacementFacts, PlacementMediaKind, MAX_FILTER_RULES, MAX_FILTER_VALUE_BYTES,
};
pub use platform::detect_distro;
pub use plugins::{
    host_cpu_rate_max, node_logical_cpus, IntegrationsConfig, PluginRegistryEntry, PluginsConfig,
    PluginsJailConfig,
};
pub use redact::{
    contains_registered_secret, is_sensitive_field, is_upload_identifying_field,
    redact_field_value, redact_str, register_secret, register_secret_bytes, register_secrets,
    register_secrets_from_env, sanitize_for_remote_upload, secrets_registry_test_lock,
    truncate_upload_message, unregister_secret, REDACTED,
};
pub use report_history::{
    find_report, load_report_history, mark_report_deleted, parse_submit_reply, record_report,
    AccessCode, ReportId, ReportRecord, SubmitReply, SubmitReplyError, ACCESS_CODE_ALPHABET,
    ACCESS_CODE_LENGTH, MAX_REPORT_HISTORY, MAX_SUBMIT_REPLY_BYTES, REPORT_HISTORY_FILE,
};
pub use report_lookup::{
    delete_report, fetch_report, parse_delete_reply, parse_lookup_reply, EventLevel, IssueState,
    LookupAnswer, LookupError, LookupEvent, LookupFix, LookupIssue, LookupReplyError, LookupReport,
    MAX_LOOKUP_REPLY_BYTES,
};
pub use settings::{
    read_node_api_spki_pin, AudioQuality, AuthConfig, Config, DaemonAuthConfig, DaemonConfig,
    DiagnosticsConfig, DiscoveryConfig, KeyStoreKind, LibraryConfig, NodeApiConfig, NodeApiMode,
    ProcessEnvGuard, LETS_ENCRYPT_DIRECTORY, NODE_API_ACME_DIR, NODE_API_SELF_SIGNED_DIR,
    NODE_API_SPKI_PIN_FILE, TRAY_HANDOFF_TTL_SECS_DEFAULT, TRAY_HANDOFF_TTL_SECS_MAX,
    TRAY_HANDOFF_TTL_SECS_MIN,
};
pub use uploads::UploadsConfig;
