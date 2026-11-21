//! Download requests: a patron asks this library to download a book it
//! holds on one of its own store accounts but has no copy of yet, so the
//! patron's clerk can take the copy once it is there.
//!
//! The body of `POST /api/download-requests` is read by
//! [`parse_download_request`] (sans IO, the fuzzed boundary).
//! [`plan_download_request`] then picks what to fetch: the first edition
//! of the work, in edition id order, that a store account of this library
//! holds, named by one of its account holdings
//! ([`super::acquire_titles_of_editions`]). The daemon queues that as an
//! ordinary acquire on the account that holds it, counted against the
//! account's pace like any download the host starts on someone's behalf.

use std::collections::BTreeSet;

use serde::Deserialize;

use super::catalog_lookup::editions_of_works;
use super::window_books::copies_of_editions;
use super::ScanRequester;
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;
use crate::EntityKind;
use bookclerk_node_proto::is_work_ref;

/// `POST /api/download-requests` body: the work to download.
#[derive(Debug, utoipa::ToSchema)]
// Read by the schema only (no unknown fields); `DownloadRequestFields`
// enforces it.
#[serde(deny_unknown_fields)]
pub struct DownloadRequestBody {
    /// This library's work id, as its window books and search name it.
    #[schema(example = "0b8f6a52-3c1d-4e2f-9a7b-6c5d4e3f2a1b")]
    work_id: String,
}

/// [`DownloadRequestBody`]'s fields as JSON carries them; read only from a
/// JSON object ([`DownloadRequestBody`]'s `Deserialize`), never from an
/// array.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DownloadRequestFields {
    /// See [`DownloadRequestBody::work_id`].
    work_id: String,
}

impl<'de> Deserialize<'de> for DownloadRequestBody {
    /// Reads the body from a JSON object only.
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let fields: DownloadRequestFields =
            super::library_links::from_object(deserializer, "a JSON object with work_id")?;
        Ok(Self {
            work_id: fields.work_id,
        })
    }
}

/// A validated [`DownloadRequestBody`]: the work id trimmed and well
/// formed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadRequestAsk {
    /// The work id: 1 to 64 ASCII letters, digits and `-`.
    pub work_id: String,
}

impl DownloadRequestBody {
    /// Checks the body: a well-formed work id (with surrounding whitespace
    /// removed).
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Invalid`] for a malformed work id.
    pub fn validate(self) -> Result<DownloadRequestAsk> {
        let work_id = self.work_id.trim();
        if !is_work_ref(work_id) {
            return Err(LibraryError::Invalid("invalid work_id".into()));
        }
        Ok(DownloadRequestAsk {
            work_id: work_id.to_string(),
        })
    }
}

/// Parses and validates a `POST /api/download-requests` body from its raw
/// bytes, sans IO, the way the route reads it: JSON into
/// [`DownloadRequestBody`], then [`DownloadRequestBody::validate`].
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for bytes that are not the body's JSON
/// shape (unknown or repeated fields, wrong types, a missing `work_id`) or a
/// malformed work id.
pub fn parse_download_request(body: &[u8]) -> Result<DownloadRequestAsk> {
    serde_json::from_slice::<DownloadRequestBody>(body)
        .map_err(|err| LibraryError::Invalid(format!("download request body: {err}")))?
        .validate()
}

/// What a download request for a work fetches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadRequestPlan {
    /// The edition to download (`editions.id`).
    pub edition_id: String,
    /// The acquire job's title: one account holding of that edition
    /// (`holdings.id`).
    pub title: String,
}

/// Plans a download request for `work_id`.
///
/// The work's editions are read in edition id order; the first one a store
/// account of this library holds is downloaded, through the account
/// holding [`super::acquire_titles_of_editions`] picks for it. The
/// request is the library's own download, so any of its accounts may
/// serve it, as for an acquire the operator starts.
///
/// # Arguments
///
/// * `work_id` - This library's work.
///
/// # Errors
///
/// - [`LibraryError::NotFound`] when the work has no edition, or none on a
///   store account of this library.
/// - [`LibraryError::Conflict`] when the library already has a copy of an
///   edition of the work (the patron can take that one).
/// - [`LibraryError::Orm`] when a read fails.
pub async fn plan_download_request(
    store: &LibraryStore,
    work_id: &str,
) -> Result<DownloadRequestPlan> {
    let editions: Vec<String> = editions_of_works(store, &[work_id.to_string()])
        .await?
        .into_iter()
        .filter(|(_, work)| work == work_id)
        .map(|(edition, _)| edition)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if !copies_of_editions(store, &editions).await?.is_empty() {
        return Err(LibraryError::Conflict(format!(
            "this library already has a copy of work {work_id}"
        )));
    }
    for edition in editions {
        if store.held_titles_of_edition(&edition).await?.is_empty() {
            continue;
        }
        let title = super::acquire_titles_of_editions(
            store,
            ScanRequester::Operator,
            None,
            None,
            std::slice::from_ref(&edition),
        )
        .await?
        .into_iter()
        .next();
        if let Some(title) = title {
            return Ok(DownloadRequestPlan {
                edition_id: edition,
                title,
            });
        }
    }
    Err(LibraryError::not_found(
        EntityKind::Work,
        format!("{work_id} on this library's store accounts"),
    ))
}

#[cfg(test)]
#[path = "download_requests_tests.rs"]
mod tests;
