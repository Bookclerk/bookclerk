//! Canonical schema admission, SchemaState-aware library DDL, FK order, row keys.
//!
//! Restore executes only statements that successfully admitted. Cyclic foreign
//! keys fail closed: SQL-v1 has no portable `ALTER TABLE ADD CONSTRAINT` or
//! deferred FK, so a cycle cannot be created on PostgreSQL in a safe order.

use std::collections::{BTreeMap, BTreeSet};

use bookclerk_plugin_abi::sql_types::TableConstraint;
use bookclerk_plugin_abi::{
    parse_create_index_sql, parse_create_table_schema, typecheck_create_index_sql,
    CreateTableSchema, SqlType, SqlTypeEnv, SQL_CONTRACT_VERSION,
};

use crate::error::{LibraryError, Result};
use crate::migrations::{
    host_migration_plan, unreleased_checksum, unreleased_ops, HostMigrationStep, MigrationOp,
    SCHEMA_MIGRATIONS_DDL, SCHEMA_VERSION,
};
use crate::schema_state::SchemaState;

use super::{CanonicalDatabaseSchema, CanonicalTableSchema};

/// Parses and fully admits a diagnostic/imported canonical SQL script.
///
/// Production capture, checksum, apply, and restore paths use already-separated
/// statement lists ([`admit_canonical_statements`]). This helper exists for
/// tests and imported scripts that still arrive as one packable string.
///
/// # Errors
///
/// Returns when the packer rejects `sql` or any statement is not admitted.
pub fn admit_canonical_schema(
    sql_contract_version: u32,
    sql: &str,
) -> Result<CanonicalDatabaseSchema> {
    let stmts = bookclerk_plugin_abi::sql_v1_pack_statements(sql).map_err(|err| {
        LibraryError::Schema(format!(
            "backup schema is not a BookclerkSQL statement list: {err}"
        ))
    })?;
    let refs: Vec<&str> = stmts.iter().map(String::as_str).collect();
    admit_canonical_statements(sql_contract_version, &refs)
}

/// Admits already-separated canonical `CREATE TABLE` / `CREATE INDEX` statements.
///
/// # Errors
///
/// Returns when any statement is not admitted Bookclerk SQL.
pub fn admit_canonical_statements(
    sql_contract_version: u32,
    statements: &[&str],
) -> Result<CanonicalDatabaseSchema> {
    if sql_contract_version == 0 || sql_contract_version > SQL_CONTRACT_VERSION {
        return Err(LibraryError::Schema(format!(
            "unsupported SQL contract version {sql_contract_version} \
             (this binary supports {SQL_CONTRACT_VERSION})"
        )));
    }
    let mut tables = Vec::new();
    let mut indexes = Vec::new();
    let mut env = SqlTypeEnv::new();
    let mut pending_indexes = Vec::new();
    for stmt in statements {
        let trimmed = stmt.trim().trim_end_matches(';').trim();
        if trimmed.is_empty() {
            continue;
        }
        if parse_create_table_schema(trimmed).is_some() || parse_create_index_sql(trimmed).is_some()
        {
            if let Some(parsed) = parse_create_table_schema(trimmed) {
                env.insert_table(parsed.table.clone(), parsed.columns.iter().cloned());
                tables.push(CanonicalTableSchema {
                    create_sql: trimmed.to_string(),
                    parsed,
                });
                continue;
            }
            if let Some(index) = parse_create_index_sql(trimmed) {
                pending_indexes.push((trimmed.to_string(), index));
                continue;
            }
        }
        if is_seed_dml(trimmed) {
            continue;
        }
        return Err(LibraryError::Schema(format!(
            "backup schema is not fully admitted Bookclerk SQL: `{trimmed}`"
        )));
    }
    for (trimmed, index) in pending_indexes {
        typecheck_create_index_sql(&trimmed, &env).map_err(|err| {
            LibraryError::Schema(format!(
                "backup schema is not fully admitted Bookclerk SQL: `{trimmed}` ({err})"
            ))
        })?;
        indexes.push(index);
    }
    Ok(CanonicalDatabaseSchema {
        sql_contract_version,
        tables,
        indexes,
    })
}

