//! Bookclerk SQL v1 types, CAST matrix, and fail-closed expression checking.
//!
//! Schema metadata for plugin-owned bindings is durable in
//! [`SQL_CATALOG_TABLE`]. This module only interprets canonical SQL; adapters
//! persist catalog rows at execute.

#![allow(clippy::missing_docs_in_private_items, clippy::missing_errors_doc)]

use crate::sql_proof::{
    IntegerArithKind, IntegerArithSite, PhysicalAccess, ResolvedAssignment, ResolvedStatement,
    SchemaAction, SqlSpan, TextCollateSite, PHYSICAL_STAR_COLUMN,
};
use crate::{
    DbType, DbValue, ExecuteReply, ExecuteRequest, PluginError, Result, StatementResult,
    TypedDbStatement, FIRST_PARTY_MAX_RESULT_ROWS,
};
use std::collections::{BTreeMap, BTreeSet};

/// Reserved catalog of `(table, column, sql_type)` for isolated bindings.
pub const SQL_CATALOG_TABLE: &str = "bookclerk_sql_catalog";

/// Reserved transactional identity high-water table (Postgres adapter-private).
pub const SQL_IDENTITY_TABLE: &str = "bookclerk_identity";

/// Internal alias used when wrapping `INSERT OR IGNORE … SELECT`.
pub const INSERT_SELECT_WRAP_ALIAS: &str = "_bc_src";

/// Portable bound for every canonical SQL v1 identifier after case fold.
///
/// PostgreSQL `NAMEDATALEN` is 64 including the NUL terminator, so 63 bytes
/// is the portable maximum for tables, columns, indexes, aliases, and CTEs.
pub const SQL_V1_MAX_IDENT_BYTES: usize = 63;

/// Table-level durable schema fingerprint (identity + constraints).
pub const SQL_SCHEMA_TABLE: &str = "bookclerk_sql_schema";

/// Admitted canonical `CREATE TABLE` / `CREATE INDEX` SQL for backup restore.
///
/// Fingerprints in [`SQL_SCHEMA_TABLE`] are not invertible. This table stores
/// the host-admitted canonical DDL so a plugin binding can be reconstructed
/// without reverse-engineering adapter catalogs.
pub const SQL_DDL_TABLE: &str = "bookclerk_sql_ddl";

/// Page size for durable catalog / schema SELECTs.
///
/// Uses the negotiated adapter `maxResultRows` when it is nonzero, then caps
/// at [`FIRST_PARTY_MAX_RESULT_ROWS`].
#[must_use]
pub fn sql_catalog_page_rows(max_result_rows: u32) -> u32 {
    let negotiated = if max_result_rows == 0 {
        FIRST_PARTY_MAX_RESULT_ROWS
    } else {
        max_result_rows
    };
    negotiated.clamp(1, FIRST_PARTY_MAX_RESULT_ROWS)
}

/// True when `message` is a missing-relation error for reserved `table`.
///
/// Matches SQLite `no such table: bookclerk_sql_catalog` (optional `main.`)
/// and PostgreSQL `relation "bookclerk_sql_catalog" does not exist`. Does not
/// match missing-column errors (`column "ordinal" does not exist`).
#[must_use]
pub fn reserved_catalog_relation_missing(message: &str, table: &str) -> bool {
    let msg = message.to_ascii_lowercase();
    let table = table.to_ascii_lowercase();
    let quoted = format!("\"{table}\"");
    let patterns = [
        format!("no such table: {table}"),
        format!("no such table: main.{table}"),
        format!("relation {quoted} does not exist"),
        format!("table {quoted} does not exist"),
        format!("undefined table: {table}"),
        format!("unknown table: {table}"),
    ];
    patterns.iter().any(|pat| {
        let Some(at) = msg.find(pat.as_str()) else {
            return false;
        };
        let after = at + pat.len();
        msg[after..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_')
    })
}

/// Requires a catalog page execute reply to contain exactly one statement.
///
/// Empty **rows** still mean end-of-catalog. An empty `statements` vec is not
/// EOF — adapters must fail closed rather than treat a dropped statement as
/// a finished scan.
///
/// # Errors
///
/// Returns when `reply.statements` is not length 1.
pub fn catalog_page_statement(reply: &ExecuteReply) -> Result<&StatementResult> {
    if reply.statements.len() != 1 {
        return Err(PluginError::internal(format!(
            "catalog page must return exactly one statement result; got {}",
            reply.statements.len()
        )));
    }
    Ok(&reply.statements[0])
}

/// Postgres adapter-private identity function prefix (`bc_if_<digest>`).
pub const POSTGRES_IDENT_FN_PREFIX: &str = "bc_if_";

/// Postgres adapter-private identity trigger prefix (`bc_it_<digest>`).
pub const POSTGRES_IDENT_TRIGGER_PREFIX: &str = "bc_it_";

/// SHA-256 hex digits used in adapter-private identity object names (64 bits).
const POSTGRES_IDENT_DIGEST_HEX: usize = 16;

/// True when `ident` (already case-folded) is a portable SQL v1 identifier.
#[must_use]
pub fn sql_v1_ident_in_bounds(ident: &str) -> bool {
    ident.len() <= SQL_V1_MAX_IDENT_BYTES
}

/// SHA-256 digest prefix of a folded table name for adapter-private objects.
#[must_use]
pub fn postgres_identity_object_digest(table: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(table.to_ascii_lowercase().as_bytes());
    hex::encode(&digest[..POSTGRES_IDENT_DIGEST_HEX / 2])
}

/// Postgres identity trigger function name for `table` (`bc_if_<digest>`).
#[must_use]
pub fn postgres_identity_function_name(table: &str) -> String {
    format!(
        "{POSTGRES_IDENT_FN_PREFIX}{}",
        postgres_identity_object_digest(table)
    )
}

/// Postgres identity trigger name for `table` (`bc_it_<digest>`).
#[must_use]
pub fn postgres_identity_trigger_name(table: &str) -> String {
    format!(
        "{POSTGRES_IDENT_TRIGGER_PREFIX}{}",
        postgres_identity_object_digest(table)
    )
}

/// SHA-256 hex of the exact canonical SQL string a [`ResolvedStatement`] proves.
#[must_use]
pub fn statement_sql_hash(sql: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(sql.as_bytes()))
}

/// Canonical SQL v1 column / expression type.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "camelCase")]
pub enum SqlType {
    /// `INTEGER` / `DbValue::Int64`.
    Integer,
    /// `REAL` / `DbValue::Float64`.
    Real,
    /// `TEXT` / `DbValue::Text`.
    Text,
    /// `BLOB` / `DbValue::Bytes`.
    Blob,
    /// `BOOLEAN` / `DbValue::Boolean`.
    Boolean,
    /// Untyped SQL `NULL` (unifies with any type).
    Null,
}

impl SqlType {
    /// Parses a canonical column type ident (already lowercased).
    #[must_use]
    pub fn from_column_ident(ty: &str) -> Option<Self> {
        match ty.to_ascii_lowercase().as_str() {
            "integer" | "bigint" | "int" | "int4" | "int8" | "smallint" => Some(Self::Integer),
            "real" | "float" | "double" => Some(Self::Real),
            "text" | "varchar" => Some(Self::Text),
            "blob" | "bytea" => Some(Self::Blob),
            "boolean" | "bool" => Some(Self::Boolean),
            _ => None,
        }
    }

    /// Catalog / docs spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Integer => "integer",
            Self::Real => "real",
            Self::Text => "text",
            Self::Blob => "blob",
            Self::Boolean => "boolean",
            Self::Null => "null",
        }
    }

    /// True when this type is TEXT.
    #[must_use]
    pub const fn is_text(self) -> bool {
        matches!(self, Self::Text)
    }

    /// True when this type is INTEGER or REAL.
    #[must_use]
    pub const fn is_numeric(self) -> bool {
        matches!(self, Self::Integer | Self::Real)
    }
}

impl From<DbType> for SqlType {
    fn from(ty: DbType) -> Self {
        match ty {
            DbType::Int64 => Self::Integer,
            DbType::Float64 => Self::Real,
            DbType::Text => Self::Text,
            DbType::Bytes => Self::Blob,
            DbType::Bool => Self::Boolean,
            DbType::Unspecified => Self::Null,
        }
    }
}

/// `(table → ordered columns)` environment used to *build* a proof.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SqlTypeEnv {
    tables: BTreeMap<String, Vec<(String, SqlType)>>,
    /// Structured fingerprints keyed by folded table name.
    fingerprints: BTreeMap<String, String>,
    /// Identity column per table, if any.
    identities: BTreeMap<String, String>,
}

impl SqlTypeEnv {
    /// Empty environment.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// True when no tables are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    /// True when `table` is in the environment.
    #[must_use]
    pub fn has_table(&self, table: &str) -> bool {
        self.tables.contains_key(&table.to_ascii_lowercase())
    }

    /// Declared type of `table.column`, if present.
    #[must_use]
    pub fn column_type(&self, table: &str, column: &str) -> Option<SqlType> {
        let col = column.to_ascii_lowercase();
        self.tables
            .get(&table.to_ascii_lowercase())?
            .iter()
            .find(|(n, _)| *n == col)
            .map(|(_, t)| *t)
    }

    /// Columns of `table` in declaration order.
    #[must_use]
    pub fn table_columns(&self, table: &str) -> Option<&[(String, SqlType)]> {
        self.tables
            .get(&table.to_ascii_lowercase())
            .map(Vec::as_slice)
    }

    /// Stored structured fingerprint for `table`.
    #[must_use]
    pub fn fingerprint(&self, table: &str) -> Option<&str> {
        self.fingerprints
            .get(&table.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// Identity column for `table`.
    #[must_use]
    pub fn identity_column(&self, table: &str) -> Option<&str> {
        self.identities
            .get(&table.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// Inserts or replaces columns for `table`.
    pub fn insert_table(
        &mut self,
        table: impl Into<String>,
        columns: impl IntoIterator<Item = (String, SqlType)>,
    ) {
        self.insert_table_schema(table, columns, None, String::new());
    }

    /// Inserts table columns plus fingerprint / identity metadata.
    pub fn insert_table_schema(
        &mut self,
        table: impl Into<String>,
        columns: impl IntoIterator<Item = (String, SqlType)>,
        identity: Option<String>,
        fingerprint: String,
    ) {
        let table = table.into().to_ascii_lowercase();
        let cols: Vec<(String, SqlType)> = columns
            .into_iter()
            .map(|(n, t)| (n.to_ascii_lowercase(), t))
            .collect();
        self.tables.insert(table.clone(), cols);
        if let Some(id) = identity {
            self.identities
                .insert(table.clone(), id.to_ascii_lowercase());
        } else {
            self.identities.remove(&table);
        }
        if fingerprint.is_empty() {
            self.fingerprints.remove(&table);
        } else {
            self.fingerprints.insert(table, fingerprint);
        }
    }

    /// Drops `table` from the environment.
    pub fn drop_table(&mut self, table: &str) {
        let table = table.to_ascii_lowercase();
        self.tables.remove(&table);
        self.fingerprints.remove(&table);
        self.identities.remove(&table);
    }

    /// Renames a table (used for rebuild `ALTER TABLE … RENAME TO`).
    pub fn rename_table(&mut self, from: &str, to: &str) {
        let from = from.to_ascii_lowercase();
        let to = to.to_ascii_lowercase();
        if let Some(cols) = self.tables.remove(&from) {
            self.tables.insert(to.clone(), cols);
        }
        if let Some(fp) = self.fingerprints.remove(&from) {
            self.fingerprints.insert(to.clone(), fp);
        }
        if let Some(id) = self.identities.remove(&from) {
            self.identities.insert(to, id);
        }
    }

    /// Merges `other` (later entries win per table).
    pub fn merge(&mut self, other: &Self) {
        for (table, cols) in &other.tables {
            self.tables.insert(table.clone(), cols.clone());
        }
        self.fingerprints.extend(other.fingerprints.clone());
        self.identities.extend(other.identities.clone());
    }

    /// Records one catalog row, appending when the column is new.
    pub fn insert_column(&mut self, table: &str, column: &str, ty: SqlType) {
        let table = table.to_ascii_lowercase();
        let column = column.to_ascii_lowercase();
        let cols = self.tables.entry(table).or_default();
        if let Some(existing) = cols.iter_mut().find(|(n, _)| *n == column) {
            existing.1 = ty;
        } else {
            cols.push((column, ty));
        }
    }

    /// Iterates `(table, column, type)` in table order then declaration order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str, SqlType)> {
        self.tables.iter().flat_map(|(t, cols)| {
            cols.iter()
                .map(move |(c, ty)| (t.as_str(), c.as_str(), *ty))
        })
    }
}

/// Parsed `CREATE TABLE` column list (canonical SQL v1).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTableSchema {
    /// Unquoted folded table name.
    pub table: String,
    /// Column names and types in declaration order.
    pub columns: Vec<(String, SqlType)>,
    /// `INTEGER PRIMARY KEY AUTOINCREMENT` column, if any.
    pub identity_column: Option<String>,
    /// Per-column NOT NULL flags (same order as `columns`).
    pub column_not_null: Vec<bool>,
    /// Per-column UNIQUE flags.
    pub column_unique: Vec<bool>,
    /// Per-column PRIMARY KEY flags (inline).
    pub column_primary_key: Vec<bool>,
    /// Per-column DEFAULT SQL (empty if none).
    pub column_defaults: Vec<String>,
    /// Per-column CHECK SQL (empty if none).
    pub column_checks: Vec<String>,
    /// Per-column `REFERENCES` targets (same order as `columns`).
    pub column_references: Vec<Option<ColumnReference>>,
    /// Table-level constraints in declaration order.
    pub table_constraints: Vec<TableConstraint>,
}

/// Column-level `REFERENCES table [(cols…)]` captured in the structured schema IR.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnReference {
    /// Referenced table.
    pub ref_table: String,
    /// Referenced columns (empty if omitted).
    pub ref_columns: Vec<String>,
}

/// Table-level constraint captured in the structured schema IR.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TableConstraint {
    /// `PRIMARY KEY (cols…)`.
    PrimaryKey(Vec<String>),
    /// `UNIQUE (cols…)`.
    Unique(Vec<String>),
    /// `CHECK (expr)`.
    Check(String),
    /// `FOREIGN KEY (cols) REFERENCES table (refcols)`.
    ForeignKey {
        /// Local columns.
        columns: Vec<String>,
        /// Referenced table.
        ref_table: String,
        /// Referenced columns (empty if omitted).
        ref_columns: Vec<String>,
    },
}

/// Parsed `CREATE [UNIQUE] INDEX` (canonical SQL v1).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateIndexSchema {
    /// Unquoted folded index name.
    pub name: String,
    /// Table the index is on.
    pub table: String,
    /// True when the statement is `CREATE UNIQUE INDEX`.
    pub unique: bool,
    /// Indexed columns in declaration order (`ident [ASC|DESC]`, no expressions).
    pub columns: Vec<String>,
    /// Optional partial-index `WHERE` predicate (no `WHERE` keyword).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub where_sql: Option<String>,
    /// Canonical statement text (no trailing semicolon).
    pub canonical_sql: String,
}

impl CreateTableSchema {
    /// Structured fingerprint (SHA-256 hex) of this schema IR.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(b"sql-v1-schema\0");
        h.update(self.table.as_bytes());
        h.update(b"\0");
        for (i, (name, ty)) in self.columns.iter().enumerate() {
            h.update(name.as_bytes());
            h.update(b"\0");
            h.update(ty.as_str().as_bytes());
            h.update(b"\0");
            h.update([u8::from(
                self.column_not_null.get(i).copied().unwrap_or(false),
            )]);
            h.update([u8::from(
                self.column_unique.get(i).copied().unwrap_or(false),
            )]);
            h.update([u8::from(
                self.column_primary_key.get(i).copied().unwrap_or(false),
            )]);
            h.update([u8::from(
                self.identity_column.as_deref() == Some(name.as_str()),
            )]);
            h.update(
                self.column_defaults
                    .get(i)
                    .map(String::as_str)
                    .unwrap_or("")
                    .as_bytes(),
            );
            h.update(b"\0");
            h.update(
                self.column_checks
                    .get(i)
                    .map(String::as_str)
                    .unwrap_or("")
                    .as_bytes(),
            );
            h.update(b"\0");
            match self.column_references.get(i).and_then(|r| r.as_ref()) {
                Some(r) => {
                    h.update(b"cfk\0");
                    h.update(r.ref_table.as_bytes());
                    h.update(b"\0");
                    for col in &r.ref_columns {
                        h.update(col.as_bytes());
                        h.update(b"\0");
                    }
                }
                None => h.update(b"cfk\0\0"),
            }
        }
        for c in &self.table_constraints {
            match c {
                TableConstraint::PrimaryKey(cols) => {
                    h.update(b"pk\0");
                    for col in cols {
                        h.update(col.as_bytes());
                        h.update(b"\0");
                    }
                }
                TableConstraint::Unique(cols) => {
                    h.update(b"uq\0");
                    for col in cols {
                        h.update(col.as_bytes());
                        h.update(b"\0");
                    }
                }
                TableConstraint::Check(expr) => {
                    h.update(b"ck\0");
                    h.update(expr.as_bytes());
                    h.update(b"\0");
                }
                TableConstraint::ForeignKey {
                    columns,
                    ref_table,
                    ref_columns,
                } => {
                    h.update(b"fk\0");
                    for col in columns {
                        h.update(col.as_bytes());
                        h.update(b"\0");
                    }
                    h.update(ref_table.as_bytes());
                    h.update(b"\0");
                    for col in ref_columns {
                        h.update(col.as_bytes());
                        h.update(b"\0");
                    }
                }
            }
        }
        hex::encode(h.finalize())
    }

    /// Foreign-key parent table names from column-level and table-level IR.
    ///
    /// Does not scan `CREATE TABLE` text, so `REFERENCES` inside DEFAULT/CHECK
    /// string literals is not a dependency.
    #[must_use]
    pub fn referenced_tables(&self) -> Vec<String> {
        let mut out = Vec::new();
        for r in self.column_references.iter().flatten() {
            out.push(r.ref_table.clone());
        }
        for c in &self.table_constraints {
            if let TableConstraint::ForeignKey { ref_table, .. } = c {
                out.push(ref_table.clone());
            }
        }
        out
    }
}

