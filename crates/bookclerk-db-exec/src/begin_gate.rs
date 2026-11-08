//! A SeaORM proxy wrapper that refuses interactive transactions the way an
//! external database adapter does.
//!
//! Production hosts reach every database through an adapter guest whose
//! session answers `begin()` with `unsupported`. A test that opens an engine
//! proxy directly gets a working `begin()` instead, so a host write that
//! opens an interactive transaction passes the test and fails in production.
//! Wrapping the engine proxy in a [`BeginGate`] closes that gap.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::{DbErr, ProxyDatabaseTrait, ProxyExecResult, ProxyRow, Statement};

use crate::proxy_txn::{is_txn_broken, note_begin_failed, take_txn_fault, txn_broken_err};

/// The message the host sees when a guest refuses `begin()`.
///
/// Matches the native-behind-workerd gateway's answer, so a test that
/// matches on it also matches production logs.
pub const INTERACTIVE_TRANSACTIONS_REFUSED: &str = "unsupported: interactive adapter transactions";

tokio::task_local! {
    /// Set while an atomic batch executor runs, so its own transaction is
    /// let through the gate.
    static BATCH_IN_PROGRESS: ();
}

/// Switch and counter shared between a [`BeginGate`] and its
/// [`BeginGateHandle`].
#[derive(Debug, Default)]
struct GateState {
    /// Whether `begin()` from the code under test is refused.
    refusing: AtomicBool,
    /// Refused `begin()` calls so far.
    refused: AtomicU32,
}

/// The test's handle on a [`BeginGate`]: the refusal switch and the count.
#[derive(Debug, Clone)]
pub struct BeginGateHandle(Arc<GateState>);

impl BeginGateHandle {
    /// Switches refusal on or off.
    ///
    /// A gate starts open, so schema application and other setup can use
    /// the engine's transactions; the test switches it to refusing before
    /// running the code under test.
    pub fn refuse(&self, refusing: bool) {
        self.0.refusing.store(refusing, Ordering::SeqCst);
    }

    /// Refused `begin()` calls so far.
    #[must_use]
    pub fn refused(&self) -> u32 {
        self.0.refused.load(Ordering::SeqCst)
    }
}

/// An engine proxy whose `begin()` can be refused.
///
/// While refusing, a `begin()` from the code under test records the proxy's
/// fail-closed per-task fault, so the first statement inside the transaction
/// fails with [`INTERACTIVE_TRANSACTIONS_REFUSED`]; the matching commit or
/// rollback clears the fault without touching the engine, which never opened
/// a transaction. Batches run inside [`with_batch_in_progress`] pass
/// through: they are the adapter's atomic batches, not interactive
/// transactions.
#[derive(Debug)]
pub struct BeginGate {
    /// The engine proxy every statement is forwarded to.
    inner: Box<dyn ProxyDatabaseTrait>,
    /// Switch and counter shared with the handle.
    state: Arc<GateState>,
}

/// Runs `batch` with every gate open for the transaction it opens.
pub async fn with_batch_in_progress<F, T>(batch: F) -> T
where
    F: std::future::Future<Output = T>,
{
    BATCH_IN_PROGRESS.scope((), batch).await
}

impl BeginGate {
    /// Wraps `inner`. The gate starts open.
    pub fn new(inner: impl ProxyDatabaseTrait + 'static) -> (Self, BeginGateHandle) {
        let state = Arc::new(GateState::default());
        (
            Self {
                inner: Box::new(inner),
                state: Arc::clone(&state),
            },
            BeginGateHandle(state),
        )
    }

    /// True when the current `begin()` must be refused.
    fn refuses_now(&self) -> bool {
        self.state.refusing.load(Ordering::SeqCst) && BATCH_IN_PROGRESS.try_with(|()| ()).is_err()
    }

    /// Clears this task's refusal fault, if the pending fault is ours.
    ///
    /// Returns true when a refused transaction was being closed, so the
    /// engine hook must not run (no engine transaction exists).
    fn close_refused() -> bool {
        if is_txn_broken()
            && txn_broken_err()
                .to_string()
                .contains(INTERACTIVE_TRANSACTIONS_REFUSED)
        {
            let _ = take_txn_fault();
            return true;
        }
        false
    }
}

#[async_trait]
impl ProxyDatabaseTrait for BeginGate {
    async fn query(&self, statement: Statement) -> Result<Vec<ProxyRow>, DbErr> {
        self.inner.query(statement).await
    }

    async fn execute(&self, statement: Statement) -> Result<ProxyExecResult, DbErr> {
        self.inner.execute(statement).await
    }

    async fn begin(&self) {
        if self.refuses_now() {
            self.state.refused.fetch_add(1, Ordering::SeqCst);
            note_begin_failed(INTERACTIVE_TRANSACTIONS_REFUSED);
            return;
        }
        self.inner.begin().await;
    }

