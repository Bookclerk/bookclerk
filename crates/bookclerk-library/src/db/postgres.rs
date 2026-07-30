//! PostgreSQL backend via SeaORM `sqlx-postgres`.
//!
//! Connects using a standard Postgres connection URL
//! (`postgres://user:pass@host:port/dbname`). The URL is resolved from
//! (in priority order):
//! 1. `[database.postgres].url_file` contents (safest — points at a secrets volume)
//! 2. `[database.postgres].url` config value
//! 3. `BOOKCLERK_DATABASE_POSTGRES_URL` environment variable (applied before
//!    this function is called by [`super::connect_from_config`] via `apply_env_overrides`)
//!
//! Fresh databases receive the consolidated Postgres DDL from
//! [`crate::migrations::postgres_bootstrap_schema`] via
//! [`super::apply_pending_migrations`].

use bookclerk_config::Config;
use sea_orm::{Database, DatabaseConnection};

use crate::error::{LibraryError, Result};

/// Resolve the Postgres connection URL from config (env already applied).
///
/// `url_file` takes precedence over `url` so operators can point at a secrets
/// volume without embedding credentials in TOML.
pub fn resolve_postgres_url(config: &Config) -> Result<String> {
    if let Some(path) = &config.database.postgres.url_file {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            LibraryError::Other(anyhow::anyhow!(
                "reading postgres url_file {}: {e}",
                path.display()
            ))
        })?;
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
        return Err(LibraryError::Other(anyhow::anyhow!(
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
    Err(LibraryError::Other(anyhow::anyhow!(
        "Postgres URL not configured — set [database.postgres].url, \
         [database.postgres].url_file, BOOKCLERK_DATABASE_POSTGRES_URL, \
         or BOOKCLERK_DATABASE_POSTGRES_URL_FILE (see docs/database.md)"
    )))
}

/// Open a Postgres database connection and return a SeaORM `DatabaseConnection`.
///
/// Pings the database after connecting and applies the Postgres bootstrap schema
/// (or verifies `schema_migrations`) via [`super::apply_pending_migrations`].
pub async fn connect_postgres(config: &Config) -> Result<DatabaseConnection> {
    let url = resolve_postgres_url(config)?;
    let db = Database::connect(&url).await.map_err(LibraryError::Orm)?;
    db.ping().await.map_err(LibraryError::Orm)?;
    super::apply_pending_migrations(&db).await?;
    tracing::debug!(
        plugin = "postgres",
        "opened library database (sea-orm sqlx-postgres)"
    );
    Ok(db)
}
