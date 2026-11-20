//! `intake_items`: what the host knows about each item in a drop folder
//! (`docs/server/drop-folders.md`).
//!
//! The intake runner lists a drop folder, groups the listing into items
//! with `bookclerk_intake::ItemGrouper`, and compares each with its stored
//! record through `bookclerk_intake::step`. This module loads those records
//! and writes the result: a new or changed item, an item handed to an
//! ingest batch, a finished ingest, or an item that is gone.
//!
//! Rows are keyed by the SHA-256 of the item's key, so a long file name
//! never makes a long primary key. Every write goes through the canonical
//! SQL-v1 contract in batches sized to the database plugin.

use std::collections::BTreeMap;

use bookclerk_intake::{ItemSignature, TrackedItem, TrackedState};
use chrono::Utc;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use sha2::{Digest, Sha256};

use super::batch::{exec, int, rows_affected, text, text_opt};
use crate::entities::intake_items;
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// Most rows one read page asks for when the plugin sets no smaller cap.
const ITEM_PAGE: u64 = 500;

/// Longest stored failure reason, in bytes; longer text is cut at a
/// character boundary.
pub const MAX_INTAKE_ERROR_BYTES: usize = 512;

/// One item's stored record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntakeItemRecord {
    /// The item's path relative to the folder root.
    pub item_key: String,
    /// State, signature and quiet time.
    pub tracked: TrackedItem,
    /// The ingest batch the item was handed to, once queued.
    pub batch_id: Option<String>,
    /// Why the last ingest failed.
    pub error: Option<String>,
}

/// One change to a drop folder's records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntakeWrite {
    /// Store a new or changed record; clears its batch and error.
    Store {
        /// The item's key.
        item_key: String,
        /// The record to store.
        tracked: TrackedItem,
    },
    /// The item settled and was handed to `batch_id`: store it `queued`.
    Queue {
        /// The item's key.
        item_key: String,
        /// The signature that settled.
        signature: ItemSignature,
        /// The ingest batch it went to.
        batch_id: String,
    },
    /// The item is gone from the folder: delete its record.
    Forget {
        /// The item's key.
        item_key: String,
    },
}

/// How an ingest batch ended for one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntakeFinish {
    /// Ingested and left in place (`after_ingest = leave`).
    Done,
    /// Ingested and moved or deleted: the record is dropped.
    Removed,
    /// The ingest failed for this reason.
    Failed(String),
}

/// The lowercase hex SHA-256 of an item key: its row's key.
#[must_use]
pub fn intake_item_hash(item_key: &str) -> String {
    hex::encode(Sha256::digest(item_key.as_bytes()))
}

/// Every stored record of the drop folder `instance_id`, by item key.
/// Read in keyset pages no larger than the plugin's `maxResultRows`.
///
/// # Errors
///
/// Returns an error when a read fails or a row holds a state or signature
/// this version does not know.
pub async fn load_intake_items(
    store: &LibraryStore,
    instance_id: &str,
) -> Result<BTreeMap<String, IntakeItemRecord>> {
    let page = match store.db_capabilities().max_result_rows {
        0 => ITEM_PAGE,
        rows => u64::from(rows).clamp(1, ITEM_PAGE),
    };
    let mut out = BTreeMap::new();
    let mut after: Option<String> = None;
    loop {
        let mut query = intake_items::Entity::find()
            .filter(intake_items::Column::PluginInstanceId.eq(instance_id))
            .order_by_asc(intake_items::Column::ItemHash);
        if let Some(after) = &after {
            query = query.filter(intake_items::Column::ItemHash.gt(after.as_str()));
        }
        let rows = query
            .limit(page)
            .all(store.db())
            .await
            .map_err(LibraryError::Orm)?;
        let full = u64::try_from(rows.len()).unwrap_or(u64::MAX) >= page;
        after = rows.last().map(|row| row.item_hash.clone());
        for row in rows {
            let record = record_from_row(row)?;
            out.insert(record.item_key.clone(), record);
        }
        if !full {
            return Ok(out);
        }
    }
}