    async fn commit(&self) {
        if Self::close_refused() {
            return;
        }
        self.inner.commit().await;
    }

    async fn rollback(&self) {
        if Self::close_refused() {
            return;
        }
        self.inner.rollback().await;
    }

    fn start_rollback(&self) {
        if Self::close_refused() {
            return;
        }
        self.inner.start_rollback();
    }

    async fn ping(&self) -> Result<(), DbErr> {
        self.inner.ping().await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Hooks the engine stand-in saw, shared with the test.
    type Log = Arc<Mutex<Vec<&'static str>>>;

    /// An engine stand-in that records which hooks ran.
    #[derive(Debug, Default)]
    struct Recorder(Log);

    #[async_trait]
    impl ProxyDatabaseTrait for Recorder {
        async fn query(&self, _: Statement) -> Result<Vec<ProxyRow>, DbErr> {
            self.0.lock().unwrap().push("query");
            Ok(Vec::new())
        }

        async fn execute(&self, _: Statement) -> Result<ProxyExecResult, DbErr> {
            self.0.lock().unwrap().push("execute");
            Ok(ProxyExecResult::default())
        }

        async fn begin(&self) {
            self.0.lock().unwrap().push("begin");
        }

        async fn commit(&self) {
            self.0.lock().unwrap().push("commit");
        }

        async fn rollback(&self) {
            self.0.lock().unwrap().push("rollback");
        }

        fn start_rollback(&self) {
            self.0.lock().unwrap().push("start_rollback");
        }

        async fn ping(&self) -> Result<(), DbErr> {
            self.0.lock().unwrap().push("ping");
            Ok(())
        }
    }

    fn gate() -> (BeginGate, BeginGateHandle, Log) {
        let log = Log::default();
        let (gate, handle) = BeginGate::new(Recorder(Arc::clone(&log)));
        (gate, handle, log)
    }

    #[tokio::test]
    async fn an_open_gate_forwards_every_hook() {
        let (gate, _handle, engine) = gate();
        gate.begin().await;
        gate.commit().await;
        gate.begin().await;
        gate.rollback().await;
        gate.start_rollback();
        assert_eq!(
            *engine.lock().unwrap(),
            ["begin", "commit", "begin", "rollback", "start_rollback"]
        );
    }

    #[tokio::test]
    async fn statements_and_pings_reach_the_engine_whatever_the_gate_does() {
        let (gate, handle, engine) = gate();
        let probe = || Statement::from_string(sea_orm::DbBackend::Sqlite, "SELECT 1");
        gate.query(probe()).await.unwrap();
        handle.refuse(true);
        gate.execute(probe()).await.unwrap();
        gate.ping().await.unwrap();
        assert_eq!(*engine.lock().unwrap(), ["query", "execute", "ping"]);
        assert_eq!(handle.refused(), 0, "statements are not begin() calls");
    }

    #[tokio::test]
    async fn a_refusing_gate_faults_the_task_until_the_transaction_closes() {
        let (gate, handle, engine) = gate();
        handle.refuse(true);
        gate.begin().await;
        assert_eq!(handle.refused(), 1);
        assert!(is_txn_broken());
        assert!(txn_broken_err()
            .to_string()
            .contains(INTERACTIVE_TRANSACTIONS_REFUSED));
        gate.rollback().await;
        assert!(!is_txn_broken(), "rollback clears the refusal");
        gate.begin().await;
        gate.commit().await;
        assert!(!is_txn_broken(), "commit clears the refusal");
        gate.begin().await;
        gate.start_rollback();
        assert!(!is_txn_broken(), "a dropped transaction clears the refusal");
        assert_eq!(handle.refused(), 3);
        assert!(
            engine.lock().unwrap().is_empty(),
            "the engine never saw a transaction"
        );
        handle.refuse(false);
        gate.begin().await;
        assert_eq!(*engine.lock().unwrap(), ["begin"]);
        assert_eq!(handle.refused(), 3);
    }

    #[tokio::test]
    async fn a_batch_in_progress_passes_through_a_refusing_gate() {
        let (gate, handle, engine) = gate();
        handle.refuse(true);
        with_batch_in_progress(async {
            gate.begin().await;
            gate.commit().await;
        })
        .await;
        assert_eq!(handle.refused(), 0);
        assert_eq!(*engine.lock().unwrap(), ["begin", "commit"]);
    }

    #[tokio::test]
    async fn another_fault_is_left_for_the_engine_to_close() {
        let (gate, handle, engine) = gate();
        handle.refuse(true);
        crate::proxy_txn::note_commit_failed("disk full");
        gate.rollback().await;
        assert!(is_txn_broken(), "a fault the gate did not raise stays");
        let _ = take_txn_fault();
        assert_eq!(*engine.lock().unwrap(), ["rollback"]);
    }
}
