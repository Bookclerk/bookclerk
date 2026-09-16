//! Host→adapter execute envelope (canonical SQL + structured proofs).
//!
//! Lifecycle (host frontend, then adapter trust boundary):
//!
//! 1. [`UnresolvedExecuteRequest`] — source SQL (not yet Bookclerk-canonical)
//! 2. [`CanonicalExecuteRequest`] — host-desugared SQL (proofs not yet bound)
//! 3. [`AdapterExecuteRequest`] — desugared SQL + 1:1 hash-bound proofs
//!
//! [`crate::guest_sql`] / granted job execute stays untrusted
//! [`crate::ExecuteRequest`]. Adapters receive only [`AdapterExecuteRequest`]
//! and must [`AdapterExecuteRequest::require_proofs`].

use serde::{Deserialize, Serialize};

use crate::sql_proof::ResolvedStatement;
use crate::{desugar_execute_request, ExecuteRequest, IsolationReq};

/// Host-stamped hint for adapters to persist guest replay payload before COMMIT.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GuestReceiptPersist {
    /// Guest statement count inside the receipt wrap (excluding prune/select).
    pub guest_statement_len: u32,
    /// Guest `requestHash` compared on replay.
    pub guest_request_hash: String,
}

#[cfg_attr(not(feature = "host"), allow(dead_code))]
impl GuestReceiptPersist {
    /// True when the host did not stamp a guest-receipt finalize hint.
    #[must_use]
    pub fn is_absent(&self) -> bool {
        self.guest_statement_len == 0
    }
}

/// Source SQL that has not been host-canonicalized.
///
/// Construct this from planner output or SeaORM SQL, then
/// [`Self::canonicalize`] exactly once before proof generation.
#[derive(Clone, Debug)]
pub struct UnresolvedExecuteRequest {
    /// Source statements (may still need `NULLS` / `NULLIF` desugar).
    pub request: ExecuteRequest,
    /// Host-stamped finalize hint for guest replay.
    pub guest_receipt: GuestReceiptPersist,
    /// Isolation the adapter must realize.
    pub isolation: IsolationReq,
}

impl UnresolvedExecuteRequest {
    /// Wraps source SQL with default isolation and no receipt hint.
    #[must_use]
    pub fn new(request: ExecuteRequest) -> Self {
        Self {
            request,
            guest_receipt: GuestReceiptPersist::default(),
            isolation: IsolationReq::AtomicBatch,
        }
    }

    /// Host-stamped finalize hint for guest replay.
    #[must_use]
    pub fn with_guest_receipt(mut self, guest_receipt: GuestReceiptPersist) -> Self {
        self.guest_receipt = guest_receipt;
        self
    }

    /// Isolation the adapter must realize.
    #[must_use]
    pub fn with_isolation(mut self, isolation: IsolationReq) -> Self {
        self.isolation = isolation;
        self
    }

    /// Applies Bookclerk semantic desugars exactly once.
    #[must_use]
    pub fn canonicalize(mut self) -> CanonicalExecuteRequest {
        desugar_execute_request(&mut self.request);
        CanonicalExecuteRequest {
            request: self.request,
            guest_receipt: self.guest_receipt,
            isolation: self.isolation,
        }
    }
}

/// Host-desugared canonical SQL. Proofs are not yet bound.
///
/// The SQL strings here are the exact text that must cross the adapter
/// boundary and that proofs must hash.
#[derive(Clone, Debug)]
pub struct CanonicalExecuteRequest {
    /// Already-desugared canonical statements (`?` placeholders).
    pub request: ExecuteRequest,
    /// Host-stamped finalize hint for guest replay.
    pub guest_receipt: GuestReceiptPersist,
    /// Isolation the adapter must realize.
    pub isolation: IsolationReq,
}

impl CanonicalExecuteRequest {
    /// Wraps already-desugared SQL (for example after [`crate::desugar_execute_request`]).
    #[must_use]
    pub fn from_desugared(request: ExecuteRequest) -> Self {
        Self {
            request,
            guest_receipt: GuestReceiptPersist::default(),
            isolation: IsolationReq::AtomicBatch,
        }
    }

    /// Host-stamped finalize hint for guest replay.
    #[must_use]
    pub fn with_guest_receipt(mut self, guest_receipt: GuestReceiptPersist) -> Self {
        self.guest_receipt = guest_receipt;
        self
    }

    /// Isolation the adapter must realize.
    #[must_use]
    pub fn with_isolation(mut self, isolation: IsolationReq) -> Self {
        self.isolation = isolation;
        self
    }

    /// Binds 1:1 proofs to the canonical SQL. Fail closed on mismatch.
    ///
    /// # Errors
    ///
    /// Returns when the proof count differs or a proof is not bound to its SQL.
    pub fn bind_proofs(
        self,
        proofs: Vec<ResolvedStatement>,
    ) -> crate::Result<AdapterExecuteRequest> {
        let req = AdapterExecuteRequest {
            request: self.request,
            guest_receipt: self.guest_receipt,
            proofs,
            isolation: self.isolation,
        };
        req.require_proofs()?;
        Ok(req)
    }
}

/// Adapter execute payload: canonical SQL, structured proofs, optional receipt.
///
/// This is the trust boundary. Transport and adapter execution must call
/// [`Self::require_proofs`] and must not regenerate, repair, or substitute
/// proofs.
#[derive(Clone, Debug)]
pub struct AdapterExecuteRequest {
    /// Canonical execute payload (no proofs).
    pub request: ExecuteRequest,
    /// Host-stamped finalize hint for guest replay.
    pub guest_receipt: GuestReceiptPersist,
    /// One resolved proof per statement, bound to that statement's SQL.
    pub proofs: Vec<crate::sql_proof::ResolvedStatement>,
    /// Isolation the adapter must realize.
    pub isolation: IsolationReq,
}

