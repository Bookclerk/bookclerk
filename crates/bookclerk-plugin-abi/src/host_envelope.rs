//! Host-only execute envelope types (not part of the public plugin-author ABI).
//!
//! Used by the plugin host and first-party database guests for guest-receipt
//! replay wrapping. Third-party plugin authors must not depend on these types.

use serde::{Deserialize, Serialize};

use crate::sql_proof::ResolvedStatement;
use crate::ExecuteRequest;

/// Host-only hint for adapters to persist guest replay payload before COMMIT.
///
/// Plugin authors must not set this field. The host stamps it when wrapping
/// guest `executeAtomic` batches with a durable receipt envelope.
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

/// Host-only guest receipt hint (not on the public Cap'n `ExecuteRequest`).
#[derive(Clone)]
pub struct HostExecuteEnvelope {
    /// Public execute payload.
    pub request: ExecuteRequest,
    /// Host-only finalize hint stamped by guest receipt wrap.
    pub guest_receipt: GuestReceiptPersist,
    /// One resolved proof per envelope statement, bound to that statement's SQL.
    pub proofs: Vec<ResolvedStatement>,
}

#[cfg_attr(not(feature = "host"), allow(dead_code))]
impl HostExecuteEnvelope {
    /// Builds a host-private envelope for adapter execution.
    #[must_use]
    pub fn new(request: ExecuteRequest, guest_receipt: GuestReceiptPersist) -> Self {
        Self {
            request,
            guest_receipt,
            proofs: Vec::new(),
        }
    }

    /// Stamps host-private proofs (one per statement, including receipt wrappers).
    #[must_use]
    pub fn with_proofs(mut self, proofs: Vec<ResolvedStatement>) -> Self {
        self.proofs = proofs;
        self
    }
}
