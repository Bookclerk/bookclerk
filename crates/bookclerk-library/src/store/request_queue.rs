//! The stored taste ranking of the shared request queue.
//!
//! `GET /api/request-queue` used to rank every open request inside the page
//! request, reading the whole library and listening history each time. The
//! `discover_request_queue` job now ranks the queue when a taste event
//! (`wishlist_changed`, `book_acquired`, `listening_synced`) says it may have
//! moved, and stores one small row per queue entry here: its taste score,
//! its taste reasons and whether the library already holds it. The route
//! joins those rows with the open requests, so titles, covers, wish counts
//! and wishers are never copied (see `docs/internals/discovery.md`,
//! *Request queue*). The table holds derived data the job rebuilds, so a
//! backend copy leaves it behind.

use bookclerk_plugin_abi::{DbValue, TypedDbStatement};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

use super::discover::DISCOVER_READ_PAGE;
use super::write_batches::WriteBatches;
use super::{now_str, parse_dt, LibraryStore};
use crate::control_plane::batch;
use crate::entities::discover_request_queue;
use crate::error::{LibraryError, Result};

/// Most taste reasons one stored row keeps, so a row stays small whatever
/// the ranking explains.
pub const MAX_QUEUE_REASONS: usize = 6;

/// The taste ranking of one queue entry, as the ranking job computed it.
#[derive(Debug, Clone, PartialEq)]
pub struct QueueRank {
    /// The work (`works.id`) the entry's wishes are for, which identifies
    /// the entry.
    pub work_id: String,
    /// True when the library already holds the entry's work.
    pub owned: bool,
    /// Household taste score before the wish-count boost. A non-finite
    /// value is stored as 0.
    pub taste_score: f64,
    /// Taste reasons; at most [`MAX_QUEUE_REASONS`] are stored.
    pub reasons: Vec<String>,
}

/// One stored queue ranking row.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredQueueRank {
    /// The ranking.
    pub rank: QueueRank,
    /// When the ranking pass that wrote the row ran.
    pub ranked_at: chrono::DateTime<chrono::Utc>,
}

/// The two statements that upsert one ranking row.
///
/// `UPDATE` first, then `INSERT OR IGNORE`, as candidate writes do:
/// canonical SQL-v1 has no `ON CONFLICT … DO UPDATE`, and the pair is
/// idempotent across batches.
///
/// # Errors
///
/// Returns an error when the reasons cannot be encoded as JSON.
fn rank_upsert(rank: &QueueRank, ranked_at: &str) -> Result<[TypedDbStatement; 2]> {
    let reasons: Vec<&String> = rank.reasons.iter().take(MAX_QUEUE_REASONS).collect();
    let reasons_json =
        serde_json::to_string(&reasons).map_err(|err| LibraryError::Other(anyhow::anyhow!(err)))?;
    let score = if rank.taste_score.is_finite() {
        rank.taste_score
    } else {
        0.0
    };
    let owned = DbValue::Int64(i64::from(rank.owned));
    Ok([
        batch::exec(
            "UPDATE discover_request_queue \
             SET owned = ?, taste_score = ?, reasons_json = ?, ranked_at = ? \
             WHERE work_id = ?",
            vec![
                owned.clone(),
                DbValue::Float64(score),
                batch::text(&reasons_json),
                batch::text(ranked_at),
                batch::text(&rank.work_id),
            ],
        ),
        batch::exec(
            "INSERT OR IGNORE INTO discover_request_queue \
             (work_id, owned, taste_score, reasons_json, ranked_at) \
             VALUES (?, ?, ?, ?, ?)",
            vec![
                batch::text(&rank.work_id),
                owned,
                DbValue::Float64(score),
                batch::text(&reasons_json),
                batch::text(ranked_at),
            ],
        ),
    ])
}

impl LibraryStore {
    /// Replaces the stored queue ranking with `ranks`.
    ///
    /// Each row is upserted with this pass's time, then every row this pass
    /// did not write (an entry whose wishes were all closed) is deleted. The
    /// writes go to the database plugin as canonical atomic batches sized
    /// from its advertised limits, never an interactive transaction; the
    /// delete rides in the last batch. A reader between two batches can see
    /// part of the new ranking next to old rows, which only shifts order.
    ///
    /// # Arguments
    ///
    /// * `ranks` - One ranking per queue entry; a repeated `work_id` keeps the last.
    ///
    /// # Errors
    ///
    /// Returns the database error. Batches written before it stay; old rows
    /// are deleted only once every new row is stored.
    pub async fn replace_request_queue_ranks(&self, ranks: &[QueueRank]) -> Result<()> {
        let now = now_str();
        let mut unique: Vec<&QueueRank> = Vec::with_capacity(ranks.len());
        for rank in ranks {
            if let Some(pos) = unique.iter().position(|r| r.work_id == rank.work_id) {
                unique.remove(pos);
            }
            unique.push(rank);
        }
        let mut batches = WriteBatches::new(self.db_capabilities(), "request queue ranking");
        for rank in unique {
            if let Some(full) = batches.push_group(rank_upsert(rank, &now)?.to_vec())? {
                self.run_queue_batch(full).await?;
            }
        }
        let stale = batch::exec(
            "DELETE FROM discover_request_queue WHERE ranked_at <> ?",
            vec![batch::text(&now)],
        );
        if let Some(full) = batches.push(stale)? {
            self.run_queue_batch(full).await?;
        }
        if let Some(last) = batches.finish() {
            self.run_queue_batch(last).await?;
        }
        Ok(())
    }

