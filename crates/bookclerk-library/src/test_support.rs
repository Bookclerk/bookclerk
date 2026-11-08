//! A library store that behaves like one opened on an external database
//! plugin, for tests.
//!
//! Production hosts reach every database through an adapter guest that
//! refuses interactive transactions and enforces the limits it advertised. A
//! plain engine connection does neither, so a test that passes on one can
//! still fail on the hosted demo. [`ProfileStore`] wraps an engine proxy in
//! a [`BeginGate`] and routes every host batch through a [`ProfileExec`]
//! that applies a [`Profile`]'s capability advertisement.
//!
//! This module is the engine-neutral core; the `bookclerk-db-testkit` crate
//! opens it on in-memory SQLite and adds fixtures.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bookclerk_db_exec::begin_gate::{with_batch_in_progress, BeginGate, BeginGateHandle};
use bookclerk_db_exec::{
    execute_typed_envelope_on_connection, AdapterExecuteRequest, AtomicSession, ExecCaps,
};
use bookclerk_plugin_abi::{DbCapabilities, ExecuteReply, ExecuteRequest, PluginError};
use sea_orm::{Database, DatabaseConnection, DbBackend, ProxyDatabaseTrait};

use crate::atomic_txn::TypedAtomicExec;
use crate::error::{LibraryError, Result};
use crate::sql_plan::{validate_execute_reply, validate_execute_request};
use crate::store::LibraryStore;

pub use bookclerk_db_exec::begin_gate::INTERACTIVE_TRANSACTIONS_REFUSED;

/// A capability advertisement a database plugin may present to the host.
///
/// Host and plugin database code is expected to pass under every profile:
/// what works on the shipped adapters must also work when a plugin
/// advertises only the host minimums.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Profile {
    /// The shipped sqlite and Postgres adapters: 256 KiB inline results,
    /// streamed reads, the first-party statement and bind caps.
    FirstParty,
    /// The Cloudflare D1 adapter: D1's bind, payload and lowered-SQL caps,
    /// no consistent backup read, 8 MiB streamed results.
    D1,
    /// The smallest advertisement the host connects to
    /// ([`DbCapabilities::host_minimum`]): 4,096-byte results, 1,024-byte
    /// cells, one row per statement, no streamed reads.
    Minimum,
}

impl Profile {
    /// Every profile, in the order conformance runs report them.
    pub const ALL: [Profile; 3] = [Profile::FirstParty, Profile::D1, Profile::Minimum];

    /// Stable name used in test output and the plugin tooling
    /// (`first-party`, `d1`, `minimum`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Profile::FirstParty => "first-party",
            Profile::D1 => "d1",
            Profile::Minimum => "minimum",
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.name() == name)
    }

    /// The capability advertisement this profile enforces.
    #[must_use]
    pub fn capabilities(self) -> DbCapabilities {
        match self {
            Profile::FirstParty => DbCapabilities::advertised_sqlite(),
            Profile::D1 => DbCapabilities::advertised_d1(),
            Profile::Minimum => DbCapabilities::host_minimum(),
        }
    }
}

/// Stands in for a database adapter's `executeAtomic`.
///
/// Each batch is admitted against the profile's capabilities the way the
/// adapter SDK admits it, run as one engine transaction under the profile's
/// result caps, and its reply checked the way the host checks a guest reply.
/// Injected faults are consumed one batch at a time.
#[derive(Debug)]
pub struct ProfileExec {
    /// The store's own connection.
    db: DatabaseConnection,
    /// The profile's advertisement.
    caps: DbCapabilities,
    /// Statement count of every batch, in arrival order.
    sizes: Mutex<Vec<usize>>,
    /// Batches whose reply is still to be lost after they are applied.
    lose_replies: Mutex<u32>,
    /// Batches still to be refused as lock contention.
    lock_requests: Mutex<u32>,
}

