//! Every file key a title wrote, per output destination (`title_files`).
//!
//! Library rows record only a title's primary audio and PDF keys, but an
//! acquire also writes chapter or part files and sidecars, and a naming
//! template may put several titles in one folder. This table records each
//! of those keys against the title that wrote it, so a host upload can be
//! refused a key another title holds on that destination, and an aborted
//! upload's copy is deleted only while the key is still that upload's.
//!
//! An acquire run on this node records each key just before it writes the
//! file ([`LibraryStore::record_title_file`]; the file is the title's from
//! then on, whichever title had it before). A host upload claims every key
//! of its manifest when it is granted ([`LibraryStore::claim_title_file`]),
//! and only keys no other library title holds; a grant refused part way
//! gives back what it claimed ([`LibraryStore::release_title_file`]).

use sea_orm::sea_query::{OnConflict, Query, UnionType};
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, QueryFilter};

use super::{now_str, LibraryStore};
use crate::entities::{books, branch_titles, title_files};
use crate::error::{LibraryError, Result};

/// The `writer` of a key an acquire run on this node wrote.
pub const ACQUIRE_WRITER: &str = "acquire";

/// Who holds a recorded file key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitleFile {
    /// Library id of the title the file belongs to.
    pub book_uuid: String,
    /// The host upload id that claimed the key last, or [`ACQUIRE_WRITER`].
    pub writer: String,
}

