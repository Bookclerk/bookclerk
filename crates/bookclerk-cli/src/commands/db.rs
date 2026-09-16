//! `bookclerk db` — schema state, backups, and explicit migrate.

use std::path::{Path, PathBuf};

use bookclerk_config::Config;
use bookclerk_library::migrations::host_migration_plan;
use bookclerk_library::{
    archive_backup, backup_library, current_schema_state, ensure_restore_target_is_replaceable,
    extract_backup_archive, list_backups, min_supported_schema_version, prune_automatic_backups,
    resolve_backup_spec, restore_backup, restore_backup_in_repo, verify_recovery_point,
    BackupReason, BackupRepository, BackupRequest, BackupResolve, CanonicalRestoreOpts,
    SchemaApplyOptions, SchemaBackupOpts, SchemaState, SCHEMA_VERSION,
};
use clap::Subcommand;
use sea_orm::DatabaseConnection;
use serde_json::json;

use crate::format_out::{emit, OutputFormat};

#[derive(Debug, Subcommand)]
/// Host schema versioning, backups, and explicit ups/downs (`migrate --to`).
pub enum DbCommand {
    /// Show this binary's frozen plan and the database's explicit schema state.
    Version,
    /// Canonical Bookclerk backup (logically full recovery points).
    Backup {
        #[command(subcommand)]
        /// Nested `backup` verb (`create`, `list`, `verify`, `prune`).
        command: BackupCommand,
    },
    /// Restore schema and data from a recovery-point id, archive, or timestamp.
    Restore {
        /// Recovery-point id, `.tar.gz` archive, or `created_at` prefix.
        ///
        /// Restores onto the currently configured database adapter (not
        /// necessarily the adapter that captured the backup).
        #[arg(long)]
        from: String,
    },
    /// Apply frozen ups or reversible downs to reach `--to`.
    Migrate {
        /// Target frozen schema version (defaults to this binary's [`SCHEMA_VERSION`]).
        #[arg(long)]
        to: Option<i64>,
        /// Include plugin-owned database bindings in the pre-migrate backup.
        #[arg(long)]
        include_plugin_databases: bool,
    },
}

#[derive(Debug, Subcommand)]
/// `bookclerk db backup` verbs.
pub enum BackupCommand {
    /// Write a manual recovery point under `files_dir/backups/`.
    Create {
        /// Optional `.tar.gz` destination for this recovery point.
        #[arg(long)]
        path: Option<PathBuf>,
        /// Capture plugin-owned database bindings in portable Bookclerk SQL.
        ///
        /// Plugin schema migration remains plugin-owned. Each database unit is
        /// replaced completely; a multi-database bundle is not one transaction.
        #[arg(long)]
        include_plugin_databases: bool,
    },
    /// List recovery points oldest-first.
    List,
    /// Verify a recovery point (objects, schema, typed rows) without restoring.
    Verify {
        /// Recovery-point id, archive, or `created_at` prefix.
        #[arg(long)]
        from: String,
    },
    /// Prune automatic pre-migrate recovery points and unreferenced objects.
    ///
    /// Manual backups are never removed.
    Prune,
}

/// Dispatches a `bookclerk db` verb.
pub async fn run(command: DbCommand, config: &Config, format: OutputFormat) -> anyhow::Result<()> {
    match command {
        DbCommand::Version => run_version(config, format).await,
        DbCommand::Backup { command } => match command {
            BackupCommand::Create {
                path,
                include_plugin_databases,
            } => run_backup_create(config, format, path, include_plugin_databases).await,
            BackupCommand::List => run_backup_list(config, format).await,
            BackupCommand::Verify { from } => run_backup_verify(config, format, from).await,
            BackupCommand::Prune => run_backup_prune(config, format).await,
        },
        DbCommand::Restore { from } => run_restore(config, format, from).await,
        DbCommand::Migrate {
            to,
            include_plugin_databases,
        } => {
            run_migrate(
                config,
                format,
                to.unwrap_or(SCHEMA_VERSION),
                include_plugin_databases,
            )
            .await
        }
    }
}

/// Opens the library guest without applying host schema (CLI migrate / version).
async fn open_unmigrated(
    config: &Config,
) -> anyhow::Result<(DatabaseConnection, bookclerk_plugin_abi::DbCapabilities)> {
    let registry = bookclerk_plugin_host::load_external_database(config).await?;
    let ext = registry.active().ok_or_else(|| {
        anyhow::anyhow!("no active database plugin — stage and enable [database].plugin")
    })?;
    let (db, caps) = ext.connect_without_migrate(config).await?;
    bookclerk_library::require_schema_migrations(&caps)?;
    Ok((db, caps))
}

