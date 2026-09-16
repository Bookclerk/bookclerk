//! Content-addressed canonical Bookclerk backups.
//!
//! A **recovery point** is one complete logical database state. The physical
//! repository may reuse immutable objects from earlier recovery points
//! (logically full, physically incremental). Restore never replays older
//! manifests. Native snapshots / PITR / Time Travel are adapter-specific and
//! are not this format.
//!
//! Portable point-in-time recovery (a base recovery point plus a canonical
//! change journal) is future work; this repository is designed so such a
//! journal can reference the same object store without chained incrementals.

pub mod capture;
pub mod encode;
pub mod repository;
pub mod restore;
pub mod schema;
pub mod util;
pub mod verify;

use std::collections::{BTreeSet, HashSet};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use bookclerk_plugin_abi::{
    CreateIndexSchema, CreateTableSchema, DbCapabilities, SharedAdapterBackupOps,
    FIRST_PARTY_MAX_RESULT_BYTES, FIRST_PARTY_MAX_RESULT_ROWS,
};
use chrono::Utc;
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};

use crate::error::{LibraryError, Result};
use crate::host_schema::{ensure_restore_target_is_replaceable, require_schema_migrations};
use crate::schema_state::SchemaState;
use crate::store::LibraryStore;

use self::capture::{capture_library_unit, capture_plugin_unit, library_skip_tables};
use self::encode::CHUNK_TARGET_UNCOMPRESSED_BYTES;
use self::repository::BackupRepository;
use self::restore::restore_backup_unit;
use self::verify::verify_recovery_point;

pub use self::capture::plugin_canonical_schema_from_ddl_catalog;
pub use self::restore::apply_admitted_sql;
pub use self::schema::{
    admit_canonical_schema, admit_canonical_statements, filter_library_pack_ddl,
    library_canonical_schema, library_canonical_schema_for_state,
};

/// Subdirectory of `$BOOKCLERK_FILES_DIR` holding the backup repository.
pub const BACKUPS_DIR: &str = "backups";

/// Automatic `pre-migrate` recovery points retained after prune.
pub const BACKUP_RETENTION: usize = 5;

/// Manifest format written by this binary.
pub const BACKUP_FORMAT_VERSION: u32 = 1;

/// Max tar members in one recovery-point `.tar.gz`.
pub const MAX_BACKUP_ARCHIVE_ENTRIES: u64 = 50_000;
/// Max expanded size of one archive member (same cap as a stored object).
pub const MAX_BACKUP_ARCHIVE_ENTRY_BYTES: u64 = 32 * 1024 * 1024;
/// Max total expanded bytes across archive members.
pub const MAX_BACKUP_ARCHIVE_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
/// Max gzip-decoded tar bytes (member payloads plus ustar headers).
pub const MAX_BACKUP_ARCHIVE_STREAM_BYTES: u64 =
    MAX_BACKUP_ARCHIVE_TOTAL_BYTES + MAX_BACKUP_ARCHIVE_ENTRIES * 1024;

/// Library registry rows are environment-local (`unit_ref` is not portable).
pub const LIBRARY_SKIP_TABLES: &[&str] = &["plugin_databases"];

/// Why a backup was written.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupReason {
    /// Operator-created (`bookclerk db backup create`). Never pruned automatically.
    Manual,
    /// Written immediately before a frozen schema walk.
    #[default]
    PreMigrate,
}

/// Logical kind of one database unit in a recovery point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseUnitKind {
    /// Host library database.
    Library,
    /// Plugin-owned binding (`plugin_id` + `binding`).
    PluginBinding,
}

/// How restore applies DDL companions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalRestoreKind {
    /// Host library: Postgres identity companions only (no plugin catalog).
    Library,
    /// Plugin binding: catalog + Postgres identity companions.
    PluginBinding,
}

/// Identity high-water for one generated column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityHighWater {
    /// Identity column name.
    pub column: String,
    /// Highest generated or stored value that must not be reused.
    pub last: i64,
}

/// Admitted canonical schema used to export or restore one database unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalDatabaseSchema {
    /// Bookclerk SQL grammar/ABI contract, not a library schema revision.
    pub sql_contract_version: u32,
    /// `CREATE TABLE` statements in FK-safe order after [`crate::sort_tables_by_foreign_keys`].
    pub tables: Vec<CanonicalTableSchema>,
    /// `CREATE INDEX` statements (after tables).
    pub indexes: Vec<CreateIndexSchema>,
}

/// One admitted `CREATE TABLE` plus its parsed IR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalTableSchema {
    /// Original canonical SQL (no trailing semicolon).
    pub create_sql: String,
    /// Parsed columns, constraints, and identity.
    pub parsed: CreateTableSchema,
}

impl CanonicalDatabaseSchema {
    /// Ordered canonical DDL (tables then indexes).
    #[must_use]
    pub fn schema_sql(&self) -> Vec<String> {
        let mut out: Vec<String> = self.tables.iter().map(|t| t.create_sql.clone()).collect();
        out.extend(self.indexes.iter().map(|i| i.canonical_sql.clone()));
        out
    }

