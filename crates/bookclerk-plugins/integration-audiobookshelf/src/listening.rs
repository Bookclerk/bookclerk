//! Sync AudioBookshelf media progress into `listening_progress`.

use bookclerk_integrations::{upsert_listening_snapshots, ListeningProgressSnapshot, Result};
use bookclerk_library::LibraryStore;
use chrono::{TimeZone, Utc};

use crate::client::AbsApiClient;

const PROVIDER: &str = "audiobookshelf";

/// Collect listening-progress snapshots from ABS without writing the library DB.
///
/// Used by both the in-process sync path and the external plugin guest RPC.
pub async fn collect_listening_snapshots(
    client: &AbsApiClient,
) -> Result<Vec<ListeningProgressSnapshot>> {
    let users = client.list_users().await?;
    let mut snapshots = Vec::new();

    for user in users {
        let detail = match client.get_user(&user.id).await {
            Ok(d) => d,
            Err(err) => {
                tracing::warn!(user_id = %user.id, error = %err, "ABS get_user failed");
                continue;
            }
        };

        for prog in detail.media_progress {
            let mut title = None;
            let mut authors = None;
            let mut asin = None;
            let mut isbn = None;

            match client.get_library_item(&prog.library_item_id).await {
                Ok(item) => {
                    if let Some(meta) = item.media.and_then(|m| m.metadata) {
                        title = meta.title;
                        authors = meta.author_name;
                        asin = meta.asin.map(|s| s.to_ascii_uppercase());
                        isbn = meta.isbn;
                    }
                }
                Err(err) => {
                    tracing::debug!(
                        item_id = %prog.library_item_id,
                        error = %err,
                        "ABS get_library_item failed"
                    );
                }
            }

            let last_listened_at = prog
                .last_update
                .and_then(|ms| Utc.timestamp_millis_opt(ms).single());

            snapshots.push(ListeningProgressSnapshot {
                external_user_id: detail.id.clone(),
                external_item_id: prog.library_item_id.clone(),
                identity_id: None,
                title,
                authors,
                asin,
                isbn,
                progress: prog.progress,
                current_time_seconds: prog.current_time,
                duration_seconds: prog.duration,
                is_finished: prog.is_finished,
                last_listened_at,
            });
        }
    }

    Ok(snapshots)
}

/// Pull ABS user media progress into the library DB.
///
/// Best-effort matches rows to `book_uuid` / `work_id` via ASIN, ISBN, or title.
/// Prefer calling this through [`bookclerk_integrations::Integration::sync_listening_progress`]
/// on the registered ABS adapter rather than from host binaries.
pub async fn sync_listening_progress(
    library: &LibraryStore,
    client: &AbsApiClient,
) -> Result<usize> {
    let snapshots = collect_listening_snapshots(client).await?;
    upsert_listening_snapshots(library, PROVIDER, &snapshots).await
}