impl ProfileExec {
    /// Wraps `db` with `caps`.
    #[must_use]
    pub fn new(db: DatabaseConnection, caps: DbCapabilities) -> Self {
        Self {
            db,
            caps,
            sizes: Mutex::new(Vec::new()),
            lose_replies: Mutex::new(0),
            lock_requests: Mutex::new(0),
        }
    }

    /// Statement counts of every batch so far, including batches a fault
    /// turned away.
    #[must_use]
    pub fn batch_sizes(&self) -> Vec<usize> {
        self.sizes().clone()
    }

    /// Arms `n` lost replies: the batch is applied, the caller sees
    /// `unavailable`.
    pub fn lose_next_replies(&self, n: u32) {
        *Self::counter(&self.lose_replies) = n;
    }

    /// Arms `n` lock-contention refusals: nothing is applied, the caller
    /// sees `unavailable` naming `SQLITE_BUSY`.
    pub fn lock_next_requests(&self, n: u32) {
        *Self::counter(&self.lock_requests) = n;
    }

    /// Takes one armed fault from `counter`, if any remain.
    fn take(counter: &Mutex<u32>) -> bool {
        let mut remaining = Self::counter(counter);
        if *remaining == 0 {
            return false;
        }
        *remaining -= 1;
        true
    }

    /// A fault counter's guard, readable even after a panicking test
    /// poisoned the lock.
    fn counter(counter: &Mutex<u32>) -> std::sync::MutexGuard<'_, u32> {
        counter.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The recorded batch sizes, readable even after a panicking test
    /// poisoned the lock.
    fn sizes(&self) -> std::sync::MutexGuard<'_, Vec<usize>> {
        self.sizes.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Maps a library-side admission or reply error onto the error the guest
/// would answer with.
fn to_plugin_error(err: LibraryError) -> PluginError {
    match err {
        LibraryError::LimitExceeded(limit) => PluginError::limit_exceeded(limit),
        other => PluginError::invalid_params(other.to_string()),
    }
}

/// Maps an engine error onto the error the guest would answer with: a
/// typed `limit_exceeded` when the message carries one, otherwise the
/// adapter's `unavailable`.
fn engine_error(err: sea_orm::DbErr) -> PluginError {
    match LibraryError::from_db_err(err) {
        LibraryError::LimitExceeded(limit) => PluginError::limit_exceeded(limit),
        LibraryError::Conflict(message) => PluginError::conflict(message),
        LibraryError::Unavailable(message) => PluginError::unavailable(message),
        other => PluginError::internal(other.to_string()),
    }
}

#[async_trait]
impl TypedAtomicExec for ProfileExec {
    async fn execute_typed(
        &self,
        envelope: AdapterExecuteRequest,
    ) -> std::result::Result<ExecuteReply, PluginError> {
        self.sizes().push(envelope.request.statements.len());
        validate_execute_request(&envelope.request, &self.caps).map_err(to_plugin_error)?;
        if Self::take(&self.lock_requests) {
            return Err(PluginError::unavailable(
                "database is locked (SQLITE_BUSY): injected lock contention",
            ));
        }
        let reply = with_batch_in_progress(execute_typed_envelope_on_connection(
            &self.db,
            &envelope,
            ExecCaps::from_capabilities(&self.caps),
            AtomicSession::from_deadline(None),
        ))
        .await
        .map_err(engine_error)?;
        validate_execute_reply(&envelope.request, &reply, &self.caps).map_err(to_plugin_error)?;
        if Self::take(&self.lose_replies) {
            return Err(PluginError::unavailable(
                "injected lost reply: the batch was applied, its reply never arrived",
            ));
        }
        Ok(reply)
    }
}

/// A library store opened on an engine proxy that behaves like an external
/// database plugin under one [`Profile`].
#[derive(Debug)]
pub struct ProfileStore {
    /// The profile this store enforces.
    profile: Profile,
    /// Store wired to [`Self::exec`] with the profile's capabilities.
    store: LibraryStore,
    /// Refusal switch and counter of the begin gate.
    gate: BeginGateHandle,
    /// The profile-enforcing batch executor.
    exec: Arc<ProfileExec>,
}

impl ProfileStore {
    /// Opens `engine` behind a begin gate, applies the host schema and wires
    /// a store that enforces `profile`.
    ///
    /// `backend` is the SQL dialect the engine proxy speaks, which SeaORM
    /// needs to render statements; the kit opens sqlite-shaped engines.
    ///
    /// # Errors
    ///
    /// Returns the engine or schema error when the database cannot be opened.
    pub async fn open(
        engine: impl ProxyDatabaseTrait + 'static,
        backend: DbBackend,
        profile: Profile,
    ) -> Result<Self> {
        let caps = profile.capabilities();
        debug_assert_eq!(caps.capability_failure_reason_opt(), None);
        let (gate, handle) = BeginGate::new(engine);
        let db = Database::connect_proxy(backend, Arc::new(Box::new(gate)))
            .await
            .map_err(LibraryError::Orm)?;
        db.ping().await.map_err(LibraryError::Orm)?;
        // The host applies its schema through its own transactions before
        // any adapter session exists; the gate starts refusing afterwards.
        crate::apply_host_schema(&db).await?;
        handle.refuse(true);
        let exec = Arc::new(ProfileExec::new(db.clone(), caps.clone()));
        let store = LibraryStore::from_connection(db)
            .with_db_capabilities(caps)
            .with_typed_exec(Arc::clone(&exec) as Arc<dyn TypedAtomicExec>);
        Ok(Self {
            profile,
            store,
            gate: handle,
            exec,
        })
    }

