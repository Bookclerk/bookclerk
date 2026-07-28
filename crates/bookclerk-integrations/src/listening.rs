//! Shared helpers for integration listening / progress sync.

use bookclerk_library::{BookRecord, LibraryStore, NewListeningProgress};

use crate::error::Result;
use crate::types::ListeningProgressSnapshot;

/// Upsert provider-agnostic listening snapshots into `listening_progress`.
///
/// `provider` should be the integration id (`audiobookshelf`, plugin id, …).
pub fn upsert_listening_snapshots(
    library: &LibraryStore,
    provider: &str,
    items: &[ListeningProgressSnapshot],
) -> Result<usize> {
    // Load once per sync — matching is O(items × books) over in-memory rows,
    // not O(items) full-table DB reads.
    let books = library.list_books(None)?;
    let mut upserted = 0usize;
    for item in items {
        let identity_id = library
            .get_portal_identity(provider, &item.external_user_id)
            .ok()
            .flatten()
            .map(|i| i.id);

        let book_uuid = match_book_uuid_in(
            &books,
            item.asin.as_deref(),
            item.isbn.as_deref(),
            item.title.as_deref(),
        );
        let work_id = if let Some(ref uuid) = book_uuid {
            library.work_id_for_book(uuid).ok().flatten()
        } else {
            None
        };

        library.upsert_listening_progress(&NewListeningProgress {
            identity_id: item.identity_id.or(identity_id),
            provider: provider.into(),
            external_user_id: item.external_user_id.clone(),
            book_uuid,
            work_id,
            external_item_id: item.external_item_id.clone(),
            title: item.title.clone(),
            authors: item.authors.clone(),
            asin: item.asin.clone(),
            isbn: item.isbn.clone(),
            progress: item.progress,
            current_time_seconds: item.current_time_seconds,
            duration_seconds: item.duration_seconds,
            is_finished: item.is_finished,
            last_listened_at: item.last_listened_at,
        })?;
        upserted += 1;
    }
    Ok(upserted)
}

/// Best-effort match to an owned library book via ASIN, ISBN, or exact title.
pub fn match_book_uuid(
    library: &LibraryStore,
    asin: Option<&str>,
    isbn: Option<&str>,
    title: Option<&str>,
) -> Result<Option<String>> {
    let books = library.list_books(None)?;
    Ok(match_book_uuid_in(&books, asin, isbn, title))
}

fn match_book_uuid_in(
    books: &[BookRecord],
    asin: Option<&str>,
    isbn: Option<&str>,
    title: Option<&str>,
) -> Option<String> {
    if let Some(asin) = asin {
        if let Some(b) = books.iter().find(|b| {
            b.asin
                .as_deref()
                .map(|a| a.eq_ignore_ascii_case(asin))
                .unwrap_or(false)
        }) {
            return Some(b.uuid.clone());
        }
    }
    if let Some(isbn) = isbn {
        if let Some(b) = books.iter().find(|b| b.isbn.as_deref() == Some(isbn)) {
            return Some(b.uuid.clone());
        }
    }
    if let Some(title) = title {
        let title_l = title.to_lowercase();
        if let Some(b) = books.iter().find(|b| b.title.to_lowercase() == title_l) {
            return Some(b.uuid.clone());
        }
    }
    None
}
