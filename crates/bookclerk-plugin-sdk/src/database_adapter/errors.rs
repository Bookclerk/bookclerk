//! Engine error classification for SQLite and PostgreSQL guests.
//!
//! Classifies by SQLSTATE / rusqlite codes first. English substrings such as
//! `"unique"` are not the primary signal. COMMIT-time and connection-loss
//! failures are `unavailable` so the host retries the same `operationId`.

use crate::{PluginError, PluginErrorCode};
use sea_orm::{DbErr, RuntimeErr};
use serde_json::json;
use std::ops::Deref;

/// Maps an engine / library failure onto a structured [`PluginError`].
///
/// # Arguments
///
/// * `err` - Display of `DbErr`, `LibraryError`, or a guest string.
#[must_use]
pub fn plugin_error_from_engine(err: impl std::fmt::Display) -> PluginError {
    classify_message(&err.to_string())
}

/// Maps a SeaORM [`DbErr`] using typed driver codes when present.
///
/// PostgreSQL SQLSTATE comes from `sqlx::DatabaseError::code()`. SQLite
/// guests format `SQLITE_*` into [`DbErr::Custom`] before this mapper runs.
#[must_use]
pub fn plugin_error_from_db_err(err: &DbErr) -> PluginError {
    if let Some(code) = sqlx_engine_code(err) {
        let message = err.to_string();
        let (abi, engine) = classify_sqlstate(&code, &message);
        return with_engine_code(abi, message, engine);
    }
    plugin_error_from_engine(err)
}