/// Parses a canonical `CREATE TABLE` into columns / identity, if the head matches.
#[must_use]
pub fn parse_create_table_schema(sql: &str) -> Option<CreateTableSchema> {
    let mut s = skip_ws(sql);
    s = skip_kw(s, "CREATE")?;
    if starts_kw(s, "TEMP") {
        s = skip_kw(s, "TEMP")?;
    } else if starts_kw(s, "TEMPORARY") {
        s = skip_kw(s, "TEMPORARY")?;
    }
    s = skip_kw(s, "TABLE")?;
    if starts_kw(s, "IF") {
        s = skip_kw(s, "IF")?;
        if starts_kw(s, "NOT") {
            s = skip_kw(s, "NOT")?;
        }
        s = skip_kw(s, "EXISTS")?;
    }
    let (table, rest) = read_ident(s)?;
    let rest = skip_ws(rest);
    if !rest.starts_with('(') {
        return None;
    }
    let inner = balanced_inner(rest)?;
    let mut columns = Vec::new();
    let mut identity_column = None;
    let mut column_not_null = Vec::new();
    let mut column_unique = Vec::new();
    let mut column_primary_key = Vec::new();
    let mut column_defaults = Vec::new();
    let mut column_checks = Vec::new();
    let mut column_references = Vec::new();
    let mut table_constraints = Vec::new();
    for def in split_top_level_commas(inner) {
        let def = skip_ws(def);
        let constraint_head = if starts_kw(def, "CONSTRAINT") {
            let Some((_, rest)) = read_ident(skip_ws(skip_kw(def, "CONSTRAINT")?)) else {
                continue;
            };
            skip_ws(rest)
        } else {
            def
        };
        if let Some(c) = parse_table_constraint_def(constraint_head) {
            table_constraints.push(c);
            continue;
        }
        let (name, rest) = read_ident(def)?;
        let rest = skip_ws(rest);
        let (ty_ident, rest) = read_ident(rest)?;
        let ty = SqlType::from_column_ident(&ty_ident)?;
        let flags = parse_column_constraint_flags(rest);
        if identity_column.is_none() && flags.identity && ty == SqlType::Integer {
            identity_column = Some(name.clone());
        }
        columns.push((name, ty));
        column_not_null.push(flags.not_null);
        column_unique.push(flags.unique);
        column_primary_key.push(flags.primary_key);
        column_defaults.push(flags.default_sql);
        column_checks.push(flags.check_sql);
        column_references.push(
            parse_column_reference(rest).map(|(ref_table, ref_columns)| ColumnReference {
                ref_table,
                ref_columns,
            }),
        );
    }
    if columns.is_empty() {
        return None;
    }
    Some(CreateTableSchema {
        table,
        columns,
        identity_column,
        column_not_null,
        column_unique,
        column_primary_key,
        column_defaults,
        column_checks,
        column_references,
        table_constraints,
    })
}

struct ColumnFlags {
    not_null: bool,
    unique: bool,
    primary_key: bool,
    identity: bool,
    default_sql: String,
    check_sql: String,
}

fn parse_column_constraint_flags(rest: &str) -> ColumnFlags {
    let u = rest.to_ascii_uppercase();
    let identity = u.contains("PRIMARY") && u.contains("KEY") && u.contains("AUTOINCREMENT");
    let primary_key = u.contains("PRIMARY") && u.contains("KEY");
    let not_null = u.contains("NOT NULL") || primary_key;
    let unique = u.contains("UNIQUE");
    let default_sql = extract_keyword_arg(rest, "DEFAULT").unwrap_or_default();
    let check_sql = extract_paren_after_kw(rest, "CHECK").unwrap_or_default();
    ColumnFlags {
        not_null,
        unique,
        primary_key,
        identity,
        default_sql,
        check_sql,
    }
}

/// Column-level `REFERENCES` outside DEFAULT/CHECK string literals.
fn parse_column_reference(rest: &str) -> Option<(String, Vec<String>)> {
    let mut scan = rest;
    loop {
        scan = skip_ws(scan);
        if scan.is_empty() {
            return None;
        }
        if starts_kw(scan, "REFERENCES") {
            let after = skip_kw(scan, "REFERENCES")?;
            let (table, after) = read_ident(after)?;
            let ref_columns = if skip_ws(after).starts_with('(') {
                ident_list(balanced_inner(skip_ws(after))?)
            } else {
                Vec::new()
            };
            return Some((table, ref_columns));
        }
        if starts_kw(scan, "DEFAULT") {
            scan = skip_kw(scan, "DEFAULT")?;
            scan = skip_ws(scan);
            if scan.starts_with('\'') {
                scan = skip_sql_string(scan)?;
            } else if scan.starts_with('(') {
                let inner = balanced_inner(scan)?;
                scan = scan.get(inner.len() + 2..).unwrap_or("");
            } else if let Some((_, rest)) = read_ident(scan) {
                scan = rest;
            } else {
                let end = scan
                    .find(|c: char| c.is_whitespace() || c == ',')
                    .unwrap_or(scan.len());
                scan = scan.get(end..).unwrap_or("");
            }
            continue;
        }
        if starts_kw(scan, "CHECK") {
            scan = skip_kw(scan, "CHECK")?;
            scan = skip_ws(scan);
            let inner = balanced_inner(scan)?;
            scan = scan.get(inner.len() + 2..).unwrap_or("");
            continue;
        }
        if starts_kw(scan, "CONSTRAINT") {
            scan = skip_kw(scan, "CONSTRAINT")?;
            let (_, rest) = read_ident(scan)?;
            scan = rest;
            continue;
        }
        if let Some((_, rest)) = read_ident(scan) {
            scan = rest;
            continue;
        }
        if scan.starts_with('(') {
            let inner = balanced_inner(scan)?;
            scan = scan.get(inner.len() + 2..).unwrap_or("");
            continue;
        }
        if scan.starts_with('\'') {
            scan = skip_sql_string(scan)?;
            continue;
        }
        scan = scan.get(1..).unwrap_or("");
    }
}

fn skip_sql_string(s: &str) -> Option<&str> {
    if !s.starts_with('\'') {
        return None;
    }
    let bytes = s.as_bytes();
    let mut i = 1;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if bytes.get(i + 1) == Some(&b'\'') {
                i += 2;
                continue;
            }
            return Some(&s[i + 1..]);
        }
        i += 1;
    }
    None
}

fn extract_keyword_arg(s: &str, kw: &str) -> Option<String> {
    let mut scan = s;
    loop {
        scan = skip_ws(scan);
        if scan.is_empty() {
            return None;
        }
        if starts_kw(scan, kw) {
            scan = skip_kw(scan, kw)?;
            scan = skip_ws(scan);
            if scan.starts_with('(') {
                return balanced_inner(scan).map(str::trim).map(str::to_string);
            }
            let end = scan
                .find(|c: char| c.is_whitespace() || c == ',')
                .unwrap_or(scan.len());
            return Some(scan[..end].trim().to_ascii_lowercase());
        }
        let Some((_, rest)) = read_ident(scan) else {
            if scan.starts_with('(') {
                scan = &scan[balanced_inner(scan).map(|i| i.len() + 2).unwrap_or(1)..];
                continue;
            }
            scan = scan.get(1..).unwrap_or("");
            continue;
        };
        scan = rest;
    }
}

fn extract_paren_after_kw(s: &str, kw: &str) -> Option<String> {
    let mut scan = s;
    loop {
        scan = skip_ws(scan);
        if scan.is_empty() {
            return None;
        }
        if starts_kw(scan, kw) {
            scan = skip_kw(scan, kw)?;
            scan = skip_ws(scan);
            return balanced_inner(scan).map(str::trim).map(str::to_string);
        }
        let Some((_, rest)) = read_ident(scan) else {
            if scan.starts_with('(') {
                let inner_len = balanced_inner(scan)?.len();
                scan = scan.get(inner_len + 2..).unwrap_or("");
                continue;
            }
            scan = scan.get(1..).unwrap_or("");
            continue;
        };
        scan = rest;
    }
}

fn parse_table_constraint_def(def: &str) -> Option<TableConstraint> {
    let def = skip_ws(def);
    if starts_kw(def, "PRIMARY") {
        let rest = skip_kw(def, "PRIMARY")?;
        let rest = skip_kw(rest, "KEY")?;
        let inner = balanced_inner(skip_ws(rest))?;
        return Some(TableConstraint::PrimaryKey(ident_list(inner)));
    }
    if starts_kw(def, "UNIQUE") {
        let rest = skip_kw(def, "UNIQUE")?;
        let inner = balanced_inner(skip_ws(rest))?;
        return Some(TableConstraint::Unique(ident_list(inner)));
    }
    if starts_kw(def, "CHECK") {
        let rest = skip_kw(def, "CHECK")?;
        let inner = balanced_inner(skip_ws(rest))?;
        return Some(TableConstraint::Check(inner.trim().to_string()));
    }
    if starts_kw(def, "FOREIGN") {
        let rest = skip_kw(def, "FOREIGN")?;
        let rest = skip_kw(rest, "KEY")?;
        let inner = balanced_inner(skip_ws(rest))?;
        let columns = ident_list(inner);
        let after = skip_ws(&skip_ws(rest)[inner.len() + 2..]);
        let after = skip_kw(after, "REFERENCES")?;
        let (ref_table, after) = read_ident(after)?;
        let ref_columns = if skip_ws(after).starts_with('(') {
            ident_list(balanced_inner(skip_ws(after))?)
        } else {
            Vec::new()
        };
        return Some(TableConstraint::ForeignKey {
            columns,
            ref_table,
            ref_columns,
        });
    }
    None
}

fn ident_list(s: &str) -> Vec<String> {
    split_top_level_commas(s)
        .into_iter()
        .filter_map(|p| read_ident(p).map(|(n, _)| n))
        .collect()
}

/// Parses `DROP TABLE [IF EXISTS] name`.
#[must_use]
pub fn parse_drop_table_name(sql: &str) -> Option<String> {
    let mut s = skip_ws(sql);
    s = skip_kw(s, "DROP")?;
    s = skip_kw(s, "TABLE")?;
    if starts_kw(s, "IF") {
        s = skip_kw(s, "IF")?;
        s = skip_kw(s, "EXISTS")?;
    }
    let (name, _) = read_ident(s)?;
    Some(name)
}

/// Parses `CREATE [UNIQUE] INDEX [IF NOT EXISTS] name ON table (cols…) [WHERE …]`.
///
/// The column list and any supported tail must be complete SQL v1. Trailing
/// tokens (`USING`, `INCLUDE`, `COLLATE`, or garbage after `ON table`) fail.
#[must_use]
pub fn parse_create_index_sql(sql: &str) -> Option<CreateIndexSchema> {
    let mut s = skip_ws(sql);
    s = skip_kw(s, "CREATE")?;
    let unique = if starts_kw(s, "UNIQUE") {
        s = skip_kw(s, "UNIQUE")?;
        true
    } else {
        false
    };
    s = skip_kw(s, "INDEX")?;
    if starts_kw(s, "IF") {
        s = skip_kw(s, "IF")?;
        s = skip_kw(s, "NOT")?;
        s = skip_kw(s, "EXISTS")?;
    }
    let (name, rest) = read_ident(s)?;
    let rest = skip_kw(skip_ws(rest), "ON")?;
    let (table, rest) = read_ident(rest)?;
    let rest = skip_ws(rest);
    if !rest.starts_with('(') {
        return None;
    }
    let inner = balanced_inner(rest)?;
    let columns = parse_index_column_list(inner)?;
    let after = skip_ws(rest.get(inner.len() + 2..).unwrap_or(""));
    let after = after.trim_end_matches(';').trim_end();
    let where_sql = if after.is_empty() {
        None
    } else if !starts_kw(after, "WHERE") {
        return None;
    } else {
        let pred = skip_ws(skip_kw(after, "WHERE")?);
        let pred = pred.trim_end_matches(';').trim();
        if pred.is_empty() {
            return None;
        }
        Some(pred.to_string())
    };
    Some(CreateIndexSchema {
        name,
        table,
        unique,
        columns,
        where_sql,
        canonical_sql: sql.trim().trim_end_matches(';').trim().to_string(),
    })
}

/// Column list of `CREATE INDEX … (ident [ASC|DESC], …)`. Leftover tokens fail.
fn parse_index_column_list(inner: &str) -> Option<Vec<String>> {
    let parts = split_top_level_commas(inner);
    if parts.is_empty() {
        return None;
    }
    let mut cols = Vec::new();
    for part in parts {
        let (name, rest) = read_ident(part)?;
        let rest = skip_ws(rest);
        let rest = if starts_kw(rest, "ASC") {
            skip_ws(skip_kw(rest, "ASC")?)
        } else if starts_kw(rest, "DESC") {
            skip_ws(skip_kw(rest, "DESC")?)
        } else {
            rest
        };
        if !rest.is_empty() {
            return None;
        }
        cols.push(name);
    }
    Some(cols)
}

/// Parses `DROP INDEX [IF EXISTS] name`.
#[must_use]
pub fn parse_drop_index_name(sql: &str) -> Option<String> {
    let mut s = skip_ws(sql);
    s = skip_kw(s, "DROP")?;
    s = skip_kw(s, "INDEX")?;
    if starts_kw(s, "IF") {
        s = skip_kw(s, "IF")?;
        s = skip_kw(s, "EXISTS")?;
    }
    let (name, _) = read_ident(s)?;
    Some(name)
}

/// `CREATE TABLE IF NOT EXISTS` for the reserved SQL v1 catalog.
#[must_use]
pub fn sql_catalog_create_table_sql() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {SQL_CATALOG_TABLE} (\
         table_name TEXT NOT NULL, column_name TEXT NOT NULL, sql_type TEXT NOT NULL, \
         ordinal INTEGER NOT NULL, is_identity INTEGER NOT NULL, default_sql TEXT NOT NULL, \
         PRIMARY KEY (table_name, column_name))"
    )
}

/// Table-level fingerprint companion table.
#[must_use]
pub fn sql_schema_create_table_sql() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {SQL_SCHEMA_TABLE} (\
         table_name TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, identity_column TEXT NOT NULL)"
    )
}

/// Durable canonical DDL companion table.
#[must_use]
pub fn sql_ddl_create_table_sql() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {SQL_DDL_TABLE} (\
         kind TEXT NOT NULL, name TEXT NOT NULL, table_name TEXT NOT NULL, \
         canonical_sql TEXT NOT NULL, PRIMARY KEY (kind, name))"
    )
}

/// Catalog DML companions for one canonical DDL statement (all backends).
///
/// Callers must skip this when [`SchemaAction::Create`] is a fingerprint no-op.
#[must_use]
pub fn catalog_companions(sql: &str) -> Vec<String> {
    catalog_companions_for_action(sql, None)
}

/// Catalog companions gated by a resolved schema action.
///
/// When `action` carries a [`SchemaAction::Create`] schema, companions are
/// built from that proof (no CREATE reparse). Callers without a proof still
/// parse canonical SQL. [`SchemaAction::None`] still persists `CREATE INDEX`
/// / `DROP INDEX` so backups can rebuild admitted indexes. A fingerprint
/// matching [`SchemaAction::Create`] no-op emits no companions: the first
/// admitted CREATE already persisted complete catalog DDL, and a binding
/// missing that catalog fails closed.
#[must_use]
pub fn catalog_companions_for_action(sql: &str, action: Option<&SchemaAction>) -> Vec<String> {
    match action {
        Some(SchemaAction::Create { noop: true, .. }) => return Vec::new(),
        Some(SchemaAction::Create {
            schema,
            fingerprint,
            noop: false,
        }) => {
            return catalog_dml_for_create(schema, fingerprint, sql);
        }
        Some(SchemaAction::Drop { table }) => return catalog_dml_for_drop(table),
        Some(SchemaAction::None) | None => {}
    }
    if let Some(schema) = parse_create_table_schema(sql) {
        let fingerprint = schema.fingerprint();
        return catalog_dml_for_create(&schema, &fingerprint, sql);
    }
    if let Some(table) = parse_drop_table_name(sql) {
        return catalog_dml_for_drop(&table);
    }
    if let Some(index) = parse_create_index_sql(sql) {
        return catalog_dml_for_create_index(&index);
    }
    if let Some(name) = parse_drop_index_name(sql) {
        return catalog_dml_for_drop_index(&name);
    }
    Vec::new()
}

fn catalog_ensure_tables() -> Vec<String> {
    vec![
        sql_catalog_create_table_sql(),
        sql_schema_create_table_sql(),
        sql_ddl_create_table_sql(),
    ]
}

/// Host-managed catalog/identity tables are not guest-owned plugin schema.
fn is_reserved_sql_catalog_ident(name: &str) -> bool {
    matches!(
        name,
        SQL_CATALOG_TABLE | SQL_SCHEMA_TABLE | SQL_DDL_TABLE | SQL_IDENTITY_TABLE
    )
}

fn catalog_dml_for_create(schema: &CreateTableSchema, fingerprint: &str, sql: &str) -> Vec<String> {
    if schema.columns.is_empty() || is_reserved_sql_catalog_ident(&schema.table) {
        return Vec::new();
    }
    let ident = schema.identity_column.clone().unwrap_or_default();
    let mut out = catalog_ensure_tables();
    out.push(catalog_insert_columns_sql(schema));
    out.push(catalog_insert_schema_sql(
        &schema.table,
        fingerprint,
        &ident,
    ));
    out.extend(catalog_insert_ddl_sql(
        "table",
        &schema.table,
        &schema.table,
        sql,
    ));
    out
}

fn catalog_dml_for_drop(table: &str) -> Vec<String> {
    if is_reserved_sql_catalog_ident(table) {
        return Vec::new();
    }
    let mut out = catalog_ensure_tables();
    out.push(catalog_delete_sql(SQL_CATALOG_TABLE, table));
    out.push(catalog_delete_sql(SQL_SCHEMA_TABLE, table));
    out.push(catalog_delete_ddl_for_table_sql(table));
    out
}

fn catalog_dml_for_create_index(index: &CreateIndexSchema) -> Vec<String> {
    if is_reserved_sql_catalog_ident(&index.table) {
        return Vec::new();
    }
    let mut out = catalog_ensure_tables();
    out.extend(catalog_insert_ddl_sql(
        "index",
        &index.name,
        &index.table,
        &index.canonical_sql,
    ));
    out
}

fn catalog_dml_for_drop_index(name: &str) -> Vec<String> {
    let mut out = catalog_ensure_tables();
    out.push(catalog_delete_ddl_kind_name_sql("index", name));
    out
}

#[cfg(feature = "host")]
fn catalog_insert_columns_sql(schema: &CreateTableSchema) -> String {
    let mut insert = sea_query::Query::insert();
    insert.into_table(SQL_CATALOG_TABLE).columns([
        "table_name",
        "column_name",
        "sql_type",
        "ordinal",
        "is_identity",
        "default_sql",
    ]);
    for (i, (col, ty)) in schema.columns.iter().enumerate() {
        let ident = i32::from(schema.identity_column.as_deref() == Some(col.as_str()));
        let default_sql = schema
            .column_defaults
            .get(i)
            .map(String::as_str)
            .unwrap_or("");
        insert.values_panic([
            schema.table.clone().into(),
            col.clone().into(),
            ty.as_str().into(),
            i64::try_from(i).unwrap_or(i64::MAX).into(),
            i64::from(ident).into(),
            default_sql.into(),
        ]);
    }
    insert.to_string(sea_query::SqliteQueryBuilder)
}

