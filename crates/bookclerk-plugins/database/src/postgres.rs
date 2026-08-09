//! PostgreSQL engine for the database plugin (SeaORM sqlx-postgres).

use bookclerk_library::{LibraryError, Result};
use sea_orm::{Database, DatabaseConnection};

/// Open Postgres with a host-mediated connection URL and apply greenfield schema.
pub async fn open(url: &str) -> Result<DatabaseConnection> {
    let db = Database::connect(url).await.map_err(LibraryError::Orm)?;
    db.ping().await.map_err(LibraryError::Orm)?;
    crate::migrate::apply_pending_migrations(&db).await?;
    tracing::debug!(plugin = "postgres", "opened library database");
    Ok(db)
}