/// Applies `writes` to the drop folder `instance_id`'s records.
///
/// Each write is its own pair of statements, and the pairs are committed in
/// batches sized to the database plugin; a crash between batches leaves
/// earlier writes in place, which the next listing reconciles.
///
/// # Errors
///
/// Returns an error when a batch fails.
pub async fn apply_intake_writes(
    store: &LibraryStore,
    instance_id: &str,
    writes: &[IntakeWrite],
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    let mut statements = Vec::with_capacity(writes.len() * 2);
    for write in writes {
        match write {
            IntakeWrite::Store { item_key, tracked } => {
                statements.extend(upsert(
                    instance_id,
                    item_key,
                    tracked.state,
                    tracked.signature,
                    tracked.quiet_since_ms,
                    None,
                    &now,
                ));
            }
            IntakeWrite::Queue {
                item_key,
                signature,
                batch_id,
            } => {
                statements.push(exec(
                    "UPDATE intake_items
                        SET state = 'queued', batch_id = ?, error = NULL, updated_at = ?
                      WHERE plugin_instance_id = ? AND item_hash = ? AND signature = ?",
                    vec![
                        text(batch_id),
                        text(&now),
                        text(instance_id),
                        text(&intake_item_hash(item_key)),
                        text(&signature.to_hex()),
                    ],
                ));
            }
            IntakeWrite::Forget { item_key } => statements.push(exec(
                "DELETE FROM intake_items WHERE plugin_instance_id = ? AND item_hash = ?",
                vec![text(instance_id), text(&intake_item_hash(item_key))],
            )),
        }
    }
    super::catalog_lookup::run_batches(store, "drop folder item", statements).await?;
    Ok(())
}

/// Records how the ingest batch `batch_id` ended for one item, if the item
/// is still queued on that batch.
///
/// # Returns
///
/// True when the record was updated; false when the item is no longer
/// queued on `batch_id` (a later listing replaced it).
///
/// # Errors
///
/// Returns an error when the write fails.
pub async fn finish_intake_item(
    store: &LibraryStore,
    instance_id: &str,
    item_key: &str,
    batch_id: &str,
    finish: &IntakeFinish,
) -> Result<bool> {
    let now = Utc::now().to_rfc3339();
    let hash = intake_item_hash(item_key);
    let statement = match finish {
        IntakeFinish::Removed => exec(
            "DELETE FROM intake_items
              WHERE plugin_instance_id = ? AND item_hash = ? AND state = 'queued' AND batch_id = ?",
            vec![text(instance_id), text(&hash), text(batch_id)],
        ),
        IntakeFinish::Done | IntakeFinish::Failed(_) => {
            let (state, error) = match finish {
                IntakeFinish::Failed(reason) => ("failed", Some(bounded(reason))),
                IntakeFinish::Done | IntakeFinish::Removed => ("done", None),
            };
            exec(
                "UPDATE intake_items SET state = ?, error = ?, updated_at = ?
                  WHERE plugin_instance_id = ? AND item_hash = ? AND state = 'queued' AND batch_id = ?",
                vec![
                    text(state),
                    text_opt(error.as_deref()),
                    text(&now),
                    text(instance_id),
                    text(&hash),
                    text(batch_id),
                ],
            )
        }
    };
    let reply = store
        .execute_host_batch(super::batch::request(
            &format!("intake-finish-{}", uuid::Uuid::new_v4().simple()),
            vec![statement],
        ))
        .await?;
    Ok(rows_affected(&reply, 0) > 0)
}

