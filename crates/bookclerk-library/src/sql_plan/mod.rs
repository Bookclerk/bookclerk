//! Host-owned SQL atomic plans for database plugins.
//!
//! The library compiles Bookclerk domain operations into a typed
//! [`bookclerk_plugin_abi::ExecuteRequest`] plus host-only
//! [`AtomicSelection`] indexes. Database guests execute the statements as
//! one transaction and return a typed [`bookclerk_plugin_abi::ExecuteReply`].

#[cfg(test)]
mod conformance;
mod exec;
mod guest_receipt;
pub mod host_ir;
mod interpret;
mod named;
mod reply;
mod slots;
mod typed_vectors;
mod vectors_typed;

/// Injected `maxResultRows` for conn-vector row-cap cases (sqlite / postgres).
pub(crate) const CONTRACT_VECTOR_ROW_CAP: u32 = 5;

use bookclerk_plugin_abi::{encoded_execute_request_bytes, DbCapabilities, ExecuteRequest};

pub use bookclerk_plugin_abi::DbPlanStatementKind;
pub use host_ir::AtomicSelection;

pub use exec::{
    execute_compiled_on, execute_compiled_on_capped, execute_typed_on, execute_typed_on_session,
    AtomicSession,
};
pub(crate) use guest_receipt::{unwrap_guest_typed_reply, wrap_guest_typed_request};
pub use interpret::{interpret_typed_exec, PlanStmtResult};
pub use named::{compile_claim_event_delivery, compile_named_request};
pub use reply::validate_execute_reply;
pub use slots::{event_inflight_slot, lock_serialization_slot, JOB_QUEUE_SLOT};
pub use typed_vectors::{run_typed_conn_vectors, run_typed_request_vectors};
pub use vectors_typed::run_typed_contract_vectors;

/// Compiled typed request plus host result-selection indexes.
#[derive(Debug, Clone)]
pub struct CompiledAtomic {
    /// Typed request executed by the guest.
    pub request: ExecuteRequest,
    /// Host indexes used to interpret the typed reply.
    pub selection: AtomicSelection,
    /// SHA-256 hex compared on receipt replay.
    pub expected_hash: String,
}

impl CompiledAtomic {
    /// Typed [`ExecuteRequest`] for adapter `execute` (Cap'n data plane).
    #[must_use]
    pub fn into_typed_request(mut self, operation_id: impl Into<String>) -> ExecuteRequest {
        self.request.operation_id = operation_id.into();
        self.request
    }
}

/// Rejects a compiled plan that exceeds negotiated guest limits or has
/// out-of-range selectors.
///
/// # Errors
///
/// Returns [`crate::LibraryError::Other`] when the plan cannot be sent.
pub fn validate_plan(compiled: &CompiledAtomic, caps: &DbCapabilities) -> crate::error::Result<()> {
    validate_selection(&compiled.selection, compiled.request.statements.len())?;
    validate_execute_request(&compiled.request, caps)
}

/// Rejects result-selection indexes that fall outside the statement list.
fn validate_selection(selection: &AtomicSelection, stmt_len: usize) -> crate::error::Result<()> {
    let mut selectors = vec![("outcomeIndex", selection.outcome_index)];
    if let Some(idx) = selection.payload_index {
        selectors.push(("payloadIndex", idx));
    }
    if let Some(idx) = selection.prior_receipt_index {
        selectors.push(("priorReceiptIndex", idx));
    }
    if let Some(idx) = selection.receipt_select_index {
        selectors.push(("receiptSelectIndex", idx));
    }
    for (name, idx) in selectors {
        if (idx as usize) >= stmt_len {
            return Err(crate::LibraryError::Other(anyhow::anyhow!(
                "atomic plan {name} {idx} is out of range for {stmt_len} statements"
            )));
        }
    }
    Ok(())
}

