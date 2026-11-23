//! A SeaORM proxy wrapper that refuses interactive transactions the way an
//! external database adapter does.
//!
//! Production hosts reach every database through an adapter guest whose
//! session answers `begin()` with `unsupported`. A test that opens an engine
//! proxy directly gets a working `begin()` instead, so a host write that
//! opens an interactive transaction passes the test and fails in production.
//! Wrapping the engine proxy in a [`BeginGate`] closes that gap.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

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
    /// Set while a test reads the tables for its own assertions, so the
    /// read is not held to the plugin's result limits.
    static READS_UNCAPPED: ();
}

/// Switch and counter shared between a [`BeginGate`] and its
/// [`BeginGateHandle`].
#[derive(Debug, Default)]
struct GateState {
    /// Whether `begin()` from the code under test is refused.
    refusing: AtomicBool,
    /// Refused `begin()` calls so far.
    refused: AtomicU32,
    /// The result limits plain reads are held to, once set: rows, encoded
    /// result bytes and cell bytes ([`ReadCaps`]).
    read_caps: Mutex<Option<ReadCaps>>,
}

/// The result limits a database plugin holds a host's plain proxy read to.
///
/// A plugin that streams results answers a read up to its streamed budget;
/// one that does not, up to its inline `maxResultRows` / `maxResultBytes`.
/// Every cell is held to `maxCellBytes` either way. `0` is unlimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReadCaps {
    /// Most rows one read returns.
    pub max_rows: u32,
    /// Most encoded JSON bytes of one read's rows.
    pub max_bytes: u32,
    /// Most UTF-8 (or base64 blob) bytes of one cell.
    pub max_cell_bytes: u32,
}

impl ReadCaps {
    /// The limits `caps` puts on a plain read: its streamed budget when it
    /// streams, else its inline result limits.
    #[must_use]
    pub fn from_capabilities(caps: &bookclerk_plugin_abi::DbCapabilities) -> Self {
        let read = caps.streamed().unwrap_or_else(|| caps.clone());
        Self {
            max_rows: read.max_result_rows,
            max_bytes: read.max_result_bytes,
            max_cell_bytes: caps.max_cell_bytes,
        }
    }

    /// Checks `rows` against these limits, the way an adapter refuses a
    /// reply: a typed `limit_exceeded` message naming the limit passed.
    ///
    /// # Errors
    ///
    /// Returns [`DbErr::Custom`] carrying the typed limit message.
    pub fn check(&self, rows: &[ProxyRow]) -> Result<(), DbErr> {
        use bookclerk_plugin_abi::LimitExceeded;
        let count = u64::try_from(rows.len()).unwrap_or(u64::MAX);
        if self.max_rows > 0 && count > u64::from(self.max_rows) {
            return Err(DbErr::Custom(
                LimitExceeded::new("maxResultRows", u64::from(self.max_rows), count).to_message(),
            ));
        }
        let mut used = 0usize;
        for row in rows {
            if self.max_cell_bytes > 0 {
                for (name, value) in &row.values {
                    let n = crate::sea_cell_utf8_len(value);
                    if n > usize::try_from(self.max_cell_bytes).unwrap_or(usize::MAX) {
                        return Err(DbErr::Custom(format!(
                            "{} (column `{name}`)",
                            LimitExceeded::new(
                                "maxCellBytes",
                                u64::from(self.max_cell_bytes),
                                u64::try_from(n).unwrap_or(u64::MAX),
                            )
                            .to_message()
                        )));
                    }
                }
            }
            crate::note_encoded_result_bytes(
                &mut used,
                crate::encoded_proxy_row_len(&row.values),
                self.max_bytes,
            )?;
        }
        Ok(())
    }
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

    /// Holds every plain read outside an atomic batch to `caps` (`None`
    /// lifts the limits). Batches are held to their own limits by their
    /// executor.
    pub fn cap_reads(&self, caps: Option<ReadCaps>) {
        *self
            .0
            .read_caps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = caps;
    }
}

/// An engine proxy whose `begin()` can be refused.
///
/// While refusing, a `begin()` from the code under test records the proxy's
/// fail-closed per-task fault, and the gate answers every statement inside
/// the transaction with [`INTERACTIVE_TRANSACTIONS_REFUSED`] (the adapters
/// refuse statements after a failed begin the same way, so none autocommits);
/// the matching commit or rollback clears the fault without touching the
/// engine, which never opened a transaction. Batches run inside [`with_batch_in_progress`] pass
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