/// Backup options used before an explicit CLI migrate.
async fn apply_opts(
    config: &Config,
    db: &DatabaseConnection,
    state: &SchemaState,
    include_plugin_databases: bool,
    caps: &bookclerk_plugin_abi::DbCapabilities,
) -> anyhow::Result<SchemaApplyOptions> {
    let backend_at_capture = bookclerk_plugin_host::backup_adapter_id(&config.database.plugin);
    let registry = bookclerk_plugin_host::load_external_database(config).await?;
    let ext = registry.active().ok_or_else(|| {
        anyhow::anyhow!("no active database plugin — stage and enable [database].plugin")
    })?;
    let plugin_units = if include_plugin_databases && !matches!(state, SchemaState::Uninitialized) {
        bookclerk_plugin_host::export_registered_plugin_units(&ext, config, db)
            .await
            .map_err(|err| anyhow::anyhow!("{err}"))?
    } else {
        Vec::new()
    };
    Ok(SchemaApplyOptions {
        backup: Some(SchemaBackupOpts {
            files_dir: config.paths().files_dir.clone(),
            include_plugin_databases,
            consistent_backup_read: caps.supports_consistent_backup_read(),
            backend_at_capture,
            max_result_rows: caps.max_result_rows,
            max_result_bytes: caps.max_result_bytes,
            max_atomic_result_bytes: caps.max_atomic_result_bytes,
            plugin_units,
            adapter: Some(ext.library_backup_ops()),
        }),
    })
}

/// Prints this binary's frozen plan and the database's [`SchemaState`].
async fn run_version(config: &Config, format: OutputFormat) -> anyhow::Result<()> {
    let (db, _) = open_unmigrated(config).await?;
    let state = current_schema_state(&db).await?;
    let plan = host_migration_plan();
    let frozen = state.frozen_version();
    let step = frozen.and_then(|v| plan.iter().find(|s| s.version == v));
    let payload = json!({
        "binary_frozen_plan_version": SCHEMA_VERSION,
        "min_supported_schema_version": min_supported_schema_version(),
        "database_schema_state": state.display(),
        "database_frozen_version": frozen,
        "checksum": state.checksum(),
        "introduced_in": step.map(|s| s.introduced_in),
        "app_version": env!("CARGO_PKG_VERSION"),
        "reversible": step.map(|s| s.reversible()).unwrap_or(false),
    });
    emit(format, &payload, || {
        println!(
            "binary frozen plan {}  database {}  app {}",
            SCHEMA_VERSION,
            state,
            env!("CARGO_PKG_VERSION")
        );
    })
}

/// Writes a manual recovery point and optionally packages `--path` as `.tar.gz`.
async fn run_backup_create(
    config: &Config,
    format: OutputFormat,
    path: Option<PathBuf>,
    include_plugin_databases: bool,
) -> anyhow::Result<()> {
    let (db, caps) = open_unmigrated(config).await?;
    if !caps.supports_consistent_backup_read() {
        anyhow::bail!(
            "database adapter does not advertise consistentBackupRead; backup is unsupported"
        );
    }
    let state = current_schema_state(&db).await?;
    let backend_at_capture = bookclerk_plugin_host::backup_adapter_id(&config.database.plugin);
    let registry = bookclerk_plugin_host::load_external_database(config).await?;
    let ext = registry.active().ok_or_else(|| {
        anyhow::anyhow!("no active database plugin — stage and enable [database].plugin")
    })?;
    let plugin_units = if include_plugin_databases && !matches!(state, SchemaState::Uninitialized) {
        bookclerk_plugin_host::export_registered_plugin_units(&ext, config, &db)
            .await
            .map_err(|err| anyhow::anyhow!("{err}"))?
    } else {
        Vec::new()
    };
    let req = BackupRequest {
        files_dir: config.paths().files_dir.clone(),
        schema_state: state,
        reason: BackupReason::Manual,
        to_version: 0,
        include_plugin_databases,
        consistent_backup_read: true,
        backend_at_capture,
        max_result_rows: caps.max_result_rows,
        max_result_bytes: caps.max_result_bytes,
        max_atomic_result_bytes: caps.max_atomic_result_bytes,
        plugin_units,
        adapter: Some(ext.library_backup_ops()),
        in_process: false,
    };
    let outcome = backup_library(&db, &req)
        .await?
        .ok_or_else(|| anyhow::anyhow!("database is uninitialized; nothing to back up"))?;
    let mut archive = None;
    if let Some(dest) = path {
        if is_tar_gz(&dest) {
            archive_backup(&config.paths().files_dir, &outcome.manifest.id, &dest)?;
            archive = Some(dest.display().to_string());
        } else {
            anyhow::bail!("--path must be a .tar.gz archive for a recovery point");
        }
    }
    let payload = json!({
        "id": outcome.manifest.id,
        "dir": outcome.dir.display().to_string(),
        "schema_state": outcome.manifest.schema_state,
        "reason": outcome.manifest.reason,
        "sql_contract_version": outcome.manifest.sql_contract_version,
        "units": outcome.manifest.units.len(),
        "archive": archive,
    });
    emit(format, &payload, || {
        println!("backup {}", outcome.manifest.id);
    })
}