/// Rejects a typed [`ExecuteRequest`] that exceeds negotiated guest limits.
///
/// `maxRequestBytes` is measured against the Cap'n-encoded [`ExecuteRequest`].
///
/// # Errors
///
/// Returns [`crate::LibraryError::Other`] when the request cannot be sent.
pub fn validate_execute_request(
    req: &ExecuteRequest,
    caps: &DbCapabilities,
) -> crate::error::Result<()> {
    if req.statements.is_empty() {
        return Err(crate::LibraryError::Other(anyhow::anyhow!(
            "executeAtomic statements must be non-empty"
        )));
    }
    let n_stmt = u32::try_from(req.statements.len()).unwrap_or(u32::MAX);
    if caps.max_statements > 0 && n_stmt > caps.max_statements {
        return Err(crate::LibraryError::Other(anyhow::anyhow!(
            "atomic request has {n_stmt} statements; guest maxStatements is {}",
            caps.max_statements
        )));
    }
    for (i, stmt) in req.statements.iter().enumerate() {
        let n_binds = u32::try_from(stmt.parameters.len()).unwrap_or(u32::MAX);
        if caps.max_binds > 0 && n_binds > caps.max_binds {
            return Err(crate::LibraryError::Other(anyhow::anyhow!(
                "atomic request statement {i} has {n_binds} binds; guest maxBinds is {}",
                caps.max_binds
            )));
        }
        if caps.max_payload_bytes > 0 {
            let mut binds_len = 0usize;
            for param in &stmt.parameters {
                binds_len = binds_len.saturating_add(
                    bookclerk_plugin_abi::encoded_db_value_bytes(param)
                        .map_err(|err| {
                            crate::LibraryError::Other(anyhow::anyhow!(err.to_string()))
                        })?
                        .len(),
                );
            }
            let payload = stmt.sql.len().saturating_add(binds_len);
            let cap = usize::try_from(caps.max_payload_bytes).unwrap_or(usize::MAX);
            if payload > cap {
                return Err(crate::LibraryError::Other(anyhow::anyhow!(
                    "atomic request statement {i} encoded payload is {payload} bytes; guest maxPayloadBytes is {}",
                    caps.max_payload_bytes
                )));
            }
        }
        caps.admit_statement(&stmt.sql, &stmt.parameters)
            .map_err(|err| crate::LibraryError::Other(anyhow::anyhow!("{err}")))?;
    }
    let cap = atomic_request_cap_bytes(caps);
    if cap == 0 {
        return Ok(());
    }
    let bytes = encoded_execute_request_bytes(req)
        .map_err(|err| crate::LibraryError::Other(anyhow::anyhow!(err.to_string())))?
        .len();
    if bytes > cap {
        return Err(crate::LibraryError::Other(anyhow::anyhow!(
            "atomic request is {bytes} bytes; guest maxRequestBytes is {cap}"
        )));
    }
    Ok(())
}

/// After host typecheck, reject a resolved batch whose proven lowered size
/// exceeds negotiated `maxLoweredStatementBytes`.
///
/// Call this with the real [`bookclerk_plugin_abi::ResolvedStatement`] proofs
/// before adapter dispatch. [`validate_execute_request`] only has an empty
/// catalog, so INTEGER column arithmetic is charged here.
///
/// # Errors
///
/// Returns [`crate::LibraryError::Other`] when proofs are missing/mismatched
/// or the standardized lowering upper bound exceeds the advertised ceiling.
pub fn validate_proven_execute_request(
    req: &ExecuteRequest,
    proofs: &[bookclerk_plugin_abi::ResolvedStatement],
    caps: &DbCapabilities,
) -> crate::error::Result<()> {
    caps.admit_proven_execute(req, proofs)
        .map_err(|err| crate::LibraryError::Other(anyhow::anyhow!("{err}")))
}

/// Host-authorizes a guest typed batch: overwrite statement kinds, enforce
/// negotiated caps, and stamp the canonical Cap'n request hash.
///
/// A non-empty guest `requestHash` must match the host digest (retry token).
/// An empty `operationId` is replaced with a fresh UUID.
///
/// # Errors
///
/// Returns [`crate::LibraryError::Other`] when the request exceeds caps, the
/// retry hash does not match, or the canonical digest cannot be encoded.
pub fn authorize_typed_request(
    req: &mut ExecuteRequest,
    caps: &DbCapabilities,
) -> crate::error::Result<()> {
    for stmt in &mut req.statements {
        stmt.kind = host_statement_kind(&stmt.sql);
    }
    // A nonzero `maxRows` on a RETURNING statement is a boundedness claim
    // adapters may rely on (D1 commits before it can inspect results). The
    // claim must be host-proven from the authorized SQL shape — a caller's
    // number is never trusted.
    for (i, stmt) in req.statements.iter().enumerate() {
        if matches!(stmt.kind, DbPlanStatementKind::Returning)
            && stmt.max_rows != 0
            && (stmt.max_rows != 1 || !bookclerk_plugin_abi::returning_single_row_proven(&stmt.sql))
        {
            return Err(crate::LibraryError::Other(anyhow::anyhow!(
                "statement {i} RETURNING maxRows={} is not provable from the SQL shape \
                 (only a single-tuple INSERT … VALUES … RETURNING proves maxRows=1)",
                stmt.max_rows
            )));
        }
    }
    if req.operation_id.is_empty() {
        req.operation_id = uuid::Uuid::new_v4().to_string();
    }
    let computed = bookclerk_plugin_abi::canonical_execute_request_hash(req)
        .map_err(|err| crate::LibraryError::Other(anyhow::anyhow!(err.to_string())))?;
    if !req.request_hash.is_empty() && req.request_hash != computed {
        return Err(crate::LibraryError::Other(anyhow::anyhow!(
            "retry token requestHash does not match the canonical request"
        )));
    }
    req.request_hash = computed;
    validate_execute_request(req, caps)?;
    Ok(())
}