#[cfg(not(feature = "host"))]
fn catalog_insert_columns_sql(schema: &CreateTableSchema) -> String {
    let values = schema
        .columns
        .iter()
        .enumerate()
        .map(|(i, (col, ty))| {
            let ident = i32::from(schema.identity_column.as_deref() == Some(col.as_str()));
            let default_sql = schema
                .column_defaults
                .get(i)
                .map(String::as_str)
                .unwrap_or("");
            format!(
                "('{}', '{}', '{}', {}, {ident}, '{}')",
                escape_sql_str(&schema.table),
                escape_sql_str(col),
                ty.as_str(),
                i,
                escape_sql_str(default_sql)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "INSERT INTO {SQL_CATALOG_TABLE} \
         (table_name, column_name, sql_type, ordinal, is_identity, default_sql) \
         VALUES {values}"
    )
}

#[cfg(feature = "host")]
fn catalog_insert_schema_sql(table: &str, fingerprint: &str, ident: &str) -> String {
    sea_query::Query::insert()
        .into_table(SQL_SCHEMA_TABLE)
        .columns(["table_name", "fingerprint", "identity_column"])
        .values_panic([table.into(), fingerprint.into(), ident.into()])
        .to_string(sea_query::SqliteQueryBuilder)
}

#[cfg(not(feature = "host"))]
fn catalog_insert_schema_sql(table: &str, fingerprint: &str, ident: &str) -> String {
    format!(
        "INSERT INTO {SQL_SCHEMA_TABLE} (table_name, fingerprint, identity_column) \
         VALUES ('{}', '{}', '{}')",
        escape_sql_str(table),
        escape_sql_str(fingerprint),
        escape_sql_str(ident)
    )
}

#[cfg(feature = "host")]
fn catalog_delete_sql(catalog_table: &str, table: &str) -> String {
    sea_query::Query::delete()
        .from_table(sea_query::Alias::new(catalog_table))
        .and_where(sea_query::ExprTrait::eq(
            sea_query::Expr::col("table_name"),
            table,
        ))
        .to_string(sea_query::SqliteQueryBuilder)
}

#[cfg(not(feature = "host"))]
fn catalog_delete_sql(catalog_table: &str, table: &str) -> String {
    format!(
        "DELETE FROM {catalog_table} WHERE table_name = '{}'",
        escape_sql_str(table)
    )
}

fn catalog_insert_ddl_sql(kind: &str, name: &str, table_name: &str, sql: &str) -> Vec<String> {
    let canonical = sql.trim().trim_end_matches(';').trim().to_string();
    vec![
        catalog_delete_ddl_kind_name_sql(kind, name),
        catalog_insert_ddl_row_sql(kind, name, table_name, &canonical),
    ]
}

#[cfg(feature = "host")]
fn catalog_insert_ddl_row_sql(kind: &str, name: &str, table_name: &str, sql: &str) -> String {
    sea_query::Query::insert()
        .into_table(SQL_DDL_TABLE)
        .columns(["kind", "name", "table_name", "canonical_sql"])
        .values_panic([kind.into(), name.into(), table_name.into(), sql.into()])
        .to_string(sea_query::SqliteQueryBuilder)
}

#[cfg(not(feature = "host"))]
fn catalog_insert_ddl_row_sql(kind: &str, name: &str, table_name: &str, sql: &str) -> String {
    format!(
        "INSERT INTO {SQL_DDL_TABLE} (kind, name, table_name, canonical_sql) \
         VALUES ('{}', '{}', '{}', '{}')",
        escape_sql_str(kind),
        escape_sql_str(name),
        escape_sql_str(table_name),
        escape_sql_str(sql)
    )
}

fn catalog_delete_ddl_for_table_sql(table: &str) -> String {
    #[cfg(feature = "host")]
    {
        sea_query::Query::delete()
            .from_table(sea_query::Alias::new(SQL_DDL_TABLE))
            .and_where(sea_query::ExprTrait::eq(
                sea_query::Expr::col("table_name"),
                table,
            ))
            .to_string(sea_query::SqliteQueryBuilder)
    }
    #[cfg(not(feature = "host"))]
    {
        format!(
            "DELETE FROM {SQL_DDL_TABLE} WHERE table_name = '{}'",
            escape_sql_str(table)
        )
    }
}

fn catalog_delete_ddl_kind_name_sql(kind: &str, name: &str) -> String {
    #[cfg(feature = "host")]
    {
        sea_query::Query::delete()
            .from_table(sea_query::Alias::new(SQL_DDL_TABLE))
            .and_where(sea_query::ExprTrait::eq(sea_query::Expr::col("kind"), kind))
            .and_where(sea_query::ExprTrait::eq(sea_query::Expr::col("name"), name))
            .to_string(sea_query::SqliteQueryBuilder)
    }
    #[cfg(not(feature = "host"))]
    {
        format!(
            "DELETE FROM {SQL_DDL_TABLE} WHERE kind = '{}' AND name = '{}'",
            escape_sql_str(kind),
            escape_sql_str(name)
        )
    }
}

/// Applies a resolved schema action to `env` without reparsing CREATE SQL.
pub fn apply_schema_action_to_env(env: &mut SqlTypeEnv, action: &SchemaAction) {
    match action {
        SchemaAction::Create { schema, noop, .. } => {
            if *noop || env.has_table(&schema.table) {
                return;
            }
            env.insert_table_schema(
                schema.table.clone(),
                schema.columns.iter().cloned(),
                schema.identity_column.clone(),
                schema.fingerprint(),
            );
        }
        SchemaAction::Drop { table } => env.drop_table(table),
        SchemaAction::None => {}
    }
}

/// Applies one statement's `CREATE`/`DROP TABLE` effects to `env`.
pub fn apply_schema_sql_to_env(env: &mut SqlTypeEnv, sql: &str) {
    if let Some(schema) = parse_create_table_schema(sql) {
        if env.has_table(&schema.table) {
            return;
        }
        env.insert_table_schema(
            schema.table.clone(),
            schema.columns.iter().cloned(),
            schema.identity_column.clone(),
            schema.fingerprint(),
        );
    } else if let Some(table) = parse_drop_table_name(sql) {
        env.drop_table(&table);
    } else if let Some((from, to)) = parse_alter_rename_table(sql) {
        env.rename_table(&from, &to);
    } else {
        apply_alter_add_column(env, sql);
    }
}

fn apply_alter_add_column(env: &mut SqlTypeEnv, sql: &str) {
    let Some((table, column, ty)) = parse_alter_add_column(sql) else {
        return;
    };
    if !env.has_table(&table) {
        return;
    }
    env.insert_column(&table, &column, ty);
}

fn parse_alter_add_column(sql: &str) -> Option<(String, String, SqlType)> {
    let mut s = skip_ws(sql);
    s = skip_kw(s, "ALTER")?;
    s = skip_kw(s, "TABLE")?;
    let (table, rest) = read_ident(s)?;
    s = skip_kw(rest, "ADD")?;
    if starts_kw(s, "COLUMN") {
        s = skip_kw(s, "COLUMN")?;
    }
    let (column, rest) = read_ident(s)?;
    let (ty_ident, _) = read_ident(rest)?;
    let ty = SqlType::from_column_ident(&ty_ident)?;
    Some((table, column, ty))
}

fn parse_alter_rename_table(sql: &str) -> Option<(String, String)> {
    let mut s = skip_ws(sql);
    s = skip_kw(s, "ALTER")?;
    s = skip_kw(s, "TABLE")?;
    let (from, rest) = read_ident(s)?;
    s = skip_kw(rest, "RENAME")?;
    s = skip_kw(s, "TO")?;
    let (to, _) = read_ident(s)?;
    Some((from, to))
}

/// Host-private proofs for one execute request (not a guest SDK surface).
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when an expression is not in the
/// v1 type contract.
#[cfg(feature = "host")]
pub fn typecheck_execute_request_proofs(
    req: &ExecuteRequest,
    env: &SqlTypeEnv,
) -> Result<Vec<ResolvedStatement>> {
    typecheck_execute_request_resolved(req, env)
}

/// Reconstructs a type environment from already-separated canonical
/// `CREATE TABLE` statements.
///
/// Statement boundaries must come from [`crate::sql_v1_pack_statements`] or an
/// explicit list — this function does not split on `;`.
#[must_use]
pub fn sql_type_env_from_canonical_statements<I, S>(statements: I) -> SqlTypeEnv
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut env = SqlTypeEnv::new();
    for stmt in statements {
        apply_schema_sql_to_env(&mut env, stmt.as_ref());
    }
    env
}

/// Reconstructs a type environment from one canonical `CREATE TABLE` statement.
///
/// Multi-statement packs must use [`sql_type_env_from_canonical_statements`].
#[must_use]
pub fn sql_type_env_from_canonical_ddl(sql: &str) -> SqlTypeEnv {
    sql_type_env_from_canonical_statements(std::iter::once(sql))
}

/// Host bookkeeping tables present on every binding/library database.
///
/// Receipt wrap SQL is typed against this plus the plugin catalog. Adapters
/// merge it when rebuilding proofs after Cap'n drops host-private proofs.
#[must_use]
pub fn sql_host_bookkeeping_type_env() -> SqlTypeEnv {
    sql_type_env_from_canonical_statements([
        "CREATE TABLE db_atomic_receipts (\
         operation_id TEXT PRIMARY KEY NOT NULL, operation_kind TEXT NOT NULL, \
         request_hash TEXT NOT NULL, status TEXT NOT NULL, payload TEXT, \
         created_at TEXT NOT NULL, expires_at TEXT NOT NULL, consume_key TEXT UNIQUE)",
        "CREATE TABLE pragma_user_version (user_version INTEGER NOT NULL)",
        "CREATE TABLE pragma_table_info (\
         cid INTEGER NOT NULL, name TEXT NOT NULL, type TEXT NOT NULL, \
         notnull INTEGER NOT NULL, dflt_value TEXT, pk INTEGER NOT NULL)",
    ])
}

/// Proven v1 CAST matrix: same type, INTEGER↔REAL, or NULL to any admitted type.
#[must_use]
pub fn cast_is_legal(from: SqlType, to: SqlType) -> bool {
    if from == SqlType::Null || from == to {
        return true;
    }
    matches!(
        (from, to),
        (SqlType::Integer, SqlType::Real) | (SqlType::Real, SqlType::Integer)
    )
}

/// Portable SQL-v1 `json_object` maximum argument count (16 key/value pairs).
///
/// Matches Cloudflare D1's physical function-argument limit
/// ([`crate::D1_MAX_FUNCTION_ARGS`]). Larger objects are not admitted; adapters
/// must not emulate them with `json_patch` (JSON Merge Patch deletes nulls).
pub const SQL_V1_JSON_OBJECT_MAX_ARGS: usize = 32;

/// SQL-v1 helper arity `(min, max)` inclusive. Unknown names return `None`.
///
/// `json_object` is even arity `2..=`[`SQL_V1_JSON_OBJECT_MAX_ARGS`] (16 pairs).
#[must_use]
pub fn sql_v1_helper_arity(name: &str) -> Option<(usize, usize)> {
    Some(match name.to_ascii_lowercase().as_str() {
        "ifnull" | "nullif" => (2, 2),
        "coalesce" => (2, 32),
        "min" | "max" => (1, 32),
        "sum" | "avg" | "abs" | "length" | "lower" | "upper" | "json_valid" | "count" => (1, 1),
        "round" => (1, 2),
        "json_extract" => (2, 2),
        "json_object" => (2, SQL_V1_JSON_OBJECT_MAX_ARGS),
        "replace" => (3, 3),
        "substr" => (2, 3),
        "trim" => (1, 2),
        "json" | "julianday" => (1, 1),
        _ => return None,
    })
}

/// True when `n` matches the SQL-v1 signature for `name`.
#[must_use]
pub fn sql_v1_helper_arity_ok(name: &str, n: usize) -> bool {
    let Some((min, max)) = sql_v1_helper_arity(name) else {
        return true;
    };
    if n < min || n > max {
        return false;
    }
    if name.eq_ignore_ascii_case("json_object") && !n.is_multiple_of(2) {
        return false;
    }
    true
}

/// Rejects a helper call whose argument count is outside the v1 matrix.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when `n` is not a legal arity.
pub fn require_sql_v1_helper_arity(index: usize, name: &str, n: usize) -> Result<()> {
    if sql_v1_helper_arity_ok(name, n) {
        Ok(())
    } else {
        Err(PluginError::invalid_params(format!(
            "statement {index} is not Bookclerk SQL v1 (function {name} has arity {n})"
        )))
    }
}

/// Unifies two types (`NULL` is the identity; INTEGER+REAL → REAL).
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when the types cannot unify.
pub fn unify_types(index: usize, a: SqlType, b: SqlType) -> Result<SqlType> {
    match (a, b) {
        (SqlType::Null, t) | (t, SqlType::Null) => Ok(t),
        (x, y) if x == y => Ok(x),
        (SqlType::Integer, SqlType::Real) | (SqlType::Real, SqlType::Integer) => Ok(SqlType::Real),
        _ => Err(PluginError::invalid_params(format!(
            "statement {index} mixes incompatible SQL v1 types {} and {}",
            a.as_str(),
            b.as_str()
        ))),
    }
}

/// Type-checks every statement in `req` against `env` plus in-request `CREATE`.
///
/// Host-authoritative SQL is still typed (authorization may be skipped
/// elsewhere). Unknown columns fail closed, including when the catalog is empty.
///
/// # Errors
///
/// Returns [`PluginError::invalid_params`] when an expression is not in the
/// v1 type contract.
pub fn typecheck_execute_request(req: &ExecuteRequest, env: &SqlTypeEnv) -> Result<()> {
    let _proofs = typecheck_execute_request_resolved(req, env)?;
    Ok(())
}

/// Typechecks one canonical `CREATE INDEX` (including a partial `WHERE`).
///
/// # Errors
///
/// Returns when the statement is not admitted SQL v1, the table is missing,
/// an indexed column is not in [`SqlTypeEnv`], or a `WHERE` predicate is not
/// a complete boolean SQL-v1 expression.
pub fn typecheck_create_index_sql(sql: &str, env: &SqlTypeEnv) -> Result<()> {
    let _ = typecheck_index_ddl(0, sql, env)?;
    Ok(())
}

/// [`typecheck_execute_request`] plus the host-private resolved proofs.
pub(crate) fn typecheck_execute_request_resolved(
    req: &ExecuteRequest,
    env: &SqlTypeEnv,
) -> Result<Vec<ResolvedStatement>> {
    let mut working = env.clone();
    let mut proofs = Vec::with_capacity(req.statements.len());
    for (index, stmt) in req.statements.iter().enumerate() {
        proofs.push(typecheck_statement(index, stmt, &mut working)?);
    }
    Ok(proofs)
}

fn typecheck_statement(
    index: usize,
    stmt: &TypedDbStatement,
    env: &mut SqlTypeEnv,
) -> Result<ResolvedStatement> {
    let sql = stmt.sql.trim();
    if let Some(schema) = parse_create_table_schema(sql) {
        return typecheck_create(index, sql, schema, env);
    }
    if let Some(table) = parse_drop_table_name(sql) {
        env.drop_table(&table);
        return Ok(ResolvedStatement {
            statement_hash: statement_sql_hash(sql),
            output_columns: Vec::new(),
            physical_accesses: vec![PhysicalAccess {
                table: table.clone(),
                column: None,
            }],
            assignments: Vec::new(),
            text_collate_sites: Vec::new(),
            integer_arith_sites: Vec::new(),
            functions: Vec::new(),
            schema_action: SchemaAction::Drop { table },
        });
    }
    if looks_like_index_ddl(sql) {
        return typecheck_index_ddl(index, sql, env);
    }
    let binds: Vec<SqlType> = stmt.parameters.iter().map(sql_type_of_value).collect();
    let mut cx = TypeCx {
        index,
        env,
        binds: &binds,
        bind_i: 0,
        from: BTreeMap::new(),
        outer_from: Vec::new(),
        ctes: BTreeMap::new(),
        physical: BTreeSet::new(),
        functions: BTreeSet::new(),
        assignments: Vec::new(),
        text_spans: Vec::new(),
        integer_arith_sites: Vec::new(),
        output_columns: Vec::new(),
        require_named_derived: false,
        suppress_string_collate: false,
    };
    typecheck_sql(sql, &mut cx)?;
    let proof = cx.into_proof(sql);
    proof.validate_for(sql)?;
    Ok(proof)
}

fn typecheck_create(
    index: usize,
    sql: &str,
    schema: CreateTableSchema,
    env: &mut SqlTypeEnv,
) -> Result<ResolvedStatement> {
    typecheck_create_checks(index, &schema, env)?;
    let fingerprint = schema.fingerprint();
    let noop = if let Some(existing) = env.fingerprint(&schema.table) {
        if existing != fingerprint {
            return Err(PluginError::invalid_params(format!(
                "statement {index} CREATE TABLE IF NOT EXISTS {} does not match the catalog schema",
                schema.table
            )));
        }
        true
    } else if env.has_table(&schema.table) {
        return Err(PluginError::invalid_params(format!(
            "statement {index} CREATE TABLE IF NOT EXISTS {} conflicts with catalog metadata",
            schema.table
        )));
    } else {
        false
    };
    if !noop {
        env.insert_table_schema(
            schema.table.clone(),
            schema.columns.iter().cloned(),
            schema.identity_column.clone(),
            fingerprint.clone(),
        );
    }
    Ok(ResolvedStatement {
        statement_hash: statement_sql_hash(sql),
        output_columns: Vec::new(),
        physical_accesses: vec![PhysicalAccess {
            table: schema.table.clone(),
            column: None,
        }],
        assignments: Vec::new(),
        text_collate_sites: Vec::new(),
        integer_arith_sites: Vec::new(),
        functions: Vec::new(),
        schema_action: SchemaAction::Create {
            schema: Box::new(schema),
            fingerprint,
            noop,
        },
    })
}

fn looks_like_index_ddl(sql: &str) -> bool {
    let mut scan = TScan {
        sql,
        i: 0,
        took_real: false,
        base: 0,
    };
    if scan.take_kw("DROP") {
        return scan.take_kw("INDEX");
    }
    if !scan.take_kw("CREATE") {
        return false;
    }
    let _ = scan.take_kw("UNIQUE");
    scan.take_kw("INDEX")
}

fn typecheck_index_ddl(index: usize, sql: &str, env: &SqlTypeEnv) -> Result<ResolvedStatement> {
    let mut scan = TScan {
        sql,
        i: 0,
        took_real: false,
        base: 0,
    };
    if scan.take_kw("DROP") {
        if !scan.take_kw("INDEX") {
            return Err(ty_err(index, "DROP INDEX"));
        }
        let _ = scan.take_kw("IF");
        let _ = scan.take_kw("EXISTS");
        let _ = scan.read_ident();
        return Ok(ResolvedStatement::bound_empty(sql));
    }
    let parsed = parse_create_index_sql(sql)
        .ok_or_else(|| ty_err(index, "CREATE INDEX is not admitted SQL v1"))?;
    require_table_in_env(index, env, &parsed.table)?;
    for column in &parsed.columns {
        require_column_in_env(index, env, &parsed.table, column)?;
    }
    if let Some(pred) = parsed.where_sql.as_deref() {
        typecheck_index_where(index, &parsed.table, pred, env)?;
    }
    Ok(ResolvedStatement {
        statement_hash: statement_sql_hash(sql),
        output_columns: Vec::new(),
        physical_accesses: vec![PhysicalAccess {
            table: parsed.table,
            column: None,
        }],
        assignments: Vec::new(),
        text_collate_sites: Vec::new(),
        integer_arith_sites: Vec::new(),
        functions: Vec::new(),
        schema_action: SchemaAction::None,
    })
}

/// Typechecks a partial-index `WHERE` predicate and requires full consumption.
fn typecheck_index_where(
    index: usize,
    table: &str,
    predicate: &str,
    env: &SqlTypeEnv,
) -> Result<()> {
    let binds: Vec<SqlType> = Vec::new();
    let mut cx = TypeCx {
        index,
        env,
        binds: &binds,
        bind_i: 0,
        from: BTreeMap::from([(table.to_string(), FromSrc::Physical(table.to_string()))]),
        outer_from: Vec::new(),
        ctes: BTreeMap::new(),
        physical: BTreeSet::new(),
        functions: BTreeSet::new(),
        assignments: Vec::new(),
        text_spans: Vec::new(),
        integer_arith_sites: Vec::new(),
        output_columns: Vec::new(),
        require_named_derived: false,
        suppress_string_collate: false,
    };
    let mut scan = TScan {
        sql: predicate,
        i: 0,
        took_real: false,
        base: 0,
    };
    let ty = infer_expr(&mut scan, &mut cx)?;
    require_booleanish(index, ty, "CREATE INDEX WHERE")?;
    if !cx.integer_arith_sites.is_empty() {
        return Err(ty_err(
            index,
            "CREATE INDEX WHERE cannot use INTEGER + - * abs (overflow wrapping is DML-only)",
        ));
    }
    scan.skip();
    let rest = scan
        .sql
        .get(scan.i..)
        .unwrap_or("")
        .trim_end_matches(';')
        .trim();
    if !rest.is_empty() {
        return Err(ty_err(
            index,
            "CREATE INDEX WHERE is not a complete SQL v1 predicate",
        ));
    }
    Ok(())
}

fn require_table_in_env(index: usize, env: &SqlTypeEnv, table: &str) -> Result<()> {
    if env.has_table(table) {
        Ok(())
    } else {
        Err(ty_err(index, &format!("unknown table {table}")))
    }
}

fn require_column_in_env(index: usize, env: &SqlTypeEnv, table: &str, column: &str) -> Result<()> {
    if env.column_type(table, column).is_some() {
        Ok(())
    } else {
        Err(ty_err(index, &format!("unknown column {table}.{column}")))
    }
}

fn typecheck_create_checks(
    index: usize,
    schema: &CreateTableSchema,
    env: &SqlTypeEnv,
) -> Result<()> {
    let mut tmp = env.clone();
    tmp.insert_table(schema.table.clone(), schema.columns.iter().cloned());
    let binds: Vec<SqlType> = Vec::new();
    let mut cx = TypeCx {
        index,
        env: &tmp,
        binds: &binds,
        bind_i: 0,
        from: BTreeMap::from([(
            schema.table.clone(),
            FromSrc::Physical(schema.table.clone()),
        )]),
        outer_from: Vec::new(),
        ctes: BTreeMap::new(),
        physical: BTreeSet::new(),
        functions: BTreeSet::new(),
        assignments: Vec::new(),
        text_spans: Vec::new(),
        integer_arith_sites: Vec::new(),
        output_columns: Vec::new(),
        require_named_derived: false,
        suppress_string_collate: false,
    };
    for check in schema
        .column_checks
        .iter()
        .chain(schema.table_constraints.iter().filter_map(|c| match c {
            TableConstraint::Check(s) => Some(s),
            _ => None,
        }))
    {
        if check.is_empty() {
            continue;
        }
        let mut scan = TScan {
            sql: check,
            i: 0,
            took_real: false,
            base: 0,
        };
        let ty = infer_expr(&mut scan, &mut cx)?;
        require_booleanish(index, ty, "CHECK")?;
        if !cx.integer_arith_sites.is_empty() {
            return Err(ty_err(
                index,
                "CHECK cannot use INTEGER + - * abs (overflow wrapping is DML-only)",
            ));
        }
    }
    Ok(())
}

fn sql_type_of_value(v: &DbValue) -> SqlType {
    match v {
        DbValue::Int64(_) => SqlType::Integer,
        DbValue::Float64(_) => SqlType::Real,
        DbValue::Text(_) => SqlType::Text,
        DbValue::Bytes(_) => SqlType::Blob,
        DbValue::Boolean(_) => SqlType::Boolean,
        DbValue::Null(ty) => SqlType::from(*ty),
    }
}

#[derive(Clone)]
enum FromSrc {
    Physical(String),
    Cte(String),
}

struct TypeCx<'a> {
    index: usize,
    env: &'a SqlTypeEnv,
    binds: &'a [SqlType],
    bind_i: usize,
    from: BTreeMap<String, FromSrc>,
    /// Outer SELECT/UPDATE/DELETE FROM maps for correlated subquery lookup.
    outer_from: Vec<BTreeMap<String, FromSrc>>,
    ctes: BTreeMap<String, Vec<(String, SqlType)>>,
    physical: BTreeSet<(String, Option<String>)>,
    functions: BTreeSet<String>,
    assignments: Vec<ResolvedAssignment>,
    text_spans: Vec<SqlSpan>,
    integer_arith_sites: Vec<IntegerArithSite>,
    output_columns: Vec<(String, SqlType)>,
    require_named_derived: bool,
    /// When set, string literals are not recorded as TEXT collate sites
    /// (`json_extract` path arguments).
    suppress_string_collate: bool,
}