    /// Folded user table names in CREATE order.
    #[must_use]
    pub fn table_names(&self) -> Vec<String> {
        self.tables.iter().map(|t| t.parsed.table.clone()).collect()
    }
}

/// Options for a consistent canonical export.
#[derive(Clone)]
pub struct CanonicalExportOpts {
    /// Guest advertised `DbCapabilities::supports_consistent_backup_read`.
    pub consistent_backup_read: bool,
    /// Folded table names omitted from row capture (library `plugin_databases`).
    pub skip_tables: BTreeSet<String>,
    /// Page size for `SELECT … LIMIT` (honors adapter `maxResultRows`).
    pub max_result_rows: u32,
    /// Encoded-byte budget for one SELECT page (`maxResultBytes`).
    pub max_result_bytes: u32,
    /// Encoded-byte budget for one atomic reply (`maxAtomicResultBytes`).
    pub max_atomic_result_bytes: u32,
    /// Uncompressed JSON target for one table-data chunk.
    pub chunk_target_bytes: usize,
    /// Adapter snapshot/identity hooks. `None` uses the in-process SDK (tests).
    pub adapter: Option<SharedAdapterBackupOps>,
    /// True when leftover SQL is physically lowered on the opened connection.
    pub in_process: bool,
}

impl std::fmt::Debug for CanonicalExportOpts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CanonicalExportOpts")
            .field("consistent_backup_read", &self.consistent_backup_read)
            .field("skip_tables", &self.skip_tables)
            .field("max_result_rows", &self.max_result_rows)
            .field("max_result_bytes", &self.max_result_bytes)
            .field("max_atomic_result_bytes", &self.max_atomic_result_bytes)
            .field("chunk_target_bytes", &self.chunk_target_bytes)
            .field("adapter", &self.adapter.as_ref().map(|_| "Some"))
            .field("in_process", &self.in_process)
            .finish()
    }
}

impl Default for CanonicalExportOpts {
    fn default() -> Self {
        Self {
            consistent_backup_read: true,
            skip_tables: BTreeSet::new(),
            max_result_rows: FIRST_PARTY_MAX_RESULT_ROWS,
            max_result_bytes: FIRST_PARTY_MAX_RESULT_BYTES,
            max_atomic_result_bytes: FIRST_PARTY_MAX_RESULT_BYTES,
            chunk_target_bytes: CHUNK_TARGET_UNCOMPRESSED_BYTES,
            adapter: None,
            in_process: false,
        }
    }
}

impl CanonicalExportOpts {
    /// Page-size and result-byte limits from negotiated adapter capabilities.
    #[must_use]
    pub fn from_caps(caps: &DbCapabilities) -> Self {
        Self {
            consistent_backup_read: caps.supports_consistent_backup_read(),
            skip_tables: BTreeSet::new(),
            max_result_rows: caps.max_result_rows.max(1),
            max_result_bytes: caps.max_result_bytes.max(1),
            max_atomic_result_bytes: caps.max_atomic_result_bytes.max(1),
            chunk_target_bytes: CHUNK_TARGET_UNCOMPRESSED_BYTES,
            adapter: None,
            in_process: false,
        }
    }

    /// Effective encoded-byte budget for one capture SELECT page.
    #[must_use]
    pub fn page_byte_budget(&self) -> usize {
        usize::try_from(
            self.max_result_bytes
                .min(self.max_atomic_result_bytes)
                .max(1),
        )
        .unwrap_or(usize::MAX)
    }
}

/// Negotiated limits for parameterized canonical restore.
#[derive(Clone)]
pub struct CanonicalRestoreOpts {
    /// Guest advertised `atomicUnitRestore`.
    pub atomic_unit_restore: bool,
    /// Maximum bound parameters per INSERT (`maxBinds`).
    pub max_binds: u32,
    /// Maximum UTF-8 bytes of SQL plus binds per statement (`maxPayloadBytes`).
    pub max_payload_bytes: u32,
    /// Maximum encoded bytes of one [`bookclerk_plugin_abi::ExecuteRequest`].
    pub max_request_bytes: u32,
    /// Adapter restore hooks. `None` uses the in-process SDK (tests).
    pub adapter: Option<SharedAdapterBackupOps>,
    /// True when leftover SQL is physically lowered on the opened connection.
    pub in_process: bool,
}

impl std::fmt::Debug for CanonicalRestoreOpts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CanonicalRestoreOpts")
            .field("atomic_unit_restore", &self.atomic_unit_restore)
            .field("max_binds", &self.max_binds)
            .field("max_payload_bytes", &self.max_payload_bytes)
            .field("max_request_bytes", &self.max_request_bytes)
            .field("adapter", &self.adapter.as_ref().map(|_| "Some"))
            .field("in_process", &self.in_process)
            .finish()
    }
}

impl Default for CanonicalRestoreOpts {
    fn default() -> Self {
        Self::from_caps(&DbCapabilities::advertised_sqlite())
            .expect("first-party sqlite capabilities are a known versioning contract")
    }
}

