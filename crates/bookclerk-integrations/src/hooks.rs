//! Helpers to persist acquire success as a durable `book_acquired` outbox event.

use bookclerk_library::{LibraryStore, PublishDomainEventOutcome};

/// Look up the acquired book and publish `book_acquired` to the durable outbox.
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
        book = library.list_books(None).await?.into_iter().find(|b| {
            b.uuid == product_or_uuid
                || b.product_id == product_or_uuid
                || b.asin.as_deref() == Some(product_or_uuid)
                || b.storage_key.as_deref() == Some(storage_key)
        });
    }

    let Some(book) = book else {
        return Err(bookclerk_library::LibraryError::NotFound(format!(
            "no book row for acquire event `{product_or_uuid}`"
        )));
    };

    let payload = serde_json::json!({
        "titleId": book.uuid,
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
        })
        .await
}
