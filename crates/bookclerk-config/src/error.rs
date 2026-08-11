use thiserror::Error;

/// Result alias for configuration load and validation helpers in this crate.
pub type Result<T> = std::result::Result<T, ConfigError>;

/// Errors produced while loading or validating configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Failed to open or read a config file from disk.
    #[error("failed to read config from {path}: {source}")]
    Read {
        /// Absolute or display path of the config file that could not be read.
        path: String,
        /// Underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// TOML syntax or type error while deserializing a config file.
    #[error("failed to parse config from {path}: {source}")]
    Parse {
        /// Absolute or display path of the config file that failed to parse.
        path: String,
        /// Underlying TOML deserialize error.
        #[source]
        source: toml::de::Error,
    },

    /// Semantic validation failure after a successful parse (missing required
    /// fields, mutually exclusive options, unknown plugin ids, …).
    #[error("invalid configuration: {0}")]
    Invalid(String),

    /// Generic I/O failure unrelated to a specific config path context.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
