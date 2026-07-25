//! Helpers to fan-out acquire success to integrations.

use bookclerk_library::LibraryStore;
use tracing::debug;

use crate::registry::IntegrationRegistry;
use crate::types::IntegrationEvent;

/// Look up the acquired book and emit [`IntegrationEvent::BookAcquired`].
pub async fn emit_book_acquired(
    registry: &IntegrationRegistry,
    library: &LibraryStore,
    product_or_uuid: &str,
    storage_key: &str,
) {
    if registry.is_empty() {
        return;
    }
    let book = library
        .get_book_by_uuid(product_or_uuid)
        .ok()
        .flatten()
        .or_else(|| {
            library
                .list_books(None)
                .ok()
                .into_iter()
                .flatten()
                .find(|b| {
                    b.uuid == product_or_uuid
                        || b.product_id == product_or_uuid
                        || b.asin.as_deref() == Some(product_or_uuid)
                        || b.storage_key.as_deref() == Some(storage_key)
                })
        });

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
