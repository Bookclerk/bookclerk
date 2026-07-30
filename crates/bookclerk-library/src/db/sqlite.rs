//! Local SQLite via SeaORM `ProxyDatabaseTrait` wrapping rusqlite.
//!
//! SeaORM 2.0's `sqlx-sqlite` driver does not currently compile against
//! `sea-query` 1.0.x (`Value` no longer boxes payloads). The proxy path keeps
//! a single `libsqlite3` (shared with audible-rs) and one ORM API for SQLite + D1.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use rusqlite::Connection;
use sea_orm::{DbErr, ProxyDatabaseTrait, ProxyExecResult, ProxyRow, Statement, Value};

/// SeaORM proxy over a shared rusqlite connection.
#[derive(Debug)]
pub struct SqliteProxy {
    conn: Mutex<Connection>,
}

impl SqliteProxy {
    #[must_use]
    pub fn new(conn: Connection) -> Self {
        Self {
            conn: Mutex::new(conn),
        }
    }
}

#[async_trait]
impl ProxyDatabaseTrait for SqliteProxy {
    async fn query(&self, statement: Statement) -> std::result::Result<Vec<ProxyRow>, DbErr> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| DbErr::Custom(format!("sqlite mutex poisoned: {e}")))?;
        let mut stmt = conn
            .prepare(&statement.sql)
            .map_err(|e| DbErr::Custom(e.to_string()))?;
        let binds = statement_binds(&statement);
        // Capture per-column declared types so SQL NULLs become correctly typed
        // `Value::*(None)` variants — SeaORM's `Option<T>` decoding checks
        // `value == T::null()`, so an integer NULL must be `BigInt(None)`, not
        // `String(None)`. Column names carry the fallback for expression columns.
        let names: Vec<String> = (0..stmt.column_count())
            .map(|i| stmt.column_name(i).unwrap_or("").to_string())
            .collect();
        let decltypes: Vec<Option<String>> = stmt
            .columns()
            .iter()
            .map(|c| c.decl_type().map(str::to_ascii_uppercase))
            .collect();
        let mut rows = stmt
            .query(rusqlite::params_from_iter(binds.iter()))
            .map_err(|e| DbErr::Custom(e.to_string()))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(|e| DbErr::Custom(e.to_string()))? {
            let mut values = BTreeMap::new();
            for (i, name) in names.iter().enumerate() {
                let v: rusqlite::types::Value =
                    row.get(i).map_err(|e| DbErr::Custom(e.to_string()))?;
                let decl = decltypes.get(i).and_then(Option::as_deref);
                values.insert(name.clone(), rusqlite_to_sea(v, decl, name));
            }
            out.push(ProxyRow { values });
        }
        Ok(out)
    }

    async fn execute(&self, statement: Statement) -> std::result::Result<ProxyExecResult, DbErr> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| DbErr::Custom(format!("sqlite mutex poisoned: {e}")))?;
        let binds = statement_binds(&statement);
        conn.execute(&statement.sql, rusqlite::params_from_iter(binds.iter()))
            .map_err(|e| DbErr::Custom(e.to_string()))?;
        Ok(ProxyExecResult {
            last_insert_id: conn.last_insert_rowid() as u64,
            rows_affected: conn.changes(),
        })
    }

    async fn ping(&self) -> std::result::Result<(), DbErr> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| DbErr::Custom(format!("sqlite mutex poisoned: {e}")))?;
        conn.prepare("SELECT 1")
            .map_err(|e| DbErr::Custom(e.to_string()))?;
        Ok(())
    }
}

fn statement_binds(statement: &Statement) -> Vec<rusqlite::types::Value> {
    match &statement.values {
        Some(values) => values.0.iter().map(sea_to_rusqlite).collect(),
        None => Vec::new(),
    }
}

fn sea_to_rusqlite(v: &Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as R;
    match v {
        Value::Bool(Some(b)) => R::Integer(i64::from(*b)),
        Value::TinyInt(Some(n)) => R::Integer(i64::from(*n)),
        Value::SmallInt(Some(n)) => R::Integer(i64::from(*n)),
        Value::Int(Some(n)) => R::Integer(i64::from(*n)),
        Value::BigInt(Some(n)) => R::Integer(*n),
        Value::TinyUnsigned(Some(n)) => R::Integer(i64::from(*n)),
        Value::SmallUnsigned(Some(n)) => R::Integer(i64::from(*n)),
        Value::Unsigned(Some(n)) => R::Integer(i64::from(*n)),
        Value::BigUnsigned(Some(n)) => {
            i64::try_from(*n).map_or_else(|_| R::Real(*n as f64), R::Integer)
        }
        Value::Float(Some(n)) => R::Real(f64::from(*n)),
        Value::Double(Some(n)) => R::Real(*n),
        Value::String(Some(s)) => R::Text(s.to_string()),
        Value::Char(Some(c)) => R::Text(c.to_string()),
        Value::Bytes(Some(b)) => R::Blob(b.to_vec()),
        Value::ChronoDateTimeUtc(Some(dt)) => R::Text(dt.to_rfc3339()),
        Value::ChronoDateTime(Some(dt)) => R::Text(dt.and_utc().to_rfc3339()),
        _ => R::Null,
    }
}

fn rusqlite_to_sea(v: rusqlite::types::Value, decl_type: Option<&str>, column: &str) -> Value {
    match v {
        rusqlite::types::Value::Null => crate::db::typed_null(decl_type, column),
        rusqlite::types::Value::Integer(n) => Value::BigInt(Some(n)),
        rusqlite::types::Value::Real(n) => Value::Double(Some(n)),
        rusqlite::types::Value::Text(s) => Value::String(Some(s)),
        rusqlite::types::Value::Blob(b) => Value::Bytes(Some(b)),
    }
}
