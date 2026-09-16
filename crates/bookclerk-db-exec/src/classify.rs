//! Adapter-edge classification of SeaORM [`DbErr`] into contract categories.
//!
//! Guests already stamp stable tokens (`SQLITE_BUSY`, SQLSTATE `40P01`,
//! `unavailable:`). Host schema apply must not parse engine prose. sqlx
//! Display for Postgres unique violations is the server message only — the
//! SQLSTATE lives on the driver `DatabaseError::code` field, so classification
//! must read that typed field (and SeaORM [`sea_orm::SqlErr`]) before scanning text.
//!
//! [`DbErrorClass::Conflict`] is uniqueness / duplicate-object only. Foreign
//! key, CHECK, and NOT NULL violations are [`DbErrorClass::Other`] so schema
//! apply does not retry them as a migrator race. Application code that needs a
//! broader "constraint failed" bucket should inspect [`sea_orm::SqlErr`]
//! directly.

use std::ops::Deref;

use sea_orm::{DbErr, RuntimeErr, SqlErr};

/// Portable Bookclerk SQL error category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbErrorClass {
    /// Retry the same operation after re-reading durable state.
    Unavailable,
    /// Uniqueness or duplicate-object; re-read state, do not blindly retry DDL.
    Conflict,
    /// Permanent or unclassified failure (including FK / CHECK / NOT NULL).
    Other,
}

/// Classifies `err` from adapter-stamped codes, not English engine messages.
#[must_use]
pub fn classify_db_err(err: &DbErr) -> DbErrorClass {
    if let Some(sql_err) = err.sql_err() {
        match sql_err {
            SqlErr::UniqueConstraintViolation(_) => return DbErrorClass::Conflict,
            SqlErr::ForeignKeyConstraintViolation(_) => return DbErrorClass::Other,
            _ => {}
        }
    }
    if let Some(code) = sqlx_engine_code(err) {
        let class = classify_sql_token(&code);
        if class != DbErrorClass::Other {
            return class;
        }
    }
    classify_db_err_message(&err.to_string())
}

/// True when host schema apply may retry after re-reading durable schema state.
///
/// Uniqueness / duplicate-object races and transient unavailability qualify.
/// Semantic integrity failures do not.
#[must_use]
pub fn is_schema_apply_retryable(err: &DbErr) -> bool {
    matches!(
        classify_db_err(err),
        DbErrorClass::Unavailable | DbErrorClass::Conflict
    )
}

/// SQLSTATE / driver token from sqlx when `err` is a driver `Database` error.
pub(crate) fn sqlx_engine_code(err: &DbErr) -> Option<String> {
    match err {
        DbErr::Exec(RuntimeErr::SqlxError(e))
        | DbErr::Query(RuntimeErr::SqlxError(e))
        | DbErr::Conn(RuntimeErr::SqlxError(e)) => match e.deref() {
            sea_orm::sqlx::Error::Database(db_err) => db_err.code().map(|c| c.into_owned()),
            _ => None,
        },
        _ => None,
    }
}

/// Classifies a SQLSTATE or `SQLITE_*` token.
fn classify_sql_token(token: &str) -> DbErrorClass {
    let upper = token.to_ascii_uppercase();
    if is_uniqueness_or_duplicate_object_token(&upper) {
        return DbErrorClass::Conflict;
    }
    if upper.starts_with("SQLITE_BUSY")
        || upper.starts_with("SQLITE_LOCKED")
        || matches!(
            upper.as_str(),
            "40001"
                | "40P01"
                | "55P03"
                | "08000"
                | "08001"
                | "08003"
                | "08006"
                | "08007"
                | "57P01"
                | "57P02"
                | "57P03"
                | "53300"
        )
    {
        return DbErrorClass::Unavailable;
    }
    DbErrorClass::Other
}

/// Unique / primary-key / duplicate-object tokens that can mean a peer already applied.
fn is_uniqueness_or_duplicate_object_token(upper: &str) -> bool {
    upper.contains("SQLITE_CONSTRAINT_UNIQUE")
        || upper.contains("SQLITE_CONSTRAINT_PRIMARYKEY")
        || matches!(upper, "23505" | "42P07" | "42710" | "42723" | "42712")
}

