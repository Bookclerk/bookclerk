//! The adapter conformance suite passes a conforming adapter and catches
//! each kind of broken adapter it claims to catch.
//!
//! Every fault wraps the platform SQLite binding adapter and breaks one
//! part of the contract; the suite must fail exactly the case that covers
//! it, so a regression that blinds a case shows up here.

// Test code: unwrap/expect are fine here.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::Cell;

use async_trait::async_trait;
use bookclerk_db_testkit::adapter::{run_adapter_suite, Case, Outcome};
use bookclerk_plugin_abi::{
    AdapterDatabaseSession, AdapterExecuteRequest, Database, DbCapabilities, DbColumn,
    DbResultSelection, DbRow, DbTiming, DbType, DbValue, ExecuteReply, ExecuteStream,
    GuestReceiptPersist, PluginError, PluginErrorCode, StatementResult,
};

/// One way to break the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    /// No fault: the wrapped adapter as shipped.
    None,
    /// The second `capabilities` call raises `maxResultRows`.
    UnstableCapabilities,
    /// `Bytes` cells come back as `Text`.
    BytesAsText,
    /// Typed `limit_exceeded` errors are flattened to `unavailable`.
    UntypedLimits,
    /// The receipt finalize hint is dropped, so no replay payload is kept.
    DropsReceiptPayload,
    /// A streamed budget is advertised but `executeStreamed` is refused.
    RefusesStreaming,
    /// A read the adapter would refuse as over a cap is answered with an
    /// oversized result instead, leaving the refusal to the host.
    ReturnsOverCap,
}

/// A database whose sessions apply `fault` around the SQLite adapter.
struct Faulty {
    /// The wrapped adapter.
    inner: Box<dyn Database>,
    /// The injected fault.
    fault: Fault,
}

/// A session of [`Faulty`].
struct FaultySession {
    /// The wrapped session.
    inner: Box<dyn AdapterDatabaseSession>,
    /// The injected fault.
    fault: Fault,
    /// `capabilities` calls answered so far.
    calls: Cell<u32>,
}

#[async_trait(?Send)]
impl Database for Faulty {
    async fn open_session(&self) -> Result<Box<dyn AdapterDatabaseSession>, PluginError> {
        Ok(Box::new(FaultySession {
            inner: self.inner.open_session().await?,
            fault: self.fault,
            calls: Cell::new(0),
        }))
    }
}

#[async_trait(?Send)]
impl AdapterDatabaseSession for FaultySession {
    async fn capabilities(&self) -> Result<DbCapabilities, PluginError> {
        let mut caps = self.inner.capabilities().await?;
        self.calls.set(self.calls.get() + 1);
        if self.fault == Fault::UnstableCapabilities && self.calls.get() > 1 {
            caps.max_result_rows += 1;
        }
        Ok(caps)
    }

    async fn execute(
        &self,
        mut request: AdapterExecuteRequest,
    ) -> Result<ExecuteReply, PluginError> {
        if self.fault == Fault::DropsReceiptPayload {
            request.guest_receipt = GuestReceiptPersist::default();
        }
        let over_cap = over_cap_reply(&request);
        let reply = self.inner.execute(request).await;
        match self.fault {
            Fault::UntypedLimits => reply.map_err(|err| {
                if err.code == PluginErrorCode::LimitExceeded {
                    PluginError::unavailable(err.message)
                } else {
                    err
                }
            }),
            Fault::BytesAsText => reply.map(|mut reply| {
                for stmt in &mut reply.statements {
                    for row in &mut stmt.rows {
                        for value in &mut row.values {
                            if let DbValue::Bytes(bytes) = value {
                                *value = DbValue::Text(format!("{bytes:?}"));
                            }
                        }
                    }
                }
                reply
            }),
            Fault::ReturnsOverCap => match reply {
                Err(err) if err.code == PluginErrorCode::LimitExceeded => Ok(over_cap),
                reply => reply,
            },
            Fault::None
            | Fault::UnstableCapabilities
            | Fault::DropsReceiptPayload
            | Fault::RefusesStreaming => reply,
        }
    }

    async fn execute_streamed(
        &self,
        request: AdapterExecuteRequest,
    ) -> Result<ExecuteStream, PluginError> {
        if self.fault == Fault::RefusesStreaming {
            return Err(PluginError::unsupported("streaming switched off"));
        }
        self.inner.execute_streamed(request).await
    }