/// Authorizes, receipt-wraps, executes, and unwraps one guest typed batch.
///
/// The composable core of
/// [`LibraryStore::execute_guest_atomic`](crate::LibraryStore::execute_guest_atomic)
/// for callers that execute on something other than the library connection —
/// e.g. an isolated plugin database binding session. `exec` receives the
/// receipt-wrapped envelope and must run it atomically on the target
/// database (which needs its own `db_atomic_receipts` table — see
/// [`crate::migrations::binding_bootstrap_sql`]).
///
/// # Errors
///
/// Returns [`bookclerk_plugin_abi::PluginError::invalid_params`] when the SQL
/// is outside the guest grammar or `policy`, and the executor's error or a
/// validation error when the batch fails.
pub async fn execute_guest_atomic_with<F, Fut>(
    mut req: ExecuteRequest,
    caps: &DbCapabilities,
    policy: &bookclerk_plugin_abi::GuestSqlPolicy,
    exec: F,
) -> std::result::Result<bookclerk_plugin_abi::ExecuteReply, bookclerk_plugin_abi::PluginError>
where
    F: FnOnce(bookclerk_db_exec::HostExecuteEnvelope) -> Fut,
    Fut: std::future::Future<
        Output = std::result::Result<
            bookclerk_plugin_abi::ExecuteReply,
            bookclerk_plugin_abi::PluginError,
        >,
    >,
{
    authorize_guest_typed_request(&mut req, caps, policy)
        .map_err(|err| bookclerk_plugin_abi::PluginError::invalid_params(err.to_string()))?;
    let guest_req = req.clone();
    let envelope = wrap_guest_typed_request(req, policy.sql_types())?;
    caps.admit_proven_execute(&envelope.request, &envelope.proofs)
        .map_err(|err| bookclerk_plugin_abi::PluginError::invalid_params(err.to_string()))?;
    let reply = exec(envelope.clone()).await?;
    crate::validate_execute_reply(&envelope.request, &reply, caps)
        .map_err(|err| bookclerk_plugin_abi::PluginError::unavailable(err.to_string()))?;
    unwrap_guest_typed_reply(reply, &guest_req, caps)
}

/// Authorizes a **guest-authored** typed batch.
///
/// Pipeline: plugin SQL → SQL-v1 grammar validation → binding
/// scope/security authorization → capability/limit validation → canonical
/// [`bookclerk_plugin_abi::ExecuteRequest`]. Adapter-edge lowering happens
/// later, at execute, against the isolated physical DB.
///
/// Host schema DDL must use [`authorize_typed_request`] directly.
///
/// # Errors
///
/// Returns [`crate::LibraryError::Other`] when the SQL is outside the guest
/// grammar or fails host validation.
pub fn authorize_guest_typed_request(
    req: &mut ExecuteRequest,
    caps: &DbCapabilities,
    policy: &bookclerk_plugin_abi::GuestSqlPolicy,
) -> crate::error::Result<()> {
    bookclerk_plugin_abi::validate_guest_execute_request_for_policy(req, policy)
        .map_err(|err| crate::LibraryError::Other(anyhow::anyhow!(err.to_string())))?;
    bookclerk_plugin_abi::authorize_guest_sql_policy(req, policy)
        .map_err(|err| crate::LibraryError::Other(anyhow::anyhow!(err.to_string())))?;
    authorize_typed_request(req, caps)
}