/// Lists catalog recovery points by time.
async fn run_backup_list(config: &Config, format: OutputFormat) -> anyhow::Result<()> {
    let entries = list_backups(&config.paths().files_dir)?;
    let payload = json!(entries
        .iter()
        .map(|e| json!({
            "id": e.manifest.id,
            "created_at": e.manifest.created_at,
            "schema_state": e.manifest.schema_state,
            "reason": e.manifest.reason,
        }))
        .collect::<Vec<_>>());
    emit(format, &payload, || {
        if entries.is_empty() {
            println!("no backups");
        }
        for entry in &entries {
            println!(
                "{}  {}  {}",
                entry.manifest.created_at, entry.manifest.id, entry.manifest.schema_state
            );
        }
    })
}

/// Verifies a recovery-point id or archive without restoring.
async fn run_backup_verify(
    config: &Config,
    format: OutputFormat,
    from: String,
) -> anyhow::Result<()> {
    let files_dir = config.paths().files_dir.clone();
    let (repo_root, id, is_archive, _unpack) = resolve_for_use(&files_dir, &from)?;
    let repo = if is_archive {
        BackupRepository::open_root(&repo_root)?
    } else {
        BackupRepository::open(&repo_root)?
    };
    let validated = verify_recovery_point(&repo, &id)?;
    let payload = json!({
        "id": validated.manifest.id,
        "schema_state": validated.manifest.schema_state,
        "units": validated.manifest.units.len(),
        "objects": validated.manifest.referenced_objects().len(),
        "ok": true,
    });
    emit(format, &payload, || {
        println!("verified {}", validated.manifest.id);
    })
}

/// Prune automatic pre-migrate recovery points; never deletes manuals.
async fn run_backup_prune(config: &Config, format: OutputFormat) -> anyhow::Result<()> {
    let removed = prune_automatic_backups(&config.paths().files_dir)?;
    let payload = json!({ "removed_automatic": removed });
    emit(format, &payload, || {
        println!("pruned {removed} automatic recovery points");
    })
}

/// Restores schema and data from a recovery point.
async fn run_restore(config: &Config, format: OutputFormat, from: String) -> anyhow::Result<()> {
    let files_dir = config.paths().files_dir.clone();
    let (repo_root, id, is_archive, _unpack) = resolve_for_use(&files_dir, &from)?;

    let (db, caps) = open_unmigrated(config).await?;
    if !caps.supports_atomic_unit_restore() {
        anyhow::bail!(
            "database adapter does not advertise atomicUnitRestore; restore is unsupported"
        );
    }
    ensure_restore_target_is_replaceable(&db)
        .await
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    let registry = bookclerk_plugin_host::load_external_database(config).await?;
    let ext = registry.active().ok_or_else(|| {
        anyhow::anyhow!("no active database plugin — stage and enable [database].plugin")
    })?;
    let opts = CanonicalRestoreOpts {
        adapter: Some(ext.library_backup_ops()),
        ..CanonicalRestoreOpts::from_caps(&caps)?
    };
    let plan = if is_archive {
        let repo = BackupRepository::open_root(&repo_root)?;
        let plan = restore_backup_in_repo(&db, &repo, &id, &opts).await?;
        if !plan.plugin_units.is_empty() {
            restore_plugins(config, &db, &repo, &plan.plugin_units).await?;
        }
        plan
    } else {
        let plan = restore_backup(&db, &repo_root, &id, &opts).await?;
        if !plan.plugin_units.is_empty() {
            let repo = BackupRepository::open(&repo_root)?;
            restore_plugins(config, &db, &repo, &plan.plugin_units).await?;
        }
        plan
    };
    let payload = json!({
        "schema_state": plan.manifest.schema_state,
        "sql_contract_version": plan.manifest.sql_contract_version,
        "units": plan.manifest.units.len(),
        "from": from,
        "recovery_point": id,
        "partial_bundle_atomicity": "per-unit; not transactional across databases",
    });
    emit(format, &payload, || {
        println!("restored {} from {from}", plan.manifest.schema_state);
    })
}