/// SQLSTATE from sqlx when `err` is a driver `Database` error.
fn sqlx_engine_code(err: &DbErr) -> Option<String> {
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

/// Classifies a formatted engine error into a [`PluginError`].
fn classify_message(message: &str) -> PluginError {
    let (code, engine_code) = classify_engine_message(message);
    with_engine_code(code, message.to_string(), engine_code)
}

/// Attaches `engineCode` details when a driver token is known.
fn with_engine_code(
    code: PluginErrorCode,
    message: String,
    engine_code: Option<String>,
) -> PluginError {
    let mut out = PluginError::new(code, message);
    if let Some(engine_code) = engine_code {
        let mut details = serde_json::Map::new();
        details.insert("engineCode".into(), json!(engine_code));
        out.details = Some(details);
    }
    out
}

/// Classifies a formatted engine error. Returns `(code, optional engine token)`.
fn classify_engine_message(message: &str) -> (PluginErrorCode, Option<String>) {
    if message.contains("invalid query cursor") {
        return (PluginErrorCode::InvalidCursor, None);
    }
    if let Some(code) = first_sqlstate(message) {
        return classify_sqlstate(code, message);
    }
    if let Some(code) = first_sqlite_token(message) {
        return classify_sqlite_token(code, message);
    }
    if is_commit_or_connection_loss(message) {
        return (PluginErrorCode::Unavailable, Some("COMMIT".into()));
    }
    if is_cancelled(message) {
        return (PluginErrorCode::Cancelled, None);
    }
    if is_deadline(message) {
        return (PluginErrorCode::DeadlineExceeded, None);
    }
    if is_unsupported_sql(message) {
        return (PluginErrorCode::Unsupported, None);
    }
    if is_syntax(message) {
        return (PluginErrorCode::InvalidParams, None);
    }
    (PluginErrorCode::Internal, None)
}

/// First known SQLSTATE (`23505`, `40001`, `40P01`, …) in `message`.
fn first_sqlstate(message: &str) -> Option<&str> {
    const KNOWN: &[&str] = &[
        "23505", "23000", "23503", "23502", "23514", "40001", "40P01", "55P03", "57014", "08000",
        "08001", "08003", "08006", "08007", "57P01", "57P02", "57P03", "53300", "42601", "42P01",
        "42883", "22P02", "0A000",
    ];
    KNOWN.iter().copied().find(|state| message.contains(state))
}

/// First `SQLITE_*` token in `message`.
fn first_sqlite_token(message: &str) -> Option<&str> {
    let start = message.find("SQLITE_")?;
    let rest = &message[start..];
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

/// Maps a PostgreSQL SQLSTATE onto an ABI code.
fn classify_sqlstate(state: &str, message: &str) -> (PluginErrorCode, Option<String>) {
    let token = state.to_string();
    match state {
        "23505" | "23000" | "23503" | "23502" | "23514" => (PluginErrorCode::Conflict, Some(token)),
        "40001" | "40P01" | "55P03" => (PluginErrorCode::Unavailable, Some(token)),
        "57014" => (PluginErrorCode::DeadlineExceeded, Some(token)),
        "08000" | "08001" | "08003" | "08006" | "08007" | "57P01" | "57P02" | "57P03" | "53300" => {
            (PluginErrorCode::Unavailable, Some(token))
        }
        "42601" | "42P01" | "42883" | "22P02" => (PluginErrorCode::InvalidParams, Some(token)),
        "0A000" => (PluginErrorCode::Unsupported, Some(token)),
        _ => {
            if is_commit_or_connection_loss(message) {
                (PluginErrorCode::Unavailable, Some(token))
            } else {
                (PluginErrorCode::Internal, Some(token))
            }
        }
    }
}

/// Maps a `SQLITE_*` token onto an ABI code.
fn classify_sqlite_token(token: &str, message: &str) -> (PluginErrorCode, Option<String>) {
    let owned = token.to_string();
    if token.starts_with("SQLITE_CONSTRAINT") {
        return (PluginErrorCode::Conflict, Some(owned));
    }
    match token {
        "SQLITE_BUSY" | "SQLITE_LOCKED" | "SQLITE_LOCKED_SHAREDCACHE" => {
            (PluginErrorCode::Unavailable, Some(owned))
        }
        "SQLITE_INTERRUPT" => {
            if is_commit_or_connection_loss(message) {
                (PluginErrorCode::Unavailable, Some(owned))
            } else {
                (PluginErrorCode::Cancelled, Some(owned))
            }
        }
        "SQLITE_IOERR" | "SQLITE_CANTOPEN" | "SQLITE_NOTADB" | "SQLITE_PROTOCOL"
        | "SQLITE_NOLFS" => (PluginErrorCode::Unavailable, Some(owned)),
        "SQLITE_ERROR" if is_syntax(message) => (PluginErrorCode::InvalidParams, Some(owned)),
        "SQLITE_MISUSE" if is_unsupported_sql(message) => {
            (PluginErrorCode::Unsupported, Some(owned))
        }
        _ => {
            if is_commit_or_connection_loss(message) {
                (PluginErrorCode::Unavailable, Some(owned))
            } else if is_syntax(message) {
                (PluginErrorCode::InvalidParams, Some(owned))
            } else {
                (PluginErrorCode::Internal, Some(owned))
            }
        }
    }
}

/// True when the failure is COMMIT-time, connection loss, or server shutdown.
fn is_commit_or_connection_loss(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("database commit failed")
        || lower.contains("connection reset")
        || lower.contains("connection aborted")
        || lower.contains("broken pipe")
        || lower.contains("server closed the connection")
        || lower.contains("server shutdown")
        || lower.contains("terminating connection")
        || lower.contains("could not connect")
        || lower.contains("connection refused")
        || lower.contains("unexpected eof")
        || lower.contains("os error 104")
        || lower.contains("i/o error") && lower.contains("commit")
}

/// True when the guest observed an explicit cancel (not at COMMIT).
fn is_cancelled(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("cancelled:") || lower.contains("atomic session cancelled")
}

/// True when a deadline elapsed before COMMIT.
fn is_deadline(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("deadline_exceeded:")
        || lower.contains("deadline exceeded")
        || lower.contains("timed out")
        || lower.contains("timeout")
}

/// True when the engine rejected the statement as unsupported SQL.
fn is_unsupported_sql(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("unsupported") || lower.contains("not implemented")
}

/// True when the engine reported a syntax / parse error.
fn is_syntax(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("syntax error") || lower.contains("near \"") || lower.contains("parser error")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::missing_panics_doc)]

    use super::*;

    #[test]
    fn unique_sqlstate_is_conflict() {
        let err = plugin_error_from_engine("Exec error: 23505 duplicate key value");
        assert_eq!(err.code, PluginErrorCode::Conflict, "{err}");
        assert_eq!(
            err.details.as_ref().and_then(|d| d.get("engineCode")),
            Some(&json!("23505"))
        );
    }

    #[test]
    fn sqlite_constraint_is_conflict() {
        let err = plugin_error_from_engine(
            "SQLITE_CONSTRAINT (2067): UNIQUE constraint failed: db_serialization_slots.slot_key",
        );
        assert_eq!(err.code, PluginErrorCode::Conflict, "{err}");
    }

    #[test]
    fn serialization_and_busy_are_unavailable() {
        let pg = plugin_error_from_engine("could not serialize access: 40001");
        assert_eq!(pg.code, PluginErrorCode::Unavailable, "{pg}");
        let deadlock = plugin_error_from_engine("40P01 deadlock detected");
        assert_eq!(deadlock.code, PluginErrorCode::Unavailable, "{deadlock}");
        let busy = plugin_error_from_engine("SQLITE_BUSY (5): database is locked");
        assert_eq!(busy.code, PluginErrorCode::Unavailable, "{busy}");
        let locked = plugin_error_from_engine("SQLITE_LOCKED (6): database table is locked");
        assert_eq!(locked.code, PluginErrorCode::Unavailable, "{locked}");
    }

    #[test]
    fn commit_failure_is_unavailable() {
        let err = plugin_error_from_engine(
            "ORM / database plugin error: database commit failed: injected commit failure",
        );
        assert_eq!(err.code, PluginErrorCode::Unavailable, "{err}");
    }

    #[test]
    fn syntax_is_invalid_params() {
        let err = plugin_error_from_engine("SQLITE_ERROR (1): near \"FROMM\": syntax error");
        assert_eq!(err.code, PluginErrorCode::InvalidParams, "{err}");
        let pg = plugin_error_from_engine("42601: syntax error at or near \"FROMM\"");
        assert_eq!(pg.code, PluginErrorCode::InvalidParams, "{pg}");
    }

    #[test]
    fn unique_english_without_code_is_not_conflict() {
        let err = plugin_error_from_engine("internal: unique helper label missing");
        assert_eq!(err.code, PluginErrorCode::Internal, "{err}");
    }
}