/// Byte budget for one encoded request (`0` = skip the check).
fn atomic_request_cap_bytes(caps: &DbCapabilities) -> usize {
    if caps.max_request_bytes == 0 {
        return 0;
    }
    usize::try_from(
        caps.max_request_bytes
            .min(bookclerk_plugin_abi::MAX_SCALAR_BYTES),
    )
    .unwrap_or(usize::MAX)
}

/// Wake page size from negotiated `maxBinds` (SET/EXISTS overhead is 4 binds).
#[must_use]
pub fn wake_page_for_max_binds(max_binds: u32) -> u64 {
    const FIXED: u32 = 4;
    u64::from(max_binds.saturating_sub(FIXED).clamp(8, 256))
}

/// Host-authored statement shape for the wire `kind` field.
///
/// Adapters must not reparse SQL; they trust this classification.
#[must_use]
pub fn host_statement_kind(sql: &str) -> bookclerk_plugin_abi::DbPlanStatementKind {
    named::authored_kind(sql)
}

/// Kind for a SeaORM proxy **read**. `SELECT`/`WITH`/`VALUES` become `Select`
/// (LIMIT-wrapped). `RETURNING` stays `Returning`. Everything else (PRAGMA,
/// schema introspection) is `Returning` so it is not LIMIT-wrapped.
#[must_use]
pub fn proxy_read_kind(sql: &str) -> bookclerk_plugin_abi::DbPlanStatementKind {
    match named::authored_kind(sql) {
        bookclerk_plugin_abi::DbPlanStatementKind::Select => {
            bookclerk_plugin_abi::DbPlanStatementKind::Select
        }
        bookclerk_plugin_abi::DbPlanStatementKind::Returning => {
            bookclerk_plugin_abi::DbPlanStatementKind::Returning
        }
        // Execute-classified SQL on the read path (PRAGMA, etc.): treat as
        // non-wrappable row-producing work.
        bookclerk_plugin_abi::DbPlanStatementKind::Execute => {
            bookclerk_plugin_abi::DbPlanStatementKind::Returning
        }
    }
}

/// Kind for a SeaORM proxy **write**. `RETURNING` is preserved; other DML is
/// `Execute`.
#[must_use]
pub fn proxy_write_kind(sql: &str) -> bookclerk_plugin_abi::DbPlanStatementKind {
    match named::authored_kind(sql) {
        bookclerk_plugin_abi::DbPlanStatementKind::Returning => {
            bookclerk_plugin_abi::DbPlanStatementKind::Returning
        }
        _ => bookclerk_plugin_abi::DbPlanStatementKind::Execute,
    }
}

#[cfg(test)]
mod limits_tests {
    use super::{
        validate_execute_request, validate_plan, AtomicSelection, CompiledAtomic, DbCapabilities,
        DbPlanStatementKind, ExecuteRequest,
    };
    use bookclerk_plugin_abi::{DbResultSelection, DbValue, TypedDbStatement};

    fn tiny_caps() -> DbCapabilities {
        let mut caps = DbCapabilities::advertised_sqlite();
        caps.max_statements = 2;
        caps.max_binds = 2;
        caps.max_payload_bytes = 64;
        caps
    }

    fn compiled(statements: Vec<TypedDbStatement>, outcome_index: u32) -> CompiledAtomic {
        CompiledAtomic {
            request: ExecuteRequest {
                operation_id: "op".into(),
                request_hash: String::new(),
                statements,
                deadline_unix_ms: 0,
            },
            selection: AtomicSelection {
                outcome_index,
                payload_index: None,
                prior_receipt_index: None,
                receipt_select_index: None,
            },
            expected_hash: String::new(),
        }
    }

    fn stmt(sql: &str, parameters: Vec<DbValue>) -> TypedDbStatement {
        TypedDbStatement {
            sql: sql.into(),
            parameters,
            kind: DbPlanStatementKind::Select,
            max_rows: 0,
            result_selection: DbResultSelection::Rows,
        }
    }

    #[test]
    fn validate_plan_rejects_too_many_statements() {
        let plan = compiled(
            vec![
                stmt("SELECT 1", vec![]),
                stmt("SELECT 2", vec![]),
                stmt("SELECT 3", vec![]),
            ],
            0,
        );
        let err = validate_plan(&plan, &tiny_caps()).unwrap_err();
        assert!(err.to_string().contains("maxStatements"), "{err}");
    }