/// Runs `read` with plain reads let past the [`ReadCaps`] set by
/// [`BeginGateHandle::cap_reads`]: for a test's own look at the tables,
/// which no plugin serves in production.
pub async fn with_uncapped_reads<F, T>(read: F) -> T
where
    F: std::future::Future<Output = T>,
{
    READS_UNCAPPED.scope((), read).await
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
        if is_txn_broken() {
            return Err(txn_broken_err());
        }
        let rows = self.inner.query(statement).await?;
        let caps = *self
            .state
            .read_caps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(caps) = caps {
            if BATCH_IN_PROGRESS.try_with(|()| ()).is_err()
                && READS_UNCAPPED.try_with(|()| ()).is_err()
            {
                caps.check(&rows)?;
            }
        }
        Ok(rows)
    }

    async fn execute(&self, statement: Statement) -> Result<ProxyExecResult, DbErr> {
        if is_txn_broken() {
            return Err(txn_broken_err());
        }
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
    async fn statements_after_a_refused_begin_fail_until_the_transaction_closes() {
        let (gate, handle, engine) = gate();
        let probe = || Statement::from_string(sea_orm::DbBackend::Sqlite, "SELECT 1");
        handle.refuse(true);
        gate.begin().await;
        // SeaORM keeps running the transaction's statements after a begin
        // that reported nothing, so the gate refuses them the way the
        // adapters do, or they would autocommit one by one.
        let err = gate.query(probe()).await.unwrap_err();
        assert!(
            err.to_string().contains(INTERACTIVE_TRANSACTIONS_REFUSED),
            "{err}"
        );
        let err = gate.execute(probe()).await.unwrap_err();
        assert!(
            err.to_string().contains(INTERACTIVE_TRANSACTIONS_REFUSED),
            "{err}"
        );
        assert!(
            engine.lock().unwrap().is_empty(),
            "nothing reached the engine"
        );
        gate.rollback().await;
        gate.execute(probe()).await.unwrap();
        assert_eq!(*engine.lock().unwrap(), ["execute"]);
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

    /// An engine stand-in whose every read answers `rows` copies of a
    /// one-cell row holding `cell`.
    #[derive(Debug)]
    struct Rows {
        /// Rows each read answers.
        rows: usize,
        /// The text of each row's one cell.
        cell: String,
    }

    #[async_trait]
    impl ProxyDatabaseTrait for Rows {
        async fn query(&self, _: Statement) -> Result<Vec<ProxyRow>, DbErr> {
            Ok(vec![one_cell(&self.cell); self.rows])
        }

        async fn execute(&self, _: Statement) -> Result<ProxyExecResult, DbErr> {
            Ok(ProxyExecResult::default())
        }
    }

    /// A row with one text cell `value`.
    fn one_cell(value: &str) -> ProxyRow {
        ProxyRow {
            values: std::collections::BTreeMap::from([(
                "v".to_string(),
                sea_orm::Value::String(Some(value.to_string())),
            )]),
        }
    }

    /// A read statement for the stand-ins.
    fn select() -> Statement {
        Statement::from_string(sea_orm::DbBackend::Sqlite, "SELECT v FROM t")
    }

    /// The limit a refused read names.
    fn refused_limit(err: &DbErr) -> String {
        bookclerk_plugin_abi::LimitExceeded::from_message(&err.to_string())
            .map(|limit| limit.limit)
            .unwrap_or_default()
    }

    #[test]
    fn read_caps_refuse_rows_bytes_and_cells_past_their_limits() {
        let caps = ReadCaps {
            max_rows: 2,
            max_bytes: 64,
            max_cell_bytes: 8,
        };
        assert!(caps.check(&[one_cell("a"), one_cell("b")]).is_ok());
        let rows = caps
            .check(&[one_cell("a"), one_cell("b"), one_cell("c")])
            .unwrap_err();
        assert_eq!(refused_limit(&rows), "maxResultRows");
        let cell = caps.check(&[one_cell("123456789")]).unwrap_err();
        assert_eq!(refused_limit(&cell), "maxCellBytes");
        assert!(cell.to_string().contains("column `v`"));
        let wide = ReadCaps {
            max_rows: 0,
            max_bytes: 40,
            max_cell_bytes: 0,
        };
        let bytes = wide.check(&[one_cell(&"x".repeat(64))]).unwrap_err();
        assert_eq!(refused_limit(&bytes), "maxResultBytes");
        assert!(ReadCaps::default()
            .check(&vec![one_cell(&"x".repeat(4096)); 512])
            .is_ok());
    }

    #[test]
    fn read_caps_follow_the_streamed_budget_when_the_plugin_streams() {
        let inline = bookclerk_plugin_abi::DbCapabilities::host_minimum();
        let caps = ReadCaps::from_capabilities(&inline);
        assert_eq!(caps.max_rows, inline.max_result_rows);
        assert_eq!(caps.max_bytes, inline.max_result_bytes);
        assert_eq!(caps.max_cell_bytes, inline.max_cell_bytes);
        let streaming = bookclerk_plugin_abi::DbCapabilities::advertised_sqlite();
        let streamed = streaming.streamed().expect("the sqlite adapter streams");
        let caps = ReadCaps::from_capabilities(&streaming);
        assert_eq!(caps.max_rows, streamed.max_result_rows);
        assert_eq!(caps.max_bytes, streamed.max_result_bytes);
    }

    #[tokio::test]
    async fn capped_reads_are_refused_outside_batches_and_uncapped_reads() {
        let (gate, handle) = BeginGate::new(Rows {
            rows: 3,
            cell: "a".into(),
        });
        assert_eq!(
            gate.query(select()).await.unwrap().len(),
            3,
            "not capped yet"
        );
        handle.cap_reads(Some(ReadCaps {
            max_rows: 1,
            ..ReadCaps::default()
        }));
        let err = gate.query(select()).await.unwrap_err();
        assert_eq!(refused_limit(&err), "maxResultRows");
        let in_batch = with_batch_in_progress(gate.query(select())).await;
        assert_eq!(
            in_batch.unwrap().len(),
            3,
            "a batch is held by its executor"
        );
        let uncapped = with_uncapped_reads(gate.query(select())).await;
        assert_eq!(uncapped.unwrap().len(), 3, "a test's own look");
        handle.cap_reads(None);
        assert_eq!(gate.query(select()).await.unwrap().len(), 3, "lifted");
    }
}