impl TypeCx<'_> {
    fn note_text(&mut self, start: usize, end: usize) {
        if start < end {
            self.text_spans.push(SqlSpan { start, end });
        }
    }

    fn note_arith(&mut self, site: IntegerArithSite) {
        if site.full.start < site.full.end {
            self.integer_arith_sites.push(site);
        }
    }

    fn note_physical(&mut self, table: &str, column: Option<&str>) {
        self.physical
            .insert((table.to_string(), column.map(str::to_string)));
    }

    fn into_proof(self, sql: &str) -> ResolvedStatement {
        let mut accesses: Vec<PhysicalAccess> = self
            .physical
            .into_iter()
            .map(|(table, column)| PhysicalAccess { table, column })
            .collect();
        accesses.sort();
        ResolvedStatement {
            statement_hash: statement_sql_hash(sql),
            output_columns: self.output_columns,
            physical_accesses: accesses,
            assignments: self.assignments,
            text_collate_sites: self
                .text_spans
                .into_iter()
                .map(|span| TextCollateSite { span })
                .collect(),
            integer_arith_sites: self.integer_arith_sites,
            functions: {
                let mut fns: Vec<String> = self.functions.into_iter().collect();
                fns.sort();
                fns
            },
            schema_action: SchemaAction::None,
        }
    }
}

fn parse_optional_with(scan: &mut TScan<'_>, cx: &mut TypeCx<'_>) -> Result<()> {
    if !scan.take_kw("WITH") {
        return Ok(());
    }
    let recursive = scan.take_kw("RECURSIVE");
    loop {
        let name = scan
            .read_ident()
            .ok_or_else(|| ty_err(cx.index, "CTE name"))?;
        let mut explicit_names = Vec::new();
        if scan.take_byte(b'(') {
            loop {
                let col = scan
                    .read_ident()
                    .ok_or_else(|| ty_err(cx.index, "CTE column"))?;
                explicit_names.push(col);
                if !scan.take_byte(b',') {
                    break;
                }
            }
            if !scan.take_byte(b')') {
                return Err(ty_err(cx.index, "CTE columns"));
            }
        }
        if !scan.take_kw("AS") || !scan.take_byte(b'(') {
            return Err(ty_err(cx.index, "CTE AS ("));
        }
        let inner = scan
            .take_balanced_inner()
            .ok_or_else(|| ty_err(cx.index, "CTE body"))?;
        let cols = if recursive {
            typecheck_recursive_cte(inner, scan.base_of(inner), cx, &name, &explicit_names)?
        } else {
            let saved = cx.require_named_derived;
            cx.require_named_derived = explicit_names.is_empty();
            let cols = typecheck_select_list_only(inner, scan.base_of(inner), cx)?;
            cx.require_named_derived = saved;
            apply_explicit_cte_names(cx.index, cols, &explicit_names)?
        };
        reject_duplicate_names(cx.index, &cols)?;
        cx.ctes.insert(name, cols);
        if !scan.take_byte(b',') {
            break;
        }
    }
    Ok(())
}

fn apply_explicit_cte_names(
    index: usize,
    mut cols: Vec<(String, SqlType)>,
    explicit: &[String],
) -> Result<Vec<(String, SqlType)>> {
    if explicit.is_empty() {
        return Ok(cols);
    }
    if explicit.len() != cols.len() {
        return Err(ty_err(index, "CTE column list arity"));
    }
    for (i, name) in explicit.iter().enumerate() {
        cols[i].0 = name.clone();
    }
    Ok(cols)
}

fn reject_duplicate_names(index: usize, cols: &[(String, SqlType)]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for (n, _) in cols {
        if !seen.insert(n) {
            return Err(ty_err(index, "duplicate derived column name"));
        }
    }
    Ok(())
}

fn typecheck_recursive_cte(
    sql: &str,
    base: usize,
    cx: &mut TypeCx<'_>,
    name: &str,
    explicit: &[String],
) -> Result<Vec<(String, SqlType)>> {
    let mut scan = TScan {
        sql,
        i: 0,
        took_real: false,
        base,
    };
    let saved = cx.require_named_derived;
    cx.require_named_derived = explicit.is_empty();
    let mut cols = infer_select_core(&mut scan, cx)?;
    cx.require_named_derived = saved;
    cols = apply_explicit_cte_names(cx.index, cols, explicit)?;
    reject_duplicate_names(cx.index, &cols)?;
    for (n, ty) in &cols {
        if *ty == SqlType::Null {
            return Err(ty_err(
                cx.index,
                "recursive CTE anchor must establish concrete column types",
            ));
        }
        if n.starts_with('c') && n[1..].chars().all(|c| c.is_ascii_digit()) && explicit.is_empty() {
            return Err(ty_err(
                cx.index,
                "derived columns require explicit aliases or inherited names",
            ));
        }
    }
    cx.ctes.insert(name.to_string(), cols.clone());
    while scan.take_kw("UNION") {
        let _ = scan.take_kw("ALL");
        let other = infer_select_core(&mut scan, cx)?;
        if cols.len() != other.len() {
            return Err(ty_err(cx.index, "UNION column counts differ"));
        }
        for ((_, a), (_, b)) in cols.iter().zip(other.iter()) {
            let u = unify_types(cx.index, *a, *b)?;
            if u != *a {
                return Err(ty_err(
                    cx.index,
                    "recursive CTE arm is not compatible with the anchor types",
                ));
            }
        }
    }
    Ok(cols)
}

fn typecheck_sql(sql: &str, cx: &mut TypeCx<'_>) -> Result<()> {
    let mut scan = TScan {
        sql,
        i: 0,
        took_real: false,
        base: 0,
    };
    parse_optional_with(&mut scan, cx)?;
    if scan.peek_kw("SELECT") || scan.peek_kw("VALUES") {
        let cols = infer_select(&mut scan, cx)?;
        cx.output_columns = cols;
        return Ok(());
    }
    if scan.take_kw("INSERT") {
        if scan.take_kw("OR") {
            let _ = scan.take_kw("IGNORE");
        }
        if !scan.take_kw("INTO") {
            return Err(ty_err(cx.index, "INSERT INTO"));
        }
        let table = scan
            .read_ident()
            .ok_or_else(|| ty_err(cx.index, "INSERT table"))?;
        require_physical_table(cx, &table)?;
        cx.from
            .insert(table.clone(), FromSrc::Physical(table.clone()));
        cx.note_physical(&table, None);
        let mut dests: Vec<(String, SqlType)> = Vec::new();
        if scan.take_byte(b'(') {
            loop {
                let col = scan
                    .read_ident()
                    .ok_or_else(|| ty_err(cx.index, "INSERT column"))?;
                let ty = lookup_column(cx, Some(&table), &col)?;
                dests.push((col, ty));
                if !scan.take_byte(b',') {
                    break;
                }
            }
            if !scan.take_byte(b')') {
                return Err(ty_err(cx.index, "INSERT columns"));
            }
        } else if let Some(cols) = cx.env.table_columns(&table) {
            dests = cols.to_vec();
        }
        if scan.peek_kw("VALUES") {
            scan.take_kw("VALUES");
            loop {
                if !scan.take_byte(b'(') {
                    return Err(ty_err(cx.index, "VALUES"));
                }
                let mut i = 0usize;
                loop {
                    let ty = infer_expr(&mut scan, cx)?;
                    unify_insert_dest(cx, &table, &dests, i, ty)?;
                    i += 1;
                    if !scan.take_byte(b',') {
                        break;
                    }
                }
                if i != dests.len() && !dests.is_empty() {
                    return Err(ty_err(cx.index, "INSERT VALUES arity"));
                }
                if !scan.take_byte(b')') {
                    return Err(ty_err(cx.index, "VALUES close"));
                }
                if !scan.take_byte(b',') {
                    break;
                }
            }
        } else {
            let dest_from = cx.from.clone();
            parse_optional_with(&mut scan, cx)?;
            let cols = infer_select(&mut scan, cx)?;
            if !dests.is_empty() && cols.len() != dests.len() {
                return Err(ty_err(cx.index, "INSERT SELECT arity"));
            }
            for (i, (_, ty)) in cols.iter().enumerate() {
                unify_insert_dest(cx, &table, &dests, i, *ty)?;
            }
            cx.from = dest_from;
        }
        if scan.take_kw("RETURNING") {
            cx.output_columns = parse_returning_list(&mut scan, cx)?;
        }
        return Ok(());
    }
    if scan.take_kw("UPDATE") {
        let table = scan
            .read_ident()
            .ok_or_else(|| ty_err(cx.index, "UPDATE table"))?;
        require_physical_table(cx, &table)?;
        cx.from
            .insert(table.clone(), FromSrc::Physical(table.clone()));
        cx.note_physical(&table, None);
        if !scan.take_kw("SET") {
            return Err(ty_err(cx.index, "UPDATE SET"));
        }
        loop {
            let col = scan
                .read_ident()
                .ok_or_else(|| ty_err(cx.index, "UPDATE column"))?;
            if !scan.take_byte(b'=') {
                return Err(ty_err(cx.index, "UPDATE ="));
            }
            let dest = lookup_column(cx, Some(&table), &col)?;
            let src = infer_expr(&mut scan, cx)?;
            let unified = unify_types(cx.index, dest, src)?;
            cx.assignments.push(ResolvedAssignment {
                table: table.clone(),
                column: col,
                dest,
                source: unified,
            });
            if !scan.take_byte(b',') {
                break;
            }
        }
        if scan.take_kw("WHERE") {
            let ty = infer_expr(&mut scan, cx)?;
            require_booleanish(cx.index, ty, "WHERE")?;
        }
        if scan.take_kw("RETURNING") {
            cx.output_columns = parse_returning_list(&mut scan, cx)?;
        }
        return Ok(());
    }
    if scan.take_kw("DELETE") {
        let _ = scan.take_kw("FROM");
        let table = scan
            .read_ident()
            .ok_or_else(|| ty_err(cx.index, "DELETE table"))?;
        require_physical_table(cx, &table)?;
        cx.from
            .insert(table.clone(), FromSrc::Physical(table.clone()));
        cx.note_physical(&table, None);
        if scan.take_kw("WHERE") {
            let ty = infer_expr(&mut scan, cx)?;
            require_booleanish(cx.index, ty, "WHERE")?;
        }
        return Ok(());
    }
    if scan.take_kw("CREATE") || scan.take_kw("DROP") {
        return Ok(());
    }
    Ok(())
}

fn unify_insert_dest(
    cx: &mut TypeCx<'_>,
    table: &str,
    dests: &[(String, SqlType)],
    i: usize,
    ty: SqlType,
) -> Result<()> {
    let Some((col, want)) = dests.get(i) else {
        return Ok(());
    };
    let unified = unify_types(cx.index, ty, *want)?;
    cx.assignments.push(ResolvedAssignment {
        table: table.to_string(),
        column: col.clone(),
        dest: *want,
        source: unified,
    });
    Ok(())
}

fn require_physical_table(cx: &TypeCx<'_>, table: &str) -> Result<()> {
    if cx.env.has_table(table) {
        return Ok(());
    }
    Err(PluginError::invalid_params(format!(
        "statement {} names unknown table {table}",
        cx.index
    )))
}

fn require_booleanish(index: usize, ty: SqlType, ctx: &str) -> Result<()> {
    if ty == SqlType::Boolean || ty == SqlType::Null {
        Ok(())
    } else {
        Err(PluginError::invalid_params(format!(
            "statement {index} {ctx} requires BOOLEAN or NULL, found {}",
            ty.as_str()
        )))
    }
}

fn typecheck_select_list_only(
    sql: &str,
    base: usize,
    cx: &mut TypeCx<'_>,
) -> Result<Vec<(String, SqlType)>> {
    let mut scan = TScan {
        sql,
        i: 0,
        took_real: false,
        base,
    };
    infer_select(&mut scan, cx)
}

fn infer_select(scan: &mut TScan<'_>, cx: &mut TypeCx<'_>) -> Result<Vec<(String, SqlType)>> {
    cx.outer_from.push(std::mem::take(&mut cx.from));
    let saved_ctes = cx.ctes.clone();
    let result = (|| {
        let mut cols = infer_select_core(scan, cx)?;
        while scan.take_kw("UNION") {
            let _ = scan.take_kw("ALL");
            let other = infer_select_core(scan, cx)?;
            if cols.len() != other.len() {
                return Err(ty_err(cx.index, "UNION column counts differ"));
            }
            for ((_, a), (_, b)) in cols.iter_mut().zip(other.iter()) {
                *a = unify_types(cx.index, *a, *b)?;
            }
        }
        Ok(cols)
    })();
    cx.ctes = saved_ctes;
    cx.from = cx.outer_from.pop().unwrap_or_default();
    result
}

fn infer_select_core(scan: &mut TScan<'_>, cx: &mut TypeCx<'_>) -> Result<Vec<(String, SqlType)>> {
    if scan.take_kw("VALUES") {
        let mut cols = Vec::new();
        if !scan.take_byte(b'(') {
            return Err(ty_err(cx.index, "VALUES tuple"));
        }
        let mut i = 0;
        loop {
            let ty = infer_expr(scan, cx)?;
            cols.push((format!("c{i}"), ty));
            i += 1;
            if !scan.take_byte(b',') {
                break;
            }
        }
        if !scan.take_byte(b')') {
            return Err(ty_err(cx.index, "VALUES close"));
        }
        return Ok(cols);
    }
    if !scan.take_kw("SELECT") {
        return Err(ty_err(cx.index, "SELECT"));
    }
    let _ = scan.take_kw("DISTINCT");
    lookahead_from(scan, cx);
    let mut cols = Vec::new();
    loop {
        if let Some(star) = take_select_star(scan, cx)? {
            cols.extend(star);
        } else {
            let start = scan.i;
            let ty = infer_expr(scan, cx)?;
            let expr_sql = scan.sql.get(start..scan.i).unwrap_or("");
            let name = select_item_name(scan, cx, cols.len(), expr_sql)?;
            cols.push((name, ty));
        }
        if !scan.take_byte(b',') {
            break;
        }
    }
    if scan.take_kw("FROM") {
        cx.from.clear();
        take_from_item(scan, cx)?;
        loop {
            if scan.take_kw("JOIN")
                || scan.take_kw("INNER")
                || scan.take_kw("LEFT")
                || scan.take_kw("CROSS")
            {
                let _ = scan.take_kw("OUTER");
                let _ = scan.take_kw("JOIN");
                take_from_item(scan, cx)?;
                if scan.take_kw("ON") {
                    let ty = infer_expr(scan, cx)?;
                    require_booleanish(cx.index, ty, "JOIN ON")?;
                }
                continue;
            }
            if scan.take_byte(b',') {
                take_from_item(scan, cx)?;
                continue;
            }
            break;
        }
    }
    if scan.take_kw("WHERE") {
        let ty = infer_expr(scan, cx)?;
        require_booleanish(cx.index, ty, "WHERE")?;
    }
    if scan.take_kw("GROUP") {
        let _ = scan.take_kw("BY");
        loop {
            let _ = infer_expr(scan, cx)?;
            if !scan.take_byte(b',') {
                break;
            }
        }
    }
    if scan.take_kw("HAVING") {
        let ty = infer_expr(scan, cx)?;
        require_booleanish(cx.index, ty, "HAVING")?;
    }
    if scan.take_kw("ORDER") {
        let _ = scan.take_kw("BY");
        loop {
            take_order_key(scan, cx, &cols)?;
            let _ = scan.take_kw("ASC");
            let _ = scan.take_kw("DESC");
            if scan.take_kw("NULLS") {
                let _ = scan.take_kw("FIRST");
                let _ = scan.take_kw("LAST");
            }
            if !scan.take_byte(b',') {
                break;
            }
        }
    }
    if scan.take_kw("LIMIT") {
        let _ = infer_expr(scan, cx)?;
    }
    if scan.take_kw("OFFSET") {
        let _ = infer_expr(scan, cx)?;
    }
    Ok(cols)
}