/// The insert-or-update pair for one record (SQL-v1 has no `DO UPDATE`).
fn upsert(
    instance_id: &str,
    item_key: &str,
    state: TrackedState,
    signature: ItemSignature,
    quiet_since_ms: u64,
    error: Option<&str>,
    now: &str,
) -> [bookclerk_plugin_abi::TypedDbStatement; 2] {
    let hash = intake_item_hash(item_key);
    let quiet = i64::try_from(quiet_since_ms).unwrap_or(i64::MAX);
    let signature = signature.to_hex();
    [
        exec(
            "INSERT OR IGNORE INTO intake_items (
                plugin_instance_id, item_hash, item_key, state, signature,
                quiet_since_ms, batch_id, error, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, NULL, ?, ?)",
            vec![
                text(instance_id),
                text(&hash),
                text(item_key),
                text(state_text(state)),
                text(&signature),
                int(quiet),
                text_opt(error),
                text(now),
            ],
        ),
        exec(
            "UPDATE intake_items
                SET item_key = ?, state = ?, signature = ?, quiet_since_ms = ?,
                    batch_id = NULL, error = ?, updated_at = ?
              WHERE plugin_instance_id = ? AND item_hash = ?",
            vec![
                text(item_key),
                text(state_text(state)),
                text(&signature),
                int(quiet),
                text_opt(error),
                text(now),
                text(instance_id),
                text(&hash),
            ],
        ),
    ]
}

/// The stored text of a state.
fn state_text(state: TrackedState) -> &'static str {
    match state {
        TrackedState::Settling => "settling",
        TrackedState::Queued => "queued",
        TrackedState::Done => "done",
        TrackedState::Failed => "failed",
    }
}

/// Reads a stored state.
fn parse_state(text: &str) -> Option<TrackedState> {
    match text {
        "settling" => Some(TrackedState::Settling),
        "queued" => Some(TrackedState::Queued),
        "done" => Some(TrackedState::Done),
        "failed" => Some(TrackedState::Failed),
        _ => None,
    }
}

/// `reason` cut to [`MAX_INTAKE_ERROR_BYTES`] at a character boundary.
fn bounded(reason: &str) -> String {
    if reason.len() <= MAX_INTAKE_ERROR_BYTES {
        return reason.to_string();
    }
    let mut end = MAX_INTAKE_ERROR_BYTES;
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason[..end].to_string()
}

