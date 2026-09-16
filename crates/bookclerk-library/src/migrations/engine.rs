//! Shared apply engine for namespaced host-owned [`super::MigrationPlan`]s.
//!
//! Host library frozen steps and Bookclerk binding bootstrap persist
//! [`crate::SchemaState`] in `schema_migrations` keyed by namespace.
//! Plugin-owned binding evolution uses [`super::plugin`] (opaque IDs and
//! `plugin_migrations`), not this numeric engine.

use std::time::Duration;

use bookclerk_plugin_abi::{
    DbPlanStatementKind, DbResultSelection, ExecuteRequest, SqlTypeEnv, TypedDbStatement,
};
use sea_orm::DatabaseConnection;

use super::plan::{
    frozen_marker_delete_sql, frozen_marker_sql, schema_slot_key, sql_string_literal,
    MigrationPlan, MigrationStep, PlanOp,
};
use crate::error::{LibraryError, Result};
use crate::host_schema::{current_schema_state_in, ensure_schema_migrations, HostSchemaKind};
use crate::schema_state::SchemaState;
use crate::sql_plan::execute_typed_on_binding;

/// Timing label for shared schema apply (not an adapter identity).
const SCHEMA_TXN_TIMING: &str = "schema_txn";

/// Whether [`apply_migration_plan`] / [`downgrade_migration_plan`] is walking up or down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyDirection {
    /// Forward upgrade (installed plan newer than stored state).
    Upgrade,
    /// Explicit downgrade (every traversed step must have `down`).
    Downgrade,
}

/// Applies remaining frozen steps in `plan` for `plan.namespace`.
///
/// Takes the portable `schema:{namespace}` serialization slot, re-reads
/// durable state, and applies each missing step plus its marker as one
/// atomic [`ExecuteRequest`]. Matching version+checksum after an ambiguous
/// completion is success. Absent marker retries the same step.
/// Contradictory checksum/version fails closed. Constraint/type failures
/// are not retries.
///
/// Stored frozen state newer than this plan, or a checksum that is not in
/// the installed plan, fails closed. Empty plan: uninitialized is a no-op;
/// any stored frozen/unreleased rows fail closed.
///
/// Does **not** run as part of destructive restore. Restore writes captured
/// ledger rows; a later ordinary open walks forward.
///
/// # Errors
///
/// Returns [`LibraryError::Schema`] when the ledger is contradictory, the
/// installed plan cannot understand stored state, or apply exhausts retries.
pub async fn apply_migration_plan(
    db: &DatabaseConnection,
    plan: &MigrationPlan,
) -> Result<SchemaState> {
    ensure_schema_migrations(db).await?;
    lock_schema_slot(db, &plan.namespace).await?;
    let state = current_schema_state_in(db, HostSchemaKind::RowMarker, &plan.namespace).await?;
    let remaining = forward_steps(plan, &state)?;
    for step in remaining {
        apply_one_frozen_step(db, plan, step, ApplyDirection::Upgrade).await?;
    }
    current_schema_state_in(db, HostSchemaKind::RowMarker, &plan.namespace).await
}

/// Explicit downgrade of `plan.namespace` toward `to_version` (0 = fully reverse).
///
/// Never runs on ordinary open. Every traversed step must have a proven `down`;
/// otherwise this fails closed and the operator must restore a recovery point.
/// An older plugin binary must not call this implicitly.
///
/// # Errors
///
/// Returns when stored state is not frozen, a step is irreversible, or apply fails.
pub async fn downgrade_migration_plan(
    db: &DatabaseConnection,
    plan: &MigrationPlan,
    to_version: i64,
) -> Result<SchemaState> {
    if to_version < 0 {
        return Err(LibraryError::Schema(
            "cannot downgrade to a negative schema version".into(),
        ));
    }
    ensure_schema_migrations(db).await?;
    lock_schema_slot(db, &plan.namespace).await?;
    let state = current_schema_state_in(db, HostSchemaKind::RowMarker, &plan.namespace).await?;
    let downs = reverse_steps(plan, &state, to_version)?;
    for step in downs {
        apply_one_frozen_step(db, plan, step, ApplyDirection::Downgrade).await?;
    }
    current_schema_state_in(db, HostSchemaKind::RowMarker, &plan.namespace).await
}

