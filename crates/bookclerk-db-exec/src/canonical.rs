//! Canonical sqlite-shaped SeaORM transport.
//!
//! Host/library leftover SQL stays BookclerkSQL (`?`, `LIKE`). This module
//! builds SeaORM [`Statement`]s on the sqlite-shaped backend so the production
//! plugin-host proxy never emits `$n` / `GLOB`. Adapters physically lower
//! after RPC. Library code must not name [`sea_orm::DatabaseBackend`].

use sea_orm::{ConnectionTrait, DatabaseBackend, DbErr, ExecResult, QueryResult, Statement, Value};

/// SeaORM backend used for canonical `?` transport (not a physical engine).
const CANONICAL_TRANSPORT: DatabaseBackend = DatabaseBackend::Sqlite;

/// Builds a SeaORM statement that keeps canonical `?` placeholders.
#[must_use]
pub fn canonical_statement(
    sql: impl Into<String>,
    values: impl IntoIterator<Item = Value>,
) -> Statement {
    Statement::from_sql_and_values(CANONICAL_TRANSPORT, sql, values)
}

/// Executes already-canonical host SQL. Never desugars or physically lowers.
///
/// # Errors
///
/// Returns when the connection rejects the statement.
pub async fn execute_canonical<C>(
    db: &C,
    sql: &str,
    values: impl IntoIterator<Item = Value>,
) -> Result<ExecResult, DbErr>
where
    C: ConnectionTrait,
{
    db.execute_raw(canonical_statement(sql, values)).await
}

/// Queries already-canonical host SQL. Never desugars or physically lowers.
///
/// # Errors
///
/// Returns when the connection rejects the statement.
pub async fn query_canonical<C>(
    db: &C,
    sql: &str,
    values: impl IntoIterator<Item = Value>,
) -> Result<Vec<QueryResult>, DbErr>
where
    C: ConnectionTrait,
{
    db.query_all_raw(canonical_statement(sql, values)).await
}

#[cfg(test)]
mod tests {
    use super::{canonical_statement, CANONICAL_TRANSPORT};
    use sea_orm::{DatabaseBackend, Statement};

    #[test]
    fn canonical_statements_keep_question_marks_and_like() {
        let sql = "SELECT id FROM t WHERE title LIKE ? AND a = ?";
        let stmt = canonical_statement(sql, Vec::<sea_orm::Value>::new());
        assert_eq!(stmt.db_backend, CANONICAL_TRANSPORT);
        assert_eq!(stmt.db_backend, DatabaseBackend::Sqlite);
        assert!(stmt.sql.contains('?'), "{}", stmt.sql);
        assert!(stmt.sql.contains("LIKE"), "{}", stmt.sql);
        assert!(!stmt.sql.contains("GLOB"), "{}", stmt.sql);
        assert!(!stmt.sql.contains("$1"), "{}", stmt.sql);
        let rebuilt = Statement::from_sql_and_values(DatabaseBackend::Sqlite, sql, []);
        assert_eq!(rebuilt.sql, sql);
    }
}