/// Restore plugin units onto the active adapter; reports how many succeeded.
async fn restore_plugins(
    config: &Config,
    db: &DatabaseConnection,
    repo: &BackupRepository,
    units: &[bookclerk_library::BackupUnit],
) -> anyhow::Result<()> {
    let registry = bookclerk_plugin_host::load_external_database(config).await?;
    let ext = registry.active().ok_or_else(|| {
        anyhow::anyhow!("no active database plugin — stage and enable [database].plugin")
    })?;
    bookclerk_plugin_host::restore_plugin_backup_units(ext.as_ref(), config, db, repo, units)
        .await
        .map_err(|err| anyhow::anyhow!("{err}"))
}

/// Resolve `--from` to a repository root and recovery-point id.
fn resolve_for_use(
    files_dir: &Path,
    from: &str,
) -> anyhow::Result<(PathBuf, String, bool, UnpackCleanup)> {
    match resolve_backup_spec(files_dir, from)? {
        BackupResolve::Id(id) => Ok((files_dir.to_path_buf(), id, false, UnpackCleanup(None))),
        BackupResolve::Archive(archive) => {
            let unpack_root = files_dir.join("backups").join(format!(
                ".unpack-{}",
                chrono::Utc::now().format("%Y%m%dT%H%M%S%.fZ")
            ));
            extract_backup_archive(&archive, &unpack_root)?;
            let repo = BackupRepository::open_root(&unpack_root)?;
            let manifests = repo.list_manifests();
            let id = manifests
                .last()
                .map(|m| m.id.clone())
                .ok_or_else(|| anyhow::anyhow!("backup archive contains no recovery point"))?;
            Ok((
                unpack_root.clone(),
                id,
                true,
                UnpackCleanup(Some(unpack_root)),
            ))
        }
    }
}

/// Applies ups or last-reversible downs toward `target`.
async fn run_migrate(
    config: &Config,
    format: OutputFormat,
    target: i64,
    include_plugin_databases: bool,
) -> anyhow::Result<()> {
    let (db, caps) = open_unmigrated(config).await?;
    let state = current_schema_state(&db).await?;
    let walk = bookclerk_plugin_host::migrate_library_schema(
        config,
        target,
        apply_opts(config, &db, &state, include_plugin_databases, &caps).await?,
    )
    .await?;
    let (db, _) = open_unmigrated(config).await?;
    let after = current_schema_state(&db).await?;
    let payload = json!({
        "from": walk.from,
        "requested_to": walk.requested_to,
        "stopped_at": walk.stopped_at,
        "blocked": walk.blocked,
        "after_state": after.display(),
    });
    if walk.blocked || frozen_or_zero(&after) != target {
        emit(format, &payload, || {
            eprintln!(
                "stopped at schema {} (requested {target}); restore a backup if this binary cannot start",
                walk.stopped_at
            );
        })?;
        anyhow::bail!(
            "schema is still at {} (target {target}); restore a backup",
            walk.stopped_at
        );
    }
    emit(format, &payload, || {
        println!("schema {} -> {}", walk.from, walk.stopped_at);
    })
}

/// Frozen revision, or `0` when the database is uninitialized or unreleased.
fn frozen_or_zero(state: &SchemaState) -> i64 {
    state.frozen_version().unwrap_or(0)
}

/// True when `path` looks like a gzipped tar archive.
fn is_tar_gz(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    name.ends_with(".tar.gz") || name.ends_with(".tgz")
}

/// Deletes a temporary archive unpack directory when the CLI command ends.
struct UnpackCleanup(Option<PathBuf>);
impl Drop for UnpackCleanup {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

#[cfg(test)]
#[allow(clippy::missing_docs_in_private_items)]
mod tests {
    use super::*;

    #[test]
    fn version_display_never_says_schema_zero_for_unreleased() {
        let unreleased = SchemaState::Unreleased {
            base_version: 0,
            checksum: "abc".into(),
        };
        assert_eq!(unreleased.display(), "unreleased@base0+abc");
        assert_ne!(unreleased.display(), "0");
        assert_eq!(SchemaState::Uninitialized.display(), "uninitialized");
    }
}