/// True when a binding session's expected host schema still matches durable state.
///
/// # Errors
///
/// Returns when `observed` is a newer frozen revision, a different checksum,
/// or a different discriminant than `expected`.
pub fn schema_session_matches(expected: &SchemaState, observed: &SchemaState) -> Result<()> {
    if expected == observed {
        return Ok(());
    }
    Err(LibraryError::Schema(format!(
        "binding schema advanced under this session (expected {}, found {}); \
         reopen the binding",
        expected.display(),
        observed.display()
    )))
}

/// Remaining forward apply batches for `plan` given durable `state`.
///
/// Each inner `Vec` is one atomic unit (slot lock, ops, marker). Used by the
/// RPC binding path that cannot run SeaORM [`apply_migration_plan`].
///
/// # Errors
///
/// Returns when stored state cannot be understood by `plan`.
pub fn remaining_upgrade_batches(
    plan: &MigrationPlan,
    state: &SchemaState,
) -> Result<Vec<Vec<String>>> {
    Ok(forward_steps(plan, state)?
        .into_iter()
        .map(|step| upgrade_statements(&plan.namespace, step))
        .collect())
}

/// Frozen steps still needed to bring `state` up to `plan`.
fn forward_steps<'a>(
    plan: &'a MigrationPlan,
    state: &SchemaState,
) -> Result<Vec<&'a MigrationStep>> {
    match state {
        SchemaState::Uninitialized => {
            if plan.steps.is_empty() {
                return Ok(Vec::new());
            }
            Ok(plan.steps.iter().collect())
        }
        SchemaState::Unreleased {
            base_version,
            checksum,
        } => Err(LibraryError::Schema(format!(
            "namespace `{}` is unreleased@base{base_version}+{checksum}; \
             plugin migration plans are frozen steps and will not reshape \
             an unreleased ledger — restore or drop the binding",
            plan.namespace
        ))),
        SchemaState::Frozen { version, checksum } => {
            if plan.steps.is_empty() {
                return Err(LibraryError::Schema(format!(
                    "namespace `{}` is frozen@{version}+{checksum} but the installed \
                     plan has no frozen steps; run a plugin that understands this \
                     schema, or restore a backup",
                    plan.namespace
                )));
            }
            let max = plan.max_version();
            if *version > max {
                return Err(LibraryError::Schema(format!(
                    "namespace `{}` is frozen@{version}+{checksum}, newer than the \
                     installed plan (ends at {max}); upgrade the plugin or restore \
                     a backup captured before that freeze",
                    plan.namespace
                )));
            }
            let Some(applied) = plan.step(*version) else {
                return Err(LibraryError::Schema(format!(
                    "namespace `{}` frozen version {version} is not in the installed plan",
                    plan.namespace
                )));
            };
            let expected = applied.checksum();
            if checksum != &expected {
                return Err(LibraryError::Schema(format!(
                    "namespace `{}` frozen@{version} checksum {checksum} does not match \
                     installed plan ({expected})",
                    plan.namespace
                )));
            }
            Ok(plan.steps.iter().filter(|s| s.version > *version).collect())
        }
    }
}

