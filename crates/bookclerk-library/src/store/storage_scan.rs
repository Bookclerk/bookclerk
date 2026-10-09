//! Durable, paged storage-scan index on [`LibraryStore`].

use chrono::Utc;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};

use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, Condition, EntityTrait, PaginatorTrait,
    QueryFilter, QueryOrder, QuerySelect, TransactionTrait,
};

use super::{map_book, LibraryStore};
use crate::entities::{books, storage_scan_generations, storage_scan_rows};
use crate::error::{LibraryError, Result};
use crate::models::BookRecord;

/// Fields for one idempotent scan-row insert.
struct ScanInsert<'a> {
    /// Scan generation.
    scan_id: &'a str,
    /// `object` or `identity`.
    kind: &'a str,
    /// Identity token, or empty for object rows.
    identity: &'a str,
    /// Storage key.
    key: &'a str,
    /// Object size.
    size: u64,
    /// Packaged-format rank.
    media_rank: u8,
    /// Whether the key is audio.
    is_audio: bool,
}

/// Row kind for a stored object key.
const KIND_OBJECT: &str = "object";
/// Row kind for an identity token pointing at a key.
const KIND_IDENTITY: &str = "identity";

impl LibraryStore {
    /// Inserts an object row. A repeat of the same key is a no-op.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the write fails.
    pub async fn storage_scan_put_object(
        &self,
        scan_id: &str,
        key: &str,
        size: u64,
        media_rank: u8,
        is_audio: bool,
    ) -> Result<()> {
        self.insert_scan_row(ScanInsert {
            scan_id,
            kind: KIND_OBJECT,
            identity: "",
            key,
            size,
            media_rank,
            is_audio,
        })
        .await
    }

    /// Inserts an identity → key row. Repeats are no-ops.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the write fails.
    pub async fn storage_scan_put_identity(
        &self,
        scan_id: &str,
        identity: &str,
        key: &str,
        size: u64,
        media_rank: u8,
    ) -> Result<()> {
        let upper = identity.to_ascii_uppercase();
        self.insert_scan_row(ScanInsert {
            scan_id,
            kind: KIND_IDENTITY,
            identity: &upper,
            key,
            size,
            media_rank,
            is_audio: true,
        })
        .await
    }

