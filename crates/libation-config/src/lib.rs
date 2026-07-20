//! Configuration, data-directory resolution, and logging setup for Libation.

mod error;
mod extras;
mod logging;
mod overrides;
mod paths;
mod settings;

pub use error::{ConfigError, Result};
pub use extras::{
    apply_replacements, classic_key_aliases, default_replacement_characters,
    key_matches_reconcile_pattern, parse_replacement_characters, posix_replacement_characters,
    reconciliation_wildcard_rules, resolve_replacement_characters, s3_replacement_characters,
    windows_replacement_characters, FileTimestampMode, LameConfig, PathSanitizationMode,
    ReplacementRule, RECONCILE_WILDCARD,
};
pub use logging::{init_tracing, LogFormat};
pub use overrides::apply_setting_overrides;
pub use paths::{resolve_files_dir, Paths};
pub use settings::{
    AudioQuality, AuthConfig, BadBookAction, Config, DaemonConfig, DownloadConfig, DownloadFormat,
    LibraryConfig, StorageBackendKind, StorageConfig, StorageLocalConfig, StorageS3Config,
};