    /// Sends one chunk of ranking writes as one atomic host batch.
    ///
    /// # Errors
    ///
    /// Returns the database error; nothing in the chunk is kept then.
    async fn run_queue_batch(&self, statements: Vec<TypedDbStatement>) -> Result<()> {
        let operation_id = format!("discover-request-queue-{}", uuid::Uuid::new_v4());
        self.execute_host_batch(batch::request(&operation_id, statements))
            .await?;
        Ok(())
    }

    /// Lists every stored queue ranking row.
    ///
    /// Rows are read in work id pages of a bounded size; a page whose result
    /// would exceed the plugin's `maxResultBytes` is halved and read again.
    /// A row whose reasons do not parse keeps its score with no reasons.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn list_request_queue_ranks(&self) -> Result<Vec<StoredQueueRank>> {
        let mut out = Vec::new();
        let mut after = String::new();
        let mut width = DISCOVER_READ_PAGE;
        loop {
            let rows = loop {
                let read = discover_request_queue::Entity::find()
                    .filter(discover_request_queue::Column::WorkId.gt(after.as_str()))
                    .order_by_asc(discover_request_queue::Column::WorkId)
                    .limit(width)
                    .all(&self.db)
                    .await
                    .map_err(LibraryError::Orm);
                match read {
                    Ok(rows) => break rows,
                    Err(err) if super::books_page::is_result_too_large(&err) && width > 1 => {
                        width /= 2;
                    }
                    Err(err) => return Err(err),
                }
            };
            let Some(last) = rows.last() else { break };
            after.clone_from(&last.work_id);
            let full = u64::try_from(rows.len()).is_ok_and(|n| n >= width);
            out.extend(rows.into_iter().map(|m| StoredQueueRank {
                ranked_at: parse_dt(&m.ranked_at),
                rank: QueueRank {
                    reasons: serde_json::from_str(&m.reasons_json).unwrap_or_default(),
                    work_id: m.work_id,
                    owned: m.owned != 0,
                    taste_score: m.taste_score,
                },
            }));
            if !full {
                break;
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ranking of `key` with one reason.
    fn rank(key: &str, score: f64) -> QueueRank {
        QueueRank {
            work_id: key.into(),
            owned: false,
            taste_score: score,
            reasons: vec![format!("reason for {key}")],
        }
    }

    /// An empty in-memory library with the host schema, advertising D1's
    /// limits with the smallest statement cap a plugin may advertise, so
    /// writes split into several batches.
    async fn store() -> LibraryStore {
        let mut caps = bookclerk_plugin_abi::DbCapabilities::advertised_d1();
        caps.max_statements = bookclerk_plugin_abi::HOST_MIN_STATEMENTS;
        LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .expect("sqlite"),
        )
        .with_db_capabilities(caps)
    }

    #[tokio::test]
    async fn a_pass_replaces_the_previous_ranking() {
        let library = store().await;
        library
            .replace_request_queue_ranks(&[rank("work:a", 0.5), rank("work:b", 0.2)])
            .await
            .expect("first pass");
        let mut owned = rank("work:a", 0.9);
        owned.owned = true;
        library
            .replace_request_queue_ranks(&[owned.clone(), rank("work:c", 0.1)])
            .await
            .expect("second pass");
        let mut rows: Vec<QueueRank> = library
            .list_request_queue_ranks()
            .await
            .expect("list")
            .into_iter()
            .map(|r| r.rank)
            .collect();
        rows.sort_by(|a, b| a.work_id.cmp(&b.work_id));
        assert_eq!(rows, vec![owned, rank("work:c", 0.1)]);
    }

    #[tokio::test]
    async fn an_empty_pass_clears_the_ranking() {
        let library = store().await;
        library
            .replace_request_queue_ranks(&[rank("work:a", 0.5)])
            .await
            .expect("first pass");
        library
            .replace_request_queue_ranks(&[])
            .await
            .expect("empty pass");
        assert!(library
            .list_request_queue_ranks()
            .await
            .expect("list")
            .is_empty());
    }

    #[tokio::test]
    async fn rows_stay_small_and_finite() {
        let library = store().await;
        let mut wide = rank("work:a", f64::NAN);
        wide.reasons = (0..20).map(|i| format!("reason {i}")).collect();
        library
            .replace_request_queue_ranks(&[wide.clone(), wide])
            .await
            .expect("pass");
        let rows = library.list_request_queue_ranks().await.expect("list");
        assert_eq!(rows.len(), 1, "a repeated entry keeps one row");
        assert_eq!(rows[0].rank.taste_score, 0.0);
        assert_eq!(rows[0].rank.reasons.len(), MAX_QUEUE_REASONS);
    }

    #[tokio::test]
    async fn many_rows_span_several_batches_and_pages() {
        let library = store().await;
        let ranks: Vec<QueueRank> = (0..450)
            .map(|i| rank(&format!("work:{i:04}"), f64::from(i)))
            .collect();
        library
            .replace_request_queue_ranks(&ranks)
            .await
            .expect("pass");
        assert_eq!(
            library
                .list_request_queue_ranks()
                .await
                .expect("list")
                .len(),
            450
        );
    }
}