fn take_order_key(
    scan: &mut TScan<'_>,
    cx: &mut TypeCx<'_>,
    cols: &[(String, SqlType)],
) -> Result<SqlType> {
    scan.skip();
    let start = scan.i;
    if let Some(ident) = scan.read_ident() {
        scan.skip();
        let terminal = scan.i >= scan.sql.len()
            || scan.peek_byte(b',')
            || scan.peek_byte(b')')
            || scan.peek_kw("ASC")
            || scan.peek_kw("DESC")
            || scan.peek_kw("NULLS")
            || scan.peek_kw("LIMIT")
            || scan.peek_kw("OFFSET")
            || scan.peek_kw("UNION")
            || scan.peek_kw("INTERSECT")
            || scan.peek_kw("EXCEPT");
        if terminal {
            if let Some((_, ty)) = cols.iter().find(|(n, _)| *n == ident) {
                return Ok(*ty);
            }
        }
        scan.i = start;
    }
    infer_expr(scan, cx)
}

fn select_item_name(
    scan: &mut TScan<'_>,
    cx: &TypeCx<'_>,
    i: usize,
    expr_sql: &str,
) -> Result<String> {
    if scan.take_kw("AS") {
        return scan
            .read_ident()
            .ok_or_else(|| ty_err(cx.index, "select alias"));
    }
    if let Some(name) = inherited_column_name(expr_sql) {
        return Ok(name);
    }
    if cx.require_named_derived {
        return Err(ty_err(
            cx.index,
            "derived columns require explicit aliases or inherited names",
        ));
    }
    Ok(format!("c{i}"))
}

fn inherited_column_name(expr: &str) -> Option<String> {
    let mut s = skip_ws(expr);
    let (first, rest) = read_ident(s)?;
    s = skip_ws(rest);
    let name = if s.starts_with('.') {
        let (col, rest) = read_ident(skip_ws(&s[1..]))?;
        s = skip_ws(rest);
        col
    } else {
        first
    };
    if s.is_empty() {
        Some(name)
    } else {
        None
    }
}

fn lookahead_from(scan: &TScan<'_>, cx: &mut TypeCx<'_>) {
    let mut ahead = TScan {
        sql: scan.sql,
        i: scan.i,
        took_real: false,
        base: scan.base,
    };
    let mut depth = 0i32;
    while ahead.i < ahead.sql.len() {
        ahead.skip();
        if ahead.i >= ahead.sql.len() {
            break;
        }
        if ahead.sql.as_bytes()[ahead.i] == b'\'' {
            let _ = ahead.take_string();
            continue;
        }
        if ahead.take_byte(b'(') {
            depth += 1;
            continue;
        }
        if ahead.take_byte(b')') {
            depth -= 1;
            continue;
        }
        if depth == 0 && ahead.take_kw("FROM") {
            let mut tmp = TScan {
                sql: ahead.sql,
                i: ahead.i,
                took_real: false,
                base: ahead.base,
            };
            let saved_from = cx.from.clone();
            cx.from.clear();
            loop {
                if tmp.take_byte(b'(') {
                    let inner = tmp.take_balanced_inner().unwrap_or("");
                    let saved_named = cx.require_named_derived;
                    let saved_bind = cx.bind_i;
                    let saved_spans = cx.text_spans.len();
                    let saved_arith = cx.integer_arith_sites.len();
                    let saved_physical = cx.physical.clone();
                    let saved_assign = cx.assignments.len();
                    cx.require_named_derived = true;
                    let cols = typecheck_select_list_only(inner, tmp.base_of(inner), cx).ok();
                    cx.require_named_derived = saved_named;
                    cx.bind_i = saved_bind;
                    cx.text_spans.truncate(saved_spans);
                    cx.integer_arith_sites.truncate(saved_arith);
                    cx.physical = saved_physical;
                    cx.assignments.truncate(saved_assign);
                    let _ = tmp.take_kw("AS");
                    if let Some(alias) = tmp.read_ident() {
                        if let Some(cols) = cols {
                            let _ = reject_duplicate_names(cx.index, &cols);
                            cx.ctes.insert(alias.clone(), cols);
                        }
                        cx.from.insert(alias.clone(), FromSrc::Cte(alias));
                    }
                } else if let Some(name) = tmp.read_ident() {
                    let mut visible = name.clone();
                    if tmp.take_kw("AS") {
                        if let Some(alias) = tmp.read_ident() {
                            visible = alias;
                        }
                    } else if !tmp.peek_kw("JOIN")
                        && !tmp.peek_kw("INNER")
                        && !tmp.peek_kw("LEFT")
                        && !tmp.peek_kw("CROSS")
                        && !tmp.peek_kw("ON")
                        && !tmp.peek_kw("WHERE")
                        && !tmp.peek_byte(b',')
                    {
                        if let Some(alias) = tmp.read_ident_if_not_clause() {
                            visible = alias;
                        }
                    }
                    let src = if cx.ctes.contains_key(&name) {
                        FromSrc::Cte(name)
                    } else {
                        FromSrc::Physical(name)
                    };
                    cx.from.insert(visible, src);
                } else {
                    break;
                }
                loop {
                    if tmp.take_kw("ON") {
                        skip_join_on_predicate(&mut tmp);
                        continue;
                    }
                    if tmp.take_kw("JOIN")
                        || tmp.take_kw("INNER")
                        || tmp.take_kw("LEFT")
                        || tmp.take_kw("CROSS")
                    {
                        let _ = tmp.take_kw("OUTER");
                        let _ = tmp.take_kw("JOIN");
                        break;
                    }
                    if tmp.take_byte(b',') {
                        break;
                    }
                    if cx.from.is_empty() {
                        cx.from = saved_from;
                    }
                    return;
                }
            }
        }
        let ch = ahead.sql[ahead.i..].chars().next().unwrap_or('\0');
        ahead.i += ch.len_utf8();
    }
}

fn skip_join_on_predicate(scan: &mut TScan<'_>) {
    let mut depth = 0i32;
    while scan.i < scan.sql.len() {
        scan.skip();
        if scan.i >= scan.sql.len() {
            break;
        }
        if scan.sql.as_bytes()[scan.i] == b'\'' {
            let _ = scan.take_string();
            continue;
        }
        if scan.take_byte(b'(') {
            depth += 1;
            continue;
        }
        if scan.take_byte(b')') {
            depth -= 1;
            continue;
        }
        if depth > 0 {
            let ch = scan.sql[scan.i..].chars().next().unwrap_or('\0');
            scan.i += ch.len_utf8();
            continue;
        }
        if scan.peek_kw("JOIN")
            || scan.peek_kw("INNER")
            || scan.peek_kw("LEFT")
            || scan.peek_kw("CROSS")
            || scan.peek_kw("WHERE")
            || scan.peek_kw("GROUP")
            || scan.peek_kw("HAVING")
            || scan.peek_kw("ORDER")
            || scan.peek_kw("LIMIT")
            || scan.peek_kw("UNION")
            || scan.peek_byte(b',')
        {
            break;
        }
        let ch = scan.sql[scan.i..].chars().next().unwrap_or('\0');
        scan.i += ch.len_utf8();
    }
}

fn parse_returning_list(
    scan: &mut TScan<'_>,
    cx: &mut TypeCx<'_>,
) -> Result<Vec<(String, SqlType)>> {
    let mut out = Vec::new();
    loop {
        if let Some(star) = take_select_star(scan, cx)? {
            out.extend(star);
        } else {
            let start = scan.i;
            let ty = infer_expr(scan, cx)?;
            let expr_sql = scan.sql.get(start..scan.i).unwrap_or("");
            let name = select_item_name(scan, cx, out.len(), expr_sql)?;
            out.push((name, ty));
        }
        if !scan.take_byte(b',') {
            break;
        }
    }
    Ok(out)
}

/// `*` or `alias.*` in a SELECT/RETURNING list.
///
/// # Errors
///
/// Returns when the starred source is unknown.
fn take_select_star(
    scan: &mut TScan<'_>,
    cx: &mut TypeCx<'_>,
) -> Result<Option<Vec<(String, SqlType)>>> {
    scan.skip();
    if scan.take_byte(b'*') {
        return Ok(Some(star_columns(cx)?));
    }
    let saved = scan.i;
    if let Some(qual) = scan.read_ident() {
        if scan.take_byte(b'.') && scan.take_byte(b'*') {
            return Ok(Some(qualified_star_columns(cx, &qual)?));
        }
    }
    scan.i = saved;
    Ok(None)
}

fn star_columns(cx: &mut TypeCx<'_>) -> Result<Vec<(String, SqlType)>> {
    let mut out = Vec::new();
    let sources: Vec<FromSrc> = cx.from.values().cloned().collect();
    if sources.is_empty() {
        return Err(PluginError::invalid_params(format!(
            "statement {} SELECT * names unknown source",
            cx.index
        )));
    }
    for src in &sources {
        out.extend(star_columns_for_src(cx, src)?);
    }
    Ok(out)
}

fn qualified_star_columns(cx: &mut TypeCx<'_>, qual: &str) -> Result<Vec<(String, SqlType)>> {
    if let Some(src) = lookup_visible_src(cx, qual) {
        return star_columns_for_src(cx, &src);
    }
    if let Some(cols) = cx.ctes.get(qual).cloned() {
        return Ok(cols);
    }
    if cx.env.has_table(qual) {
        return star_columns_for_src(cx, &FromSrc::Physical(qual.to_string()));
    }
    Err(PluginError::invalid_params(format!(
        "statement {} SELECT * names unknown source",
        cx.index
    )))
}

fn star_columns_for_src(cx: &mut TypeCx<'_>, src: &FromSrc) -> Result<Vec<(String, SqlType)>> {
    match src {
        FromSrc::Cte(name) => {
            if let Some(cols) = cx.ctes.get(name) {
                return Ok(cols.clone());
            }
        }
        FromSrc::Physical(table) => {
            if let Some(cols) = cx.env.table_columns(table) {
                let cols = cols.to_vec();
                for (col, _) in &cols {
                    cx.note_physical(table, Some(col));
                }
                cx.note_physical(table, Some(PHYSICAL_STAR_COLUMN));
                return Ok(cols);
            }
        }
    }
    Err(PluginError::invalid_params(format!(
        "statement {} SELECT * names unknown source",
        cx.index
    )))
}

fn take_from_item(scan: &mut TScan<'_>, cx: &mut TypeCx<'_>) -> Result<()> {
    if scan.take_byte(b'(') {
        let inner = scan
            .take_balanced_inner()
            .ok_or_else(|| ty_err(cx.index, "FROM subquery"))?;
        let saved = cx.require_named_derived;
        cx.require_named_derived = true;
        let cols = typecheck_select_list_only(inner, scan.base_of(inner), cx)?;
        cx.require_named_derived = saved;
        reject_duplicate_names(cx.index, &cols)?;
        let _ = scan.take_kw("AS");
        let alias = scan
            .read_ident()
            .ok_or_else(|| ty_err(cx.index, "subquery alias required"))?;
        cx.ctes.insert(alias.clone(), cols);
        cx.from.insert(alias.clone(), FromSrc::Cte(alias));
        return Ok(());
    }
    let name = scan
        .read_ident()
        .ok_or_else(|| ty_err(cx.index, "FROM table"))?;
    let mut visible = name.clone();
    if scan.take_kw("AS")
        || !(scan.peek_kw("JOIN")
            || scan.peek_kw("INNER")
            || scan.peek_kw("LEFT")
            || scan.peek_kw("CROSS")
            || scan.peek_kw("ON")
            || scan.peek_kw("WHERE")
            || scan.peek_kw("GROUP")
            || scan.peek_kw("HAVING")
            || scan.peek_kw("ORDER")
            || scan.peek_kw("LIMIT")
            || scan.peek_kw("UNION")
            || scan.peek_byte(b','))
    {
        if let Some(alias) = scan.read_ident() {
            visible = alias;
        }
    }
    if cx.ctes.contains_key(&name) {
        cx.from.insert(visible, FromSrc::Cte(name));
    } else {
        require_physical_table(cx, &name)?;
        cx.note_physical(&name, None);
        cx.from.insert(visible, FromSrc::Physical(name));
    }
    Ok(())
}

fn infer_expr(scan: &mut TScan<'_>, cx: &mut TypeCx<'_>) -> Result<SqlType> {
    infer_or(scan, cx)
}

fn infer_or(scan: &mut TScan<'_>, cx: &mut TypeCx<'_>) -> Result<SqlType> {
    let mut t = infer_and(scan, cx)?;
    while scan.take_kw("OR") {
        require_booleanish(cx.index, t, "OR")?;
        let r = infer_and(scan, cx)?;
        require_booleanish(cx.index, r, "OR")?;
        t = SqlType::Boolean;
    }
    Ok(t)
}

fn infer_and(scan: &mut TScan<'_>, cx: &mut TypeCx<'_>) -> Result<SqlType> {
    let mut t = infer_not(scan, cx)?;
    while scan.take_kw("AND") {
        require_booleanish(cx.index, t, "AND")?;
        let r = infer_not(scan, cx)?;
        require_booleanish(cx.index, r, "AND")?;
        t = SqlType::Boolean;
    }
    Ok(t)
}

fn infer_not(scan: &mut TScan<'_>, cx: &mut TypeCx<'_>) -> Result<SqlType> {
    if scan.take_kw("NOT") {
        let t = infer_not(scan, cx)?;
        require_booleanish(cx.index, t, "NOT")?;
        return Ok(SqlType::Boolean);
    }
    infer_cmp(scan, cx)
}

fn infer_cmp(scan: &mut TScan<'_>, cx: &mut TypeCx<'_>) -> Result<SqlType> {
    let left = infer_concat(scan, cx)?;
    if scan.take_kw("IS") {
        let _ = scan.take_kw("NOT");
        if !scan.take_kw("NULL") {
            return Err(ty_err(cx.index, "IS NULL"));
        }
        return Ok(SqlType::Boolean);
    }
    if scan.take_kw("NOT") {
        if scan.take_kw("LIKE") {
            let right = infer_concat(scan, cx)?;
            require_textish(cx.index, left)?;
            require_textish(cx.index, right)?;
            return Ok(SqlType::Boolean);
        }
        if scan.take_kw("IN") {
            infer_in_list(scan, cx, left)?;
            return Ok(SqlType::Boolean);
        }
        if scan.take_kw("BETWEEN") {
            let a = infer_concat(scan, cx)?;
            if !scan.take_kw("AND") {
                return Err(ty_err(cx.index, "BETWEEN AND"));
            }
            let b = infer_concat(scan, cx)?;
            let _ = unify_types(cx.index, left, a)?;
            let _ = unify_types(cx.index, left, b)?;
            return Ok(SqlType::Boolean);
        }
        return Err(ty_err(cx.index, "NOT LIKE/IN/BETWEEN"));
    }
    if scan.take_kw("LIKE") {
        let right = infer_concat(scan, cx)?;
        require_textish(cx.index, left)?;
        require_textish(cx.index, right)?;
        return Ok(SqlType::Boolean);
    }
    if scan.take_kw("IN") {
        infer_in_list(scan, cx, left)?;
        return Ok(SqlType::Boolean);
    }
    if scan.take_kw("BETWEEN") {
        let a = infer_concat(scan, cx)?;
        if !scan.take_kw("AND") {
            return Err(ty_err(cx.index, "BETWEEN AND"));
        }
        let b = infer_concat(scan, cx)?;
        let _ = unify_types(cx.index, left, a)?;
        let _ = unify_types(cx.index, left, b)?;
        return Ok(SqlType::Boolean);
    }
    for op in ["!=", "<>", "<=", ">=", "=", "<", ">"] {
        if scan.take_op(op) {
            let right = infer_concat(scan, cx)?;
            let _ = unify_types(cx.index, left, right)?;
            return Ok(SqlType::Boolean);
        }
    }
    Ok(left)
}

fn infer_in_list(scan: &mut TScan<'_>, cx: &mut TypeCx<'_>, left: SqlType) -> Result<()> {
    if !scan.take_byte(b'(') {
        return Err(ty_err(cx.index, "IN ("));
    }
    let inner = scan
        .take_balanced_inner()
        .ok_or_else(|| ty_err(cx.index, "IN ("))?;
    let mut body = TScan {
        sql: inner,
        i: 0,
        took_real: false,
        base: scan.base_of(inner),
    };
    if body.peek_kw("SELECT") || body.peek_kw("WITH") {
        let saved_named = cx.require_named_derived;
        cx.require_named_derived = false;
        let cols = infer_select(&mut body, cx)?;
        cx.require_named_derived = saved_named;
        if let Some((_, ty)) = cols.first() {
            let _ = unify_types(cx.index, left, *ty)?;
        }
        return Ok(());
    }
    loop {
        let t = infer_expr(&mut body, cx)?;
        let _ = unify_types(cx.index, left, t)?;
        if !body.take_byte(b',') {
            break;
        }
    }
    Ok(())
}

fn infer_concat(scan: &mut TScan<'_>, cx: &mut TypeCx<'_>) -> Result<SqlType> {
    let mut t = infer_add(scan, cx)?;
    while scan.take_op("||") {
        let r = infer_add(scan, cx)?;
        require_textish(cx.index, t)?;
        require_textish(cx.index, r)?;
        t = SqlType::Text;
    }
    Ok(t)
}

