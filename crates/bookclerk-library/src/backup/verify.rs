//! Full preflight of a recovery point before any destructive restore.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use bookclerk_plugin_abi::{DbValue, SQL_CONTRACT_VERSION};

use super::encode::CanonicalObject;
use super::repository::BackupRepository;
use super::schema::admit_canonical_statements;
use super::util::validate_cell;
use super::{
    BackupUnit, CanonicalDatabaseSchema, DatabaseUnitKind, IdentityHighWater, ValidatedBackup,
    BACKUP_FORMAT_VERSION, LIBRARY_SKIP_TABLES,
};
use crate::error::{LibraryError, Result};

/// Parses the manifest, rejects unsupported formats, and verifies every
/// referenced object **before** the caller starts a destructive restore.
///
/// # Errors
///
/// Returns when the manifest is malformed, an object is missing/corrupt, schema
/// is not fully admitted, or typed cells violate the contract.
pub fn verify_recovery_point(repo: &BackupRepository, id: &str) -> Result<ValidatedBackup> {
    let manifest = repo.read_manifest(id)?;
    if manifest.format_version != BACKUP_FORMAT_VERSION {
        return Err(LibraryError::Schema(format!(
            "unsupported backup format version {} (this binary supports {BACKUP_FORMAT_VERSION})",
            manifest.format_version
        )));
    }
    if manifest.id.is_empty() || manifest.created_at.is_empty() {
        return Err(LibraryError::Schema(
            "backup manifest is missing id or created_at".into(),
        ));
    }
    if manifest.units.is_empty() {
        return Err(LibraryError::Schema(
            "backup manifest lists no database units".into(),
        ));
    }
    let mut library = None;
    let mut plugin_units = Vec::new();
    let mut plugin_ids = HashSet::new();
    for unit in &manifest.units {
        verify_unit(repo, unit)?;
        match unit.kind {
            DatabaseUnitKind::Library => {
                if library.is_some() {
                    return Err(LibraryError::Schema(
                        "backup lists more than one library unit".into(),
                    ));
                }
                library = Some(unit.clone());
            }
            DatabaseUnitKind::PluginBinding => {
                let plugin_id = unit.plugin_id.as_deref().ok_or_else(|| {
                    LibraryError::Schema("plugin backup unit is missing plugin_id".into())
                })?;
                let binding = unit.binding.as_deref().ok_or_else(|| {
                    LibraryError::Schema("plugin backup unit is missing binding".into())
                })?;
                if plugin_id.is_empty() || binding.is_empty() {
                    return Err(LibraryError::Schema(
                        "plugin backup unit has an empty logical identity".into(),
                    ));
                }
                if !plugin_ids.insert((plugin_id.to_string(), binding.to_string())) {
                    return Err(LibraryError::Schema(format!(
                        "backup lists duplicate plugin identity `{plugin_id}/{binding}`"
                    )));
                }
                plugin_units.push(unit.clone());
            }
        }
    }
    let library = library
        .ok_or_else(|| LibraryError::Schema("backup is missing a library database unit".into()))?;
    Ok(ValidatedBackup {
        manifest,
        library,
        plugin_units,
    })
}

/// Verifies one unit's objects, admitted schema, and typed rows.
///
/// # Errors
///
/// Returns when any object is missing, corrupt, or semantically invalid.
pub fn verify_unit(repo: &BackupRepository, unit: &BackupUnit) -> Result<()> {
    if unit.sql_contract_version == 0 || unit.sql_contract_version > SQL_CONTRACT_VERSION {
        return Err(LibraryError::Schema(format!(
            "backup SQL contract version {} is not supported (this binary supports {SQL_CONTRACT_VERSION})",
            unit.sql_contract_version
        )));
    }
    let schema = load_admitted_schema(repo, unit)?;
    let identity = repo.get_object(&unit.identity_object)?;
    let CanonicalObject::Identity { entries } = identity else {
        return Err(LibraryError::Schema(format!(
            "backup identity object is `{identity:?}`, not Identity"
        )));
    };
    verify_exact_table_set(&schema, unit)?;
    let mut seen = HashSet::new();
    let mut captured_identity_max: BTreeMap<String, i64> = BTreeMap::new();
    for table in &unit.tables {
        if !seen.insert(table.name.clone()) {
            return Err(LibraryError::Schema(format!(
                "backup unit lists table `{}` more than once",
                table.name
            )));
        }
        let expected = schema
            .tables
            .iter()
            .find(|t| t.parsed.table == table.name)
            .ok_or_else(|| {
                LibraryError::Schema(format!(
                    "backup has data for `{}` without CREATE TABLE SQL",
                    table.name
                ))
            })?;
        let cols: Vec<String> = expected
            .parsed
            .columns
            .iter()
            .map(|(c, _)| c.clone())
            .collect();
        if table.columns != cols {
            return Err(LibraryError::Schema(format!(
                "backup columns for `{}` do not match admitted schema",
                table.name
            )));
        }
        let identity_idx = expected
            .parsed
            .identity_column
            .as_ref()
            .and_then(|c| table.columns.iter().position(|n| n == c));
        for digest in &table.chunks {
            let object = repo.get_object(digest)?;
            let CanonicalObject::TableChunk {
                table: chunk_table,
                columns,
                rows,
            } = object
            else {
                return Err(LibraryError::Schema(format!(
                    "backup chunk `{digest}` is not table data"
                )));
            };
            if chunk_table != table.name || columns != table.columns {
                return Err(LibraryError::Schema(format!(
                    "backup chunk `{digest}` does not match table `{}`",
                    table.name
                )));
            }
            for (i, row) in rows.iter().enumerate() {
                if row.len() != columns.len() {
                    return Err(LibraryError::Schema(format!(
                        "backup `{}` row {i} has {} cells; expected {}",
                        table.name,
                        row.len(),
                        columns.len()
                    )));
                }
                for (j, cell) in row.iter().enumerate() {
                    let (col, ty) = &expected.parsed.columns[j];
                    let not_null = expected
                        .parsed
                        .column_not_null
                        .get(j)
                        .copied()
                        .unwrap_or(false);
                    validate_cell(&table.name, col, cell, *ty, not_null)?;
                }
                if let Some(idx) = identity_idx {
                    if let Some(DbValue::Int64(v)) = row.get(idx) {
                        let slot = captured_identity_max.entry(table.name.clone()).or_insert(0);
                        *slot = (*slot).max(*v);
                    }
                }
            }
        }
    }
    verify_identity_completeness(&schema, unit, &entries, &captured_identity_max)?;
    Ok(())
}