/// Frozen steps to reverse from `state` down through versions above `to_version`.
fn reverse_steps<'a>(
    plan: &'a MigrationPlan,
    state: &SchemaState,
    to_version: i64,
) -> Result<Vec<&'a MigrationStep>> {
    let SchemaState::Frozen { version, checksum } = state else {
        return Err(LibraryError::Schema(format!(
            "namespace `{}` is {}; explicit downgrade requires frozen host schema",
            plan.namespace,
            state.display()
        )));
    };
    let Some(applied) = plan.step(*version) else {
        return Err(LibraryError::Schema(format!(
            "namespace `{}` frozen version {version} is not in the installed plan; restore a backup",
            plan.namespace
        )));
    };
    if checksum != &applied.checksum() {
        return Err(LibraryError::Schema(format!(
            "namespace `{}` frozen@{version} checksum {checksum} does not match \
             installed plan ({})",
            plan.namespace,
            applied.checksum()
        )));
    }
    let mut downs: Vec<&'a MigrationStep> = plan
        .steps
        .iter()
        .filter(|step| step.version > to_version && step.version <= *version)
        .collect();
    downs.reverse();
    for step in &downs {
        if step.down.is_none() {
            return Err(LibraryError::Schema(format!(
                "namespace `{}` version {} has no validated down; restore a \
                 compatible recovery point instead of downgrading",
                plan.namespace, step.version
            )));
        }
    }
    Ok(downs)
}

/// Applies one frozen step (ops + marker) as a retried atomic unit.
async fn apply_one_frozen_step(
    db: &DatabaseConnection,
    plan: &MigrationPlan,
    step: &MigrationStep,
    direction: ApplyDirection,
) -> Result<()> {
    let expected = match direction {
        ApplyDirection::Upgrade => SchemaState::Frozen {
            version: step.version,
            checksum: step.checksum(),
        },
        ApplyDirection::Downgrade => {
            if step.version <= 1 {
                SchemaState::Uninitialized
            } else {
                let prev = step.version - 1;
                let Some(prev_step) = plan.step(prev) else {
                    return Err(LibraryError::Schema(format!(
                        "namespace `{}` downgrade from {} is missing version {prev}",
                        plan.namespace, step.version
                    )));
                };
                SchemaState::Frozen {
                    version: prev,
                    checksum: prev_step.checksum(),
                }
            }
        }
    };
    let stmts = match direction {
        ApplyDirection::Upgrade => upgrade_statements(&plan.namespace, step),
        ApplyDirection::Downgrade => downgrade_statements(&plan.namespace, step),
    };
    let mut delay_ms = 20u64;
    for attempt in 0..8 {
        match current_schema_state_in(db, HostSchemaKind::RowMarker, &plan.namespace).await {
            Ok(state) if schema_states_equivalent(&state, &expected) => return Ok(()),
            Ok(_) | Err(_) => {}
        }
        match run_atomic_ddl(
            db,
            &format!(
                "schema-{}-{}-{}",
                plan.namespace,
                match direction {
                    ApplyDirection::Upgrade => "up",
                    ApplyDirection::Downgrade => "down",
                },
                step.version
            ),
            stmts.clone(),
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(err) => {
                match current_schema_state_in(db, HostSchemaKind::RowMarker, &plan.namespace).await
                {
                    Ok(state) if schema_states_equivalent(&state, &expected) => return Ok(()),
                    Ok(SchemaState::Uninitialized) | Ok(SchemaState::Frozen { .. })
                        if attempt + 1 < 8 && err.is_schema_apply_retryable() =>
                    {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        delay_ms = delay_ms.saturating_mul(2).min(250);
                        continue;
                    }
                    Ok(other) => {
                        if !err.is_schema_apply_retryable() {
                            return Err(err);
                        }
                        return Err(LibraryError::Schema(format!(
                            "namespace `{}` schema apply left contradictory state {} \
                         (wanted {}); {err}",
                            plan.namespace,
                            other.display(),
                            expected.display()
                        )));
                    }
                    Err(_) => {
                        if attempt + 1 < 8 && err.is_schema_apply_retryable() {
                            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                            delay_ms = delay_ms.saturating_mul(2).min(250);
                            continue;
                        }
                        return Err(err);
                    }
                }
            }
        }
    }
    match current_schema_state_in(db, HostSchemaKind::RowMarker, &plan.namespace).await {
        Ok(state) if schema_states_equivalent(&state, &expected) => Ok(()),
        Ok(state) => Err(LibraryError::Schema(format!(
            "namespace `{}` schema apply exhausted retries; durable state is {}",
            plan.namespace,
            state.display()
        ))),
        Err(err) => Err(err),
    }
}