    /// One page of identity candidates, ordered by `(media_rank, object_key)`.
    ///
    /// `after` is the last key already considered. The page is bounded by
    /// `limit` (clamped to 1..=64). Callers try each key and request the next
    /// page; this does not load every match.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn storage_scan_identity_page(
        &self,
        scan_id: &str,
        identity: &str,
        after: Option<(i64, &str)>,
        limit: u64,
    ) -> Result<Vec<(String, i64)>> {
        let limit = limit.clamp(1, 64);
        let mut query = storage_scan_rows::Entity::find()
            .filter(storage_scan_rows::Column::ScanId.eq(scan_id))
            .filter(storage_scan_rows::Column::Kind.eq(KIND_IDENTITY))
            .filter(storage_scan_rows::Column::Identity.eq(identity.to_ascii_uppercase()));
        if let Some((rank, key)) = after {
            query = query.filter(
                Condition::any()
                    .add(storage_scan_rows::Column::MediaRank.gt(rank))
                    .add(
                        Condition::all()
                            .add(storage_scan_rows::Column::MediaRank.eq(rank))
                            .add(storage_scan_rows::Column::ObjectKey.gt(key)),
                    ),
            );
        }
        let rows = query
            .order_by_asc(storage_scan_rows::Column::MediaRank)
            .order_by_asc(storage_scan_rows::Column::ObjectKey)
            .limit(limit)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(rows
            .into_iter()
            .map(|row| (row.object_key, row.media_rank))
            .collect())
    }

    /// One page of object keys at or above `prefix`, in `object_key` order.
    ///
    /// The upper bound is the byte-wise successor of `prefix`, not a SQL `LIKE`
    /// pattern, so wildcard characters in a storage key stay literal.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn storage_scan_object_page(
        &self,
        scan_id: &str,
        prefix: &str,
        after_key: Option<&str>,
        limit: u64,
    ) -> Result<Vec<String>> {
        let limit = limit.clamp(1, 64);
        let mut query = storage_scan_rows::Entity::find()
            .filter(storage_scan_rows::Column::ScanId.eq(scan_id))
            .filter(storage_scan_rows::Column::Kind.eq(KIND_OBJECT));
        if !prefix.is_empty() {
            query = query.filter(storage_scan_rows::Column::ObjectKey.gte(prefix));
            if let Some(upper) = prefix_upper_bound(prefix) {
                query = query.filter(storage_scan_rows::Column::ObjectKey.lt(upper));
            }
        }
        if let Some(after_key) = after_key {
            query = query.filter(storage_scan_rows::Column::ObjectKey.gt(after_key));
        }
        let rows = query
            .order_by_asc(storage_scan_rows::Column::ObjectKey)
            .limit(limit)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(rows.into_iter().map(|row| row.object_key).collect())
    }

    /// Marks an object claimed so unmatched-audio counts stay in the database.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the update fails.
    pub async fn storage_scan_claim(&self, scan_id: &str, key: &str) -> Result<()> {
        storage_scan_rows::Entity::update_many()
            .col_expr(
                storage_scan_rows::Column::Claimed,
                sea_orm::sea_query::Expr::value(1_i64),
            )
            .filter(storage_scan_rows::Column::ScanId.eq(scan_id))
            .filter(storage_scan_rows::Column::Kind.eq(KIND_OBJECT))
            .filter(storage_scan_rows::Column::ObjectKey.eq(key))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Audio objects in the scan that no library row claimed.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the count fails.
    pub async fn storage_scan_unclaimed_audio(&self, scan_id: &str) -> Result<u64> {
        let count = storage_scan_rows::Entity::find()
            .filter(storage_scan_rows::Column::ScanId.eq(scan_id))
            .filter(storage_scan_rows::Column::Kind.eq(KIND_OBJECT))
            .filter(storage_scan_rows::Column::IsAudio.eq(1))
            .filter(storage_scan_rows::Column::Claimed.eq(0))
            .count(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(count)
    }

    /// Deletes inventory rows and the generation ownership row for `scan_id`.
    ///
    /// Both deletes commit together. A failure leaves the generation and its
    /// rows in place so an apply checkpoint cannot adopt an empty inventory.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the delete fails.
    pub async fn storage_scan_delete(&self, scan_id: &str) -> Result<()> {
        let txn = self.db.begin().await.map_err(LibraryError::Orm)?;
        storage_scan_rows::Entity::delete_many()
            .filter(storage_scan_rows::Column::ScanId.eq(scan_id))
            .exec(&txn)
            .await
            .map_err(LibraryError::Orm)?;
        if scan_delete_should_fail_before_generation() {
            return Err(LibraryError::Other(anyhow::anyhow!(
                "injected storage scan delete failure before generation removal"
            )));
        }
        storage_scan_generations::Entity::delete_many()
            .filter(storage_scan_generations::Column::ScanId.eq(scan_id))
            .exec(&txn)
            .await
            .map_err(LibraryError::Orm)?;
        txn.commit().await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Binds `scan_id` to `instance_id` and an optional job.
    ///
    /// A second register for the same id and instance refreshes the heartbeat.
    /// A different instance fails closed so a reused id cannot adopt another scan.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the write fails, or
    /// [`LibraryError::Other`] when `instance_id` does not match the existing row.
    pub async fn storage_scan_register(
        &self,
        scan_id: &str,
        instance_id: &str,
        job_id: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let job_id = job_id.unwrap_or("").to_string();
        if let Some(existing) = storage_scan_generations::Entity::find_by_id(scan_id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
        {
            if existing.instance_id != instance_id {
                return Err(LibraryError::Other(anyhow::anyhow!(
                    "storage scan `{scan_id}` is bound to `{}`, not `{instance_id}`",
                    existing.instance_id
                )));
            }
            let mut model: storage_scan_generations::ActiveModel = existing.into();
            if !job_id.is_empty() {
                model.job_id = Set(job_id);
            }
            model.updated_at = Set(now);
            model.update(&self.db).await.map_err(LibraryError::Orm)?;
            return Ok(());
        }
        let row = storage_scan_generations::ActiveModel {
            scan_id: Set(scan_id.to_string()),
            instance_id: Set(instance_id.to_string()),
            job_id: Set(job_id),
            updated_at: Set(now),
            completed: Set(0),
        };
        row.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Marks a generation's inventory complete so a later checkpoint can adopt it.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the update fails.
    pub async fn storage_scan_mark_complete(&self, scan_id: &str) -> Result<()> {
        storage_scan_generations::Entity::update_many()
            .col_expr(
                storage_scan_generations::Column::Completed,
                sea_orm::sea_query::Expr::value(1i64),
            )
            .filter(storage_scan_generations::Column::ScanId.eq(scan_id))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Returns `(instance_id, completed)` when the generation row still exists.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn storage_scan_adoption(&self, scan_id: &str) -> Result<Option<(String, bool)>> {
        let row = storage_scan_generations::Entity::find_by_id(scan_id)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(row.map(|row| (row.instance_id, row.completed != 0)))
    }

    /// Refreshes the generation heartbeat after a page is stored.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the update fails.
    pub async fn storage_scan_touch(&self, scan_id: &str) -> Result<()> {
        storage_scan_generations::Entity::update_many()
            .col_expr(
                storage_scan_generations::Column::UpdatedAt,
                sea_orm::sea_query::Expr::value(Utc::now().to_rfc3339()),
            )
            .filter(storage_scan_generations::Column::ScanId.eq(scan_id))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// How many generation rows exist. Tests use this to bound retained scans.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the count fails.
    pub async fn storage_scan_generation_count(&self) -> Result<u64> {
        storage_scan_generations::Entity::find()
            .count(&self.db)
            .await
            .map_err(LibraryError::Orm)
    }

    /// Deletes generations that are not live resumable work.
    ///
    /// A generation with an active job is kept even when `updated_at` is older
    /// than `stale_before`. A jobless generation is kept only while its
    /// heartbeat is at least `stale_before`. Terminal, missing, and stale
    /// jobless generations are removed with their inventory rows.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read or delete fails.
    pub async fn storage_scan_reclaim_abandoned(&self, stale_before: &str) -> Result<u64> {
        let rows = storage_scan_generations::Entity::find()
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let mut removed = 0u64;
        for row in rows {
            let keep = if row.job_id.is_empty() {
                row.updated_at.as_str() >= stale_before
            } else {
                matches!(
                    self.get_job(&row.job_id).await?,
                    Some(job) if job.state.is_active()
                )
            };
            if keep {
                continue;
            }
            self.storage_scan_delete(&row.scan_id).await?;
            removed += 1;
        }
        Ok(removed)
    }

    /// Deletes this job's inventories when the job is terminal or already gone.
    ///
    /// Pending and running jobs, including a retry after one failed attempt,
    /// are left untouched.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read or delete fails.
    pub async fn storage_scan_reclaim_if_terminal(&self, job_id: &str) -> Result<u64> {
        if let Some(job) = self.get_job(job_id).await? {
            if !job.state.is_terminal() {
                return Ok(0);
            }
        }
        let rows = storage_scan_generations::Entity::find()
            .filter(storage_scan_generations::Column::JobId.eq(job_id))
            .limit(32)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let mut removed = 0u64;
        for row in rows {
            self.storage_scan_delete(&row.scan_id).await?;
            removed += 1;
        }
        Ok(removed)
    }

    /// Deletes up to `limit` generations whose job is terminal or missing.
    ///
    /// Pending, running, and jobless generations are kept. `after_scan_id`
    /// walks past a page of still-active rows so one sweep cannot stall on
    /// them. Pass the returned cursor back; `None` means the walk wrapped.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read or delete fails. A delete
    /// error leaves that generation for a later retry.
    pub async fn storage_scan_reclaim_terminal_page(
        &self,
        after_scan_id: Option<&str>,
        limit: u64,
    ) -> Result<(u64, Option<String>)> {
        let limit = limit.clamp(1, 64);
        let mut query = storage_scan_generations::Entity::find()
            .order_by_asc(storage_scan_generations::Column::ScanId);
        if let Some(after) = after_scan_id {
            query = query.filter(storage_scan_generations::Column::ScanId.gt(after));
        }
        let rows = query
            .limit(limit)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let next = if rows.len() < usize::try_from(limit).unwrap_or(usize::MAX) {
            None
        } else {
            rows.last().map(|row| row.scan_id.clone())
        };
        let mut removed = 0u64;
        for row in rows {
            if row.job_id.is_empty() {
                continue;
            }
            let terminal = match self.get_job(&row.job_id).await? {
                Some(job) => job.state.is_terminal(),
                None => true,
            };
            if !terminal {
                continue;
            }
            self.storage_scan_delete(&row.scan_id).await?;
            removed += 1;
        }
        Ok((removed, next))
    }

    /// Pages books by surrogate id so a scan does not load the catalog at once.
    ///
    /// `limit` is clamped to 1..=64. The read starts at that width and halves
    /// when the guest result would exceed `maxResultBytes` (256 KiB). A full
    /// page of enriched `books` rows is larger than that cap. The smaller
    /// width that fits is reused until `limit` rows are collected or the
    /// catalog ends.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails for a reason other
    /// than the result cap, or when a single row still exceeds the cap.
    pub async fn list_books_page(
        &self,
        account_id: Option<&str>,
        after_id: Option<i64>,
        limit: u64,
    ) -> Result<Vec<BookRecord>> {
        let want = limit.clamp(1, 64);
        let mut books = Vec::new();
        let mut cursor = after_id;
        let mut width = want;
        while u64::try_from(books.len()).unwrap_or(u64::MAX) < want {
            let have = u64::try_from(books.len()).unwrap_or(0);
            let mut take = (want - have).min(width).max(1);
            let rows = loop {
                match self.fetch_books_page(account_id, cursor, take).await {
                    Ok(rows) => break rows,
                    Err(err) if super::books_page::is_result_too_large(&err) && take > 1 => {
                        take /= 2;
                        width = take;
                    }
                    Err(err) => return Err(err),
                }
            };
            let n = u64::try_from(rows.len()).unwrap_or(0);
            let last = rows.last().map(|row| row.id);
            books.extend(rows);
            if n < take {
                break;
            }
            cursor = last;
        }
        let cap = usize::try_from(want).unwrap_or(usize::MAX);
        if books.len() > cap {
            books.truncate(cap);
        }
        Ok(books)
    }

    /// One catalog slice of at most `limit` books after `after_id`.
    async fn fetch_books_page(
        &self,
        account_id: Option<&str>,
        after_id: Option<i64>,
        limit: u64,
    ) -> Result<Vec<BookRecord>> {
        let mut query = books::Entity::find().order_by_asc(books::Column::Id);
        if let Some(account_id) = account_id {
            query = query.filter(books::Column::AccountId.eq(account_id));
        }
        if let Some(after_id) = after_id {
            query = query.filter(books::Column::Id.gt(after_id));
        }
        let rows = query
            .limit(limit.max(1))
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        rows.into_iter().map(map_book).collect()
    }

    /// Inserts one scan row when that primary key is absent.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read or insert fails.
    async fn insert_scan_row(&self, row: ScanInsert<'_>) -> Result<()> {
        let ScanInsert {
            scan_id,
            kind,
            identity,
            key,
            size,
            media_rank,
            is_audio,
        } = row;
        let existing = storage_scan_rows::Entity::find_by_id((
            scan_id.to_string(),
            kind.to_string(),
            identity.to_string(),
            key.to_string(),
        ))
        .one(&self.db)
        .await
        .map_err(LibraryError::Orm)?;
        if existing.is_some() {
            return Ok(());
        }
        let row = storage_scan_rows::ActiveModel {
            scan_id: Set(scan_id.to_string()),
            kind: Set(kind.to_string()),
            identity: Set(identity.to_string()),
            object_key: Set(key.to_string()),
            size: Set(i64::try_from(size).unwrap_or(i64::MAX)),
            media_rank: Set(i64::from(media_rank)),
            is_audio: Set(i64::from(is_audio)),
            claimed: Set(0),
        };
        row.insert(&self.db).await.map_err(LibraryError::Orm)?;
        Ok(())
    }
}

/// Exclusive upper bound for keys that start with `prefix` under byte order.
fn prefix_upper_bound(prefix: &str) -> Option<String> {
    let mut bytes = prefix.as_bytes().to_vec();
    while let Some(last) = bytes.pop() {
        if last < 0xFF {
            bytes.push(last + 1);
            return String::from_utf8(bytes).ok();
        }
    }
    None
}

/// Test switch: fail `storage_scan_delete` after the row delete, before commit.
fn scan_delete_should_fail_before_generation() -> bool {
    #[cfg(test)]
    {
        FAIL_STORAGE_SCAN_DELETE_BEFORE_GENERATION.load(Ordering::SeqCst)
    }
    #[cfg(not(test))]
    {
        false
    }
}

/// When set, [`LibraryStore::storage_scan_delete`] rolls back instead of
/// removing the generation row.
#[cfg(test)]
pub(crate) static FAIL_STORAGE_SCAN_DELETE_BEFORE_GENERATION: AtomicBool = AtomicBool::new(false);