#[cfg_attr(not(feature = "host"), allow(dead_code))]
impl AdapterExecuteRequest {
    /// Builds an adapter execute request without proofs.
    ///
    /// Prefer [`UnresolvedExecuteRequest::canonicalize`] plus
    /// [`CanonicalExecuteRequest::bind_proofs`]. This constructor exists for
    /// staged host assembly; [`Self::require_proofs`] must still succeed before
    /// the request crosses the adapter boundary.
    #[must_use]
    pub fn new(request: ExecuteRequest, guest_receipt: GuestReceiptPersist) -> Self {
        Self {
            request,
            guest_receipt,
            proofs: Vec::new(),
            isolation: IsolationReq::AtomicBatch,
        }
    }

    /// Stamps 1:1 proofs (including receipt wrappers).
    #[must_use]
    pub fn with_proofs(mut self, proofs: Vec<ResolvedStatement>) -> Self {
        self.proofs = proofs;
        self
    }

    /// Sets isolation (default [`IsolationReq::AtomicBatch`]).
    #[must_use]
    pub fn with_isolation(mut self, isolation: IsolationReq) -> Self {
        self.isolation = isolation;
        self
    }

    /// Fail closed unless proofs are 1:1 and hash-bound to each statement.
    ///
    /// # Errors
    ///
    /// Returns when the proof count differs or a proof is not bound to its SQL.
    pub fn require_proofs(&self) -> crate::Result<()> {
        if self.proofs.len() != self.request.statements.len() {
            return Err(crate::PluginError::internal(format!(
                "adapter execute proofs must match statement count ({} proofs, {} statements)",
                self.proofs.len(),
                self.request.statements.len()
            )));
        }
        for (stmt, proof) in self.request.statements.iter().zip(self.proofs.iter()) {
            if proof.statement_hash != crate::statement_sql_hash(stmt.sql.trim()) {
                return Err(crate::PluginError::internal(
                    "resolved SQL proof is not bound to this canonical statement",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use crate::{DbPlanStatementKind, DbResultSelection, TypedDbStatement};

    fn select_req(sql: &str) -> ExecuteRequest {
        ExecuteRequest {
            operation_id: "op".into(),
            request_hash: String::new(),
            statements: vec![TypedDbStatement {
                sql: sql.into(),
                parameters: vec![],
                kind: DbPlanStatementKind::Select,
                max_rows: 1,
                result_selection: DbResultSelection::Rows,
            }],
            deadline_unix_ms: 0,
        }
    }

    #[test]
    fn require_proofs_rejects_missing_proofs() {
        let env =
            AdapterExecuteRequest::new(select_req("SELECT 1"), GuestReceiptPersist::default());
        assert!(env.require_proofs().is_err());
    }

    #[test]
    fn require_proofs_rejects_hash_mismatch() {
        let proof = crate::sql_proof::ResolvedStatement::bound_empty("SELECT 2");
        let err =
            AdapterExecuteRequest::new(select_req("SELECT 1"), GuestReceiptPersist::default())
                .with_proofs(vec![proof])
                .require_proofs()
                .expect_err("mismatched SQL must fail closed");
        assert!(err.to_string().contains("not bound"), "{err}");
    }

    #[test]
    fn require_proofs_accepts_hash_bound_proof() {
        let sql = "SELECT 1";
        let proof = crate::sql_proof::ResolvedStatement::bound_empty(sql);
        AdapterExecuteRequest::new(select_req(sql), GuestReceiptPersist::default())
            .with_proofs(vec![proof])
            .require_proofs()
            .expect("hash-bound proof");
    }

    #[test]
    fn like_stays_like_across_host_canonicalize() {
        let canonical = UnresolvedExecuteRequest::new(select_req(
            "SELECT title FROM books WHERE title LIKE ? ORDER BY title",
        ))
        .canonicalize();
        let sql = &canonical.request.statements[0].sql;
        assert!(sql.contains("LIKE"), "{sql}");
        assert!(!sql.contains("GLOB"), "{sql}");
        assert!(sql.contains('?'), "{sql}");
        assert!(!sql.contains("$1"), "{sql}");
        assert!(sql.contains("NULLS FIRST"), "{sql}");
        let proof = crate::sql_proof::ResolvedStatement::bound_empty(sql);
        canonical
            .bind_proofs(vec![proof])
            .expect("bound to canonical LIKE SQL");
    }

    #[test]
    fn canonicalize_desugars_before_proof_bind() {
        let canonical = UnresolvedExecuteRequest::new(select_req("SELECT 1 / n FROM t ORDER BY n"))
            .canonicalize();
        let sql = &canonical.request.statements[0].sql;
        assert!(sql.contains("NULLIF(n, 0)"), "{sql}");
        assert!(sql.contains("NULLS FIRST"), "{sql}");
        let proof = crate::sql_proof::ResolvedStatement::bound_empty(sql);
        canonical
            .bind_proofs(vec![proof])
            .expect("bound to desugared SQL");
    }

    #[test]
    fn bind_proofs_rejects_hash_of_source_sql() {
        let source = "SELECT 1 / n FROM t ORDER BY n";
        let canonical = UnresolvedExecuteRequest::new(select_req(source)).canonicalize();
        let proof = crate::sql_proof::ResolvedStatement::bound_empty(source);
        let err = canonical
            .bind_proofs(vec![proof])
            .expect_err("proof of pre-desugar SQL must fail");
        assert!(err.to_string().contains("not bound"), "{err}");
    }
}
