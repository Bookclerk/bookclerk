//! Durable, paged storage-scan index on [`LibraryStore`].

use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect,
};

use super::{map_book, LibraryStore};
use crate::entities::{books, storage_scan_rows};
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

    /// Best stored key for `identity` (lowest media rank), if the scan recorded one.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn storage_scan_best_identity(
        &self,
        scan_id: &str,
        identity: &str,
    ) -> Result<Option<String>> {
        let row = storage_scan_rows::Entity::find()
            .filter(storage_scan_rows::Column::ScanId.eq(scan_id))
            .filter(storage_scan_rows::Column::Kind.eq(KIND_IDENTITY))
            .filter(storage_scan_rows::Column::Identity.eq(identity.to_ascii_uppercase()))
            .order_by_asc(storage_scan_rows::Column::MediaRank)
            .limit(1)
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(row.map(|row| row.object_key))
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

    /// Deletes every row for `scan_id`.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the delete fails.
    pub async fn storage_scan_delete(&self, scan_id: &str) -> Result<()> {
        storage_scan_rows::Entity::delete_many()
            .filter(storage_scan_rows::Column::ScanId.eq(scan_id))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Pages books by surrogate id so a scan does not load the catalog at once.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn list_books_page(
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
            .limit(limit.clamp(1, 256))
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
