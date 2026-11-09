//! Splits a stream of host write statements into atomic batches that fit the
//! database plugin's advertised limits.
//!
//! Host writes go to the database plugin as atomic batches, never as an
//! interactive transaction (`docs/plugins/database-limits.md`). A write that
//! touches many rows (a Discover candidate refresh, a listening sync) is cut
//! into as few batches as the plugin's `maxStatements` and `maxRequestBytes`
//! allow, so it costs a handful of round trips instead of one per row.

use bookclerk_plugin_abi::{DbCapabilities, ExecuteRequest, TypedDbStatement};

use crate::error::{LibraryError, Result};

/// Share of the plugin's `maxRequestBytes` a batch of several statements
/// fills, in eighths. The batch size is summed from each statement's own
/// encoding, so the margin covers the framing that summing leaves out.
const BATCH_BYTES_EIGHTHS: usize = 7;

/// Bytes of the plugin's `maxRequestBytes` a request holding one statement
/// leaves free. That request is sized whole, so only the request hash and
/// deadline, which are empty while sizing, still need room.
const SINGLE_STATEMENT_HEADROOM: usize = 256;

/// Splits write statements into atomic batches the database plugin accepts.
///
/// A batch closes before it would pass the plugin's `maxStatements`, or
/// [`BATCH_BYTES_EIGHTHS`] of its `maxRequestBytes`. A statement too large to
/// share a batch still goes alone while it fits the plugin's limit less
/// [`SINGLE_STATEMENT_HEADROOM`].
pub(crate) struct WriteBatches {
    /// Statements waiting for the next batch.
    pending: Vec<TypedDbStatement>,
    /// Summed encoded bytes of `pending` plus an empty request's framing.
    pending_bytes: usize,
    /// Encoded bytes of a request with no statements.
    empty_bytes: usize,
    /// Most statements one batch may hold.
    max_statements: usize,
    /// Most encoded bytes one batch of several statements may hold.
    max_bytes: usize,
    /// Most encoded bytes a request holding one statement may hold.
    max_single_bytes: usize,
    /// What the statements write, named in the error for an oversized one
    /// (for example `discover candidate`).
    what: &'static str,
}

impl WriteBatches {
    /// Starts an empty batch sized to the plugin's advertised limits.
    ///
    /// # Arguments
    ///
    /// * `caps` - The database plugin's advertised limits.
    /// * `what` - What the statements write, for the oversized-row error.
    pub(crate) fn new(caps: &DbCapabilities, what: &'static str) -> Self {
        let empty_bytes = encoded_len(&[]).unwrap_or(0);
        let request_cap = usize::try_from(caps.max_request_bytes).unwrap_or(usize::MAX);
        Self {
            pending: Vec::new(),
            pending_bytes: empty_bytes,
            empty_bytes,
            max_statements: usize::try_from(caps.max_statements)
                .unwrap_or(usize::MAX)
                .max(1),
            max_bytes: request_cap / 8 * BATCH_BYTES_EIGHTHS,
            max_single_bytes: request_cap.saturating_sub(SINGLE_STATEMENT_HEADROOM),
            what,
        }
    }

    /// Adds one statement.
    ///
    /// # Returns
    ///
    /// The batch that had to close to make room, when one did.
    ///
    /// # Errors
    ///
    /// Returns an error when the statement alone is larger than one request
    /// may carry, or cannot be encoded.
    pub(crate) fn push(&mut self, stmt: TypedDbStatement) -> Result<Option<Vec<TypedDbStatement>>> {
        let size = encoded_len(std::slice::from_ref(&stmt))?.saturating_sub(self.empty_bytes);
        if self.empty_bytes.saturating_add(size) > self.max_single_bytes {
            return Err(LibraryError::Other(anyhow::anyhow!(
                "a {} write is {size} bytes, over the database plugin's request limit",
                self.what
            )));
        }
        let full = self.pending.len() >= self.max_statements
            || self.pending_bytes.saturating_add(size) > self.max_bytes;
        let closed = if full && !self.pending.is_empty() {
            self.pending_bytes = self.empty_bytes;
            Some(std::mem::take(&mut self.pending))
        } else {
            None
        };
        self.pending_bytes = self.pending_bytes.saturating_add(size);
        self.pending.push(stmt);
        Ok(closed)
    }

    /// The last, partly filled batch, when anything is left.
    pub(crate) fn finish(self) -> Option<Vec<TypedDbStatement>> {
        (!self.pending.is_empty()).then_some(self.pending)
    }
}

/// Operation-id prefix used while sizing. It is at least as long as every
/// real batch prefix (`discover-candidates-`, `listening-progress-`), so a
/// sized request is never smaller than the one sent.
const SIZING_PREFIX: &str = "host-write-batch-sizing-";

/// Encoded bytes of a request carrying `statements`.
///
/// # Errors
///
/// Returns an error when the statements cannot be encoded (a NUL in the SQL
/// or a text bind).
fn encoded_len(statements: &[TypedDbStatement]) -> Result<usize> {
    let req = ExecuteRequest {
        operation_id: format!("{SIZING_PREFIX}{}", uuid::Uuid::nil()),
        request_hash: String::new(),
        statements: statements.to_vec(),
        deadline_unix_ms: 0,
    };
    bookclerk_plugin_abi::encoded_execute_request_bytes(&req)
        .map(|bytes| bytes.len())
        .map_err(|err| LibraryError::Other(anyhow::anyhow!(err.to_string())))
}