fn infer_add(scan: &mut TScan<'_>, cx: &mut TypeCx<'_>) -> Result<SqlType> {
    scan.skip();
    let lhs_start = scan.abs(scan.i);
    let mut t = infer_mul(scan, cx)?;
    loop {
        scan.skip();
        let lhs_end = scan.abs(scan.i);
        let kind = if scan.take_op("+") {
            IntegerArithKind::Add
        } else if scan.take_op("-") {
            IntegerArithKind::Sub
        } else {
            break;
        };
        scan.skip();
        let rhs_start = scan.abs(scan.i);
        let r = infer_mul(scan, cx)?;
        let rhs_end = scan.abs(scan.i);
        t = unify_numeric(cx.index, t, r)?;
        if t == SqlType::Integer {
            cx.note_arith(IntegerArithSite {
                full: SqlSpan {
                    start: lhs_start,
                    end: rhs_end,
                },
                lhs: SqlSpan {
                    start: lhs_start,
                    end: lhs_end,
                },
                rhs: SqlSpan {
                    start: rhs_start,
                    end: rhs_end,
                },
                kind,
            });
        }
    }
    Ok(t)
}

fn infer_mul(scan: &mut TScan<'_>, cx: &mut TypeCx<'_>) -> Result<SqlType> {
    scan.skip();
    let lhs_start = scan.abs(scan.i);
    let mut t = infer_prefix(scan, cx)?;
    loop {
        scan.skip();
        let lhs_end = scan.abs(scan.i);
        if scan.take_op("%") {
            let r = infer_prefix(scan, cx)?;
            t = unify_integer_mod(cx.index, t, r)?;
        } else if scan.take_op("/") {
            let r = infer_prefix(scan, cx)?;
            t = unify_numeric(cx.index, t, r)?;
        } else if scan.take_op("*") {
            scan.skip();
            let rhs_start = scan.abs(scan.i);
            let r = infer_prefix(scan, cx)?;
            let rhs_end = scan.abs(scan.i);
            t = unify_numeric(cx.index, t, r)?;
            if t == SqlType::Integer {
                cx.note_arith(IntegerArithSite {
                    full: SqlSpan {
                        start: lhs_start,
                        end: rhs_end,
                    },
                    lhs: SqlSpan {
                        start: lhs_start,
                        end: lhs_end,
                    },
                    rhs: SqlSpan {
                        start: rhs_start,
                        end: rhs_end,
                    },
                    kind: IntegerArithKind::Mul,
                });
            }
        } else {
            break;
        }
    }
    Ok(t)
}

fn unify_integer_mod(index: usize, a: SqlType, b: SqlType) -> Result<SqlType> {
    let t = unify_types(index, a, b)?;
    if t != SqlType::Null && t != SqlType::Integer {
        return Err(PluginError::invalid_params(format!(
            "statement {index} % requires INTEGER (REAL modulo is not SQL v1)"
        )));
    }
    Ok(SqlType::Integer)
}

fn infer_prefix(scan: &mut TScan<'_>, cx: &mut TypeCx<'_>) -> Result<SqlType> {
    if scan.take_op("+") || scan.take_op("-") {
        let t = infer_prefix(scan, cx)?;
        if t != SqlType::Null && !t.is_numeric() {
            return Err(ty_err(cx.index, "unary +/- requires a numeric type"));
        }
        return Ok(t);
    }
    infer_atom(scan, cx)
}

fn infer_atom(scan: &mut TScan<'_>, cx: &mut TypeCx<'_>) -> Result<SqlType> {
    if scan.take_kw("NULL") {
        return Ok(SqlType::Null);
    }
    if scan.take_kw("TRUE") || scan.take_kw("FALSE") {
        return Ok(SqlType::Boolean);
    }
    if scan.take_kw("CAST") {
        if !scan.take_byte(b'(') {
            return Err(ty_err(cx.index, "CAST ("));
        }
        let from = infer_expr(scan, cx)?;
        if !scan.take_kw("AS") {
            return Err(ty_err(cx.index, "CAST AS"));
        }
        let ty_ident = scan
            .read_ident()
            .ok_or_else(|| ty_err(cx.index, "CAST type"))?;
        let to =
            SqlType::from_column_ident(&ty_ident).ok_or_else(|| ty_err(cx.index, "CAST type"))?;
        if !scan.take_byte(b')') {
            return Err(ty_err(cx.index, "CAST )"));
        }
        if !cast_is_legal(from, to) {
            return Err(PluginError::invalid_params(format!(
                "statement {} CAST from {} to {} is not SQL v1",
                cx.index,
                from.as_str(),
                to.as_str()
            )));
        }
        return Ok(to);
    }
    if scan.take_kw("CASE") {
        return infer_case(scan, cx);
    }
    if scan.take_kw("EXISTS") {
        if !scan.take_byte(b'(') {
            return Err(ty_err(cx.index, "EXISTS ("));
        }
        let inner = scan
            .take_balanced_inner()
            .ok_or_else(|| ty_err(cx.index, "EXISTS subquery"))?;
        let saved = cx.require_named_derived;
        cx.require_named_derived = false;
        let _ = typecheck_select_list_only(inner, scan.base_of(inner), cx)?;
        cx.require_named_derived = saved;
        return Ok(SqlType::Boolean);
    }
    if scan.take_byte(b'?') {
        let ty = cx.binds.get(cx.bind_i).copied().unwrap_or(SqlType::Null);
        cx.bind_i += 1;
        return Ok(ty);
    }
    if scan.take_blob_hex() {
        return Ok(SqlType::Blob);
    }
    if scan.take_string() {
        let end = scan.i;
        let start = string_lit_start(scan, end);
        if !cx.suppress_string_collate {
            cx.note_text(scan.abs(start), scan.abs(end));
        }
        return Ok(SqlType::Text);
    }
    if scan.take_number() {
        return Ok(if scan.took_real {
            SqlType::Real
        } else {
            SqlType::Integer
        });
    }
    if scan.take_byte(b'(') {
        let inner = scan
            .take_balanced_inner()
            .ok_or_else(|| ty_err(cx.index, "parenthesized expr"))?;
        let mut body = TScan {
            sql: inner,
            i: 0,
            took_real: false,
            base: scan.base_of(inner),
        };
        if body.peek_kw("SELECT") || body.peek_kw("WITH") || body.peek_kw("VALUES") {
            let saved_named = cx.require_named_derived;
            cx.require_named_derived = false;
            let cols = infer_select(&mut body, cx)?;
            cx.require_named_derived = saved_named;
            return Ok(cols.first().map(|(_, t)| *t).unwrap_or(SqlType::Null));
        }
        return infer_expr(&mut body, cx);
    }
    let ident_start = {
        scan.skip();
        scan.i
    };
    let Some(name) = scan.read_ident() else {
        return Err(ty_err(cx.index, "expression atom"));
    };
    if scan.take_byte(b'(') {
        return infer_call(scan, cx, &name, ident_start);
    }
    if scan.take_byte(b'.') {
        let col_start = scan.i;
        let col = scan
            .read_ident()
            .ok_or_else(|| ty_err(cx.index, "qualified column"))?;
        let ty = lookup_column(cx, Some(&name), &col)?;
        if ty.is_text() {
            cx.note_text(scan.abs(ident_start), scan.abs(scan.i));
        }
        let _ = col_start;
        return Ok(ty);
    }
    let ty = lookup_column(cx, None, &name)?;
    if ty.is_text() {
        cx.note_text(scan.abs(ident_start), scan.abs(scan.i));
    }
    Ok(ty)
}

fn string_lit_start(scan: &TScan<'_>, end: usize) -> usize {
    let bytes = scan.sql.as_bytes();
    if end == 0 {
        return 0;
    }
    let mut i = end.saturating_sub(1);
    if bytes.get(i) == Some(&b'\'') {
        i = i.saturating_sub(1);
    }
    while i > 0 {
        if bytes[i] == b'\'' {
            if bytes.get(i.saturating_sub(1)) == Some(&b'\'') {
                i = i.saturating_sub(2);
                continue;
            }
            return i;
        }
        i -= 1;
    }
    0
}

fn infer_call(
    scan: &mut TScan<'_>,
    cx: &mut TypeCx<'_>,
    name: &str,
    ident_start: usize,
) -> Result<SqlType> {
    let mut args = Vec::new();
    cx.functions.insert(name.to_ascii_lowercase());
    if name == "json_extract" {
        if !scan.peek_byte(b')') {
            args.push(infer_expr(scan, cx)?);
            if scan.take_byte(b',') {
                cx.suppress_string_collate = true;
                args.push(infer_expr(scan, cx)?);
                cx.suppress_string_collate = false;
                while scan.take_byte(b',') {
                    args.push(infer_expr(scan, cx)?);
                }
            }
        }
    } else if !scan.peek_byte(b')') {
        if name == "count" && scan.take_byte(b'*') {
            args.push(SqlType::Integer);
        } else {
            loop {
                args.push(infer_expr(scan, cx)?);
                if !scan.take_byte(b',') {
                    break;
                }
            }
        }
    }
    if !scan.take_byte(b')') {
        return Err(ty_err(cx.index, "call )"));
    }
    require_sql_v1_helper_arity(cx.index, name, args.len())?;
    match name {
        "ifnull" | "coalesce" | "nullif" => {
            let mut t = SqlType::Null;
            for a in args {
                t = unify_types(cx.index, t, a)?;
            }
            Ok(t)
        }
        "min" | "max" if args.len() >= 2 => {
            let mut t = SqlType::Null;
            for a in args {
                t = unify_types(cx.index, t, a)?;
            }
            Ok(t)
        }
        "min" | "max" if args.len() == 1 => Ok(args[0]),
        "sum" => {
            let t = args.first().copied().unwrap_or(SqlType::Null);
            if t != SqlType::Null && t != SqlType::Integer {
                return Err(PluginError::invalid_params(format!(
                    "statement {} sum() requires INTEGER (REAL-sum is not SQL v1)",
                    cx.index
                )));
            }
            Ok(SqlType::Integer)
        }
        "avg" => {
            let t = args.first().copied().unwrap_or(SqlType::Null);
            if t != SqlType::Null && !t.is_numeric() {
                return Err(ty_err(cx.index, "avg() requires INTEGER or REAL"));
            }
            Ok(SqlType::Real)
        }
        "abs" => {
            let t = args.first().copied().unwrap_or(SqlType::Null);
            if t != SqlType::Null && !t.is_numeric() {
                return Err(ty_err(cx.index, "abs() requires INTEGER or REAL"));
            }
            let out = if t == SqlType::Null {
                SqlType::Integer
            } else {
                t
            };
            if out == SqlType::Integer {
                let full = SqlSpan {
                    start: scan.abs(ident_start),
                    end: scan.abs(scan.i),
                };
                cx.note_arith(IntegerArithSite {
                    full,
                    lhs: full,
                    rhs: full,
                    kind: IntegerArithKind::Abs,
                });
            }
            Ok(out)
        }
        "round" => {
            if args.is_empty() || args.len() > 2 {
                return Err(ty_err(cx.index, "round() requires 1 or 2 arguments"));
            }
            let t = args.first().copied().unwrap_or(SqlType::Null);
            if t != SqlType::Null && !t.is_numeric() {
                return Err(ty_err(cx.index, "round() requires INTEGER or REAL"));
            }
            if let Some(prec) = args.get(1) {
                if *prec != SqlType::Null && *prec != SqlType::Integer {
                    return Err(ty_err(cx.index, "round() precision must be INTEGER"));
                }
            }
            Ok(SqlType::Real)
        }
        "count" => Ok(SqlType::Integer),
        "lower" | "upper" => {
            require_textish(cx.index, args.first().copied().unwrap_or(SqlType::Null))?;
            Ok(SqlType::Text)
        }
        "trim" => {
            for a in &args {
                require_textish(cx.index, *a)?;
            }
            Ok(SqlType::Text)
        }
        "replace" => {
            if args.len() != 3 {
                return Err(ty_err(cx.index, "replace() requires 3 TEXT arguments"));
            }
            for a in &args {
                require_textish(cx.index, *a)?;
            }
            Ok(SqlType::Text)
        }
        "substr" => {
            if args.len() != 2 && args.len() != 3 {
                return Err(ty_err(cx.index, "substr() requires 2 or 3 arguments"));
            }
            require_textish(cx.index, args.first().copied().unwrap_or(SqlType::Null))?;
            let idx = args.get(1).copied().unwrap_or(SqlType::Null);
            if idx != SqlType::Null && idx != SqlType::Integer {
                return Err(ty_err(cx.index, "substr() index must be INTEGER"));
            }
            if let Some(len) = args.get(2) {
                if *len != SqlType::Null && *len != SqlType::Integer {
                    return Err(ty_err(cx.index, "substr() length must be INTEGER"));
                }
            }
            Ok(SqlType::Text)
        }
        "length" => {
            let t = args.first().copied().unwrap_or(SqlType::Null);
            if t != SqlType::Null && t != SqlType::Text && t != SqlType::Blob {
                return Err(ty_err(cx.index, "length() requires TEXT or BLOB"));
            }
            Ok(SqlType::Integer)
        }
        "json_extract" => {
            if args.len() != 2 {
                return Err(ty_err(cx.index, "json_extract() requires 2 arguments"));
            }
            require_textish(cx.index, args.get(1).copied().unwrap_or(SqlType::Null))?;
            Ok(SqlType::Text)
        }
        "json_object" => Ok(SqlType::Text),
        "json_valid" => Ok(SqlType::Integer),
        "json" => {
            if args.len() != 1 {
                return Err(ty_err(cx.index, "json() requires 1 argument"));
            }
            Ok(SqlType::Text)
        }
        "julianday" => {
            if args.len() != 1 {
                return Err(ty_err(cx.index, "julianday() requires 1 argument"));
            }
            Ok(SqlType::Real)
        }
        _ => Err(ty_err(cx.index, &format!("unknown helper {name}"))),
    }
}

fn infer_case(scan: &mut TScan<'_>, cx: &mut TypeCx<'_>) -> Result<SqlType> {
    let simple = if !scan.peek_kw("WHEN") {
        Some(infer_expr(scan, cx)?)
    } else {
        None
    };
    let mut result = SqlType::Null;
    while scan.take_kw("WHEN") {
        let w = infer_expr(scan, cx)?;
        if let Some(scrutinee) = simple {
            let _ = unify_types(cx.index, scrutinee, w)?;
        } else {
            require_booleanish(cx.index, w, "CASE WHEN")?;
        }
        if !scan.take_kw("THEN") {
            return Err(ty_err(cx.index, "CASE THEN"));
        }
        let t = infer_expr(scan, cx)?;
        result = unify_types(cx.index, result, t)?;
    }
    if scan.take_kw("ELSE") {
        let t = infer_expr(scan, cx)?;
        result = unify_types(cx.index, result, t)?;
    }
    if !scan.take_kw("END") {
        return Err(ty_err(cx.index, "CASE END"));
    }
    Ok(result)
}

fn lookup_column(cx: &mut TypeCx<'_>, table: Option<&str>, column: &str) -> Result<SqlType> {
    if let Some(table) = table {
        if let Some(src) = lookup_visible_src(cx, table) {
            return lookup_from_src(cx, &src, column);
        }
        if let Some(cols) = cx.ctes.get(table) {
            if let Some((_, ty)) = cols.iter().find(|(n, _)| n == column) {
                return Ok(*ty);
            }
        }
        if let Some(ty) = cx.env.column_type(table, column) {
            cx.note_physical(table, Some(column));
            return Ok(ty);
        }
        return unknown_column(cx, Some(table), column);
    }
    let mut found = None;
    for src in cx.from.values().cloned().collect::<Vec<_>>() {
        if let Ok(ty) = lookup_from_src(cx, &src, column) {
            if found.is_some() {
                return Err(PluginError::invalid_params(format!(
                    "statement {} unqualified column {column} is ambiguous",
                    cx.index
                )));
            }
            found = Some(ty);
        }
    }
    if let Some(ty) = found {
        return Ok(ty);
    }
    let outer_sources: Vec<Vec<FromSrc>> = cx
        .outer_from
        .iter()
        .rev()
        .map(|outer| outer.values().cloned().collect())
        .collect();
    for sources in outer_sources {
        let mut outer_found = None;
        for src in sources {
            if let Ok(ty) = lookup_from_src(cx, &src, column) {
                if outer_found.is_some() {
                    return Err(PluginError::invalid_params(format!(
                        "statement {} unqualified column {column} is ambiguous",
                        cx.index
                    )));
                }
                outer_found = Some(ty);
            }
        }
        if let Some(ty) = outer_found {
            return Ok(ty);
        }
    }
    unknown_column(cx, None, column)
}

fn lookup_visible_src(cx: &TypeCx<'_>, table: &str) -> Option<FromSrc> {
    if let Some(src) = cx.from.get(table) {
        return Some(src.clone());
    }
    for outer in cx.outer_from.iter().rev() {
        if let Some(src) = outer.get(table) {
            return Some(src.clone());
        }
    }
    None
}

fn lookup_from_src(cx: &mut TypeCx<'_>, src: &FromSrc, column: &str) -> Result<SqlType> {
    match src {
        FromSrc::Cte(name) => {
            if let Some(cols) = cx.ctes.get(name) {
                if let Some((_, ty)) = cols.iter().find(|(n, _)| n == column) {
                    return Ok(*ty);
                }
            }
            unknown_column(cx, Some(name), column)
        }
        FromSrc::Physical(table) => {
            if let Some(ty) = cx.env.column_type(table, column) {
                cx.note_physical(table, Some(column));
                return Ok(ty);
            }
            unknown_column(cx, Some(table), column)
        }
    }
}

fn unknown_column(cx: &TypeCx<'_>, table: Option<&str>, column: &str) -> Result<SqlType> {
    let msg = match table {
        Some(table) => format!(
            "statement {} names unknown column {table}.{column}",
            cx.index
        ),
        None => format!("statement {} names unknown column {column}", cx.index),
    };
    Err(PluginError::invalid_params(msg))
}

fn require_textish(index: usize, ty: SqlType) -> Result<()> {
    if ty == SqlType::Null || ty == SqlType::Text {
        Ok(())
    } else {
        Err(PluginError::invalid_params(format!(
            "statement {index} TEXT operator applied to {}",
            ty.as_str()
        )))
    }
}

fn unify_numeric(index: usize, a: SqlType, b: SqlType) -> Result<SqlType> {
    let t = unify_types(index, a, b)?;
    if t != SqlType::Null && !t.is_numeric() {
        return Err(PluginError::invalid_params(format!(
            "statement {index} arithmetic requires INTEGER or REAL"
        )));
    }
    Ok(t)
}

fn ty_err(index: usize, msg: &str) -> PluginError {
    PluginError::invalid_params(format!("statement {index} {msg}"))
}

#[cfg(not(feature = "host"))]
fn escape_sql_str(s: &str) -> String {
    s.replace('\'', "''")
}

fn skip_ws(s: &str) -> &str {
    s.trim_start()
}

fn starts_kw(s: &str, kw: &str) -> bool {
    let s = skip_ws(s);
    if s.len() < kw.len() || !s.is_char_boundary(kw.len()) {
        return false;
    }
    s[..kw.len()].eq_ignore_ascii_case(kw)
        && s.as_bytes()
            .get(kw.len())
            .is_none_or(|b| !b.is_ascii_alphanumeric() && *b != b'_')
}

fn skip_kw<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let s = skip_ws(s);
    if !starts_kw(s, kw) {
        return None;
    }
    Some(s[kw.len()..].trim_start())
}