/// Equality helper so match guards can name the comparison.
fn schema_states_equivalent(left: &SchemaState, right: &SchemaState) -> bool {
    left == right
}

/// Slot lock, up ops, and frozen marker for one upgrade unit.
fn upgrade_statements(namespace: &str, step: &MigrationStep) -> Vec<String> {
    let mut stmts = slot_lock_sql(namespace);
    stmts.extend(step.up.iter().map(|op| op.sql().to_string()));
    stmts.push(frozen_marker_sql(namespace, step.version, &step.checksum()));
    stmts
}

/// Slot lock, down ops, and frozen-marker delete for one downgrade unit.
fn downgrade_statements(namespace: &str, step: &MigrationStep) -> Vec<String> {
    let mut stmts = slot_lock_sql(namespace);
    if let Some(down) = &step.down {
        stmts.extend(down.iter().map(PlanOp::sql).map(str::to_string));
    }
    stmts.push(frozen_marker_delete_sql(namespace, step.version));
    stmts
}

/// Portable insert-or-ignore + bump for `schema:{namespace}`.
fn slot_lock_sql(namespace: &str) -> Vec<String> {
    let key = sql_string_literal(&schema_slot_key(namespace));
    vec![
        format!(
            "INSERT OR IGNORE INTO db_serialization_slots (slot_key, bump) \
             VALUES ({key}, 0)"
        ),
        format!("UPDATE db_serialization_slots SET bump = bump + 1 WHERE slot_key = {key}"),
    ]
}

/// Takes the `schema:{namespace}` serialization slot before walking steps.
async fn lock_schema_slot(db: &DatabaseConnection, namespace: &str) -> Result<()> {
    let mut delay_ms = 20u64;
    for attempt in 0..8 {
        match run_atomic_ddl(
            db,
            &format!("schema-lock-{namespace}"),
            slot_lock_sql(namespace),
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(err) => {
                let msg = err.to_string().to_ascii_lowercase();
                if msg.contains("db_serialization_slots")
                    && (msg.contains("no such table")
                        || msg.contains("does not exist")
                        || msg.contains("no such relation"))
                {
                    return Err(LibraryError::Schema(format!(
                        "schema apply for namespace `{namespace}` requires db_serialization_slots \
                         (apply binding bootstrap first): {err}"
                    )));
                }
                if err.is_schema_apply_retryable() && attempt + 1 < 8 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    delay_ms = delay_ms.saturating_mul(2).min(250);
                    continue;
                }
                return Err(err);
            }
        }
    }
    Err(LibraryError::Schema(format!(
        "schema apply for namespace `{namespace}` exhausted slot-lock retries"
    )))
}

/// Runs `stmts` as one typed atomic on the binding catalog (no host type env).
async fn run_atomic_ddl(
    db: &DatabaseConnection,
    operation_id: &str,
    stmts: Vec<String>,
) -> Result<()> {
    if stmts.is_empty() {
        return Ok(());
    }
    let req = ExecuteRequest {
        operation_id: operation_id.to_string(),
        request_hash: String::new(),
        deadline_unix_ms: 0,
        statements: stmts
            .into_iter()
            .map(|sql| TypedDbStatement {
                sql,
                parameters: Vec::new(),
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::AffectedRows,
            })
            .collect(),
    };
    execute_typed_on_binding(db, &req, SCHEMA_TXN_TIMING, 0, &SqlTypeEnv::new()).await?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::missing_docs_in_private_items)]
