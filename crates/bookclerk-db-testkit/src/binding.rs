//! A plugin-owned database that behaves like a granted `[[databases]]`
//! binding on a deployed host.
//!
//! A plugin's `env.DB` reaches an isolated unit through the host, which
//! authorizes the SQL against the binding-owned scope, wraps every batch in
//! a receipt and holds it to the adapter's advertisement. The in-process
//! fakes an author would otherwise test against do none of that.
//! [`BindingDatabase`] runs the same host path on an in-memory SQLite unit
//! under one [`Profile`], so a plugin's queries meet the caps, the typed
//! `limit_exceeded` error and the refused interactive transaction before
//! they meet a real adapter.
//!
//! The TypeScript and Python SDKs reach it through the `bookclerk-db-testkit`
//! binary, which serves one binding over the granted channel the isolates
//! already speak (`POST /db/execute`, `POST /db/execute-streamed`,
//! `GET /db/limits`).

use std::sync::Mutex;

use async_trait::async_trait;
use bookclerk_db_exec::begin_gate::with_batch_in_progress;
use bookclerk_db_exec::stamp_adapter_execute;
use bookclerk_db_exec::{execute_typed_envelope_on_connection, AtomicSession, ExecCaps};
use bookclerk_library::migrations::{
    load_plugin_migration_history, next_pending_plugin_migration, plugin_apply_statements,
    plugin_binding_type_env, prove_plugin_migration_sequence, PluginMigrationSequence,
};
use bookclerk_library::sql_plan::execute_guest_atomic_with;
use bookclerk_library::test_support::{engine_error, Profile, ProfileExec};
use bookclerk_library::{apply_binding_bootstrap, GuestSqlPolicy, LibraryError, TypedAtomicExec};
use bookclerk_plugin_abi::{
    DbCapabilities, DbPlanStatementKind, DbResultSelection, ExecuteReply, ExecuteRequest,
    ExecuteStream, GuestDatabase, PluginError, PluginMigration, TypedDbStatement,
};
use sea_orm::DatabaseConnection;

/// An in-memory plugin database binding under one capability profile.
///
/// Open one with [`BindingDatabase::open`], giving it the plugin's
/// `databaseMigrations` registration, and run the plugin's SQL through the
/// [`GuestDatabase`] methods the host serves.
#[derive(Debug)]
pub struct BindingDatabase {
    /// The profile this binding enforces.
    profile: Profile,
    /// The profile's advertisement.
    caps: DbCapabilities,
    /// The isolated unit: binding bootstrap tables plus the plugin's schema.
    db: DatabaseConnection,
    /// The proven registration, every migration of which is applied.
    registered: PluginMigrationSequence,
    /// Statement count of every batch, in arrival order.
    sizes: Mutex<Vec<usize>>,
}

impl BindingDatabase {
    /// Opens an empty in-memory unit under `profile` and applies
    /// `migrations` the way the host applies a plugin's
    /// `databaseMigrations` registration at enrollment: one typed batch per
    /// migration, stamped with the binding's schema and held to the
    /// profile's advertisement, as the adapter holds it.
    ///
    /// # Errors
    ///
    /// Returns `invalid_params` when the registration fails the BookclerkSQL
    /// proof (an unsupported statement, a reserved table, a repeated id),
    /// `limit_exceeded` when a migration's apply batch breaks one of the
    /// profile's caps, and the engine's error when the unit cannot be
    /// opened or a migration cannot be applied.
    pub async fn open(
        profile: Profile,
        migrations: Vec<PluginMigration>,
    ) -> Result<Self, PluginError> {
        let db = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
            .await
            .map_err(engine_error)?;
        apply_binding_bootstrap(&db).await.map_err(library_error)?;
        let registered = prove_plugin_migration_sequence(migrations)
            .map_err(|err| PluginError::invalid_params(err.to_string()))?;
        let caps = profile.capabilities();
        let exec = ProfileExec::new(db.clone(), caps.clone());
        apply_registration_under_profile(&db, &exec, &registered).await?;
        Ok(Self {
            profile,
            caps,
            db,
            registered,
            sizes: Mutex::new(Vec::new()),
        })
    }

    /// The profile this binding enforces.
    #[must_use]
    pub fn profile(&self) -> Profile {
        self.profile
    }

    /// The advertisement every batch is held to.
    #[must_use]
    pub fn capabilities(&self) -> &DbCapabilities {
        &self.caps
    }

    /// The unit itself, for a test that inspects what the plugin wrote.
    #[must_use]
    pub fn connection(&self) -> &DatabaseConnection {
        &self.db
    }