    /// The enforced profile.
    #[must_use]
    pub fn profile(&self) -> Profile {
        self.profile
    }

    /// The store alone, for fixtures that only need a store that behaves
    /// like one on an external plugin. The gate keeps refusing `begin()`.
    #[must_use]
    pub fn into_store(self) -> LibraryStore {
        self.store
    }

    /// The enforced capability advertisement.
    #[must_use]
    pub fn capabilities(&self) -> &DbCapabilities {
        self.store.db_capabilities()
    }

    /// The library store under test.
    #[must_use]
    pub fn store(&self) -> &LibraryStore {
        &self.store
    }

    /// The begin-refusing connection, for reads a test makes directly.
    #[must_use]
    pub fn connection(&self) -> &DatabaseConnection {
        self.store.db()
    }

    /// How many interactive transactions the code under test tried to open.
    ///
    /// A host write that works on an external adapter leaves this at zero.
    #[must_use]
    pub fn refused_begins(&self) -> u32 {
        self.gate.refused()
    }

    /// Statement counts of every batch sent so far, in arrival order,
    /// including batches a fault turned away.
    #[must_use]
    pub fn batch_sizes(&self) -> Vec<usize> {
        self.exec.batch_sizes()
    }

    /// The next `n` batches are applied but their replies are lost: the
    /// caller sees `unavailable`, the way a dropped RPC reply looks, and a
    /// retry with the same operation id must not repeat the write.
    pub fn lose_next_replies(&self, n: u32) {
        self.exec.lose_next_replies(n);
    }

    /// The next `n` batches are refused with SQLite lock contention before
    /// anything is applied, the way a busy file lock answers.
    pub fn lock_next_requests(&self, n: u32) {
        self.exec.lock_next_requests(n);
    }