impl CanonicalRestoreOpts {
    /// Restore limits copied from negotiated adapter capabilities.
    ///
    /// The versioning contract is checked with [`require_schema_migrations`],
    /// never inferred from SeaORM [`sea_orm::DbBackend`]. Host policy requires
    /// `schemaMigrations`.
    ///
    /// # Errors
    ///
    /// Returns when `caps` is not a known versioning contract (`schemaMigrations`
    /// missing).
    pub fn from_caps(caps: &DbCapabilities) -> Result<Self> {
        require_schema_migrations(caps)?;
        Ok(Self {
            atomic_unit_restore: caps.supports_atomic_unit_restore(),
            max_binds: caps.max_binds.max(1),
            max_payload_bytes: caps.max_payload_bytes.max(1),
            max_request_bytes: caps.max_request_bytes.max(1),
            adapter: None,
            in_process: false,
        })
    }
}

/// One table's chunk list inside a recovery-point unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupTable {
    /// Folded table name.
    pub name: String,
    /// Column names in CREATE declaration order.
    pub columns: Vec<String>,
    /// SHA-256 hex of each table-data chunk (canonical order).
    pub chunks: Vec<String>,
}

/// One logical database in a recovery point (library or plugin binding).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupUnit {
    /// Library vs plugin binding.
    pub kind: DatabaseUnitKind,
    /// Plugin id when [`DatabaseUnitKind::PluginBinding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    /// Binding name when [`DatabaseUnitKind::PluginBinding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    /// Adapter that captured this unit (diagnostic only; restore is cross-adapter).
    pub backend_at_capture: String,
    /// Bookclerk SQL grammar/ABI contract, not a library schema revision.
    pub sql_contract_version: u32,
    /// SHA-256 of the schema object.
    pub schema_object: String,
    /// SHA-256 of the identity object.
    pub identity_object: String,
    /// User-visible tables (not catalog/identity companions).
    pub tables: Vec<BackupTable>,
    /// Plugin id when [`DatabaseUnitKind::PluginBinding`] (owner identity).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_schema_namespace: Option<String>,
    /// [`crate::PluginMigrationHistory::display`] at capture (`history@{n}+{digest}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_schema_state: Option<String>,
    /// Digest of the complete ordered `(migration_id, checksum)` journal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_schema_checksum: Option<String>,
}

impl BackupUnit {
    /// Restore companion kind for this unit.
    #[must_use]
    pub fn restore_kind(&self) -> CanonicalRestoreKind {
        match self.kind {
            DatabaseUnitKind::Library => CanonicalRestoreKind::Library,
            DatabaseUnitKind::PluginBinding => CanonicalRestoreKind::PluginBinding,
        }
    }

    /// Object digests this unit needs to restore.
    #[must_use]
    pub fn referenced_objects(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        out.insert(self.schema_object.clone());
        out.insert(self.identity_object.clone());
        for table in &self.tables {
            out.extend(table.chunks.iter().cloned());
        }
        out
    }
}

/// On-disk recovery-point metadata (`manifests/<id>.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupManifest {
    /// Catalog format (`1` for content-addressed canonical objects).
    pub format_version: u32,
    /// Stable recovery-point id (UUID).
    pub id: String,
    /// RFC 3339 UTC timestamp.
    pub created_at: String,
    /// [`SchemaState::display`] at capture.
    pub schema_state: String,
    /// SHA-256 recorded on the library schema state marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_checksum: Option<String>,
    /// Frozen revision when the source library was frozen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frozen_version: Option<i64>,
    /// Bookclerk SQL contract of the library unit.
    pub sql_contract_version: u32,
    /// Bookclerk version that wrote the backup.
    pub app_version: String,
    /// Operator vs automatic.
    pub reason: BackupReason,
    /// Target frozen version a following migrate step was heading toward.
    pub migrate_to: i64,
    /// When true, every registered plugin binding was required at capture.
    pub include_plugin_databases: bool,
    /// Logical database units (library first).
    pub units: Vec<BackupUnit>,
}

impl BackupManifest {
    /// Union of every object digest referenced by this recovery point.
    #[must_use]
    pub fn referenced_objects(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for unit in &self.units {
            out.extend(unit.referenced_objects());
        }
        out
    }

    /// Library unit from this manifest.
    ///
    /// # Errors
    ///
    /// Returns when the library unit is missing or duplicated.
    pub fn library_unit(&self) -> Result<&BackupUnit> {
        let mut found = None;
        for unit in &self.units {
            if unit.kind == DatabaseUnitKind::Library {
                if found.is_some() {
                    return Err(LibraryError::Schema(
                        "backup lists more than one library unit".into(),
                    ));
                }
                found = Some(unit);
            }
        }
        found
            .ok_or_else(|| LibraryError::Schema("backup is missing a library database unit".into()))
    }

    /// Plugin binding units.
    #[must_use]
    pub fn plugin_units(&self) -> Vec<&BackupUnit> {
        self.units
            .iter()
            .filter(|u| u.kind == DatabaseUnitKind::PluginBinding)
            .collect()
    }
}