    #[test]
    fn validate_plan_rejects_too_many_binds() {
        let plan = compiled(
            vec![stmt(
                "SELECT ?, ?, ?",
                vec![DbValue::Int64(1), DbValue::Int64(2), DbValue::Int64(3)],
            )],
            0,
        );
        let err = validate_plan(&plan, &tiny_caps()).unwrap_err();
        assert!(err.to_string().contains("maxBinds"), "{err}");
    }

    #[test]
    fn advertised_d1_rejects_more_than_max_binds() {
        let caps = DbCapabilities::advertised_d1();
        let binds = vec![DbValue::Int64(1); (caps.max_binds as usize) + 1];
        let plan = compiled(vec![stmt("SELECT 1", binds)], 0);
        let err = validate_plan(&plan, &caps).unwrap_err();
        assert!(err.to_string().contains("maxBinds"), "{err}");
    }

    #[test]
    fn validate_plan_rejects_payload_bytes() {
        let plan = compiled(
            vec![stmt("SELECT ?", vec![DbValue::Text("x".repeat(200))])],
            0,
        );
        let err = validate_plan(&plan, &tiny_caps()).unwrap_err();
        assert!(err.to_string().contains("maxPayloadBytes"), "{err}");
    }

    #[test]
    fn validate_plan_rejects_oversized_dispatch_page() {
        use crate::atomic_ops::DbAtomicParams;
        use crate::sql_plan::compile_named_request;
        let subs: Vec<serde_json::Value> = (0..50)
            .map(|i| serde_json::json!({ "pluginId": format!("p{i}") }))
            .collect();
        let compiled = compile_named_request(
            "oversize",
            &DbAtomicParams::DispatchEventDeliveries {
                event_id: "evt".into(),
                subscribers_json: serde_json::to_string(&subs).unwrap(),
                mark_dispatched: true,
            },
            "2024-06-01T00:00:00Z",
        )
        .unwrap();
        let mut caps = DbCapabilities::advertised_sqlite();
        caps.max_statements = 40;
        let err = validate_plan(&compiled, &caps).unwrap_err();
        assert!(err.to_string().contains("maxStatements"), "{err}");
        assert!(
            compiled.request.statements.len() > 40,
            "planner must emit inserts for every subscriber, not take(24)"
        );
    }

    #[test]
    fn validate_execute_request_rejects_oversized_envelope() {
        let typed = ExecuteRequest {
            operation_id: "op".into(),
            request_hash: "abc".into(),
            statements: vec![stmt(&format!("SELECT '{}'", "x".repeat(5000)), vec![])],
            deadline_unix_ms: 0,
        };
        let mut caps = DbCapabilities::advertised_sqlite();
        caps.max_payload_bytes = 1_048_576;
        caps.max_request_bytes = 4_096;
        caps.max_atomic_result_bytes = 4_096;
        caps.max_result_bytes = 4_096;
        let err = validate_execute_request(&typed, &caps).unwrap_err();
        assert!(err.to_string().contains("maxRequestBytes"), "{err}");
        let mut typed = typed;
        typed.request_hash.clear();
        let err = super::authorize_typed_request(&mut typed, &caps).unwrap_err();
        assert!(err.to_string().contains("maxRequestBytes"), "{err}");
    }

    #[test]
    fn authorize_typed_request_rejects_unprovable_returning_max_rows() {
        use bookclerk_plugin_abi::{DbResultSelection, ExecuteRequest, TypedDbStatement};
        let caps = DbCapabilities::advertised_sqlite();
        let stmt = |sql: &str, max_rows: u32| TypedDbStatement {
            sql: sql.into(),
            parameters: vec![],
            kind: DbPlanStatementKind::Execute, // overwritten by authorize
            max_rows,
            result_selection: DbResultSelection::Rows,
        };
        let req_of = |sql: &str, max_rows: u32| ExecuteRequest {
            operation_id: "op".into(),
            request_hash: String::new(),
            statements: vec![stmt(sql, max_rows)],
            deadline_unix_ms: 0,
        };
        // Guest-asserted bound on an unbounded mutation is rejected.
        let mut update = req_of("UPDATE books SET title = 'x' RETURNING id", 1);
        let err = super::authorize_typed_request(&mut update, &caps).unwrap_err();
        assert!(err.to_string().contains("not provable"), "{err}");
        // A claim other than 1 is never provable.
        let mut many = req_of("INSERT INTO books (id) VALUES (?) RETURNING id", 2);
        let err = super::authorize_typed_request(&mut many, &caps).unwrap_err();
        assert!(err.to_string().contains("not provable"), "{err}");
        // Shape-proven single-tuple INSERT VALUES passes.
        let mut proven = req_of("INSERT INTO books (id) VALUES (?) RETURNING id", 1);
        super::authorize_typed_request(&mut proven, &caps).unwrap();
        assert_eq!(proven.statements[0].kind, DbPlanStatementKind::Returning);
        // No claim (maxRows = 0) stays allowed; bounded adapters reject it
        // themselves before execution when they need a proof.
        let mut unclaimed = req_of("UPDATE books SET title = 'x' RETURNING id", 0);
        super::authorize_typed_request(&mut unclaimed, &caps).unwrap();
    }

