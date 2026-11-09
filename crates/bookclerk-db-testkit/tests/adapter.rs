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
    AdapterDatabaseSession, AdapterExecuteRequest, Database, DbCapabilities, DbValue, ExecuteReply,
    ExecuteStream, GuestReceiptPersist, PluginError, PluginErrorCode,
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
        let reply = self.inner.execute(request).await;
        match (self.fault, reply) {
            (Fault::UntypedLimits, Err(err)) if err.code == PluginErrorCode::LimitExceeded => {
                Err(PluginError::unavailable(err.message))
            }
            (Fault::BytesAsText, Ok(mut reply)) => {
                for stmt in &mut reply.statements {
                    for row in &mut stmt.rows {
                        for value in &mut row.values {
                            if let DbValue::Bytes(bytes) = value {
                                *value = DbValue::Text(format!("{bytes:?}"));
                            }
                        }
                    }
                }
                Ok(reply)
            }
            (_, reply) => reply,
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