/// Open plugin binding captured through the active adapter session.
#[derive(Clone)]
pub struct PreparedPluginUnit {
    /// Owning plugin id.
    pub plugin_id: String,
    /// Binding name.
    pub binding: String,
    /// Adapter that produced this unit (diagnostic).
    pub backend_at_capture: String,
    /// SeaORM connection opened via the active adapter (not a native dump).
    pub db: DatabaseConnection,
    /// Negotiated `maxResultRows` for this binding session.
    pub max_result_rows: u32,
    /// Negotiated `maxResultBytes` for this binding session.
    pub max_result_bytes: u32,
    /// Negotiated `maxAtomicResultBytes` for this binding session.
    pub max_atomic_result_bytes: u32,
    /// Binding-scoped adapter hooks. `None` uses the in-process SDK (tests).
    pub adapter: Option<SharedAdapterBackupOps>,
}

impl std::fmt::Debug for PreparedPluginUnit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedPluginUnit")
            .field("plugin_id", &self.plugin_id)
            .field("binding", &self.binding)
            .field("backend_at_capture", &self.backend_at_capture)
            .field("max_result_rows", &self.max_result_rows)
            .field("max_result_bytes", &self.max_result_bytes)
            .field("max_atomic_result_bytes", &self.max_atomic_result_bytes)
            .field("adapter", &self.adapter.as_ref().map(|_| "Some"))
            .finish_non_exhaustive()
    }
}

/// Inputs for an automatic or CLI backup.
#[derive(Clone)]
pub struct BackupRequest {
    /// `$BOOKCLERK_FILES_DIR`.
    pub files_dir: PathBuf,
    /// Durable library schema state at capture. [`SchemaState::Uninitialized`] skips.
    pub schema_state: SchemaState,
    /// Operator vs pre-migrate.
    pub reason: BackupReason,
    /// Version the upcoming migrate step will target (`0` for manual).
    pub to_version: i64,
    /// Include every registered plugin-owned database binding.
    pub include_plugin_databases: bool,
    /// Guest advertised consistent backup read.
    pub consistent_backup_read: bool,
    /// Adapter id at capture (diagnostic; not used to branch restore).
    pub backend_at_capture: String,
    /// Negotiated `maxResultRows` (clamped).
    pub max_result_rows: u32,
    /// Negotiated `maxResultBytes` for one SELECT page.
    pub max_result_bytes: u32,
    /// Negotiated `maxAtomicResultBytes` (capture page budget is the min of both).
    pub max_atomic_result_bytes: u32,
    /// Plugin sessions prepared by the host (must match the registry when inclusion is on).
    pub plugin_units: Vec<PreparedPluginUnit>,
    /// Library adapter hooks. `None` uses the in-process SDK (tests).
    pub adapter: Option<SharedAdapterBackupOps>,
    /// True when leftover SQL is physically lowered on the opened connection.
    pub in_process: bool,
}

impl std::fmt::Debug for BackupRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackupRequest")
            .field("files_dir", &self.files_dir)
            .field("schema_state", &self.schema_state)
            .field("reason", &self.reason)
            .field("to_version", &self.to_version)
            .field("include_plugin_databases", &self.include_plugin_databases)
            .field("consistent_backup_read", &self.consistent_backup_read)
            .field("backend_at_capture", &self.backend_at_capture)
            .field("max_result_rows", &self.max_result_rows)
            .field("max_result_bytes", &self.max_result_bytes)
            .field("max_atomic_result_bytes", &self.max_atomic_result_bytes)
            .field("plugin_units", &self.plugin_units)
            .field("adapter", &self.adapter.as_ref().map(|_| "Some"))
            .field("in_process", &self.in_process)
            .finish()
    }
}

/// Result of publishing a recovery point.
#[derive(Debug, Clone)]
pub struct BackupOutcome {
    /// Repository root (`files_dir/backups`).
    pub dir: PathBuf,
    /// Parsed manifest.
    pub manifest: BackupManifest,
}

/// Catalog entry used by `bookclerk db backup list`.
#[derive(Debug, Clone)]
pub struct BackupListEntry {
    /// Parsed manifest.
    pub manifest: BackupManifest,
}

/// Recovery point whose objects, schema, and typed rows have been verified.
#[derive(Debug, Clone)]
pub struct ValidatedBackup {
    /// Parsed, supported manifest.
    pub manifest: BackupManifest,
    /// Library unit metadata (objects already verified in the repository).
    pub library: BackupUnit,
    /// Plugin binding units (already verified).
    pub plugin_units: Vec<BackupUnit>,
}

/// Library restore result plus plugin units for the host to apply.
///
/// Plugin databases are restored separately after this returns. A failure
/// while restoring a later unit leaves earlier units replaced (no bundle-level
/// atomicity). Each unit still uses complete replacement semantics.
#[derive(Debug, Clone)]
pub struct RestorePlan {
    /// Verified manifest.
    pub manifest: BackupManifest,
    /// Plugin units to restore onto the target adapter.
    pub plugin_units: Vec<BackupUnit>,
}