mod tests {
    use super::*;
    use crate::apply_binding_bootstrap;

    fn notes_plan(namespace: &str, reversible: bool) -> MigrationPlan {
        let down =
            reversible.then(|| vec![PlanOp::Schema("DROP TABLE IF EXISTS notes".to_string())]);
        MigrationPlan::try_new(
            namespace,
            vec![MigrationStep {
                version: 1,
                up: vec![PlanOp::Schema(
                    "CREATE TABLE IF NOT EXISTS notes (\
                        id INTEGER PRIMARY KEY, \
                        body TEXT NOT NULL\
                    )"
                    .into(),
                )],
                down,
                introduced_in: "0.1.0".into(),
            }],
        )
        .expect("notes plan")
    }

    fn two_step_plan(namespace: &str) -> MigrationPlan {
        MigrationPlan::try_new(
            namespace,
            vec![
                MigrationStep {
                    version: 1,
                    up: vec![PlanOp::Schema(
                        "CREATE TABLE IF NOT EXISTS notes (\
                            id INTEGER PRIMARY KEY, \
                            body TEXT NOT NULL\
                        )"
                        .into(),
                    )],
                    down: Some(vec![PlanOp::Schema("DROP TABLE IF EXISTS notes".into())]),
                    introduced_in: "0.1.0".into(),
                },
                MigrationStep {
                    version: 2,
                    up: vec![PlanOp::Schema(
                        "CREATE TABLE IF NOT EXISTS tags (\
                            id INTEGER PRIMARY KEY, \
                            label TEXT NOT NULL\
                        )"
                        .into(),
                    )],
                    down: Some(vec![PlanOp::Schema("DROP TABLE IF EXISTS tags".into())]),
                    introduced_in: "0.2.0".into(),
                },
            ],
        )
        .expect("two-step plan")
    }

    async fn binding_with_bootstrap() -> DatabaseConnection {
        let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
            .await
            .unwrap();
        apply_binding_bootstrap(&db).await.unwrap();
        db
    }

    #[test]
    fn postgres_expanded_binding_bootstrap_is_ddl_or_marker() {
        let planned = crate::binding_bootstrap_plan(&SchemaState::Uninitialized)
            .unwrap()
            .expect("uninitialized bootstrap");
        let expanded = bookclerk_db_exec::expand_host_schema_batch(
            sea_orm::DatabaseBackend::Postgres,
            &planned,
        )
        .unwrap_or(planned);
        for (i, sql) in expanded.iter().enumerate() {
            let ddl = bookclerk_plugin_abi::statement_is_ddl(sql);
            let marker = bookclerk_db_exec::is_host_schema_version_marker(sql);
            assert!(
                ddl || marker,
                "bootstrap statement {i} is not ddl/marker: {sql}"
            );
        }
    }

    #[test]
    fn schema_slot_lock_sql_is_insert_values() {
        let stmts = slot_lock_sql("echo_sql");
        assert!(stmts[0].contains("VALUES"), "{}", stmts[0]);
        assert!(
            !stmts[0].to_ascii_uppercase().contains("SELECT"),
            "{}",
            stmts[0]
        );
        let lowered = bookclerk_db_exec::lower_canonical_ddl_to_postgres(&stmts[0]);
        assert!(
            !lowered.contains("SELECT *"),
            "postgres lowering must not wrap slot insert as SELECT *: {lowered}"
        );
    }