/// Every admitted schema table (except the library skip set) has exactly one
/// [`super::BackupTable`]. Missing or extra entries fail closed.
fn verify_exact_table_set(schema: &CanonicalDatabaseSchema, unit: &BackupUnit) -> Result<()> {
    let skip: BTreeSet<&str> = match unit.kind {
        DatabaseUnitKind::Library => LIBRARY_SKIP_TABLES.iter().copied().collect(),
        DatabaseUnitKind::PluginBinding => BTreeSet::new(),
    };
    let expected: BTreeSet<String> = schema
        .tables
        .iter()
        .map(|t| t.parsed.table.clone())
        .filter(|name| !skip.contains(name.as_str()))
        .collect();
    let listed: BTreeSet<String> = unit.tables.iter().map(|t| t.name.clone()).collect();
    if listed == expected {
        return Ok(());
    }
    let missing: Vec<_> = expected.difference(&listed).cloned().collect();
    let extra: Vec<_> = listed.difference(&expected).cloned().collect();
    Err(LibraryError::Schema(format!(
        "backup table set does not match admitted schema (missing {missing:?}, extra {extra:?})"
    )))
}

/// Identity object has exactly one entry per captured identity column, the
/// expected name, and a high-water at least the max captured value.
fn verify_identity_completeness(
    schema: &CanonicalDatabaseSchema,
    unit: &BackupUnit,
    entries: &BTreeMap<String, IdentityHighWater>,
    captured_max: &BTreeMap<String, i64>,
) -> Result<()> {
    let skip: BTreeSet<&str> = match unit.kind {
        DatabaseUnitKind::Library => LIBRARY_SKIP_TABLES.iter().copied().collect(),
        DatabaseUnitKind::PluginBinding => BTreeSet::new(),
    };
    let mut expected: BTreeSet<String> = BTreeSet::new();
    for table in &schema.tables {
        if skip.contains(table.parsed.table.as_str()) {
            continue;
        }
        let Some(col) = table.parsed.identity_column.as_deref() else {
            if entries.contains_key(&table.parsed.table) {
                return Err(LibraryError::Schema(format!(
                    "backup identity object has an entry for `{}` which has no identity column",
                    table.parsed.table
                )));
            }
            continue;
        };
        expected.insert(table.parsed.table.clone());
        let hw = entries.get(&table.parsed.table).ok_or_else(|| {
            LibraryError::Schema(format!(
                "backup identity object is missing table `{}`",
                table.parsed.table
            ))
        })?;
        if hw.column != col {
            return Err(LibraryError::Schema(format!(
                "backup identity column for `{}` is `{}`; admitted schema has `{col}`",
                table.parsed.table, hw.column
            )));
        }
        let captured = captured_max.get(&table.parsed.table).copied().unwrap_or(0);
        if hw.last < captured {
            return Err(LibraryError::Schema(format!(
                "backup identity high-water for `{}` is {}; captured rows reach {captured}",
                table.parsed.table, hw.last
            )));
        }
        if hw.last < 0 {
            return Err(LibraryError::Schema(format!(
                "backup identity high-water for `{}` is {}; identity values must be non-negative",
                table.parsed.table, hw.last
            )));
        }
    }
    for name in entries.keys() {
        if !expected.contains(name) {
            return Err(LibraryError::Schema(format!(
                "backup identity object has an extra entry for `{name}`"
            )));
        }
    }
    Ok(())
}

/// Loads and fully admits the schema object for `unit`.
///
/// # Errors
///
/// Returns when the object is missing or not fully admitted Bookclerk SQL.
pub fn load_admitted_schema(
    repo: &BackupRepository,
    unit: &BackupUnit,
) -> Result<CanonicalDatabaseSchema> {
    let object = repo.get_object(&unit.schema_object)?;
    let CanonicalObject::Schema {
        sql_contract_version,
        statements,
    } = object
    else {
        return Err(LibraryError::Schema(
            "backup schema object is not Schema".into(),
        ));
    };
    if sql_contract_version != unit.sql_contract_version {
        return Err(LibraryError::Schema(
            "backup schema object sql_contract_version does not match the unit".into(),
        ));
    }
    // The schema object already stores an ordered statement list. Admit that
    // list directly — do not join with `;\n` and rediscover boundaries.
    let refs: Vec<&str> = statements.iter().map(String::as_str).collect();
    let admitted = admit_canonical_statements(sql_contract_version, &refs)?;
    let expected_tables = statements
        .iter()
        .filter(|s| bookclerk_plugin_abi::parse_create_table_schema(s).is_some())
        .count();
    let expected_indexes = statements
        .iter()
        .filter(|s| bookclerk_plugin_abi::parse_create_index_sql(s).is_some())
        .count();
    if admitted.tables.len() != expected_tables || admitted.indexes.len() != expected_indexes {
        return Err(LibraryError::Schema(
            "backup schema object is not fully admitted Bookclerk SQL".into(),
        ));
    }
    Ok(admitted)
}