/// Library migration packs may include seed `INSERT`s. Those are table data
/// (captured from rows), not schema objects. This keeps admitted
/// `CREATE TABLE` / `CREATE INDEX` and skips seed DML. Any other statement
/// fails closed.
///
/// # Errors
///
/// Returns when a non-empty statement is neither admitted DDL nor seed DML.
pub fn filter_library_pack_ddl(sql: &str) -> Result<String> {
    let mut out = String::new();
    for stmt in bookclerk_plugin_abi::sql_v1_pack_statements(sql).map_err(|err| {
        LibraryError::Schema(format!(
            "backup schema is not a BookclerkSQL statement list: {err}"
        ))
    })? {
        let trimmed = stmt.trim().trim_end_matches(';').trim();
        if trimmed.is_empty() {
            continue;
        }
        if parse_create_table_schema(trimmed).is_some() || parse_create_index_sql(trimmed).is_some()
        {
            out.push_str(trimmed);
            out.push_str(";\n");
            continue;
        }
        if is_seed_dml(trimmed) {
            continue;
        }
        return Err(LibraryError::Schema(format!(
            "backup schema is not fully admitted Bookclerk SQL: `{trimmed}`"
        )));
    }
    Ok(out)
}

/// True for library-pack seed INSERT statements that must not enter a backup.
fn is_seed_dml(sql: &str) -> bool {
    let upper = sql.trim_start().to_ascii_uppercase();
    upper.starts_with("INSERT") || upper.starts_with("UPDATE") || upper.starts_with("DELETE")
}

/// FK-safe table order (parents before children). Cycles fail closed.
///
/// Column-level `REFERENCES` and table-level `FOREIGN KEY` both count.
/// Self-references are ignored. References to tables outside this schema
/// are ignored. Ready nodes keep original declaration order so a child named
/// `a_child` that references `z_parent` still waits for the parent.
///
/// # Errors
///
/// Returns when the FK graph contains a cycle.
pub fn sort_tables_by_foreign_keys(
    tables: Vec<CanonicalTableSchema>,
) -> Result<Vec<CanonicalTableSchema>> {
    let n = tables.len();
    let names: Vec<String> = tables.iter().map(|t| t.parsed.table.clone()).collect();
    let index: BTreeMap<String, usize> = names
        .iter()
        .enumerate()
        .map(|(i, n)| (n.clone(), i))
        .collect();
    let mut remaining_parents: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
    for (i, table) in tables.iter().enumerate() {
        for parent in table.parsed.referenced_tables() {
            if parent == table.parsed.table {
                continue;
            }
            if let Some(&p) = index.get(&parent) {
                remaining_parents[i].insert(p);
            }
        }
    }
    let mut placed = vec![false; n];
    let mut ordered_idx = Vec::with_capacity(n);
    for _ in 0..n {
        let mut pick = None;
        for (i, deps) in remaining_parents.iter().enumerate() {
            if placed[i] {
                continue;
            }
            if deps.iter().all(|&p| placed[p]) {
                pick = Some(i);
                break;
            }
        }
        let Some(i) = pick else {
            return Err(LibraryError::Schema(
                "plugin/library schema has cyclic foreign keys; Bookclerk SQL \
                 restore requires an acyclic FK graph (no portable deferred constraints)"
                    .into(),
            ));
        };
        placed[i] = true;
        ordered_idx.push(i);
    }
    Ok(ordered_idx.into_iter().map(|i| tables[i].clone()).collect())
}