    async fn close(&self) -> Result<(), PluginError> {
        self.inner.close().await
    }
}

/// A reply to `request` that returns one row more than every row-reading
/// statement allows, as an adapter that never refuses would.
fn over_cap_reply(request: &AdapterExecuteRequest) -> ExecuteReply {
    let statements = request
        .request
        .statements
        .iter()
        .map(|stmt| match stmt.result_selection {
            DbResultSelection::Rows => StatementResult {
                rows: (0..=stmt.max_rows)
                    .map(|_| DbRow {
                        values: vec![DbValue::Int64(0)],
                    })
                    .collect(),
                columns: vec![DbColumn {
                    name: "id".into(),
                    db_type: DbType::Int64,
                }],
                rows_affected: 0,
            },
            DbResultSelection::AffectedRows | DbResultSelection::Discard => StatementResult {
                rows: Vec::new(),
                columns: Vec::new(),
                rows_affected: 0,
            },
        })
        .collect();
    ExecuteReply {
        operation_id: request.request.operation_id.clone(),
        statements,
        timing: DbTiming::default(),
    }
}

/// The suite's outcomes for the SQLite adapter under `fault`, on a fresh
/// empty unit.
async fn outcomes_under(fault: Fault) -> Vec<(Case, Outcome)> {
    let conn = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .expect("empty in-memory unit");
    let database = Faulty {
        inner: bookclerk_plugin_database_sqlite::plugin::binding_database(conn),
        fault,
    };
    run_adapter_suite(&database).await.outcomes
}

/// The cases that failed.
fn failed_cases(outcomes: &[(Case, Outcome)]) -> Vec<Case> {
    outcomes
        .iter()
        .filter(|(_, outcome)| matches!(outcome, Outcome::Failed(_)))
        .map(|(case, _)| *case)
        .collect()
}

#[tokio::test]
async fn a_conforming_adapter_passes_every_case() {
    let outcomes = outcomes_under(Fault::None).await;
    assert_eq!(outcomes.len(), Case::ALL.len());
    for (case, outcome) in outcomes {
        assert_eq!(outcome, Outcome::Passed, "{case}");
    }
}

#[tokio::test]
async fn unstable_capabilities_fail_negotiation_and_skip_the_rest() {
    let outcomes = outcomes_under(Fault::UnstableCapabilities).await;
    assert_eq!(failed_cases(&outcomes), vec![Case::Capabilities]);
    assert!(outcomes
        .iter()
        .skip(1)
        .all(|(_, outcome)| matches!(outcome, Outcome::Skipped(_))));
}

#[tokio::test]
async fn bytes_read_back_as_text_fail_the_value_case() {
    let outcomes = outcomes_under(Fault::BytesAsText).await;
    assert_eq!(failed_cases(&outcomes), vec![Case::Values]);
}

#[tokio::test]
async fn untyped_limit_errors_fail_every_limit_case() {
    let outcomes = outcomes_under(Fault::UntypedLimits).await;
    assert_eq!(
        failed_cases(&outcomes),
        vec![Case::ResultRows, Case::CellBytes, Case::ResultBytes]
    );
}

#[tokio::test]
async fn a_dropped_receipt_payload_fails_replay() {
    let outcomes = outcomes_under(Fault::DropsReceiptPayload).await;
    assert_eq!(failed_cases(&outcomes), vec![Case::ReceiptReplay]);
    assert!(outcomes
        .iter()
        .any(|(case, outcome)| *case == Case::ReceiptConflict
            && matches!(outcome, Outcome::Skipped(_))));
}

#[tokio::test]
async fn an_advertised_but_refused_stream_fails_the_streamed_case() {
    let outcomes = outcomes_under(Fault::RefusesStreaming).await;
    assert_eq!(failed_cases(&outcomes), vec![Case::Streamed]);
}

#[tokio::test]
async fn an_adapter_that_leaves_refusal_to_the_host_fails_every_limit_case() {
    let outcomes = outcomes_under(Fault::ReturnsOverCap).await;
    assert_eq!(
        failed_cases(&outcomes),
        vec![Case::ResultRows, Case::CellBytes, Case::ResultBytes]
    );
    for (case, outcome) in &outcomes {
        if let Outcome::Failed(reason) = outcome {
            assert!(
                reason.contains("only the host's reply check"),
                "{case}: {reason}"
            );
        }
    }
}