/// Classifies an already-formatted adapter/ORM error string.
#[must_use]
pub fn classify_db_err_message(message: &str) -> DbErrorClass {
    let upper = message.to_ascii_uppercase();
    if upper.contains("UNAVAILABLE:")
        || upper.contains("SQLITE_BUSY")
        || upper.contains("SQLITE_LOCKED")
        || upper.contains("40P01")
        || upper.contains("40001")
        || upper.contains("55P03")
        || message.contains("database commit failed")
    {
        return DbErrorClass::Unavailable;
    }
    if is_uniqueness_or_duplicate_object_token(&upper)
        || (upper.contains("SQLITE_CONSTRAINT") && upper.contains("UNIQUE"))
        || upper.contains("23505")
        || upper.contains("42P07")
        || upper.contains("42710")
    {
        return DbErrorClass::Conflict;
    }
    DbErrorClass::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_adapter_code_tokens() {
        assert_eq!(
            classify_db_err_message("SQLITE_BUSY (5): database is locked"),
            DbErrorClass::Unavailable
        );
        assert_eq!(
            classify_db_err_message("SQLITE_LOCKED (6)"),
            DbErrorClass::Unavailable
        );
        assert_eq!(
            classify_db_err_message("error 40P01 deadlock detected"),
            DbErrorClass::Unavailable
        );
        assert_eq!(
            classify_db_err_message("unavailable: D1 ambiguous commit"),
            DbErrorClass::Unavailable
        );
        assert_eq!(
            classify_db_err_message("SQLITE_CONSTRAINT (2067): UNIQUE"),
            DbErrorClass::Conflict
        );
        assert_eq!(
            classify_db_err_message("duplicate key value violates unique constraint 23505"),
            DbErrorClass::Conflict
        );
        assert_eq!(
            classify_db_err_message("cancelled: atomic session cancelled"),
            DbErrorClass::Other
        );
        assert_eq!(
            classify_db_err_message("syntax error near TABLE"),
            DbErrorClass::Other
        );
    }

    #[test]
    fn postgres_unique_display_without_sqlstate_is_not_prose_matched() {
        assert_eq!(
            classify_db_err_message(
                "duplicate key value violates unique constraint \"pg_type_typname_nsp_index\""
            ),
            DbErrorClass::Other,
            "sqlx Display omits 23505; do not match English unique-constraint prose"
        );
    }

    #[test]
    fn custom_db_err_uses_sqlite_constraint_token() {
        let err = DbErr::Custom(
            "SQLITE_CONSTRAINT (2067): UNIQUE constraint failed: schema_migrations.version".into(),
        );
        assert_eq!(classify_db_err(&err), DbErrorClass::Conflict);
        assert!(is_schema_apply_retryable(&err));
    }

    #[test]
    fn classifies_sqlstate_tokens() {
        assert_eq!(classify_sql_token("23505"), DbErrorClass::Conflict);
        assert_eq!(classify_sql_token("42P07"), DbErrorClass::Conflict);
        assert_eq!(classify_sql_token("42710"), DbErrorClass::Conflict);
        assert_eq!(classify_sql_token("40P01"), DbErrorClass::Unavailable);
        assert_eq!(classify_sql_token("40001"), DbErrorClass::Unavailable);
        assert_eq!(classify_sql_token("SQLITE_BUSY"), DbErrorClass::Unavailable);
        assert_eq!(
            classify_sql_token("SQLITE_CONSTRAINT_UNIQUE"),
            DbErrorClass::Conflict
        );
        assert_eq!(classify_sql_token("42601"), DbErrorClass::Other);
    }

    #[test]
    fn integrity_violations_are_not_schema_apply_conflicts() {
        assert_eq!(classify_sql_token("23503"), DbErrorClass::Other);
        assert_eq!(classify_sql_token("23502"), DbErrorClass::Other);
        assert_eq!(classify_sql_token("23514"), DbErrorClass::Other);
        assert_eq!(classify_sql_token("23000"), DbErrorClass::Other);
        assert_eq!(
            classify_sql_token("SQLITE_CONSTRAINT_FOREIGNKEY"),
            DbErrorClass::Other
        );
        assert_eq!(
            classify_sql_token("SQLITE_CONSTRAINT_CHECK"),
            DbErrorClass::Other
        );
        assert_eq!(
            classify_sql_token("SQLITE_CONSTRAINT_NOTNULL"),
            DbErrorClass::Other
        );
        assert_eq!(
            classify_db_err_message("SQLITE_CONSTRAINT_FOREIGNKEY (787)"),
            DbErrorClass::Other
        );
        let fk = DbErr::Custom("FOREIGN KEY constraint failed".into());
        assert!(!is_schema_apply_retryable(&fk));
        let busy = DbErr::Custom("SQLITE_BUSY (5): database is locked".into());
        assert!(is_schema_apply_retryable(&busy));
        let unique = DbErr::Custom("23505 unique_violation".into());
        assert!(is_schema_apply_retryable(&unique));
    }
}
