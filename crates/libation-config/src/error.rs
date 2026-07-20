use thiserror::Error;

/// Result alias for config operations.
pub type Result<T> = std::result::Result<T, ConfigError>;

/// Errors produced while loading or validating configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config from {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse config from {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },

    #[error("invalid configuration: {0}")]
    Invalid(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
