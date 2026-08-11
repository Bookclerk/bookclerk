//! Rebuild canonical works from ownership rows.

use bookclerk_enrich::canonicalize_isbn;
use bookclerk_library::{LibraryStore, NewWork};

use crate::error::Result;

/// Link every book into a work keyed by ASIN and/or ISBN, else book uuid.
///
/// When a book has both identifiers, prefer an existing work that already
/// carries either alias so ISBN-only and ASIN-only rows consolidate.
///
/// # Arguments
///
/// * `library` - Open library store used for reads/writes.
///
/// # Returns
///
/// On success, the inner `usize` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn rebuild_works_from_library(library: &LibraryStore) -> Result<usize> {
    let books = library.list_books(None).await?;
    let mut linked = 0usize;

    for book in books {
        let asin = book
            .asin
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_ascii_uppercase);
        let isbn = book
            .isbn
            .as_deref()
            .map(canonicalize_isbn)
            .filter(|s| !s.is_empty());

        let existing = match (&asin, &isbn) {
            (Some(a), Some(i)) => match library.find_work_by_asin(a).await? {
                Some(w) => Some(w),
                None => library.find_work_by_isbn(i).await?,
            },
            (Some(a), None) => library.find_work_by_asin(a).await?,
            (None, Some(i)) => library.find_work_by_isbn(i).await?,
            (None, None) => match library.work_id_for_book(&book.uuid).await? {
                Some(id) => library.get_work(&id).await.ok().flatten(),
                None => None,
            },
        };

        let work_id = if let Some(existing) = existing {
            let updated = NewWork {
                id: Some(existing.id.clone()),
                canonical_asin: asin.or(existing.canonical_asin),
                canonical_isbn: isbn.or(existing.canonical_isbn),
                title: book.title.clone(),
                authors: book.authors.clone().or(existing.authors),
                narrators: book.narrators.clone().or(existing.narrators),
                description: book.description.clone().or(existing.description),
                subjects: book.subjects.clone().or(existing.subjects),
                categories: book.categories.clone().or(existing.categories),
                language: book.language.clone().or(existing.language),
                series: book.series.clone().or(existing.series),
                series_index: book.series_index.clone().or(existing.series_index),
                cover_url: book.cover_url.clone().or(existing.cover_url),
                openlibrary_id: existing.openlibrary_id,
            };
            library.upsert_work(&updated).await?.id
        } else {
            let work = NewWork {
                id: None,
                canonical_asin: asin,
                canonical_isbn: isbn,
                title: book.title.clone(),
                authors: book.authors.clone(),
                narrators: book.narrators.clone(),
                description: book.description.clone(),
                subjects: book.subjects.clone(),
                categories: book.categories.clone(),
                language: book.language.clone(),
                series: book.series.clone(),
                series_index: book.series_index.clone(),
                cover_url: book.cover_url.clone(),
                openlibrary_id: None,
            };
            library.upsert_work(&work).await?.id
        };

        library.link_book_to_work(&work_id, &book.uuid).await?;
        linked += 1;
    }

    Ok(linked)
}
