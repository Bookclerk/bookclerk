//! Helpers to persist acquire success as a durable `book_acquired` outbox event.

use bookclerk_library::{LibraryStore, PublishDomainEventOutcome};

/// Look up the acquired book and publish `book_acquired` to the durable outbox.
///
/// The book is found by uuid, then by store product id or ASIN, then by its
/// primary storage key, each with an indexed or single-row read.
///
/// Prefer [`bookclerk_library::LibraryStore::set_acquire_status`], which publishes
/// in the same transaction as the acquire state change. This helper is a
/// fail-closed catch-up for callers that already committed the book row.
///
/// # Errors
///
/// Returns when the library lookup or outbox insert fails, including a missing
/// book row (so the caller cannot report success without an outbox record).
pub async fn emit_book_acquired(
    library: &LibraryStore,
    product_or_uuid: &str,
    storage_key: &str,
) -> bookclerk_library::Result<PublishDomainEventOutcome> {
    let mut book = library.get_book_by_uuid(product_or_uuid).await?;
    if book.is_none() {
        // Indexed lookups, not a whole-library scan: product id or ASIN,
        // else the stored primary file.
        book = library
            .find_book_by_store_id_or_key(product_or_uuid, storage_key)
            .await?;
    }

    let Some(book) = book else {
        return Err(bookclerk_library::LibraryError::not_found(
            bookclerk_library::EntityKind::Book,
            product_or_uuid,
        ));
    };

    let edition = library.edition_of_book(&book.uuid).await?;
    let payload = serde_json::json!({
        "titleId": book.uuid,
        "editionId": edition,
        "source": book.source,
        "asin": book.asin,
        "isbn": book.isbn,
        "pathKeys": [storage_key],
        "accountId": book.account_id,
    });
    let payload = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into());
    library
        .publish_domain_event(bookclerk_library::PublishDomainEventSpec {
            id: String::new(),
            event_type: "book_acquired".into(),
            schema_version: 1,
            account_id: book.account_id.clone(),
            source: book.source.clone(),
            correlation_id: book.uuid.clone(),
            causation_id: String::new(),
            dedup_key: format!("book_acquired:{}", book.uuid),
            payload,
            ordering_key: book.uuid.clone(),
            ..bookclerk_library::PublishDomainEventSpec::default()
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_library::{AcquireStatus, LibraryError, NewBook};

    /// An in-memory library with one acquired book (`P-1`, ASIN `B0AAA`,
    /// file `x/p1.m4b`) and returns its uuid.
    async fn library_with_book() -> (LibraryStore, String) {
        let library = LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .expect("memory store"),
        );
        library
            .upsert_account("audible", "a1", "us", None, true, "audible")
            .await
            .expect("account");
        let mut book = NewBook::minimal("P-1", "a1", "us", "One");
        book.asin = Some("B0AAA".into());
        let book = library.upsert_book(&book).await.expect("book");
        library
            .set_acquire_status("P-1", "a1", AcquireStatus::Acquired, Some("x/p1.m4b"), None)
            .await
            .expect("acquired");
        (library, book.uuid)
    }

    /// The book is found by uuid, product id, ASIN or stored file, and an
    /// unknown book fails closed.
    #[tokio::test]
    async fn the_event_finds_its_book_by_any_id_or_file() {
        let (library, uuid) = library_with_book().await;
        for id in [uuid.as_str(), "P-1", "B0AAA"] {
            emit_book_acquired(&library, id, "other.m4b")
                .await
                .unwrap_or_else(|err| panic!("{id}: {err}"));
        }
        emit_book_acquired(&library, "unknown", "x/p1.m4b")
            .await
            .expect("found by its file");
        let missing = emit_book_acquired(&library, "unknown", "y/none.m4b").await;
        assert!(
            matches!(missing, Err(LibraryError::NotFound(_))),
            "{missing:?}"
        );
    }
}
