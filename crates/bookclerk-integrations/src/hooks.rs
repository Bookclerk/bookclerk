//! Helpers to fan-out acquire success to integrations.

use bookclerk_library::LibraryStore;
use tracing::debug;

use crate::registry::IntegrationRegistry;
use crate::types::IntegrationEvent;

/// Look up the acquired book and emit [`IntegrationEvent::BookAcquired`].
///
/// # Arguments
///
/// * `registry` - Configured content-source or integration registry.
/// * `library` - Open library store used for reads/writes.
/// * `product_or_uuid` - String `product_or_uuid` for this call.
/// * `storage_key` - String `storage_key` for this call.
pub async fn emit_book_acquired(
    registry: &IntegrationRegistry,
    library: &LibraryStore,
    product_or_uuid: &str,
    storage_key: &str,
) {
    if registry.is_empty() {
        return;
    }
    let mut book = library
        .get_book_by_uuid(product_or_uuid)
        .await
        .ok()
        .flatten();
    if book.is_none() {
        book = library
            .list_books(None)
            .await
            .ok()
            .into_iter()
            .flatten()
            .find(|b| {
                b.uuid == product_or_uuid
                    || b.product_id == product_or_uuid
                    || b.asin.as_deref() == Some(product_or_uuid)
                    || b.storage_key.as_deref() == Some(storage_key)
            });
    }

    let Some(book) = book else {
        debug!(id = %product_or_uuid, "no book row for acquire event");
        return;
    };

    registry
        .emit(&IntegrationEvent::BookAcquired {
            book: Box::new(book),
            storage_key: storage_key.to_string(),
            absolute_path: None,
        })
        .await;
}
