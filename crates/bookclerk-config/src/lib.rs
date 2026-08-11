//! Host configuration, files-dir layout, and logging for Bookclerk.
//!
//! # Audience
//!
//! Host binaries (`bookclerk`, `bookclerkd`) and in-process host crates — not
//! guest plugins. Parses `config.toml`, applies env overrides, resolves
//! `BOOKCLERK_FILES_DIR`, and installs tracing / diagnostics.
//!
//! Product narrative: `docs/configuration.md`, `docs/diagnostics.md`. Style:
//! `docs/code-documentation.md`.

mod cookie_flags;
mod database;
mod desktop;
mod diagnostics;
mod error;
mod extras;
mod isolation;
mod journal;
mod listen;
mod logging;
mod media;
mod naming_profile;
mod operator_auth;
mod output;
mod overrides;
mod path_limits;
mod paths;
mod pipeline_opts;
mod platform;
mod plugins;
mod redact;
mod settings;

pub use cookie_flags::{cookie_secure_suffix, session_cookie_flags};
pub use database::{
    resolve_d1_api_token, resolve_postgres_url, DatabaseConfig, DatabaseD1Config,
    DatabasePluginKind, DatabasePostgresConfig, DatabaseSqliteConfig,
};
pub use desktop::graphical_session_available;
pub use diagnostics::{
    global as diagnostics_global, BufferedEvent, DiagnosticsHandle, UploadPayload,
};
pub use error::{ConfigError, Result};
pub use extras::{
    apply_replacements, classic_key_aliases, default_replacement_characters,
    key_matches_reconcile_pattern, parse_replacement_characters, posix_replacement_characters,
    reconciliation_wildcard_rules, resolve_replacement_characters, s3_replacement_characters,
    windows_replacement_characters, FileTimestampMode, LameConfig, PathSanitizationMode,
    ReplacementRule, RECONCILE_WILDCARD,
};
pub use isolation::Isolation;
pub use journal::{journald_available, os_log_available, OsLogFacility, OsLogLayer};
pub use listen::ListenAddrs;
pub use logging::{init_tracing, init_tracing_with, LogFormat, LoggingHandle, TracingOptions};
pub use media::MediaConfig;
pub use naming_profile::{NamingProfile, NamingProfileTemplates, ResolvedNamingTemplates};
pub use operator_auth::{
    generate_operator_token, read_operator_token_env, validate_operator_token,
    ResolveOperatorTokenEnv,
};
pub use output::{
    normalize_storage_prefix, BadBookAction, DestinationNaming, MultiDestinationMode,
    OutputBackendKind, OutputConfig, OutputLocalConfig, OutputS3Config,
};
pub use overrides::{
    apply_config_updates, apply_config_updates_from_path, apply_setting_overrides,
};
pub use path_limits::{
    enforce_storage_key_limits, path_len, truncate_filename_stem, truncate_path_component,
    PathLengthMeasure, PathLimits, DEFAULT_MAX_FILENAME_LENGTH, S3_MAX_OBJECT_KEY_BYTES,
};
pub use paths::{resolve_config_path, resolve_files_dir, Paths};
pub use pipeline_opts::{ChapterJsonMode, OutputFormat};
pub use platform::detect_distro;
pub use plugins::{
    AudiobookshelfConfig, IntegrationsConfig, PluginRegistryEntry, PluginsConfig, SourcesConfig,
};
pub use redact::{
    contains_registered_secret, is_sensitive_field, is_upload_identifying_field,
    redact_field_value, redact_str, register_secret, register_secrets, register_secrets_from_env,
    sanitize_for_remote_upload, secrets_registry_test_lock, truncate_upload_message, REDACTED,
};
pub use settings::{
    AudioQuality, AuthConfig, Config, DaemonAuthConfig, DaemonConfig, DiagnosticsConfig,
    DiscoveryConfig, LibraryConfig,
};