/// Reads one row, refusing a state or signature this version cannot read.
fn record_from_row(row: intake_items::Model) -> Result<IntakeItemRecord> {
    let invalid = |what: &str| {
        LibraryError::Invalid(format!(
            "intake_items row {}/{}: {what}",
            row.plugin_instance_id, row.item_hash
        ))
    };
    let state = parse_state(&row.state).ok_or_else(|| invalid("unknown state"))?;
    let signature =
        ItemSignature::from_hex(&row.signature).ok_or_else(|| invalid("bad signature"))?;
    let quiet_since_ms = u64::try_from(row.quiet_since_ms).unwrap_or(0);
    Ok(IntakeItemRecord {
        tracked: TrackedItem {
            state,
            signature,
            quiet_since_ms,
        },
        item_key: row.item_key,
        batch_id: row.batch_id,
        error: row.error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_plane::{create_plugin_instance, ConfigActor};
    use crate::test_support::{sqlite::memory, Profile};

    async fn drop_folder(store: &LibraryStore) -> String {
        let operator = ConfigActor::Operator {
            id: "operator:test".into(),
        };
        create_plugin_instance(store, &operator, "local", "Drop")
            .await
            .unwrap()
            .id
            .as_str()
            .to_string()
    }

    fn sig(byte: u8) -> ItemSignature {
        ItemSignature::from_hex(&hex::encode([byte; 32])).unwrap()
    }

    #[tokio::test]
    async fn many_records_page_and_batch_under_every_profile() {
        for profile in Profile::ALL {
            let name = profile.name();
            let store = memory(profile).await.into_store();
            let id = drop_folder(&store).await;
            let writes: Vec<IntakeWrite> = (0..700u64)
                .map(|n| IntakeWrite::Store {
                    item_key: format!("Book {n:04} {}", "y".repeat(200)),
                    tracked: TrackedItem {
                        state: TrackedState::Settling,
                        signature: sig(u8::try_from(n % 251).unwrap()),
                        quiet_since_ms: n,
                    },
                })
                .collect();
            apply_intake_writes(&store, &id, &writes).await.unwrap();
            let items = load_intake_items(&store, &id).await.unwrap();
            assert_eq!(items.len(), 700, "{name}");
            let forget: Vec<IntakeWrite> = items
                .keys()
                .map(|key| IntakeWrite::Forget {
                    item_key: key.clone(),
                })
                .collect();
            apply_intake_writes(&store, &id, &forget).await.unwrap();
            assert!(
                load_intake_items(&store, &id).await.unwrap().is_empty(),
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn records_round_trip_through_settle_queue_and_finish() {
        let store = crate::control_plane::tests::memory_store().await;
        let id = drop_folder(&store).await;
        let long_key = "x".repeat(1000);
        apply_intake_writes(
            &store,
            &id,
            &[
                IntakeWrite::Store {
                    item_key: "Book A".into(),
                    tracked: TrackedItem {
                        state: TrackedState::Settling,
                        signature: sig(1),
                        quiet_since_ms: 10,
                    },
                },
                IntakeWrite::Store {
                    item_key: long_key.clone(),
                    tracked: TrackedItem {
                        state: TrackedState::Settling,
                        signature: sig(2),
                        quiet_since_ms: 20,
                    },
                },
            ],
        )
        .await
        .unwrap();
        let items = load_intake_items(&store, &id).await.unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items["Book A"].tracked.quiet_since_ms, 10);

        // A queue write for a signature that moved does nothing.
        apply_intake_writes(
            &store,
            &id,
            &[
                IntakeWrite::Queue {
                    item_key: "Book A".into(),
                    signature: sig(9),
                    batch_id: "b0".into(),
                },
                IntakeWrite::Queue {
                    item_key: long_key.clone(),
                    signature: sig(2),
                    batch_id: "b1".into(),
                },
            ],
        )
        .await
        .unwrap();
        let items = load_intake_items(&store, &id).await.unwrap();
        assert_eq!(items["Book A"].tracked.state, TrackedState::Settling);
        assert_eq!(items[&long_key].tracked.state, TrackedState::Queued);
        assert_eq!(items[&long_key].batch_id.as_deref(), Some("b1"));

        // Only the batch the item is queued on may finish it.
        let reason = "é".repeat(400);
        assert!(
            !finish_intake_item(&store, &id, &long_key, "b0", &IntakeFinish::Done)
                .await
                .unwrap()
        );
        assert!(
            finish_intake_item(&store, &id, &long_key, "b1", &IntakeFinish::Failed(reason))
                .await
                .unwrap()
        );
        let items = load_intake_items(&store, &id).await.unwrap();
        let failed = &items[&long_key];
        assert_eq!(failed.tracked.state, TrackedState::Failed);
        assert!(failed.error.as_ref().unwrap().len() <= MAX_INTAKE_ERROR_BYTES);

        apply_intake_writes(
            &store,
            &id,
            &[
                IntakeWrite::Forget {
                    item_key: long_key.clone(),
                },
                IntakeWrite::Forget {
                    item_key: "Book A".into(),
                },
            ],
        )
        .await
        .unwrap();
        assert!(load_intake_items(&store, &id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_removed_item_drops_its_record() {
        let store = crate::control_plane::tests::memory_store().await;
        let id = drop_folder(&store).await;
        apply_intake_writes(
            &store,
            &id,
            &[IntakeWrite::Store {
                item_key: "B".into(),
                tracked: TrackedItem {
                    state: TrackedState::Settling,
                    signature: sig(3),
                    quiet_since_ms: 0,
                },
            }],
        )
        .await
        .unwrap();
        apply_intake_writes(
            &store,
            &id,
            &[IntakeWrite::Queue {
                item_key: "B".into(),
                signature: sig(3),
                batch_id: "b".into(),
            }],
        )
        .await
        .unwrap();
        assert!(
            finish_intake_item(&store, &id, "B", "b", &IntakeFinish::Removed)
                .await
                .unwrap()
        );
        assert!(load_intake_items(&store, &id).await.unwrap().is_empty());
    }
}