    #[test]
    fn authorize_typed_request_rejects_over_max_binds_and_stamps_hash() {
        use bookclerk_plugin_abi::{
            canonical_execute_request_hash, DbResultSelection, DbValue, ExecuteRequest,
            TypedDbStatement,
        };
        let mut caps = DbCapabilities::advertised_sqlite();
        caps.max_binds = 1;
        let mut req = ExecuteRequest {
            operation_id: "guest-op".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: "SELECT ?, ?".into(),
                parameters: vec![DbValue::Int64(1), DbValue::Int64(2)],
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        };
        let err = super::authorize_typed_request(&mut req, &caps).unwrap_err();
        assert!(err.to_string().contains("maxBinds"), "{err}");

        caps.max_binds = 32;
        req.request_hash.clear();
        super::authorize_typed_request(&mut req, &caps).unwrap();
        assert_eq!(req.statements[0].kind, DbPlanStatementKind::Select);
        let expected = canonical_execute_request_hash(&req).unwrap();
        assert_eq!(req.request_hash, expected);
        req.request_hash = "deadbeef".into();
        let err = super::authorize_typed_request(&mut req, &caps).unwrap_err();
        assert!(err.to_string().contains("requestHash"), "{err}");
    }

    #[test]
    fn authorize_typed_request_measures_size_after_stamping_hash() {
        use bookclerk_plugin_abi::{
            encoded_execute_request_bytes, DbResultSelection, ExecuteRequest, TypedDbStatement,
        };
        let mut req = ExecuteRequest {
            operation_id: "guest-op".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: "SELECT 1".into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Select,
                max_rows: 1,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        };
        let empty_len = encoded_execute_request_bytes(&req).unwrap().len();
        let mut stamped = req.clone();
        stamped.request_hash = "a".repeat(64);
        let stamped_len = encoded_execute_request_bytes(&stamped).unwrap().len();
        assert!(
            stamped_len > empty_len,
            "stamped hash must grow the Cap'n encoding ({stamped_len} vs {empty_len})"
        );
        let mut caps = DbCapabilities::advertised_sqlite();
        caps.max_payload_bytes = 1_048_576;
        caps.max_request_bytes = u32::try_from(empty_len + 1).unwrap();
        caps.max_atomic_result_bytes = 1_048_576;
        caps.max_result_bytes = 1_048_576;
        let err = super::authorize_typed_request(&mut req, &caps).unwrap_err();
        assert!(err.to_string().contains("maxRequestBytes"), "{err}");
    }

