//! Catalog lookup on this library: the questions a branch pulls and the
//! answers it sends back (`control_plane::catalog_lookup`).
//!
//! | Frame | Answer |
//! |---|---|
//! | `catalog.questions` | `catalog.ask {works}` or `error {code: "refused"}` |
//! | `catalog.found` | `ok`, `error {code: "invalid"}` or `error {code: "refused"}` |
//!
//! The branch is this connection's authenticated node, and only questions
//! stored for it are sent or settled.

use bookclerk_library::control_plane::{pending_questions, record_answers, AnswersRecorded};
use bookclerk_library::{LibraryError, LibraryStore};
use bookclerk_node_proto::{Body, Frame, LookupAnswer};

use super::work::{error, HostContext};

/// `catalog.questions`.
pub(super) async fn questions(
    library: &LibraryStore,
    host: &HostContext,
    id: String,
) -> bookclerk_library::Result<Frame> {
    match pending_questions(library, &host.node_id).await? {
        Some(works) => Ok(Frame::new(id, Body::CatalogAsk { works })),
        None => Ok(error(id, "refused", "not_a_branch")),
    }
}

/// `catalog.found`.
pub(super) async fn found(
    library: &LibraryStore,
    host: &HostContext,
    id: String,
    answers: &[LookupAnswer],
    missing: &[String],
) -> bookclerk_library::Result<Frame> {
    match record_answers(library, &host.node_id, answers, missing).await {
        Ok(AnswersRecorded::Recorded(report)) => {
            tracing::debug!(
                node_id = %host.node_id,
                works = report.works,
                listings = report.listings,
                dropped = report.dropped,
                expired = report.expired,
                "recorded a branch's catalog answers"
            );
            Ok(Frame::new(id, Body::Accepted {}))
        }
        Ok(AnswersRecorded::Refused(reason)) => Ok(error(id, "refused", reason)),
        Err(LibraryError::Invalid(reason)) => Ok(error(id, "invalid", &reason)),
        Err(err) => Err(err),
    }
}