/// Writes a crash-safe recovery point under `files_dir/backups/`.
///
/// Skips only [`SchemaState::Uninitialized`]. The manifest is published only
/// after every referenced object exists. Pre-migrate backups prune older
/// automatic recovery points and garbage-collect unreferenced objects.
/// Manual backups are never auto-pruned.
///
/// # Errors
///
/// Returns when the files dir cannot be written, capture fails, or a requested
/// plugin binding is missing from [`BackupRequest::plugin_units`].
pub async fn backup_library(
    db: &DatabaseConnection,
    req: &BackupRequest,
) -> Result<Option<BackupOutcome>> {
    if matches!(req.schema_state, SchemaState::Uninitialized) {
        return Ok(None);
    }
    if !req.consistent_backup_read {
        return Err(LibraryError::Schema(
            "database adapter does not advertise consistentBackupRead; \
             backup of this backend is unsupported"
                .into(),
        ));
    }
    let repo = BackupRepository::open(&req.files_dir)?;
    let plugin_prepared = collect_plugin_units(db, req).await?;

    let export_opts = CanonicalExportOpts {
        consistent_backup_read: true,
        skip_tables: library_skip_tables(),
        max_result_rows: req.max_result_rows.max(1),
        max_result_bytes: req.max_result_bytes.max(1),
        max_atomic_result_bytes: req.max_atomic_result_bytes.max(1),
        chunk_target_bytes: CHUNK_TARGET_UNCOMPRESSED_BYTES,
        adapter: req.adapter.clone(),
        in_process: req.in_process,
    };
    let manifest = {
        let _lock = repo.lock_exclusive()?;
        let library_unit = capture_library_unit(
            db,
            &repo,
            &req.schema_state,
            &export_opts,
            &req.backend_at_capture,
        )
        .await?;

        let mut units = vec![library_unit];
        for prepared in &plugin_prepared {
            let plugin_opts = CanonicalExportOpts {
                consistent_backup_read: true,
                skip_tables: BTreeSet::new(),
                max_result_rows: prepared.max_result_rows.max(1),
                max_result_bytes: prepared.max_result_bytes.max(1),
                max_atomic_result_bytes: prepared.max_atomic_result_bytes.max(1),
                chunk_target_bytes: CHUNK_TARGET_UNCOMPRESSED_BYTES,
                adapter: prepared.adapter.clone().or_else(|| req.adapter.clone()),
                in_process: req.in_process,
            };
            units.push(
                capture_plugin_unit(
                    &prepared.db,
                    &repo,
                    &plugin_opts,
                    &prepared.plugin_id,
                    &prepared.binding,
                    &prepared.backend_at_capture,
                )
                .await?,
            );
        }

        let id = uuid::Uuid::new_v4().to_string();
        let created = Utc::now();
        let manifest = BackupManifest {
            format_version: BACKUP_FORMAT_VERSION,
            id: id.clone(),
            created_at: created.to_rfc3339(),
            schema_state: req.schema_state.display(),
            schema_checksum: req.schema_state.checksum().map(str::to_string),
            frozen_version: req.schema_state.frozen_version(),
            sql_contract_version: units[0].sql_contract_version,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            reason: req.reason,
            migrate_to: req.to_version,
            include_plugin_databases: req.include_plugin_databases,
            units: units.clone(),
        };
        repo.publish_manifest(&manifest)?;
        manifest
    };
    if req.reason == BackupReason::PreMigrate {
        prune_automatic_backups(&req.files_dir)?;
    }
    Ok(Some(BackupOutcome {
        dir: repo.root().to_path_buf(),
        manifest,
    }))
}

/// Match prepared plugin sessions to registry rows when inclusion is requested.
async fn collect_plugin_units(
    db: &DatabaseConnection,
    req: &BackupRequest,
) -> Result<Vec<PreparedPluginUnit>> {
    if !req.include_plugin_databases {
        if !req.plugin_units.is_empty() {
            return Err(LibraryError::Schema(
                "plugin backup units were provided without --include-plugin-databases".into(),
            ));
        }
        return Ok(Vec::new());
    }
    if matches!(req.schema_state, SchemaState::Uninitialized) {
        return Ok(Vec::new());
    }
    let registered = match list_plugin_registry(db).await {
        Ok(rows) => rows,
        Err(err) if registry_missing(&err) => Vec::new(),
        Err(err) => return Err(err),
    };
    let mut unused = req.plugin_units.clone();
    let mut out = Vec::with_capacity(registered.len());
    let mut seen = HashSet::new();
    for rec in &registered {
        let key = (rec.plugin_id.clone(), rec.binding.clone());
        if !seen.insert(key.clone()) {
            return Err(LibraryError::Schema(format!(
                "plugin database `{}/{}` is registered more than once",
                rec.plugin_id, rec.binding
            )));
        }
        let idx = unused
            .iter()
            .position(|u| u.plugin_id == rec.plugin_id && u.binding == rec.binding);
        let Some(idx) = idx else {
            return Err(LibraryError::Schema(format!(
                "plugin database `{}/{}` is registered but was not captured; \
                 --include-plugin-databases fails closed rather than omitting bindings",
                rec.plugin_id, rec.binding
            )));
        };
        out.push(unused.remove(idx));
    }
    if let Some(extra) = unused.first() {
        return Err(LibraryError::Schema(format!(
            "plugin backup unit `{}/{}` is not in the plugin_databases registry",
            extra.plugin_id, extra.binding
        )));
    }
    Ok(out)
}