    #[test]
    fn empty_plan_means_no_frozen_versions() {
        let plan = MigrationPlan::try_new("echo_sql", Vec::new()).unwrap();
        assert_eq!(plan.max_version(), 0);
        assert_eq!(plan.min_version(), None);
        assert!(forward_steps(&plan, &SchemaState::Uninitialized)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn newer_frozen_than_empty_plan_fails_closed() {
        let plan = MigrationPlan::try_new("echo_sql", Vec::new()).unwrap();
        let err = forward_steps(
            &plan,
            &SchemaState::Frozen {
                version: 1,
                checksum: "abc".into(),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("no frozen steps"), "{err}");
    }

    #[test]
    fn checksum_mismatch_fails_closed() {
        let plan = notes_plan("echo_sql", true);
        let err = forward_steps(
            &plan,
            &SchemaState::Frozen {
                version: 1,
                checksum: "deadbeef".repeat(8),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
    }

    #[test]
    fn irreversible_downgrade_fails_closed() {
        let plan = notes_plan("echo_sql", false);
        let checksum = plan.steps[0].checksum();
        let err = reverse_steps(
            &plan,
            &SchemaState::Frozen {
                version: 1,
                checksum,
            },
            0,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no validated down"), "{err}");
        assert!(err.to_string().contains("recovery point"), "{err}");
    }

    fn skipped_version_plan() -> MigrationPlan {
        MigrationPlan::try_new(
            "echo_sql",
            vec![
                MigrationStep {
                    version: 1,
                    up: vec![PlanOp::Schema(
                        "CREATE TABLE IF NOT EXISTS notes (\
                            id INTEGER PRIMARY KEY, \
                            body TEXT NOT NULL\
                        )"
                        .into(),
                    )],
                    down: Some(vec![PlanOp::Schema("DROP TABLE IF EXISTS notes".into())]),
                    introduced_in: "0.1.0".into(),
                },
                MigrationStep {
                    version: 3,
                    up: vec![PlanOp::Schema(
                        "CREATE TABLE IF NOT EXISTS tags (\
                            id INTEGER PRIMARY KEY, \
                            label TEXT NOT NULL\
                        )"
                        .into(),
                    )],
                    down: Some(vec![PlanOp::Schema("DROP TABLE IF EXISTS tags".into())]),
                    introduced_in: "0.3.0".into(),
                },
            ],
        )
        .expect("skipped-version plan")
    }

    #[test]
    fn reverse_walks_nonconsecutive_versions() {
        let plan = skipped_version_plan();
        let checksum = plan.step(3).expect("v3").checksum();
        let to_zero = reverse_steps(
            &plan,
            &SchemaState::Frozen {
                version: 3,
                checksum: checksum.clone(),
            },
            0,
        )
        .unwrap();
        assert_eq!(
            to_zero.iter().map(|s| s.version).collect::<Vec<_>>(),
            vec![3, 1]
        );
        let to_one = reverse_steps(
            &plan,
            &SchemaState::Frozen {
                version: 3,
                checksum,
            },
            1,
        )
        .unwrap();
        assert_eq!(
            to_one.iter().map(|s| s.version).collect::<Vec<_>>(),
            vec![3]
        );
    }

    #[test]
    fn session_mismatch_fails_closed() {
        let err = schema_session_matches(
            &SchemaState::Frozen {
                version: 1,
                checksum: "aaa".into(),
            },
            &SchemaState::Frozen {
                version: 2,
                checksum: "bbb".into(),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("schema advanced"), "{err}");
    }

    #[tokio::test]
    async fn apply_is_idempotent_on_sqlite() {
        let db = binding_with_bootstrap().await;
        let plan = notes_plan("echo_sql", true);
        let first = apply_migration_plan(&db, &plan).await.unwrap();
        let second = apply_migration_plan(&db, &plan).await.unwrap();
        assert_eq!(first, second);
        match first {
            SchemaState::Frozen { version, checksum } => {
                assert_eq!(version, 1);
                assert_eq!(checksum, plan.steps[0].checksum());
            }
            other => panic!("expected frozen, got {other}"),
        }
    }

    #[tokio::test]
    async fn two_nodes_racing_upgrade_converge() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("race.db");
        let a = bookclerk_plugin_database_sqlite::open(&path).await.unwrap();
        apply_binding_bootstrap(&a).await.unwrap();
        let b = bookclerk_plugin_database_sqlite::open(&path).await.unwrap();
        let plan_a = notes_plan("echo_sql", true);
        let plan_b = notes_plan("echo_sql", true);
        let (ra, rb) = tokio::join!(
            apply_migration_plan(&a, &plan_a),
            apply_migration_plan(&b, &plan_b)
        );
        let sa = ra.expect("node a");
        let sb = rb.expect("node b");
        assert_eq!(sa, sb);
        assert_eq!(sa.frozen_version(), Some(1));
    }

    #[tokio::test]
    async fn lost_completion_retries_same_step() {
        let db = binding_with_bootstrap().await;
        let plan = notes_plan("echo_sql", true);
        let batch = remaining_upgrade_batches(&plan, &SchemaState::Uninitialized)
            .unwrap()
            .into_iter()
            .next()
            .expect("one frozen step");
        let expanded =
            bookclerk_db_exec::expand_host_schema_batch(sea_orm::DatabaseBackend::Sqlite, &batch)
                .unwrap_or(batch);
        let skip = u32::try_from(expanded.len().saturating_sub(1)).unwrap_or(0);
        crate::inject_atomic_interrupt_after(
            crate::AtomicInterruptPhase::BetweenStatements,
            crate::AtomicInterruptKind::Cancel,
            skip,
        );
        let err = apply_migration_plan(&db, &plan)
            .await
            .expect_err("interrupt before marker");
        assert!(err.to_string().to_lowercase().contains("cancel"), "{err}");
        let before = current_schema_state_in(&db, HostSchemaKind::RowMarker, "echo_sql")
            .await
            .unwrap();
        assert_eq!(before, SchemaState::Uninitialized);
        let state = apply_migration_plan(&db, &plan).await.unwrap();
        assert_eq!(state.frozen_version(), Some(1));
    }

    #[tokio::test]
    async fn newer_schema_older_plan_fails_closed() {
        let db = binding_with_bootstrap().await;
        let v2 = two_step_plan("echo_sql");
        apply_migration_plan(&db, &v2).await.unwrap();
        let v1 = notes_plan("echo_sql", true);
        let err = apply_migration_plan(&db, &v1).await.unwrap_err();
        assert!(
            err.to_string().contains("newer than the installed plan"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn reversible_downgrade_drops_tables() {
        let db = binding_with_bootstrap().await;
        let plan = two_step_plan("echo_sql");
        apply_migration_plan(&db, &plan).await.unwrap();
        let after = downgrade_migration_plan(&db, &plan, 1).await.unwrap();
        assert_eq!(after.frozen_version(), Some(1));
        let tags = sea_orm::ConnectionTrait::query_all_raw(
            &db,
            sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'tags'",
            ),
        )
        .await
        .unwrap();
        assert!(tags.is_empty(), "tags should be dropped");
    }

    #[tokio::test]
    async fn bookclerk_bootstrap_and_plugin_plan_are_separate_namespaces() {
        let db = binding_with_bootstrap().await;
        let boot = current_schema_state_in(
            &db,
            HostSchemaKind::RowMarker,
            crate::BOOKCLERK_SCHEMA_NAMESPACE,
        )
        .await
        .unwrap();
        match boot {
            SchemaState::Unreleased { base_version, .. } => assert_eq!(base_version, 0),
            other => panic!("expected bookclerk unreleased, got {other}"),
        }
        let plan = notes_plan("echo_sql", true);
        apply_migration_plan(&db, &plan).await.unwrap();
        let plugin = current_schema_state_in(&db, HostSchemaKind::RowMarker, "echo_sql")
            .await
            .unwrap();
        assert_eq!(plugin.frozen_version(), Some(1));
        let boot_after = current_schema_state_in(
            &db,
            HostSchemaKind::RowMarker,
            crate::BOOKCLERK_SCHEMA_NAMESPACE,
        )
        .await
        .unwrap();
        assert_eq!(boot, boot_after);
    }

    #[tokio::test]
    async fn stale_session_fails_closed_after_peer_upgrade() {
        let db = binding_with_bootstrap().await;
        let v1 = notes_plan("echo_sql", true);
        let expected = apply_migration_plan(&db, &v1).await.unwrap();
        let v2 = two_step_plan("echo_sql");
        apply_migration_plan(&db, &v2).await.unwrap();
        let observed = current_schema_state_in(&db, HostSchemaKind::RowMarker, "echo_sql")
            .await
            .unwrap();
        let err = schema_session_matches(&expected, &observed).unwrap_err();
        assert!(err.to_string().contains("schema advanced"), "{err}");
    }

    #[tokio::test]
    async fn restore_does_not_walk_then_open_forwards() {
        let src = binding_with_bootstrap().await;
        let v1 = notes_plan("echo_sql", true);
        apply_migration_plan(&src, &v1).await.unwrap();
        let restored = binding_with_bootstrap().await;
        // Simulate restore of captured ledger + table without running the plan.
        sea_orm::ConnectionTrait::execute_raw(
            &restored,
            sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                format!(
                    "INSERT INTO schema_migrations \
                     (namespace, version, state, checksum, app_version, applied_at) \
                     VALUES ('echo_sql', 1, 'frozen', {}, 'test', 't')",
                    crate::migrations::sql_string_literal(&v1.steps[0].checksum())
                ),
            ),
        )
        .await
        .unwrap();
        sea_orm::ConnectionTrait::execute_raw(
            &restored,
            sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                "CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL)",
            ),
        )
        .await
        .unwrap();
        let before = current_schema_state_in(&restored, HostSchemaKind::RowMarker, "echo_sql")
            .await
            .unwrap();
        assert_eq!(before.frozen_version(), Some(1));
        let v2 = two_step_plan("echo_sql");
        let after = apply_migration_plan(&restored, &v2).await.unwrap();
        assert_eq!(after.frozen_version(), Some(2));
        let tags = sea_orm::ConnectionTrait::query_all_raw(
            &restored,
            sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'tags'",
            ),
        )
        .await
        .unwrap();
        assert_eq!(tags.len(), 1);
    }

    #[tokio::test]
    #[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL"]
    async fn apply_is_idempotent_on_postgres() {
        if std::env::var("BOOKCLERK_TEST_POSTGRES_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .is_none()
        {
            return;
        }
        let url = std::env::var("BOOKCLERK_TEST_POSTGRES_URL").unwrap();
        let db_name = format!("plmig_{}", uuid::Uuid::new_v4().as_simple());
        let admin = sea_orm::Database::connect(url.as_str())
            .await
            .expect("admin");
        let backend = sea_orm::ConnectionTrait::get_database_backend(&admin);
        sea_orm::ConnectionTrait::execute_raw(
            &admin,
            sea_orm::Statement::from_string(backend, format!("CREATE DATABASE {db_name}")),
        )
        .await
        .expect("create db");
        let (base, query) = match url.split_once('?') {
            Some((base, q)) => (base, Some(q)),
            None => (url.as_str(), None),
        };
        let trimmed = base.trim_end_matches('/');
        let slash = trimmed.rfind('/').expect("url path");
        let db_url = match query {
            Some(q) => format!("{}/{db_name}?{q}", &trimmed[..slash]),
            None => format!("{}/{db_name}", &trimmed[..slash]),
        };
        let db = sea_orm::Database::connect(&db_url).await.expect("connect");
        apply_binding_bootstrap(&db).await.unwrap();
        let plan = notes_plan("echo_sql", true);
        let first = apply_migration_plan(&db, &plan).await.unwrap();
        let second = apply_migration_plan(&db, &plan).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(first.frozen_version(), Some(1));
    }
}