    /// Runs one host-authored batch through the profile the way a store
    /// write does: stamped against the host schema, admitted against the
    /// advertisement, run as one transaction, its reply checked.
    ///
    /// # Errors
    ///
    /// Returns the typed `limit_exceeded` error when the request or the
    /// reply exceeds a cap, `invalid_params` when the batch does not
    /// typecheck against the host schema, and `unavailable` for an injected
    /// fault or an engine failure.
    pub async fn execute_batch(
        &self,
        request: ExecuteRequest,
    ) -> std::result::Result<ExecuteReply, PluginError> {
        let envelope = bookclerk_db_exec::stamp_adapter_execute(
            request,
            &crate::migrations::host_sql_type_env(),
        )
        .map_err(|err| PluginError::invalid_params(err.to_string()))?;
        self.exec.execute_typed(envelope).await
    }
}

/// In-memory and file-backed SQLite profile stores for this crate's own
/// tests. Sibling crates open theirs through `bookclerk-db-testkit`.
#[cfg(test)]
pub(crate) mod sqlite {
    use bookclerk_plugin_database_sqlite::sqlite::SqliteProxy;
    use sea_orm::DbBackend;

    use super::{Profile, ProfileStore};

    /// An empty in-memory library under `profile`.
    pub(crate) async fn memory(profile: Profile) -> ProfileStore {
        let conn = rusqlite::Connection::open_in_memory().expect("open sqlite memory");
        let engine = SqliteProxy::new(conn).expect("sqlite proxy");
        ProfileStore::open(engine, DbBackend::Sqlite, profile)
            .await
            .expect("profile store")
    }

    /// The store tests' default: the shipped D1 advertisement, the
    /// tightest of the first-party adapters.
    pub(crate) async fn store() -> crate::LibraryStore {
        memory(Profile::D1).await.into_store()
    }
}

#[cfg(test)]
mod tests {
    use bookclerk_plugin_abi::{LimitExceeded, PluginErrorCode};
    use sea_orm::DbErr;

    use super::*;

    #[test]
    fn admission_errors_map_to_the_guest_error_codes() {
        let limit = LimitExceeded::new("maxStatements", 8, 9);
        let err = to_plugin_error(LibraryError::LimitExceeded(limit.clone()));
        assert_eq!(err.code, PluginErrorCode::LimitExceeded);
        assert_eq!(err.limit(), Some(limit));
        let err = to_plugin_error(LibraryError::Other(anyhow::anyhow!(
            "statements must be non-empty"
        )));
        assert_eq!(err.code, PluginErrorCode::InvalidParams);
    }

    #[test]
    fn engine_errors_map_to_the_guest_error_codes() {
        let limit = LimitExceeded::new("maxResultRows", 1, 2);
        let err = engine_error(DbErr::Custom(limit.to_string()));
        assert_eq!(err.code, PluginErrorCode::LimitExceeded);
        assert_eq!(err.limit(), Some(limit));
        let err = engine_error(DbErr::Custom(
            "SQLITE_CONSTRAINT: UNIQUE constraint failed: t.id".into(),
        ));
        assert_eq!(err.code, PluginErrorCode::Conflict);
        let err = engine_error(DbErr::Custom("database is locked (SQLITE_BUSY)".into()));
        assert_eq!(err.code, PluginErrorCode::Unavailable);
        let err = engine_error(DbErr::Custom("no such table: missing".into()));
        assert_eq!(err.code, PluginErrorCode::Internal);
    }

    #[test]
    fn fault_counters_are_consumed_once_each() {
        let counter = Mutex::new(2);
        assert!(ProfileExec::take(&counter));
        assert!(ProfileExec::take(&counter));
        assert!(!ProfileExec::take(&counter));
        assert_eq!(*ProfileExec::counter(&counter), 0);
    }

    #[tokio::test]
    async fn batch_sizes_survive_a_poisoned_lock() {
        let exec = Arc::new(ProfileExec::new(
            bookclerk_plugin_database_sqlite::open_memory_unmigrated()
                .await
                .expect("sqlite"),
            Profile::D1.capabilities(),
        ));
        exec.sizes().push(3);
        let poisoner = Arc::clone(&exec);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.sizes.lock().expect("fresh lock");
            panic!("poison the sizes lock");
        })
        .join();
        assert_eq!(exec.batch_sizes(), [3]);
    }
}