    /// Statement counts of every batch sent so far, in arrival order,
    /// including batches the profile turned away.
    #[must_use]
    pub fn batch_sizes(&self) -> Vec<usize> {
        self.sizes.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The binding-owned scope with the column types of the applied schema,
    /// as the host builds it from the plugin's registration.
    fn policy(&self) -> GuestSqlPolicy {
        GuestSqlPolicy::binding_owned().with_sql_types(plugin_binding_type_env(
            &self.registered,
            self.registered.migrations.len(),
        ))
    }

    /// Authorizes, receipt-wraps and runs one guest batch under `caps`, the
    /// host's path for a binding session.
    async fn run(
        &self,
        request: ExecuteRequest,
        caps: &DbCapabilities,
    ) -> Result<ExecuteReply, PluginError> {
        self.sizes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(request.statements.len());
        let policy = self.policy();
        execute_guest_atomic_with(request, caps, &policy, |envelope| async move {
            // The adapters run the batch under the request's deadline (zero
            // means none), so an expired batch is refused, not committed.
            let deadline = (envelope.request.deadline_unix_ms > 0)
                .then_some(envelope.request.deadline_unix_ms);
            with_batch_in_progress(execute_typed_envelope_on_connection(
                &self.db,
                &envelope,
                ExecCaps::from_capabilities(caps),
                AtomicSession::from_deadline(deadline),
            ))
            .await
            .map_err(engine_error)
        })
        .await
    }
}

#[async_trait(?Send)]
impl GuestDatabase for BindingDatabase {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteReply, PluginError> {
        self.run(request, &self.caps).await
    }

    async fn execute_streamed(
        &self,
        request: ExecuteRequest,
    ) -> Result<ExecuteStream, PluginError> {
        let Some(streamed) = self.caps.streamed() else {
            return Err(PluginError::unsupported(
                "GuestDatabase.executeStreamed: this profile advertises no streamed result budget",
            ));
        };
        let reply = self.run(request, &streamed).await?;
        ExecuteStream::from_reply(&reply)
    }

    async fn limits(&self) -> Result<DbCapabilities, PluginError> {
        Ok(self.caps.clone())
    }
}

/// Applies every pending migration of `registered` on `db`, one apply unit
/// (serialization slot, the migration's operations, the journal row) per
/// typed batch through `exec`, which holds the batch to the profile the way
/// the adapter would. The unit is fresh, so a failure is final: the host's
/// retry loop over a lost reply has nothing to recover here.
async fn apply_registration_under_profile(
    db: &DatabaseConnection,
    exec: &ProfileExec,
    registered: &PluginMigrationSequence,
) -> Result<(), PluginError> {
    loop {
        let history = load_plugin_migration_history(db)
            .await
            .map_err(library_error)?;
        let Some((ordinal, migration)) =
            next_pending_plugin_migration(&history, registered).map_err(library_error)?
        else {
            return Ok(());
        };
        let ordinal_i64 = i64::try_from(ordinal).unwrap_or(i64::MAX);
        let request = ExecuteRequest {
            operation_id: format!("testkit-plugin-migrate-ord{ordinal}-{}", migration.id),
            request_hash: String::new(),
            deadline_unix_ms: 0,
            statements: plugin_apply_statements(ordinal_i64, migration)
                .into_iter()
                .map(|sql| TypedDbStatement {
                    sql,
                    parameters: Vec::new(),
                    kind: DbPlanStatementKind::Execute,
                    max_rows: 0,
                    result_selection: DbResultSelection::Discard,
                })
                .collect(),
        };
        let envelope =
            stamp_adapter_execute(request, &plugin_binding_type_env(registered, ordinal))
                .map_err(engine_error)?;
        exec.execute_typed(envelope).await?;
    }
}

/// Maps a bootstrap or migration failure onto the error the host reports.
fn library_error(err: LibraryError) -> PluginError {
    match err {
        LibraryError::LimitExceeded(limit) => PluginError::limit_exceeded(limit),
        LibraryError::Schema(message) => PluginError::invalid_params(message),
        LibraryError::Orm(err) => engine_error(err),
        other => PluginError::internal(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_plugin_abi::{LimitExceeded, PluginErrorCode};

    #[test]
    fn library_errors_map_to_the_codes_a_guest_sees() {
        let limit = LimitExceeded::new("maxResultBytes", 4096, 5000);
        let typed = library_error(LibraryError::LimitExceeded(limit.clone()));
        assert_eq!(typed.code, PluginErrorCode::LimitExceeded);
        assert_eq!(typed.message, limit.to_message());

        let schema = library_error(LibraryError::Schema("bad ddl".into()));
        assert_eq!(schema.code, PluginErrorCode::InvalidParams);
        assert_eq!(schema.message, "bad ddl");

        let orm = library_error(LibraryError::Orm(sea_orm::DbErr::Custom(
            "deadline_exceeded: batch ran past its deadline".into(),
        )));
        assert_eq!(orm.code, PluginErrorCode::DeadlineExceeded);

        let other = library_error(LibraryError::missing(bookclerk_library::EntityKind::Book));
        assert_eq!(other.code, PluginErrorCode::Internal);
        assert!(other.message.contains("book"), "{other}");
    }
}
