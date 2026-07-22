//! Configuration, data-directory resolution, and logging setup for Libation.

mod diagnostics;
mod error;
mod extras;
mod journal;
mod logging;
mod overrides;
mod paths;
mod platform;
mod redact;
mod settings;

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
pub use journal::{journald_available, os_log_available, OsLogFacility, OsLogLayer};
pub use logging::{init_tracing, init_tracing_with, LogFormat, LoggingHandle, TracingOptions};
pub use overrides::apply_setting_overrides;
pub use paths::{resolve_files_dir, Paths};
pub use platform::detect_distro;
pub use redact::{
    contains_registered_secret, is_sensitive_field, is_upload_identifying_field,
    redact_field_value, redact_str, register_secret, register_secrets, register_secrets_from_env,
    sanitize_for_remote_upload, truncate_upload_message, REDACTED,
};
pub use settings::{
    AudioQuality, AuthConfig, BadBookAction, Config, DaemonConfig, DiagnosticsConfig,
    DownloadConfig, DownloadFormat, LibraryConfig, StorageBackendKind, StorageConfig,
    StorageLocalConfig, StorageS3Config,
};
