//! Telling a read that exceeded a database plugin's result caps from one
//! that failed for another reason, so pagers can retry it in smaller pieces.

use crate::error::LibraryError;

/// True when `err` is a result cap a smaller read can satisfy, not a lock.
///
/// The inline caps (`maxResultBytes`, `maxAtomicResultBytes`) match for an
/// adapter that only answers inline; `maxStreamedResultBytes` matches so a
/// page whose rows are wide enough to pass even the streamed budget is read
/// in smaller pieces instead of failing. The row caps (`maxResultRows`,
/// `maxStreamedResultRows`) match for the same reason: an adapter that
/// advertises a row budget below a page still meets the host minimums, and
/// the page it rejects whole is read in pieces that fit its budget.
pub(crate) fn is_result_too_large(err: &LibraryError) -> bool {
    err.limit().is_some_and(|limit| {
        matches!(
            limit.limit.as_str(),
            "maxResultBytes"
                | "maxAtomicResultBytes"
                | "maxStreamedResultBytes"
                | "maxResultRows"
                | "maxStreamedResultRows"
        )
    })
}