/// List `plugin_databases` rows from the library connection.
async fn list_plugin_registry(db: &DatabaseConnection) -> Result<Vec<crate::PluginDatabaseRecord>> {
    LibraryStore::from_connection(db.clone())
        .list_plugin_databases(None)
        .await
        .map_err(|err| {
            LibraryError::Schema(format!(
                "backup cannot list plugin_databases registry: {err}"
            ))
        })
}

/// True when `err` indicates the plugin-database registry table does not exist yet.
fn registry_missing(err: &LibraryError) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("plugin_databases")
        && (msg.contains("no such table")
            || msg.contains("does not exist")
            || msg.contains("no such relation"))
}

/// Lists published recovery points oldest-first.
///
/// # Errors
///
/// Returns when the repository cannot be opened.
pub fn list_backups(files_dir: &Path) -> Result<Vec<BackupListEntry>> {
    let repo = BackupRepository::open(files_dir)?;
    Ok(repo
        .list_manifests()
        .into_iter()
        .map(|manifest| BackupListEntry { manifest })
        .collect())
}

/// Resolves a recovery-point id, timestamp prefix, or archive path.
///
/// # Errors
///
/// Returns when no complete backup matches or the spec is ambiguous.
pub fn resolve_backup_spec(files_dir: &Path, spec: &str) -> Result<BackupResolve> {
    let as_path = PathBuf::from(spec);
    if as_path.is_file() {
        return Ok(BackupResolve::Archive(as_path));
    }
    let repo = BackupRepository::open(files_dir)?;
    if repo.read_manifest(spec).is_ok() {
        return Ok(BackupResolve::Id(spec.to_string()));
    }
    let entries = repo.list_manifests();
    if let Some(found) = entries.iter().rev().find(|m| m.id == spec) {
        return Ok(BackupResolve::Id(found.id.clone()));
    }
    let matches: Vec<_> = entries
        .iter()
        .filter(|m| m.created_at.starts_with(spec) || m.id.starts_with(spec))
        .collect();
    match matches.as_slice() {
        [one] => Ok(BackupResolve::Id(one.id.clone())),
        [] => Err(LibraryError::Schema(format!(
            "no backup recovery point matches `{spec}`"
        ))),
        many => Err(LibraryError::Schema(format!(
            "backup spec `{spec}` is ambiguous ({} matches)",
            many.len()
        ))),
    }
}

/// How [`resolve_backup_spec`] located a recovery point.
#[derive(Debug, Clone)]
pub enum BackupResolve {
    /// Catalog id under `backups/manifests/`.
    Id(String),
    /// `.tar.gz` archive of one recovery point.
    Archive(PathBuf),
}

/// Restores the library unit after verifying **all** objects and units.
///
/// Plugin units are returned for the host/CLI to restore onto the target
/// adapter. Restore replaces Bookclerk-visible schema for the library unit
/// and does not merge or auto-migrate. Plugin-owned migrations are not run.
///
/// The library `plugin_databases` registry is preserved when the recovery
/// point carries no plugin units (including `--include-plugin-databases` with
/// an empty registry at capture). When plugin units are present, library
/// restore clears the registry so leftover bindings cannot outlive the
/// captured set; the host/CLI rebinds while restoring those units.
///
/// # Errors
///
/// Returns when the backup is unsupported, an object is missing/corrupt, or
/// library replace fails. Destructive SQL starts only after complete preflight.
pub async fn restore_backup(
    db: &DatabaseConnection,
    files_dir: &Path,
    id: &str,
    opts: &CanonicalRestoreOpts,
) -> Result<RestorePlan> {
    let repo = BackupRepository::open(files_dir)?;
    restore_backup_in_repo(db, &repo, id, opts).await
}

/// Restores from an already-open repository (extracted archives).
///
/// Target [`crate::SchemaState`] is read from the canonical
/// `bookclerk_schema_migrations` rows. This function never infers a
/// versioning contract from [`sea_orm::DbBackend`].
///
/// # Errors
///
/// Same as [`restore_backup`].
pub async fn restore_backup_in_repo(
    db: &DatabaseConnection,
    repo: &BackupRepository,
    id: &str,
    opts: &CanonicalRestoreOpts,
) -> Result<RestorePlan> {
    ensure_restore_target_is_replaceable(db).await?;
    let validated = verify_recovery_point(repo, id)?;
    // Preserve when no plugin units were captured — `include_plugin_databases`
    // alone is not enough (an empty registry at backup still sets the flag).
    let preserve_plugin_registry = validated.plugin_units.is_empty();
    restore_backup_unit(
        db,
        repo,
        &validated.library,
        CanonicalRestoreKind::Library,
        opts,
        preserve_plugin_registry,
    )
    .await?;
    Ok(RestorePlan {
        manifest: validated.manifest,
        plugin_units: validated.plugin_units,
    })
}