    #[test]
    fn canonical_hash_ignores_deadline_unix_ms() {
        use bookclerk_plugin_abi::{
            canonical_execute_request_hash, DbResultSelection, ExecuteRequest, TypedDbStatement,
        };
        let mut req = ExecuteRequest {
            operation_id: "op".into(),
            request_hash: "abc".into(),
            statements: vec![TypedDbStatement {
                sql: "SELECT 1".into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Select,
                max_rows: 1,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        };
        let a = canonical_execute_request_hash(&req).unwrap();
        req.deadline_unix_ms = 1;
        let b = canonical_execute_request_hash(&req).unwrap();
        req.deadline_unix_ms = 9_999_999_999;
        let c = canonical_execute_request_hash(&req).unwrap();
        assert_eq!(a, b);
        assert_eq!(a, c);
        req.operation_id = "other".into();
        req.request_hash = "ffff".into();
        assert_eq!(a, canonical_execute_request_hash(&req).unwrap());
    }

    #[test]
    fn validate_plan_rejects_out_of_range_selector() {
        let plan = compiled(vec![stmt("SELECT 1", vec![])], 3);
        let err = validate_plan(&plan, &DbCapabilities::advertised_sqlite()).unwrap_err();
        assert!(err.to_string().contains("outcomeIndex"), "{err}");
    }

    #[test]
    fn authorize_guest_typed_request_rejects_ddl_tables_binds_and_selection() {
        use bookclerk_plugin_abi::{DbResultSelection, DbValue, ExecuteRequest, TypedDbStatement};
        let caps = DbCapabilities::advertised_sqlite();
        let books = bookclerk_plugin_abi::GuestSqlPolicy::allow_tables(["books"]);
        let mut ddl = ExecuteRequest {
            operation_id: "g".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: "DROP TABLE books".into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Execute,
                max_rows: 0,
                result_selection: DbResultSelection::AffectedRows,
            }],
            deadline_unix_ms: 0,
        };
        let err = super::authorize_guest_typed_request(&mut ddl, &caps, &books).unwrap_err();
        assert!(err.to_string().contains("disallowed"), "{err}");

        let mut secrets = ddl.clone();
        secrets.statements[0].sql = "SELECT token FROM encrypted_secrets".into();
        secrets.statements[0].result_selection = DbResultSelection::Rows;
        let err = super::authorize_guest_typed_request(&mut secrets, &caps, &books).unwrap_err();
        assert!(err.to_string().contains("unauthorized"), "{err}");

        let mut binds = ddl.clone();
        binds.statements[0].sql = "SELECT ? FROM books WHERE id = ?".into();
        binds.statements[0].parameters = vec![DbValue::Int64(1)];
        binds.statements[0].result_selection = DbResultSelection::Rows;
        let err = super::authorize_guest_typed_request(&mut binds, &caps, &books).unwrap_err();
        assert!(err.to_string().contains("placeholder"), "{err}");

        let mut rows_on_insert = ddl.clone();
        rows_on_insert.statements[0].sql = "INSERT INTO books (id) VALUES (?)".into();
        rows_on_insert.statements[0].parameters = vec![DbValue::Int64(1)];
        rows_on_insert.statements[0].result_selection = DbResultSelection::Rows;
        rows_on_insert.statements[0].max_rows = 1;
        super::authorize_guest_typed_request(&mut rows_on_insert, &caps, &books).unwrap();

        let mut select = ddl.clone();
        select.statements[0].sql = "SELECT id FROM books".into();
        select.statements[0].kind = DbPlanStatementKind::Returning;
        select.statements[0].result_selection = DbResultSelection::Rows;
        super::authorize_guest_typed_request(&mut select, &caps, &books).unwrap();
        assert_eq!(select.statements[0].kind, DbPlanStatementKind::Select);
        assert!(!select.request_hash.is_empty());

        let mut jobs = select.clone();
        jobs.statements[0].sql = "SELECT id FROM jobs".into();
        let err = super::authorize_guest_typed_request(&mut jobs, &caps, &books).unwrap_err();
        assert!(err.to_string().contains("unauthorized table"), "{err}");
    }

