//! Database backend plugins (`[database]`, `[database.sqlite]`, `[database.d1]`, `[database.postgres]`).
//!
//! Exactly one backend is active (`plugin`). Default is local SQLite.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Result};

fn default_plugin() -> String {
    String::from("sqlite")
}

fn default_true() -> bool {
    true
}

fn default_d1_api_base() -> String {
    String::from("https://api.cloudflare.com/client/v4")
}

/// Which first-party database plugin is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DatabasePluginKind {
    #[default]
    Sqlite,
    D1,
    Postgres,
}

impl DatabasePluginKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::D1 => "d1",
            Self::Postgres => "postgres",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "sqlite" | "local" => Some(Self::Sqlite),
            "d1" | "cloudflare-d1" | "cloudflare_d1" => Some(Self::D1),
            "postgres" | "postgresql" | "pg" => Some(Self::Postgres),
            _ => None,
        }
    }
}

/// Top-level `[database]` — selects the active backend plugin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DatabaseConfig {
    /// Active plugin id (`sqlite`, `d1`, or `postgres`).
    pub plugin: String,
    pub sqlite: DatabaseSqliteConfig,
    pub d1: DatabaseD1Config,
    pub postgres: DatabasePostgresConfig,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            plugin: default_plugin(),
            sqlite: DatabaseSqliteConfig::default(),
            d1: DatabaseD1Config::default(),
            postgres: DatabasePostgresConfig::default(),
        }
    }
}

impl DatabaseConfig {
    /// Parsed active plugin, or error if unknown.
    pub fn active_plugin(&self) -> Result<DatabasePluginKind> {
        DatabasePluginKind::parse(&self.plugin).ok_or_else(|| {
            ConfigError::Invalid(format!(
                "unknown [database].plugin `{}` (expected sqlite, d1, or postgres)",
                self.plugin
            ))
        })
    }

    /// Resolve the SQLite file path (relative paths join `files_dir`).
    #[must_use]
    pub fn sqlite_path(&self, files_dir: &std::path::Path) -> PathBuf {
        let raw = self
            .sqlite
            .path
            .clone()
            .unwrap_or_else(|| PathBuf::from("library.db"));
        if raw.is_absolute() {
            raw
        } else {
            files_dir.join(raw)
        }
    }

    /// Soft validation for the selected plugin.
    pub fn validate(&self) -> Result<()> {
        // Empty plugin means no backend is selected (Settings can clear the
        // active database by unchecking the enabled toggle).
        if self.plugin.trim().is_empty() {
            return Ok(());
        }
        match self.active_plugin()? {
            DatabasePluginKind::Sqlite => Ok(()),
            DatabasePluginKind::D1 => {
                if self.d1.account_id.trim().is_empty() {
                    return Err(ConfigError::Invalid(
                        "[database.d1].account_id is required when plugin = \"d1\"".into(),
                    ));
                }
                if self.d1.database_id.trim().is_empty() {
                    return Err(ConfigError::Invalid(
                        "[database.d1].database_id is required when plugin = \"d1\"".into(),
                    ));
                }
                Ok(())
            }
            DatabasePluginKind::Postgres => {
                let url_empty = self
                    .postgres
                    .url
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty();
                let url_file_empty = self.postgres.url_file.is_none();
                if url_empty && url_file_empty {
                    return Err(ConfigError::Invalid(
                        "[database.postgres].url or url_file is required when plugin = \"postgres\" \
                         (or set BOOKCLERK_DATABASE_POSTGRES_URL / BOOKCLERK_DATABASE_POSTGRES_URL_FILE)"
                            .into(),
                    ));
                }
                Ok(())
            }
        }
    }
}

/// Local SQLite destination (`[database.sqlite]`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DatabaseSqliteConfig {
    /// Kept for symmetry with other plugins; ignored when another plugin is active.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// DB file path (default `library.db` under the files dir).
    pub path: Option<PathBuf>,
}

impl Default for DatabaseSqliteConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: None,
        }
    }
}

/// Cloudflare D1 backend (`[database.d1]`).
///
/// The API token is supplied via the `BOOKCLERK_D1_API_TOKEN` or
/// `CLOUDFLARE_API_TOKEN` environment variable (no credentials file).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DatabaseD1Config {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub account_id: String,
    pub database_id: String,
    /// Cloudflare API base (default `https://api.cloudflare.com/client/v4`).
    #[serde(default = "default_d1_api_base")]
    pub api_base: String,
}

impl Default for DatabaseD1Config {
    fn default() -> Self {
        Self {
            enabled: true,
            account_id: String::new(),
            database_id: String::new(),
            api_base: default_d1_api_base(),
        }
    }
}