/// Deterministic `ORDER BY` columns for one table.
///
/// Preference: primary key columns, else a table-level UNIQUE key, else a
/// single-column UNIQUE, else every declared column (keyless full-row sort).
/// Remaining declared columns are always appended as tie-breakers so two
/// rows that share a nullable UNIQUE key still have a total order.
/// Physical order, `rowid`, and heap order are never used.
#[must_use]
pub fn order_key_columns(parsed: &CreateTableSchema) -> Vec<String> {
    let inline_pk: Vec<String> = parsed
        .columns
        .iter()
        .enumerate()
        .filter(|(i, _)| parsed.column_primary_key.get(*i).copied().unwrap_or(false))
        .map(|(_, (n, _))| n.clone())
        .collect();
    let leading = if !inline_pk.is_empty() {
        inline_pk
    } else if let Some(cols) = parsed.table_constraints.iter().find_map(|c| match c {
        TableConstraint::PrimaryKey(cols) if !cols.is_empty() => Some(cols.clone()),
        _ => None,
    }) {
        cols
    } else if let Some(cols) = parsed.table_constraints.iter().find_map(|c| match c {
        TableConstraint::Unique(cols) if !cols.is_empty() => Some(cols.clone()),
        _ => None,
    }) {
        cols
    } else {
        let uniques: Vec<String> = parsed
            .columns
            .iter()
            .enumerate()
            .filter(|(i, _)| parsed.column_unique.get(*i).copied().unwrap_or(false))
            .map(|(_, (n, _))| n.clone())
            .collect();
        if uniques.len() == 1 {
            uniques
        } else {
            parsed.columns.iter().map(|(n, _)| n.clone()).collect()
        }
    };
    append_declared_tiebreakers(parsed, leading)
}