fn read_ident(s: &str) -> Option<(String, &str)> {
    let s = skip_ws(s);
    let mut chars = s.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() && first != '_' {
        return None;
    }
    let mut n = first.len_utf8();
    for c in chars {
        if c.is_ascii_alphanumeric() || c == '_' {
            n += c.len_utf8();
        } else {
            break;
        }
    }
    Some((s[..n].to_ascii_lowercase(), &s[n..]))
}

fn balanced_inner(s: &str) -> Option<&str> {
    if !s.starts_with('(') {
        return None;
    }
    let mut depth = 0i32;
    let mut i = 0;
    while i < s.len() {
        let b = s.as_bytes()[i];
        if b == b'\'' {
            i += 1;
            while i < s.len() {
                if s.as_bytes()[i] == b'\'' {
                    if s.as_bytes().get(i + 1) == Some(&b'\'') {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if b == b'(' {
            depth += 1;
        } else if b == b')' {
            depth -= 1;
            if depth == 0 {
                return Some(&s[1..i]);
            }
        }
        i += 1;
    }
    None
}

fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut depth = 0i32;
    let mut i = 0;
    while i < s.len() {
        let b = s.as_bytes()[i];
        if b == b'\'' {
            i += 1;
            while i < s.len() {
                if s.as_bytes()[i] == b'\'' {
                    if s.as_bytes().get(i + 1) == Some(&b'\'') {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if b == b'(' {
            depth += 1;
        } else if b == b')' {
            depth -= 1;
        } else if b == b',' && depth == 0 {
            out.push(&s[start..i]);
            start = i + 1;
        }
        i += 1;
    }
    out.push(&s[start..]);
    out
}

fn subslice_offset(outer: &str, inner: &str) -> Option<usize> {
    let outer_addr = outer.as_ptr() as usize;
    let inner_addr = inner.as_ptr() as usize;
    if inner_addr >= outer_addr && inner_addr + inner.len() <= outer_addr + outer.len() {
        Some(inner_addr - outer_addr)
    } else {
        None
    }
}

struct TScan<'a> {
    sql: &'a str,
    i: usize,
    took_real: bool,
    base: usize,
}

impl<'a> TScan<'a> {
    fn abs(&self, local: usize) -> usize {
        self.base.saturating_add(local)
    }

    fn base_of(&self, inner: &str) -> usize {
        subslice_offset(self.sql, inner)
            .map(|off| self.base.saturating_add(off))
            .unwrap_or(self.base)
    }

    fn read_ident_if_not_clause(&mut self) -> Option<String> {
        if self.peek_select_clause_kw()
            || self.peek_kw("JOIN")
            || self.peek_kw("INNER")
            || self.peek_kw("LEFT")
            || self.peek_kw("CROSS")
            || self.peek_kw("ON")
        {
            return None;
        }
        self.read_ident()
    }
    fn skip(&mut self) {
        while self.i < self.sql.len() {
            let rest = &self.sql[self.i..];
            if rest.starts_with("--") {
                self.i += rest.find('\n').unwrap_or(rest.len());
                continue;
            }
            if rest.starts_with("/*") {
                self.i += rest.find("*/").map(|p| p + 2).unwrap_or(rest.len());
                continue;
            }
            let Some(ch) = rest.chars().next() else {
                break;
            };
            if ch.is_whitespace() {
                self.i += ch.len_utf8();
                continue;
            }
            break;
        }
    }

    fn take_kw(&mut self, kw: &str) -> bool {
        self.skip();
        if starts_kw(&self.sql[self.i..], kw) {
            self.i += kw.len();
            true
        } else {
            false
        }
    }

    fn peek_kw(&mut self, kw: &str) -> bool {
        self.skip();
        starts_kw(&self.sql[self.i..], kw)
    }

    fn peek_select_clause_kw(&mut self) -> bool {
        [
            "FROM",
            "WHERE",
            "GROUP",
            "HAVING",
            "ORDER",
            "LIMIT",
            "OFFSET",
            "UNION",
            "RETURNING",
        ]
        .iter()
        .any(|kw| self.peek_kw(kw))
    }

    fn take_byte(&mut self, b: u8) -> bool {
        self.skip();
        if self.sql.as_bytes().get(self.i) == Some(&b) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn peek_byte(&mut self, b: u8) -> bool {
        self.skip();
        self.sql.as_bytes().get(self.i) == Some(&b)
    }

    fn take_op(&mut self, op: &str) -> bool {
        self.skip();
        if self.sql[self.i..].starts_with(op) {
            let after = self.i + op.len();
            if op.chars().all(|c| c.is_ascii_alphabetic()) {
                return false;
            }
            self.i = after;
            true
        } else {
            false
        }
    }

    fn read_ident(&mut self) -> Option<String> {
        self.skip();
        let (name, rest) = read_ident(&self.sql[self.i..])?;
        self.i = self.sql.len() - rest.len();
        Some(name)
    }

    fn take_balanced_inner(&mut self) -> Option<&'a str> {
        // Caller consumed the opening `(`. Do not skip whitespace first or
        // `i - 1` would no longer be that paren (`(\n SELECT …)`).
        let from = if self.i > 0 && self.sql.as_bytes()[self.i - 1] == b'(' {
            self.i - 1
        } else {
            return None;
        };
        let inner = balanced_inner(&self.sql[from..])?;
        self.i = from + 1 + inner.len() + 1;
        Some(inner)
    }

    fn take_string(&mut self) -> bool {
        self.skip();
        if self.sql.as_bytes().get(self.i) != Some(&b'\'') {
            return false;
        }
        self.i += 1;
        while self.i < self.sql.len() {
            if self.sql.as_bytes()[self.i] == b'\'' {
                if self.sql.as_bytes().get(self.i + 1) == Some(&b'\'') {
                    self.i += 2;
                    continue;
                }
                self.i += 1;
                return true;
            }
            self.i += 1;
        }
        true
    }

    fn take_blob_hex(&mut self) -> bool {
        self.skip();
        let rest = &self.sql[self.i..];
        if rest.len() < 3 {
            return false;
        }
        if !rest.as_bytes()[0].eq_ignore_ascii_case(&b'x') || rest.as_bytes()[1] != b'\'' {
            return false;
        }
        self.i += 2;
        while self.i < self.sql.len() {
            let b = self.sql.as_bytes()[self.i];
            if b == b'\'' {
                self.i += 1;
                return true;
            }
            if !b.is_ascii_hexdigit() {
                return false;
            }
            self.i += 1;
        }
        false
    }

    fn take_number(&mut self) -> bool {
        self.skip();
        self.took_real = false;
        let bytes = self.sql.as_bytes();
        let mut i = self.i;
        if i >= bytes.len() {
            return false;
        }
        if bytes[i] == b'+' || bytes[i] == b'-' {
            i += 1;
        }
        let start = i;
        let mut saw = false;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            saw = true;
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'.' {
            self.took_real = true;
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                saw = true;
                i += 1;
            }
        }
        if !saw || i == start {
            return false;
        }
        self.i = i;
        true
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use crate::{DbPlanStatementKind, DbResultSelection};

    fn json_object_n_pairs(n: usize) -> String {
        let args: Vec<String> = (0..n)
            .flat_map(|i| [format!("'k{i:02}'"), format!("'v{i:02}'")])
            .collect();
        format!("json_object({})", args.join(", "))
    }

    fn stmt(sql: &str) -> TypedDbStatement {
        TypedDbStatement {
            sql: sql.into(),
            parameters: Vec::new(),
            kind: DbPlanStatementKind::Select,
            max_rows: 0,
            result_selection: DbResultSelection::Rows,
        }
    }

    fn req(sql: &str) -> ExecuteRequest {
        ExecuteRequest {
            operation_id: "t".into(),
            request_hash: String::new(),
            statements: vec![stmt(sql)],
            deadline_unix_ms: 0,
        }
    }

    #[test]
    fn parse_identity_column_is_not_always_id() {
        let schema = parse_create_table_schema(
            "CREATE TABLE IF NOT EXISTS t (pk INTEGER PRIMARY KEY AUTOINCREMENT, n INTEGER)",
        )
        .expect("schema");
        assert_eq!(schema.identity_column.as_deref(), Some("pk"));
        let lowered = parse_create_table_schema(
            "CREATE TABLE IF NOT EXISTS t (id BIGINT PRIMARY KEY, body TEXT, blob BYTEA, r DOUBLE PRECISION)",
        )
        .expect("postgres-lowered schema");
        assert_eq!(
            lowered.columns,
            vec![
                ("id".into(), SqlType::Integer),
                ("body".into(), SqlType::Text),
                ("blob".into(), SqlType::Blob),
                ("r".into(), SqlType::Real),
            ]
        );
    }

    #[test]
    fn cast_matrix_rejects_text_to_integer() {
        assert!(!cast_is_legal(SqlType::Text, SqlType::Integer));
        assert!(cast_is_legal(SqlType::Integer, SqlType::Real));
        assert!(cast_is_legal(SqlType::Null, SqlType::Boolean));
    }

    #[test]
    fn json_extract_version_compare_stays_text() {
        let mut env = SqlTypeEnv::new();
        env.insert_column("jobs", "payload", SqlType::Text);
        typecheck_execute_request(
            &req("SELECT 1 FROM jobs WHERE IFNULL(json_extract(payload, '$.v') || '', '') = '1'"),
            &env,
        )
        .unwrap();
        let err = typecheck_execute_request(
            &req(
                "SELECT 1 FROM jobs WHERE IFNULL(CAST(json_extract(payload, '$.v') AS INTEGER), -1) = 1",
            ),
            &env,
        )
        .unwrap_err();
        assert!(err.to_string().contains("CAST"), "{err}");
        let err = typecheck_execute_request(&req("SELECT typed_bump() AS n"), &SqlTypeEnv::new())
            .unwrap_err();
        assert!(
            err.to_string().contains("unknown helper typed_bump"),
            "{err}"
        );
    }

    #[test]
    fn ifnull_mixed_literals_fail_closed() {
        let err = typecheck_execute_request(&req("SELECT IFNULL('x', 0)"), &SqlTypeEnv::new())
            .unwrap_err();
        assert!(err.to_string().contains("incompatible"), "{err}");
    }

    #[test]
    fn union_mixed_types_fail_closed() {
        let err =
            typecheck_execute_request(&req("SELECT 1 UNION ALL SELECT 'x'"), &SqlTypeEnv::new())
                .unwrap_err();
        assert!(err.to_string().contains("incompatible"), "{err}");
    }

    #[test]
    fn catalog_ddl_round_trip_and_unknown_column() {
        let mut env = SqlTypeEnv::new();
        apply_schema_sql_to_env(
            &mut env,
            "CREATE TABLE IF NOT EXISTS typed (n INTEGER, body TEXT)",
        );
        assert_eq!(env.column_type("typed", "body"), Some(SqlType::Text));
        let err = typecheck_execute_request(&req("SELECT missing FROM typed"), &env).unwrap_err();
        assert!(err.to_string().contains("unknown column"), "{err}");
        let from_ddl = sql_type_env_from_canonical_statements([
            "CREATE TABLE a (id INTEGER PRIMARY KEY)",
            "CREATE TABLE b (body TEXT)",
        ]);
        assert_eq!(from_ddl.column_type("a", "id"), Some(SqlType::Integer));
        assert_eq!(from_ddl.column_type("b", "body"), Some(SqlType::Text));
    }

    #[test]
    fn insert_with_source_typechecks() {
        let mut env = SqlTypeEnv::new();
        apply_schema_sql_to_env(&mut env, "CREATE TABLE IF NOT EXISTS ign_sel (id INTEGER)");
        typecheck_execute_request(
            &req("INSERT OR IGNORE INTO ign_sel (id) WITH s(id) AS (SELECT 1) SELECT * FROM s"),
            &env,
        )
        .expect("INSERT … WITH");
    }

    #[test]
    fn sum_real_without_cast_fails_closed() {
        let mut env = SqlTypeEnv::new();
        apply_schema_sql_to_env(&mut env, "CREATE TABLE IF NOT EXISTS typed (r REAL)");
        let err = typecheck_execute_request(&req("SELECT sum(r) FROM typed"), &env).unwrap_err();
        assert!(err.to_string().contains("sum()"), "{err}");
    }

    #[test]
    fn insert_select_and_update_unify_destinations() {
        let mut env = SqlTypeEnv::new();
        apply_schema_sql_to_env(&mut env, "CREATE TABLE IF NOT EXISTS t (n INTEGER)");
        let err = typecheck_execute_request(&req("INSERT INTO t(n) SELECT 'x'"), &env).unwrap_err();
        assert!(err.to_string().contains("incompatible"), "{err}");
        let err = typecheck_execute_request(&req("UPDATE t SET n = 'x'"), &env).unwrap_err();
        assert!(err.to_string().contains("incompatible"), "{err}");
    }

    #[test]
    fn boolean_contexts_reject_integers() {
        let err =
            typecheck_execute_request(&req("SELECT 1 WHERE 1"), &SqlTypeEnv::new()).unwrap_err();
        assert!(err.to_string().contains("BOOLEAN"), "{err}");
        let err = typecheck_execute_request(&req("SELECT NOT 1"), &SqlTypeEnv::new()).unwrap_err();
        assert!(err.to_string().contains("BOOLEAN"), "{err}");
        let err = typecheck_execute_request(
            &req("SELECT CASE WHEN 1 THEN 1 ELSE 0 END"),
            &SqlTypeEnv::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("BOOLEAN"), "{err}");
        let mut env = SqlTypeEnv::new();
        apply_schema_sql_to_env(
            &mut env,
            "CREATE TABLE IF NOT EXISTS checked (n INTEGER CHECK (n))",
        );
        let err = typecheck_execute_request(
            &req("CREATE TABLE IF NOT EXISTS checked (n INTEGER CHECK (n))"),
            &env,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("BOOLEAN") || err.to_string().contains("does not match"),
            "{err}"
        );
        let fresh = SqlTypeEnv::new();
        let err = typecheck_execute_request(
            &req("CREATE TABLE IF NOT EXISTS checked (n INTEGER CHECK (n))"),
            &fresh,
        )
        .unwrap_err();
        assert!(err.to_string().contains("BOOLEAN"), "{err}");
    }

    #[test]
    fn modulo_real_and_helper_signatures_fail_closed() {
        let err =
            typecheck_execute_request(&req("SELECT 1.5 % 2"), &SqlTypeEnv::new()).unwrap_err();
        assert!(err.to_string().contains("%"), "{err}");
        let err =
            typecheck_execute_request(&req("SELECT replace(1, 'a', 'b')"), &SqlTypeEnv::new())
                .unwrap_err();
        assert!(err.to_string().contains("TEXT"), "{err}");
        let err =
            typecheck_execute_request(&req("SELECT substr('x')"), &SqlTypeEnv::new()).unwrap_err();
        assert!(
            err.to_string().contains("substr") && err.to_string().contains("arity"),
            "{err}"
        );
        let err = typecheck_execute_request(&req("SELECT round(1, 'x')"), &SqlTypeEnv::new())
            .unwrap_err();
        assert!(err.to_string().contains("round()"), "{err}");
    }

    #[test]
    fn alias_cte_and_recursive_anchor_first() {
        let mut env = SqlTypeEnv::new();
        apply_schema_sql_to_env(
            &mut env,
            "CREATE TABLE IF NOT EXISTS books (id INTEGER, title TEXT)",
        );
        typecheck_execute_request(&req("SELECT b.id FROM books AS b"), &env).unwrap();
        typecheck_execute_request(
            &req("WITH c(x) AS (SELECT id FROM books) SELECT x FROM c"),
            &env,
        )
        .unwrap();
        typecheck_execute_request(
            &req(
                "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3) SELECT n FROM t",
            ),
            &SqlTypeEnv::new(),
        )
        .unwrap();
        let err = typecheck_execute_request(
            &req("WITH c AS (SELECT 1) SELECT * FROM c"),
            &SqlTypeEnv::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("derived columns"), "{err}");
        let err = typecheck_execute_request(
            &req("WITH c(a, a) AS (SELECT 1, 2) SELECT a FROM c"),
            &SqlTypeEnv::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("duplicate"), "{err}");
        typecheck_execute_request(
            &req("SELECT operation_id FROM db_atomic_receipts"),
            &sql_host_bookkeeping_type_env(),
        )
        .unwrap();
        typecheck_execute_request(
            &req("UPDATE books SET id = (\
                    SELECT CASE o.status WHEN 'ok' THEN 1 ELSE 0 END FROM (\
                        SELECT CASE WHEN NOT EXISTS (SELECT 1 FROM books WHERE id = 1) \
                          THEN 'missing' ELSE 'ok' END AS status\
                    ) o\
                 )"),
            &env,
        )
        .unwrap();
        apply_schema_sql_to_env(
            &mut env,
            "CREATE TABLE IF NOT EXISTS identities (id INTEGER, user_id INTEGER)",
        );
        apply_schema_sql_to_env(
            &mut env,
            "CREATE TABLE IF NOT EXISTS users (id INTEGER, role TEXT, status TEXT)",
        );
        typecheck_execute_request(
            &req(
                "SELECT CASE \
                   WHEN NOT EXISTS (SELECT 1 FROM users WHERE id = ?) THEN 'notFound' \
                   WHEN ((SELECT role FROM users WHERE id = ?) = 'owner' \
                     AND (SELECT status FROM users WHERE id = ?) = 'active' \
                     AND (SELECT COUNT(*) FROM users WHERE role = 'owner' AND status = 'active') <= 1) \
                   THEN 'lastOwner' ELSE 'ok' END AS status",
            ),
            &env,
        )
        .unwrap();
        typecheck_execute_request(
            &req(
                "UPDATE books SET id = (\
                    SELECT CASE o.status WHEN 'ok' THEN 1 ELSE 0 END FROM (\
                        SELECT CASE \
                          WHEN NOT EXISTS (SELECT 1 FROM users WHERE id = ?) THEN 'notFound' \
                          WHEN ((SELECT role FROM users WHERE id = ?) = 'owner' \
                            AND (SELECT status FROM users WHERE id = ?) = 'active' \
                            AND (SELECT COUNT(*) FROM users WHERE role = 'owner' AND status = 'active') <= 1) \
                          THEN 'lastOwner' ELSE 'ok' END AS status\
                    ) o\
                 )",
            ),
            &env,
        )
        .unwrap();
        typecheck_execute_request(
            &req("SELECT i.user_id FROM books t \
                 JOIN identities i ON i.id = t.id"),
            &env,
        )
        .unwrap();
        typecheck_execute_request(
            &req("UPDATE identities SET user_id = 1 WHERE NOT EXISTS (\
                    SELECT 1 FROM identities i \
                    WHERE i.id = identities.id \
                      AND i.user_id IS NOT NULL \
                      AND NOT EXISTS (SELECT 1 FROM users WHERE id = i.user_id)\
                 )"),
            &env,
        )
        .expect("correlated nested EXISTS alias");
        typecheck_execute_request(
            &req(
                "DELETE FROM books WHERE id IN ( SELECT i.user_id FROM identities i JOIN users u ON u.id = i.user_id WHERE i.user_id IS NOT NULL )",
            ),
            &env,
        )
        .expect("IN subquery after space");
    }

    #[test]
    fn create_if_not_exists_fingerprint_noop_or_reject() {
        let mut env = SqlTypeEnv::new();
        let sql = "CREATE TABLE IF NOT EXISTS t (n INTEGER, body TEXT)";
        let proofs = typecheck_execute_request_resolved(&req(sql), &env).unwrap();
        assert!(matches!(
            proofs[0].schema_action,
            SchemaAction::Create { noop: false, .. }
        ));
        apply_schema_sql_to_env(&mut env, sql);
        let proofs = typecheck_execute_request_resolved(&req(sql), &env).unwrap();
        assert!(matches!(
            proofs[0].schema_action,
            SchemaAction::Create { noop: true, .. }
        ));
        let companions = catalog_companions_for_action(sql, Some(&proofs[0].schema_action));
        assert!(
            companions.is_empty(),
            "fingerprint-matching IF NOT EXISTS CREATE must not emit catalog companions: {companions:?}"
        );
        let err = typecheck_execute_request(&req("CREATE TABLE IF NOT EXISTS t (n TEXT)"), &env)
            .unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
    }

    #[test]
    fn order_by_select_list_alias_without_from() {
        let mut env = SqlTypeEnv::new();
        apply_schema_sql_to_env(&mut env, "CREATE TABLE IF NOT EXISTS ign_sel (id INTEGER)");
        typecheck_execute_request(
            &req("INSERT OR IGNORE INTO ign_sel (id) SELECT 1 AS id ORDER BY id LIMIT 1 RETURNING id"),
            &env,
        )
        .expect("ORDER BY select-list alias");
        typecheck_execute_request(
            &req("CREATE INDEX IF NOT EXISTS idx_ign_sel_id ON ign_sel (id)"),
            &env,
        )
        .expect("CREATE INDEX");
        typecheck_execute_request(
            &req(
                "SELECT CAST((julianday('2020-01-02') - julianday('2020-01-01')) * 86400000 AS INTEGER)",
            ),
            &SqlTypeEnv::new(),
        )
        .expect("host julianday helper");
    }

    #[test]
    fn identifier_helpers_stay_under_63_bytes() {
        let table = "t";
        let fn_name = postgres_identity_function_name(table);
        let trig = postgres_identity_trigger_name(table);
        assert!(fn_name.len() <= SQL_V1_MAX_IDENT_BYTES);
        assert!(trig.len() <= SQL_V1_MAX_IDENT_BYTES);
        assert!(fn_name.starts_with(POSTGRES_IDENT_FN_PREFIX));
        assert!(trig.starts_with(POSTGRES_IDENT_TRIGGER_PREFIX));
        assert_ne!(fn_name, trig);
    }

    #[test]
    fn substr_index_must_be_integer() {
        let err =
            typecheck_execute_request(&req("SELECT substr('hello', 'x')"), &SqlTypeEnv::new())
                .unwrap_err();
        assert!(err.to_string().contains("INTEGER"), "{err}");
    }

    #[test]
    fn reserved_catalog_relation_missing_is_structured() {
        assert!(reserved_catalog_relation_missing(
            "no such table: bookclerk_sql_catalog",
            SQL_CATALOG_TABLE
        ));
        assert!(reserved_catalog_relation_missing(
            "no such table: main.bookclerk_sql_catalog",
            SQL_CATALOG_TABLE
        ));
        assert!(reserved_catalog_relation_missing(
            r#"relation "bookclerk_sql_schema" does not exist"#,
            SQL_SCHEMA_TABLE
        ));
        assert!(
            !reserved_catalog_relation_missing(
                r#"column "ordinal" does not exist"#,
                SQL_CATALOG_TABLE
            ),
            "missing column must not look like a missing catalog table"
        );
        assert!(!reserved_catalog_relation_missing(
            r#"relation "other" does not exist"#,
            SQL_CATALOG_TABLE
        ));
        assert_eq!(sql_catalog_page_rows(0), FIRST_PARTY_MAX_RESULT_ROWS);
        assert_eq!(sql_catalog_page_rows(2), 2);
        assert_eq!(
            sql_catalog_page_rows(FIRST_PARTY_MAX_RESULT_ROWS.saturating_mul(4)),
            FIRST_PARTY_MAX_RESULT_ROWS
        );
        let empty = ExecuteReply {
            operation_id: "c".into(),
            statements: Vec::new(),
            timing: crate::DbTiming::default(),
        };
        assert!(catalog_page_statement(&empty).is_err());
    }

    #[test]
    fn select_star_records_physical_wildcard_and_columns() {
        let mut env = SqlTypeEnv::new();
        env.insert_table(
            "books",
            [
                ("id".into(), SqlType::Integer),
                ("token".into(), SqlType::Text),
            ],
        );
        for sql in [
            "SELECT * FROM books",
            "SELECT books.* FROM books",
            "SELECT b.* FROM books b",
        ] {
            let proofs = typecheck_execute_request_resolved(&req(sql), &env).unwrap();
            let accesses = &proofs[0].physical_accesses;
            assert!(
                accesses.iter().any(|a| {
                    a.table == "books" && a.column.as_deref() == Some(PHYSICAL_STAR_COLUMN)
                }),
                "{sql}: {accesses:?}"
            );
            assert!(
                accesses
                    .iter()
                    .any(|a| a.table == "books" && a.column.as_deref() == Some("id")),
                "{sql}: {accesses:?}"
            );
            assert!(
                accesses
                    .iter()
                    .any(|a| a.table == "books" && a.column.as_deref() == Some("token")),
                "{sql}: {accesses:?}"
            );
        }
        let cte = typecheck_execute_request_resolved(
            &req("WITH secret AS (SELECT id FROM books) SELECT * FROM secret"),
            &env,
        )
        .unwrap();
        assert!(
            !cte[0].physical_accesses.iter().any(|a| {
                a.table == "secret" && a.column.as_deref() == Some(PHYSICAL_STAR_COLUMN)
            }),
            "CTE star must not mark a physical wildcard: {:?}",
            cte[0].physical_accesses
        );
    }

    #[test]
    fn integer_arith_sites_are_recorded() {
        let proofs = typecheck_execute_request_resolved(
            &req("SELECT 1 + 2, abs(-3), 4 * 5"),
            &SqlTypeEnv::new(),
        )
        .unwrap();
        assert!(
            proofs[0]
                .integer_arith_sites
                .iter()
                .any(|s| s.kind == IntegerArithKind::Add),
            "{:?}",
            proofs[0].integer_arith_sites
        );
        assert!(proofs[0]
            .integer_arith_sites
            .iter()
            .any(|s| s.kind == IntegerArithKind::Abs));
        assert!(proofs[0]
            .integer_arith_sites
            .iter()
            .any(|s| s.kind == IntegerArithKind::Mul));
    }

    #[test]
    fn json_extract_path_is_not_a_text_collate_site() {
        let env = sql_type_env_from_canonical_ddl("CREATE TABLE t (payload TEXT)");
        let sql = "SELECT json_extract(payload, '$.k') FROM t";
        let proofs = typecheck_execute_request_resolved(&req(sql), &env).unwrap();
        for site in &proofs[0].text_collate_sites {
            let piece = &sql[site.span.start..site.span.end];
            assert!(
                !piece.contains("$."),
                "json_extract path must not collate: {piece:?}"
            );
        }
        let cmp = "SELECT '$.ä' < 'a'";
        let cmp_proofs = typecheck_execute_request_resolved(&req(cmp), &SqlTypeEnv::new()).unwrap();
        assert!(
            cmp_proofs[0]
                .text_collate_sites
                .iter()
                .any(|s| { cmp[s.span.start..s.span.end].contains("$.ä") }),
            "{:?}",
            cmp_proofs[0].text_collate_sites
        );
    }

    #[test]
    fn helper_arity_matrix_rejects_wrong_counts() {
        assert!(!sql_v1_helper_arity_ok("abs", 0));
        assert!(!sql_v1_helper_arity_ok("ifnull", 1));
        assert!(!sql_v1_helper_arity_ok("json_object", 1));
        assert!(!sql_v1_helper_arity_ok("json_object", 3));
        assert!(sql_v1_helper_arity_ok("json_object", 2));
        assert!(sql_v1_helper_arity_ok("json_object", 4));
        assert!(sql_v1_helper_arity_ok(
            "json_object",
            SQL_V1_JSON_OBJECT_MAX_ARGS
        ));
        assert!(!sql_v1_helper_arity_ok(
            "json_object",
            SQL_V1_JSON_OBJECT_MAX_ARGS + 2
        ));
        assert!(!sql_v1_helper_arity_ok("json_object", 42));
        assert!(!sql_v1_helper_arity_ok("json_object", 64));
        assert!(!sql_v1_helper_arity_ok("json_object", 65));
        assert_eq!(
            sql_v1_helper_arity("json_object"),
            Some((2, crate::D1_MAX_FUNCTION_ARGS as usize))
        );
        assert!(sql_v1_helper_arity_ok("coalesce", 2));
        for sql in [
            "SELECT abs()",
            "SELECT ifnull(1)",
            "SELECT json_object('k')",
            "SELECT json_object('a', 1, 'b')",
        ] {
            let err = typecheck_execute_request(&req(sql), &SqlTypeEnv::new()).unwrap_err();
            assert!(err.to_string().contains("arity"), "{sql}: {err}");
        }
    }

    #[test]
    fn json_object_portable_arity_is_32_args() {
        assert!(sql_v1_helper_arity_ok("json_object", 2));
        assert!(sql_v1_helper_arity_ok("json_object", 32));
        assert!(!sql_v1_helper_arity_ok("json_object", 34));
        assert!(!sql_v1_helper_arity_ok("json_object", 33));
        typecheck_execute_request(&req("SELECT json_object('a', 1)"), &SqlTypeEnv::new())
            .expect("2 args");
        typecheck_execute_request(
            &req(&format!("SELECT {}", json_object_n_pairs(16))),
            &SqlTypeEnv::new(),
        )
        .expect("32 args = 16 pairs");
        for sql in [
            format!("SELECT {}", json_object_n_pairs(17)),
            "SELECT json_object('k')".into(),
            "SELECT json_object('a', 1, 'b')".into(),
        ] {
            let err = typecheck_execute_request(&req(&sql), &SqlTypeEnv::new()).unwrap_err();
            assert!(err.to_string().contains("arity"), "{sql}: {err}");
        }
    }

    #[test]
    fn create_check_rejects_integer_overflow_ops() {
        typecheck_execute_request(
            &req("CREATE TABLE IF NOT EXISTS checked (n INTEGER CHECK (n > 0))"),
            &SqlTypeEnv::new(),
        )
        .expect("comparison CHECK");
        let err = typecheck_execute_request(
            &req("CREATE TABLE IF NOT EXISTS checked (n INTEGER CHECK (n + 1 > n))"),
            &SqlTypeEnv::new(),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("CHECK") && err.to_string().contains("overflow"),
            "{err}"
        );
        let err = typecheck_execute_request(
            &req("CREATE TABLE IF NOT EXISTS checked (n INTEGER CHECK (abs(n) > 0))"),
            &SqlTypeEnv::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("CHECK"), "{err}");
    }

    #[test]
    fn validate_for_rejects_malformed_sidecar() {
        let sql = "SELECT 1 + 2";
        let mut proof = typecheck_execute_request_resolved(&req(sql), &SqlTypeEnv::new())
            .unwrap()
            .remove(0);
        proof.validate_for(sql).expect("fresh proof");
        proof.integer_arith_sites[0].full.end = sql.len() + 4;
        assert!(proof.validate_for(sql).is_err());

        let lit = "SELECT 'ä'";
        let mut text = typecheck_execute_request_resolved(&req(lit), &SqlTypeEnv::new())
            .unwrap()
            .remove(0);
        let span = text.text_collate_sites[0].span;
        let mid = (span.start + 1..span.end)
            .find(|&i| !lit.is_char_boundary(i))
            .expect("ä is multi-byte");
        text.text_collate_sites[0].span.end = mid;
        assert!(text.validate_for(lit).is_err());
    }

    #[test]
    fn create_proof_carries_full_schema() {
        let sql =
            "CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY AUTOINCREMENT, body TEXT)";
        let proofs = typecheck_execute_request_resolved(&req(sql), &SqlTypeEnv::new()).unwrap();
        match &proofs[0].schema_action {
            SchemaAction::Create {
                schema,
                fingerprint,
                noop,
            } => {
                assert!(!*noop);
                assert_eq!(schema.table, "notes");
                assert_eq!(schema.identity_column.as_deref(), Some("id"));
                assert_eq!(schema.columns.len(), 2);
                assert_eq!(schema.fingerprint(), *fingerprint);
                let companions = catalog_companions_for_action(sql, Some(&proofs[0].schema_action));
                assert!(
                    companions
                        .iter()
                        .any(|s| s.contains("notes") && s.to_ascii_uppercase().contains("INSERT")),
                    "{companions:?}"
                );
                assert!(
                    companions
                        .iter()
                        .any(|s| s.contains(SQL_DDL_TABLE)
                            && s.to_ascii_uppercase().contains("INSERT")),
                    "CREATE TABLE must persist canonical DDL: {companions:?}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parse_create_index_sql_reads_unique_and_table() {
        let idx = parse_create_index_sql(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_notes_body ON notes (body)",
        )
        .expect("index");
        assert_eq!(idx.name, "idx_notes_body");
        assert_eq!(idx.table, "notes");
        assert_eq!(idx.columns, vec!["body".to_string()]);
        assert!(idx.unique);
        let drop = parse_drop_index_name("DROP INDEX IF EXISTS idx_notes_body").expect("drop");
        assert_eq!(drop, "idx_notes_body");
    }

    #[test]
    fn parse_create_index_sql_rejects_trailing_tokens_and_malformed_lists() {
        assert!(
            parse_create_index_sql("CREATE INDEX idx ON t THIS IS NOT SQL").is_none(),
            "garbage after ON table must not admit"
        );
        assert!(
            parse_create_index_sql("CREATE INDEX idx ON t (a) THIS IS NOT SQL").is_none(),
            "garbage after column list must not admit"
        );
        assert!(parse_create_index_sql("CREATE INDEX idx ON t (a) USING btree").is_none());
        assert!(parse_create_index_sql("CREATE INDEX idx ON t (a) INCLUDE (id)").is_none());
        assert!(parse_create_index_sql("CREATE INDEX idx ON t (body COLLATE NOCASE)").is_none());
        assert!(parse_create_index_sql("CREATE INDEX idx ON t ()").is_none());
        let ok = parse_create_index_sql("CREATE INDEX idx ON t (a ASC, b DESC)").expect("index");
        assert_eq!(ok.table, "t");
        assert_eq!(ok.columns, vec!["a".to_string(), "b".to_string()]);
        assert!(!ok.unique);
        assert!(parse_create_index_sql("CREATE INDEX idx ON t (a) WHERE a IS NOT NULL").is_some());
        assert!(parse_create_index_sql("CREATE INDEX idx ON t (a) WHERE").is_none());
        let partial =
            parse_create_index_sql("CREATE INDEX idx ON t (a) WHERE a IS NOT NULL").expect("where");
        assert_eq!(partial.where_sql.as_deref(), Some("a IS NOT NULL"));
    }

    #[test]
    fn typecheck_create_index_where_requires_sql_v1_predicate() {
        let mut env = SqlTypeEnv::new();
        env.insert_table("t", [("a".into(), SqlType::Integer)]);
        typecheck_execute_request(&req("CREATE INDEX idx ON t (a) WHERE a IS NOT NULL"), &env)
            .expect("valid partial index");
        typecheck_execute_request(
            &req("CREATE INDEX idx ON t (a) WHERE wake_pending = 1"),
            &env,
        )
        .expect_err("unknown column");
        let err = typecheck_execute_request(
            &req("CREATE INDEX idx ON t (a) WHERE THIS IS NOT SQL"),
            &env,
        )
        .expect_err("garbage predicate");
        let msg = err.to_string();
        assert!(
            msg.contains("WHERE") || msg.contains("IS NULL") || msg.contains("unknown"),
            "{msg}"
        );
        let err = typecheck_execute_request(
            &req("CREATE INDEX idx ON t (a) WHERE a IS NOT NULL TRAILING"),
            &env,
        )
        .expect_err("leftover tokens");
        assert!(
            err.to_string().contains("WHERE") || err.to_string().contains("complete"),
            "{err}"
        );
    }

    #[test]
    fn typecheck_create_index_requires_known_columns() {
        let mut env = SqlTypeEnv::new();
        env.insert_table("t", [("a".into(), SqlType::Integer)]);
        typecheck_create_index_sql("CREATE INDEX idx ON t (a)", &env).expect("known column");
        let err = typecheck_create_index_sql("CREATE INDEX idx ON t (no_such_column)", &env)
            .expect_err("unknown indexed column");
        assert!(
            err.to_string().contains("unknown column")
                && err.to_string().contains("no_such_column"),
            "{err}"
        );
        let err = typecheck_execute_request(&req("CREATE INDEX idx ON t (no_such_column)"), &env)
            .expect_err("execute path");
        assert!(err.to_string().contains("unknown column"), "{err}");
    }

    #[test]
    fn parse_create_table_column_references_ignore_string_defaults() {
        let fake = parse_create_table_schema(
            "CREATE TABLE child (id INTEGER PRIMARY KEY, note TEXT DEFAULT 'REFERENCES parent')",
        )
        .expect("table");
        assert!(fake.column_references.iter().all(Option::is_none));
        assert!(fake.referenced_tables().is_empty());
        let real = parse_create_table_schema(
            "CREATE TABLE child (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent (id))",
        )
        .expect("table");
        assert_eq!(
            real.column_references[1]
                .as_ref()
                .map(|r| r.ref_table.as_str()),
            Some("parent")
        );
        assert_eq!(real.referenced_tables(), vec!["parent".to_string()]);
    }

    #[test]
    fn catalog_companions_persist_and_drop_index_sql() {
        let sql = "CREATE INDEX IF NOT EXISTS idx_notes_body ON notes (body)";
        let companions = catalog_companions(sql);
        assert!(
            companions
                .iter()
                .any(|s| s.contains(SQL_DDL_TABLE) && s.contains("idx_notes_body")),
            "{companions:?}"
        );
        let drop = catalog_companions("DROP INDEX IF EXISTS idx_notes_body");
        assert!(
            drop.iter()
                .any(|s| s.to_ascii_uppercase().contains("DELETE") && s.contains("idx_notes_body")),
            "{drop:?}"
        );
    }

    #[test]
    fn catalog_companions_skip_reserved_identity_and_catalog_tables() {
        let sql = format!(
            "CREATE TABLE IF NOT EXISTS {SQL_IDENTITY_TABLE} (\
             table_name TEXT PRIMARY KEY, last INTEGER NOT NULL)"
        );
        assert!(
            catalog_companions(&sql).is_empty(),
            "identity-table DDL must not be catalogued as plugin schema"
        );
        assert!(catalog_companions(&format!(
            "CREATE TABLE IF NOT EXISTS {SQL_CATALOG_TABLE} (table_name TEXT, column_name TEXT)"
        ))
        .is_empty());
        assert!(catalog_companions(&format!(
            "CREATE TABLE IF NOT EXISTS {SQL_DDL_TABLE} (\
             kind TEXT NOT NULL, name TEXT NOT NULL, table_name TEXT NOT NULL, \
             canonical_sql TEXT NOT NULL, PRIMARY KEY (kind, name))"
        ))
        .is_empty());
    }
}