    #[test]
    fn canonical_hash_deadline_golden_matches_sdks() {
        use bookclerk_plugin_abi::{
            canonical_execute_request_hash, DbResultSelection, ExecuteRequest, TypedDbStatement,
        };
        let mut req = ExecuteRequest {
            operation_id: "op".into(),
            request_hash: "abc".into(),
            statements: vec![TypedDbStatement {
                sql: "SELECT 1".into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Select,
                max_rows: 1,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        };
        const GOLDEN: &str = "648cd28b3223c825c55ea99a7c6e52321ea733656f5965abfe4c7ed4ca21d111";
        assert_eq!(canonical_execute_request_hash(&req).unwrap(), GOLDEN);
        req.deadline_unix_ms = 9_999_999_999;
        assert_eq!(canonical_execute_request_hash(&req).unwrap(), GOLDEN);
    }

    #[test]
    fn host_kind_marks_cte_insert_returning_not_select() {
        let sql = "WITH seed AS (SELECT 1) INSERT INTO t SELECT * FROM seed RETURNING id";
        assert_eq!(
            super::host_statement_kind(sql),
            DbPlanStatementKind::Returning
        );
        assert!(!DbPlanStatementKind::Returning.wrap_select_limit());
        assert_eq!(
            super::host_statement_kind("SELECT x FROM t"),
            DbPlanStatementKind::Select
        );
        assert_eq!(
            super::host_statement_kind(
                "WITH RECURSIVE t(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM t WHERE x < 3) SELECT x FROM t"
            ),
            DbPlanStatementKind::Select
        );
        assert_eq!(
            super::host_statement_kind("WITH seed AS (SELECT 1) INSERT INTO t SELECT * FROM seed"),
            DbPlanStatementKind::Execute
        );
        assert_eq!(
            super::host_statement_kind("WITH seed AS (SELECT 1) UPDATE t SET x = 1"),
            DbPlanStatementKind::Execute
        );
        assert_eq!(
            super::host_statement_kind("WITH seed AS (SELECT 1) DELETE FROM t"),
            DbPlanStatementKind::Execute
        );
        assert_eq!(
            super::host_statement_kind("WITH seed AS (SELECT 1) UPDATE t SET x = 1 RETURNING x"),
            DbPlanStatementKind::Returning
        );
        assert_eq!(
            super::host_statement_kind("WITH seed AS (SELECT 1) DELETE FROM t RETURNING id"),
            DbPlanStatementKind::Returning
        );
    }

    fn integer_column_add_sql(adds: usize) -> String {
        let mut sql = String::from("SELECT n");
        for _ in 0..adds {
            sql.push_str("+n");
        }
        sql.push_str(" FROM t");
        sql
    }

    fn integer_column_request(sql: &str) -> bookclerk_plugin_abi::ExecuteRequest {
        use bookclerk_plugin_abi::{
            DbPlanStatementKind, DbResultSelection, DbValue, ExecuteRequest, TypedDbStatement,
        };
        ExecuteRequest {
            operation_id: "col".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: sql.to_string(),
                parameters: Vec::<DbValue>::new(),
                kind: DbPlanStatementKind::Select,
                max_rows: 0,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        }
    }

    #[test]
    fn proven_execute_rejects_integer_column_overflow_without_engine_identity() {
        use bookclerk_plugin_abi::{typecheck_execute_request_proofs, SqlType, SqlTypeEnv};
        let mut caps = DbCapabilities::advertised_sqlite();
        caps.max_lowered_statement_bytes = 100_000;
        let mut env = SqlTypeEnv::new();
        env.insert_table("t", vec![("n".into(), SqlType::Integer)]);
        let proven_ok = |adds: usize| {
            let sql = integer_column_add_sql(adds);
            let req = integer_column_request(&sql);
            if super::validate_execute_request(&req, &caps).is_err() {
                return false;
            }
            let Ok(proofs) = typecheck_execute_request_proofs(&req, &env) else {
                return false;
            };
            super::validate_proven_execute_request(&req, &proofs, &caps).is_ok()
        };
        let mut lo = 0usize;
        let mut hi = 1usize;
        while hi < 800 && proven_ok(hi) {
            lo = hi;
            hi = (hi.saturating_mul(2)).min(800);
        }
        while lo + 1 < hi {
            let mid = lo + (hi - lo) / 2;
            if proven_ok(mid) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        assert!(lo >= 1, "expected an admitted INTEGER column chain");

        let sql_ok = integer_column_add_sql(lo);
        let req_ok = integer_column_request(&sql_ok);
        super::validate_execute_request(&req_ok, &caps).expect("canonical N");
        let proofs_ok = typecheck_execute_request_proofs(&req_ok, &env).expect("typecheck N");
        assert!(!proofs_ok[0].integer_arith_sites.is_empty());
        super::validate_proven_execute_request(&req_ok, &proofs_ok, &caps)
            .expect("host proven path must accept N");

        let sql_over = integer_column_add_sql(lo + 1);
        let req_over = integer_column_request(&sql_over);
        super::validate_execute_request(&req_over, &caps)
            .expect("canonical N+1 must still admit against an empty catalog");
        let proofs_over = typecheck_execute_request_proofs(&req_over, &env).expect("typecheck N+1");
        let err = super::validate_proven_execute_request(&req_over, &proofs_over, &caps)
            .expect_err("host proven path must reject N+1");
        assert!(
            err.to_string().contains("maxLoweredStatementBytes"),
            "host proven path must reject without consulting an engine: {err}"
        );
    }
}