/// Deletes oldest automatic pre-migrate recovery points. Never deletes `manual`.
///
/// # Errors
///
/// Returns when a manifest cannot be unlinked.
pub fn prune_automatic_backups(files_dir: &Path) -> Result<usize> {
    let repo = BackupRepository::open(files_dir)?;
    let mut autos: Vec<(String, String)> = Vec::new();
    for manifest in repo.list_manifests_strict()? {
        if manifest.reason == BackupReason::Manual {
            continue;
        }
        autos.push((manifest.created_at, manifest.id));
    }
    autos.sort_by(|a, b| a.0.cmp(&b.0));
    let extra = autos.len().saturating_sub(BACKUP_RETENTION);
    let mut removed = 0usize;
    for (_, id) in autos.into_iter().take(extra) {
        if repo.delete_manifest(&id)? {
            removed += 1;
        }
    }
    repo.gc_unreferenced_objects()?;
    Ok(removed)
}

/// Packages one recovery point (manifest + referenced objects) as `.tar.gz`.
///
/// # Errors
///
/// Returns when the destination cannot be written or objects are missing.
pub fn archive_backup(files_dir: &Path, id: &str, dest: &Path) -> Result<()> {
    let repo = BackupRepository::open(files_dir)?;
    let manifest = repo.read_manifest(id)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(dest)?;
    let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut builder = tar::Builder::new(enc);
    let manifest_path = repo.root().join("manifests").join(format!("{id}.json"));
    let hash_path = repo.root().join("manifests").join(format!("{id}.sha256"));
    builder.append_path_with_name(&manifest_path, format!("manifests/{id}.json"))?;
    builder.append_path_with_name(&hash_path, format!("manifests/{id}.sha256"))?;
    for digest in manifest.referenced_objects() {
        let rel = format!("objects/{}/{}", &digest[..2], &digest[2..]);
        let path = repo.root().join(&rel);
        if !path.is_file() {
            return Err(LibraryError::Schema(format!(
                "cannot archive backup `{id}`: object `{digest}` is missing"
            )));
        }
        builder.append_path_with_name(&path, rel)?;
    }
    let enc = builder
        .into_inner()
        .map_err(|err| LibraryError::Other(anyhow::anyhow!("backup tar: {err}")))?;
    enc.finish()
        .map_err(|err| LibraryError::Other(anyhow::anyhow!("backup gzip: {err}")))?;
    Ok(())
}

/// Extracts a `.tar.gz` recovery-point archive into `dest` (a backups root).
///
/// Expansion is bounded by entry count, per-entry size, total expanded bytes,
/// and gzip-decoded tar stream size so a small archive cannot fill the disk.
///
/// # Errors
///
/// Returns when the archive is missing, malformed, a path escapes `dest`, or
/// an extract budget would be exceeded.
pub fn extract_backup_archive(archive: &Path, dest: &Path) -> Result<()> {
    extract_backup_archive_limited(archive, dest, ArchiveExtractLimits::default())
}

/// Budgets applied while streaming a recovery-point archive.
#[derive(Debug, Clone, Copy)]
struct ArchiveExtractLimits {
    /// Max tar members (files and directories).
    max_entries: u64,
    /// Max expanded size of one regular file member.
    max_entry_bytes: u64,
    /// Max total expanded bytes across regular file members.
    max_total_bytes: u64,
    /// Max gzip-decoded tar stream bytes.
    max_stream_bytes: u64,
}

impl Default for ArchiveExtractLimits {
    fn default() -> Self {
        Self {
            max_entries: MAX_BACKUP_ARCHIVE_ENTRIES,
            max_entry_bytes: MAX_BACKUP_ARCHIVE_ENTRY_BYTES,
            max_total_bytes: MAX_BACKUP_ARCHIVE_TOTAL_BYTES,
            max_stream_bytes: MAX_BACKUP_ARCHIVE_STREAM_BYTES,
        }
    }
}

/// [`Read`] adapter that fails when more than `remaining` bytes are produced.
struct BudgetReader<R> {
    /// Inner gzip decoder (or other reader).
    inner: R,
    /// Bytes still allowed before the stream budget is exhausted.
    remaining: u64,
}

impl<R: Read> Read for BudgetReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut probe = [0u8; 1];
            return match self.inner.read(&mut probe) {
                Ok(0) => Ok(0),
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "backup archive gzip stream exceeded budget",
                )),
                Err(err) => Err(err),
            };
        }
        let max = usize::try_from(self.remaining)
            .unwrap_or(buf.len())
            .min(buf.len());
        let n = self.inner.read(&mut buf[..max])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}