/// PostgreSQL backend (`[database.postgres]`).
///
/// Connect via a standard Postgres connection URL:
/// `postgres://user:password@host:5432/dbname`
///
/// The URL contains credentials — prefer `url_file` (pointing at a secrets volume)
/// or the `BOOKCLERK_DATABASE_POSTGRES_URL` environment variable over embedding the
/// URL in TOML.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DatabasePostgresConfig {
    /// Kept for symmetry with other plugins; ignored when another plugin is active.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// PostgreSQL connection URL (e.g. `postgres://user:pass@host/db`).
    /// Registered as a secret for redaction.  Prefer `url_file` in production.
    pub url: Option<String>,
    /// Path to a file containing the connection URL (one line, trimmed).
    /// Relative paths are resolved under `$BOOKCLERK_FILES_DIR`.
    pub url_file: Option<PathBuf>,
}

impl Default for DatabasePostgresConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            url: None,
            url_file: None,
        }
    }
}

/// Resolve the D1 API token from the environment (host-mediated secret).
///
/// Uses `BOOKCLERK_D1_API_TOKEN`, falling back to `CLOUDFLARE_API_TOKEN`.
pub fn resolve_d1_api_token() -> Result<String> {
    if let Ok(v) = std::env::var("BOOKCLERK_D1_API_TOKEN") {
        let t = v.trim();
        if !t.is_empty() {
            return Ok(t.to_string());
        }
    }
    if let Ok(v) = std::env::var("CLOUDFLARE_API_TOKEN") {
        let t = v.trim();
        if !t.is_empty() {
            return Ok(t.to_string());
        }
    }
    Err(ConfigError::Invalid(
        "D1 API token not configured — set BOOKCLERK_D1_API_TOKEN or CLOUDFLARE_API_TOKEN \
         (see docs/database.md)"
            .into(),
    ))
}

/// Resolve the Postgres connection URL from config (env already applied).
///
/// `url_file` takes precedence over `url`.
pub fn resolve_postgres_url(config: &crate::Config) -> Result<String> {
    if let Some(path) = &config.database.postgres.url_file {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            ConfigError::Invalid(format!("reading postgres url_file {}: {e}", path.display()))
        })?;
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
        return Err(ConfigError::Invalid(format!(
            "postgres url_file {} is empty",
            path.display()
        )));
    }
    if let Some(url) = &config.database.postgres.url {
        let trimmed = url.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    Err(ConfigError::Invalid(
        "Postgres URL not configured — set [database.postgres].url, \
         [database.postgres].url_file, BOOKCLERK_DATABASE_POSTGRES_URL, \
         or BOOKCLERK_DATABASE_POSTGRES_URL_FILE (see docs/database.md)"
            .into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_sqlite() {
        let db = DatabaseConfig::default();
        assert_eq!(db.active_plugin().unwrap(), DatabasePluginKind::Sqlite);
        assert!(db.validate().is_ok());
    }

    #[test]
    fn d1_requires_ids() {
        let mut db = DatabaseConfig {
            plugin: "d1".into(),
            ..Default::default()
        };
        assert!(db.validate().is_err());
        db.d1.account_id = "acct".into();
        db.d1.database_id = "db".into();
        assert!(db.validate().is_ok());
    }

    #[test]
    fn postgres_plugin_parses_aliases() {
        assert_eq!(
            DatabasePluginKind::parse("postgres"),
            Some(DatabasePluginKind::Postgres)
        );
        assert_eq!(
            DatabasePluginKind::parse("postgresql"),
            Some(DatabasePluginKind::Postgres)
        );
        assert_eq!(
            DatabasePluginKind::parse("pg"),
            Some(DatabasePluginKind::Postgres)
        );
        assert_eq!(
            DatabasePluginKind::parse("POSTGRES"),
            Some(DatabasePluginKind::Postgres)
        );
    }

    #[test]
    fn postgres_requires_url_or_url_file() {
        let db = DatabaseConfig {
            plugin: "postgres".into(),
            ..Default::default()
        };
        let err = db.validate().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("url") || msg.contains("postgres"),
            "expected url error, got: {msg}"
        );
    }

    #[test]
    fn postgres_valid_with_url() {
        let mut db = DatabaseConfig {
            plugin: "postgres".into(),
            ..Default::default()
        };
        db.postgres.url = Some("postgres://user:pass@localhost/db".into());
        assert!(db.validate().is_ok());
    }

    #[test]
    fn postgres_valid_with_url_file() {
        let mut db = DatabaseConfig {
            plugin: "postgres".into(),
            ..Default::default()
        };
        db.postgres.url_file = Some(PathBuf::from("/run/secrets/db_url"));
        assert!(db.validate().is_ok());
    }

    #[test]
    fn unknown_plugin_error_mentions_postgres() {
        let db = DatabaseConfig {
            plugin: "unknown-db".into(),
            ..Default::default()
        };
        let err = db.active_plugin().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("postgres"),
            "error should mention postgres: {msg}"
        );
    }

    #[test]
    fn postgres_as_str() {
        assert_eq!(DatabasePluginKind::Postgres.as_str(), "postgres");
    }
}