impl LibraryStore {
    /// Records that the title `book_uuid` writes `key` on `destination`
    /// (an acquire run on this node, just before the write).
    ///
    /// The record is replaced unconditionally: the file is this title's
    /// once written, whichever title held the key before.
    ///
    /// # Errors
    ///
    /// Returns an error when the write fails.
    pub async fn record_title_file(
        &self,
        destination: &str,
        key: &str,
        book_uuid: &str,
    ) -> Result<()> {
        title_files::Entity::insert(title_files::ActiveModel {
            destination: Set(destination.to_string()),
            storage_key: Set(key.to_string()),
            book_uuid: Set(book_uuid.to_string()),
            writer: Set(ACQUIRE_WRITER.to_string()),
            written_at: Set(now_str()),
        })
        .on_conflict(
            OnConflict::columns([
                title_files::Column::Destination,
                title_files::Column::StorageKey,
            ])
            .update_columns([
                title_files::Column::BookUuid,
                title_files::Column::Writer,
                title_files::Column::WrittenAt,
            ])
            .to_owned(),
        )
        .exec_without_returning(self.db())
        .await
        .map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Claims `key` on `destination` for the title `book_uuid`, with
    /// `writer` (a host upload id) as its latest writer.
    ///
    /// The claim succeeds when the key is unrecorded, already the title's,
    /// or recorded for a title no longer in the library (neither a book nor
    /// a title a branch shares here, `branch_titles`); answers false,
    /// changing nothing, when another library title holds it. Each claim
    /// is one conditional write, so of two uploads of different titles
    /// racing for one key only one wins.
    ///
    /// `expected` is the record the caller read before ([`Self::title_file`];
    /// `None` when unrecorded), which a refused claim's release restores: the
    /// claim also answers false, changing nothing, when the record is no
    /// longer that one (an acquire recorded the key in between), so a
    /// release never removes a record the claim did not replace.
    ///
    /// # Errors
    ///
    /// Returns an error when a write fails.
    pub async fn claim_title_file(
        &self,
        destination: &str,
        key: &str,
        book_uuid: &str,
        writer: &str,
        expected: Option<&TitleFile>,
    ) -> Result<bool> {
        let now = now_str();
        // Only a key read as unrecorded is inserted: a claim expecting a
        // record that has since gone must change nothing, so its retry
        // reads the key afresh.
        if expected.is_none() {
            title_files::Entity::insert(title_files::ActiveModel {
                destination: Set(destination.to_string()),
                storage_key: Set(key.to_string()),
                book_uuid: Set(book_uuid.to_string()),
                writer: Set(writer.to_string()),
                written_at: Set(now.clone()),
            })
            .on_conflict(
                OnConflict::columns([
                    title_files::Column::Destination,
                    title_files::Column::StorageKey,
                ])
                .do_nothing()
                .to_owned(),
            )
            .exec_without_returning(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        }
        // A holder is live while its title is in the library: one of its
        // books, or a title a branch shares here (whose upload claims keys
        // under the branch title's id). Only a holder in neither is stale.
        let library_titles = Query::select()
            .column(books::Column::Uuid)
            .from(books::Entity)
            .union(
                UnionType::Distinct,
                Query::select()
                    .column(branch_titles::Column::Uuid)
                    .from(branch_titles::Entity)
                    .to_owned(),
            )
            .to_owned();
        let claimed = title_files::Entity::update_many()
            .col_expr(
                title_files::Column::BookUuid,
                sea_orm::sea_query::Expr::value(book_uuid),
            )
            .col_expr(
                title_files::Column::Writer,
                sea_orm::sea_query::Expr::value(writer),
            )
            .col_expr(
                title_files::Column::WrittenAt,
                sea_orm::sea_query::Expr::value(now),
            )
            .filter(title_files::Column::Destination.eq(destination))
            .filter(title_files::Column::StorageKey.eq(key))
            .filter(
                Condition::any()
                    .add(title_files::Column::BookUuid.eq(book_uuid))
                    .add(title_files::Column::BookUuid.not_in_subquery(library_titles)),
            )
            // Unrecorded before: only the row the insert above created.
            .filter(match expected {
                Some(prior) => Condition::all()
                    .add(title_files::Column::BookUuid.eq(prior.book_uuid.as_str()))
                    .add(title_files::Column::Writer.eq(prior.writer.as_str())),
                None => Condition::all()
                    .add(title_files::Column::BookUuid.eq(book_uuid))
                    .add(title_files::Column::Writer.eq(writer)),
            })
            .exec(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(claimed.rows_affected == 1)
    }

    /// Gives back a claim `writer` made on `key` on `destination`: the
    /// record returns to `prior` (who held it before the claim), or is
    /// removed when `prior` is `None` (the claim created it). Changes
    /// nothing once someone else has written the key since (its writer is
    /// no longer `writer`), so a release never takes a key from a later
    /// claim or acquire.
    ///
    /// # Errors
    ///
    /// Returns an error when the write fails.
    pub async fn release_title_file(
        &self,
        destination: &str,
        key: &str,
        writer: &str,
        prior: Option<&TitleFile>,
    ) -> Result<()> {
        let ours = Condition::all()
            .add(title_files::Column::Destination.eq(destination))
            .add(title_files::Column::StorageKey.eq(key))
            .add(title_files::Column::Writer.eq(writer));
        match prior {
            None => {
                title_files::Entity::delete_many()
                    .filter(ours)
                    .exec(self.db())
                    .await
                    .map_err(LibraryError::Orm)?;
            }
            Some(prior) => {
                title_files::Entity::update_many()
                    .col_expr(
                        title_files::Column::BookUuid,
                        sea_orm::sea_query::Expr::value(prior.book_uuid.as_str()),
                    )
                    .col_expr(
                        title_files::Column::Writer,
                        sea_orm::sea_query::Expr::value(prior.writer.as_str()),
                    )
                    .filter(ours)
                    .exec(self.db())
                    .await
                    .map_err(LibraryError::Orm)?;
            }
        }
        Ok(())
    }

    /// How many keys `writer` (a host upload id) holds for the title
    /// `book_uuid`, on every destination together.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn count_title_files_of(&self, book_uuid: &str, writer: &str) -> Result<u64> {
        use sea_orm::PaginatorTrait as _;
        title_files::Entity::find()
            .filter(title_files::Column::BookUuid.eq(book_uuid))
            .filter(title_files::Column::Writer.eq(writer))
            .count(self.db())
            .await
            .map_err(LibraryError::Orm)
    }

    /// Who holds `key` on `destination`; `None` when it is unrecorded.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn title_file(&self, destination: &str, key: &str) -> Result<Option<TitleFile>> {
        Ok(
            title_files::Entity::find_by_id((destination.to_string(), key.to_string()))
                .one(self.db())
                .await
                .map_err(LibraryError::Orm)?
                .map(|row| TitleFile {
                    book_uuid: row.book_uuid,
                    writer: row.writer,
                }),
        )
    }
}
