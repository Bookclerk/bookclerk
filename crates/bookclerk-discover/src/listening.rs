//! Listening rows as Discover's taste reads them.
//!
//! A stored listening row names only its work (and edition). Taste wants
//! more: the authors to like, and the library title the listen is for (its
//! series, its household weight). [`ListeningSignal::load`] reads both for
//! every row at once, from the work's credits and from the titles whose
//! listings reach the work, so nothing bibliographic is copied onto the row.

use std::collections::BTreeSet;

use bookclerk_library::{LibraryStore, ListeningProgressRecord};
use chrono::{DateTime, Utc};

use crate::error::Result;

/// One listening row with what Discover reads through its work.
#[derive(Debug, Clone)]
pub struct ListeningSignal {
    /// Portal identity the row is stored under, when any.
    pub identity_id: Option<i64>,
    /// Integration that reported the row.
    pub provider: String,
    /// User id at that integration.
    pub external_user_id: String,
    /// The work the row is about.
    pub work_id: Option<String>,
    /// A library title of that work (lowest uuid), when the library holds
    /// one.
    pub book_uuid: Option<String>,
    /// The work's credited names, comma-separated, when it has any.
    pub authors: Option<String>,
    /// Fraction complete in `0.0..=1.0`, when reported.
    pub progress: Option<f64>,
    /// Playback position in seconds, when reported.
    pub current_time_seconds: Option<f64>,
    /// Item length in seconds, when reported.
    pub duration_seconds: Option<f64>,
    /// Whether the listener finished it.
    pub is_finished: bool,
    /// Last playback update, when reported.
    pub last_listened_at: Option<DateTime<Utc>>,
}

impl ListeningSignal {
    /// Reads the works and library titles of `rows` in a few bulk lookups.
    ///
    /// # Arguments
    ///
    /// * `library` - The library the rows came from.
    /// * `rows` - Stored listening rows.
    ///
    /// # Returns
    ///
    /// One signal per row, in the same order.
    ///
    /// # Errors
    ///
    /// Returns the library read error.
    pub async fn load(
        library: &LibraryStore,
        rows: Vec<ListeningProgressRecord>,
    ) -> Result<Vec<Self>> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let work_ids: Vec<String> = rows
            .iter()
            .map(|r| r.work_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let works = library.works_by_ids(&work_ids).await?;
        let books = library.book_uuids_for_works(&work_ids).await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let work = works.get(&row.work_id);
                Self {
                    identity_id: row.identity_id,
                    provider: row.provider,
                    external_user_id: row.external_user_id,
                    book_uuid: books.get(&row.work_id).and_then(|b| b.first().cloned()),
                    authors: work
                        .filter(|w| !w.credits.is_empty())
                        .map(|w| w.credits.join(", ")),
                    work_id: Some(row.work_id),
                    progress: row.progress,
                    current_time_seconds: row.current_time_seconds,
                    duration_seconds: row.duration_seconds,
                    is_finished: row.is_finished,
                    last_listened_at: row.last_listened_at,
                }
            })
            .collect())
    }
}