/// Maps tar/gzip `InvalidData` (budget overruns) onto [`LibraryError::Schema`].
fn map_extract_io(err: io::Error) -> LibraryError {
    if err.kind() == io::ErrorKind::InvalidData {
        LibraryError::Schema(err.to_string())
    } else {
        err.into()
    }
}

/// Extracts `archive` into `dest` using `limits` instead of the production caps.
///
/// # Errors
///
/// Returns when the archive is missing, malformed, a path escapes `dest`, or
/// an extract budget would be exceeded.
fn extract_backup_archive_limited(
    archive: &Path,
    dest: &Path,
    limits: ArchiveExtractLimits,
) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    let file = std::fs::File::open(archive)?;
    let dec = flate2::read::GzDecoder::new(file);
    let bounded = BudgetReader {
        inner: dec,
        remaining: limits.max_stream_bytes,
    };
    let mut archive = tar::Archive::new(bounded);
    let mut entries = 0u64;
    let mut total_bytes = 0u64;
    for entry in archive.entries().map_err(map_extract_io)? {
        let mut entry = entry.map_err(map_extract_io)?;
        entries = entries.saturating_add(1);
        if entries > limits.max_entries {
            return Err(LibraryError::Schema(format!(
                "backup archive has more than {} entries",
                limits.max_entries
            )));
        }
        let path = entry.path().map_err(map_extract_io)?.into_owned();
        if path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(LibraryError::Schema(format!(
                "backup archive path escapes destination: {}",
                path.display()
            )));
        }
        let size = entry.header().size().map_err(map_extract_io)?;
        match entry.header().entry_type() {
            tar::EntryType::Regular => {
                if size > limits.max_entry_bytes {
                    return Err(LibraryError::Schema(format!(
                        "backup archive entry `{}` is {size} bytes (max {})",
                        path.display(),
                        limits.max_entry_bytes
                    )));
                }
                total_bytes = total_bytes.saturating_add(size);
                if total_bytes > limits.max_total_bytes {
                    return Err(LibraryError::Schema(format!(
                        "backup archive expanded size {total_bytes} exceeds {} bytes",
                        limits.max_total_bytes
                    )));
                }
            }
            tar::EntryType::Directory => {}
            other => {
                return Err(LibraryError::Schema(format!(
                    "backup archive entry type {other:?} is not allowed"
                )));
            }
        }
        let dest_path = dest.join(&path);
        if !dest_path.starts_with(dest) {
            return Err(LibraryError::Schema(format!(
                "backup archive path escapes destination: {}",
                path.display()
            )));
        }
        entry.unpack(dest_path).map_err(map_extract_io)?;
    }
    Ok(())
}

/// Optional in-place backup taken before applying ups or downs.
#[derive(Clone)]
pub struct SchemaBackupOpts {
    /// `$BOOKCLERK_FILES_DIR` root for `backups/`.
    pub files_dir: PathBuf,
    /// Include every registered plugin-owned database binding.
    pub include_plugin_databases: bool,
    /// Guest advertised consistent backup read.
    pub consistent_backup_read: bool,
    /// Adapter id at capture (diagnostic).
    pub backend_at_capture: String,
    /// Negotiated page size.
    pub max_result_rows: u32,
    /// Negotiated encoded SELECT-page budget.
    pub max_result_bytes: u32,
    /// Negotiated encoded atomic-reply budget.
    pub max_atomic_result_bytes: u32,
    /// Plugin sessions prepared by the host when inclusion is requested.
    pub plugin_units: Vec<PreparedPluginUnit>,
    /// Library adapter hooks. `None` uses the in-process SDK (tests).
    pub adapter: Option<SharedAdapterBackupOps>,
}

impl std::fmt::Debug for SchemaBackupOpts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchemaBackupOpts")
            .field("files_dir", &self.files_dir)
            .field("include_plugin_databases", &self.include_plugin_databases)
            .field("consistent_backup_read", &self.consistent_backup_read)
            .field("backend_at_capture", &self.backend_at_capture)
            .field("max_result_rows", &self.max_result_rows)
            .field("max_result_bytes", &self.max_result_bytes)
            .field("max_atomic_result_bytes", &self.max_atomic_result_bytes)
            .field("plugin_units", &self.plugin_units)
            .field("adapter", &self.adapter.as_ref().map(|_| "Some"))
            .finish()
    }
}

impl Default for SchemaBackupOpts {
    fn default() -> Self {
        Self {
            files_dir: PathBuf::new(),
            include_plugin_databases: false,
            consistent_backup_read: false,
            backend_at_capture: String::new(),
            max_result_rows: FIRST_PARTY_MAX_RESULT_ROWS,
            max_result_bytes: FIRST_PARTY_MAX_RESULT_BYTES,
            max_atomic_result_bytes: FIRST_PARTY_MAX_RESULT_BYTES,
            plugin_units: Vec::new(),
            adapter: None,
        }
    }
}

#[cfg(test)]
mod tests;