/// Canonical `ORDER BY` item list (`col ASC NULLS FIRST`, declared tie-breakers).
///
/// SQLite and PostgreSQL disagree on default NULL placement; the explicit
/// NULLS clause is the portable order. Capture SELECT uses this string;
/// the adapter SDK applies Postgres `COLLATE "C"` from TEXT proof sites
/// (SQLite stays binary).
#[must_use]
pub fn canonical_order_by_sql(parsed: &CreateTableSchema) -> String {
    order_key_columns(parsed)
        .into_iter()
        .map(|col| format!("{col} ASC NULLS FIRST"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Append remaining declared columns so two rows that share a PK (or have no PK)
/// still sort in a total, schema-stable order.
fn append_declared_tiebreakers(
    parsed: &CreateTableSchema,
    mut leading: Vec<String>,
) -> Vec<String> {
    for (name, _) in &parsed.columns {
        if !leading.iter().any(|existing| existing == name) {
            leading.push(name.clone());
        }
    }
    leading
}

/// Maps a declared SQL-v1 type onto `bookclerk_plugin_abi::DbType`.
#[must_use]
pub fn sql_type_to_db_type(ty: SqlType) -> bookclerk_plugin_abi::DbType {
    match ty {
        SqlType::Integer => bookclerk_plugin_abi::DbType::Int64,
        SqlType::Real => bookclerk_plugin_abi::DbType::Float64,
        SqlType::Text => bookclerk_plugin_abi::DbType::Text,
        SqlType::Blob => bookclerk_plugin_abi::DbType::Bytes,
        SqlType::Boolean => bookclerk_plugin_abi::DbType::Bool,
        SqlType::Null => bookclerk_plugin_abi::DbType::Unspecified,
    }
}

/// Library DDL that matches `state`, not necessarily this binary's latest pack.
///
/// # Errors
///
/// Returns when the database is uninitialized, checksums do not match, or a
/// frozen version is unknown to this binary.
pub fn library_ddl_for_schema_state(state: &SchemaState) -> Result<String> {
    library_ddl_for_schema_state_with(
        &host_migration_plan(),
        unreleased_ops(),
        &unreleased_checksum(),
        SCHEMA_MIGRATIONS_DDL,
        SCHEMA_VERSION,
        state,
    )
}

/// Ordered canonical statements matching `state` (bookclerk_schema_migrations, then ops).
///
/// # Errors
///
/// Returns when the database is uninitialized, checksums do not match, or a
/// frozen version is unknown to this binary.
pub fn library_statements_for_schema_state(state: &SchemaState) -> Result<Vec<String>> {
    library_statements_for_schema_state_with(
        &host_migration_plan(),
        unreleased_ops(),
        &unreleased_checksum(),
        SCHEMA_MIGRATIONS_DDL,
        SCHEMA_VERSION,
        state,
    )
}

/// Testable SchemaState → DDL resolver (joined diagnostic SQL).
///
/// # Errors
///
/// Returns when `state` cannot be represented by `plan` / `unreleased`.
pub fn library_ddl_for_schema_state_with(
    plan: &[HostMigrationStep],
    unreleased: &[MigrationOp],
    unreleased_checksum: &str,
    schema_migrations_ddl: &str,
    schema_version: i64,
    state: &SchemaState,
) -> Result<String> {
    Ok(library_statements_for_schema_state_with(
        plan,
        unreleased,
        unreleased_checksum,
        schema_migrations_ddl,
        schema_version,
        state,
    )?
    .join(";\n"))
}

/// Testable SchemaState → ordered statement list.
///
/// # Errors
///
/// Returns when `state` cannot be represented by `plan` / `unreleased`.
pub fn library_statements_for_schema_state_with(
    plan: &[HostMigrationStep],
    unreleased: &[MigrationOp],
    unreleased_checksum: &str,
    schema_migrations_ddl: &str,
    schema_version: i64,
    state: &SchemaState,
) -> Result<Vec<String>> {
    let mut stmts = vec![schema_migrations_ddl.trim_end_matches(';').to_string()];
    match state {
        SchemaState::Uninitialized => {
            return Err(LibraryError::Schema(
                "cannot capture a library backup from an uninitialized database".into(),
            ));
        }
        SchemaState::Frozen { version, checksum } => {
            let step = plan.iter().find(|s| s.version == *version).ok_or_else(|| {
                LibraryError::Schema(format!(
                    "frozen schema version {version} is not in this binary's plan; \
                     run a newer Bookclerk binary, or restore a backup captured \
                     with a binary that knows that freeze"
                ))
            })?;
            let expected = step.checksum();
            if expected != *checksum {
                return Err(LibraryError::Schema(format!(
                    "frozen schema version {version} checksum {checksum} does not \
                     match this binary ({expected})"
                )));
            }
            for s in plan.iter().filter(|s| s.version <= *version) {
                for op in s.steps {
                    stmts.push(op.sql().to_string());
                }
            }
        }
        SchemaState::Unreleased {
            base_version,
            checksum,
        } => {
            if *base_version != schema_version {
                return Err(LibraryError::Schema(format!(
                    "unreleased database is based on frozen {base_version}, but this \
                     binary's SCHEMA_VERSION is {schema_version}"
                )));
            }
            if checksum != unreleased_checksum {
                return Err(LibraryError::Schema(format!(
                    "unreleased checksum {checksum} does not match this binary \
                     ({unreleased_checksum}); restore a backup or reset the database"
                )));
            }
            for s in plan.iter().filter(|s| s.version <= *base_version) {
                for op in s.steps {
                    stmts.push(op.sql().to_string());
                }
            }
            for op in unreleased {
                stmts.push(op.sql().to_string());
            }
        }
    }
    Ok(stmts)
}

/// Admitted library schema for `state`.
///
/// # Errors
///
/// Returns when DDL cannot be resolved or is not fully admitted.
pub fn library_canonical_schema_for_state(state: &SchemaState) -> Result<CanonicalDatabaseSchema> {
    let stmts = library_statements_for_schema_state(state)?;
    let refs: Vec<&str> = stmts.iter().map(String::as_str).collect();
    sort_schema(admit_canonical_statements(SQL_CONTRACT_VERSION, &refs)?)
}

/// Admitted library schema for this binary's current pack (frozen ups + unreleased).
///
/// # Errors
///
/// Returns when current canonical SQL is not fully admitted.
pub fn library_canonical_schema() -> Result<CanonicalDatabaseSchema> {
    let mut stmts = vec![SCHEMA_MIGRATIONS_DDL.to_string()];
    stmts.extend(
        crate::migrations::current_canonical_statements()
            .iter()
            .cloned(),
    );
    let refs: Vec<&str> = stmts.iter().map(String::as_str).collect();
    sort_schema(admit_canonical_statements(SQL_CONTRACT_VERSION, &refs)?)
}

/// Sorts tables in `schema` into FK-safe order.
///
/// # Errors
///
/// Returns on cyclic foreign keys.
pub fn sort_schema(mut schema: CanonicalDatabaseSchema) -> Result<CanonicalDatabaseSchema> {
    schema.tables = sort_tables_by_foreign_keys(schema.tables)?;
    Ok(schema)
}
