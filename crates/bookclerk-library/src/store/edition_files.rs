//! Every file key an edition has, per output destination (`edition_files`).
//!
//! Files belong to editions, not to the account rows that hold them
//! ([library data model ADR](../../../../docs/adr/library-data-model.md),
//! section 3): two accounts holding one edition share its files, and an
//! acquire of one of them serves both. Library rows record only a title's
//! primary audio and PDF keys, but an acquire also writes chapter or part
//! files and sidecars, and a naming template may put several titles in one
//! folder. This table records each of those keys against the edition that
//! wrote it, so a host upload can be refused a key another edition holds on
//! that destination, an acquire can give a second edition its own path, and
//! an aborted upload's copy is deleted only while the key is still that
//! upload's.
//!
//! An acquire run on this node records each key just before it writes the
//! file ([`LibraryStore::record_edition_file`]; the file is the edition's
//! from then on, whichever edition had it before). A host upload claims
//! every key of its manifest when it is granted
//! ([`LibraryStore::claim_edition_file`]), and only keys no other edition
//! holds (a Host job's upload of a book may take over a branch title's
//! keys, as an acquire does); a grant refused part way gives back what it
//! claimed ([`LibraryStore::release_edition_file`]).
//!
//! A title a branch shares here has no edition yet, so its keys are held
//! by the branch title ([`FileOwner::BranchTitle`]) until branch titles
//! become holdings.

use std::collections::BTreeSet;

use sea_orm::sea_query::{Expr, OnConflict, Query};
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder};

use super::{now_str, LibraryStore};
use crate::entities::{
    books, branch_titles, edition_files, editions, node_upload_sessions, store_listings,
};
use crate::error::{LibraryError, Result};

/// The `writer` of a key an acquire run on this node wrote.
pub const ACQUIRE_WRITER: &str = "acquire";

/// Who a recorded file key belongs to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum FileOwner {
    /// An edition (`editions.id`): every account row that holds it shares
    /// the file.
    Edition(String),
    /// A title a branch shares here (`branch_titles.uuid`), which has no
    /// edition until branch titles become holdings.
    BranchTitle(String),
}

impl FileOwner {
    /// The stored `owner_kind`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Edition(_) => "edition",
            Self::BranchTitle(_) => "branch_title",
        }
    }

    /// The stored `owner_id`.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Edition(id) | Self::BranchTitle(id) => id,
        }
    }

    /// The owner a stored row names; `None` for an unknown `owner_kind`.
    fn from_row(kind: &str, id: String) -> Option<Self> {
        match kind {
            "edition" => Some(Self::Edition(id)),
            "branch_title" => Some(Self::BranchTitle(id)),
            _ => None,
        }
    }
}

/// What a recorded file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FileRole {
    /// The edition's primary audio file.
    Audio,
    /// Another audio file of the edition: a part or a chapter file.
    Part,
    /// A chapter file of a title split by chapter.
    Chapter,
    /// The companion PDF.
    Pdf,
    /// Cover art.
    Cover,
    /// Any other file beside the audio (`.cue`, chapter JSON, metadata).
    Sidecar,
    /// A transcript or lyrics file.
    Transcript,
    /// An EPUB.
    Epub,
}

impl FileRole {
    /// Every role, in stored order (the `edition_files.role` CHECK list).
    pub const ALL: [Self; 8] = [
        Self::Audio,
        Self::Part,
        Self::Chapter,
        Self::Pdf,
        Self::Cover,
        Self::Sidecar,
        Self::Transcript,
        Self::Epub,
    ];

    /// The stored spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Part => "part",
            Self::Chapter => "chapter",
            Self::Pdf => "pdf",
            Self::Cover => "cover",
            Self::Sidecar => "sidecar",
            Self::Transcript => "transcript",
            Self::Epub => "epub",
        }
    }

    /// The role a stored spelling names; [`Self::Sidecar`] for an unknown
    /// one, the role that gates nothing.
    #[must_use]
    pub fn parse(stored: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|role| role.as_str() == stored)
            .unwrap_or(Self::Sidecar)
    }

    /// The role of the file at `key`, from its extension.
    ///
    /// # Arguments
    ///
    /// * `key` - The storage key.
    /// * `primary` - The edition's primary audio key, when known: another
    ///   audio file is then a [`Self::Part`]. Without it every audio file
    ///   is [`Self::Audio`].
    #[must_use]
    pub fn for_key(key: &str, primary: Option<&str>) -> Self {
        let name = key.rsplit('/').next().unwrap_or(key);
        let ext = name
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "m4b" | "m4a" | "mp3" | "mp4" | "aac" | "flac" | "ogg" | "oga" | "opus" | "wav" => {
                match primary {
                    Some(primary) if primary != key => Self::Part,
                    _ => Self::Audio,
                }
            }
            "pdf" => Self::Pdf,
            "epub" => Self::Epub,
            "jpg" | "jpeg" | "png" | "webp" | "gif" => Self::Cover,
            "vtt" | "srt" | "lrc" => Self::Transcript,
            _ => Self::Sidecar,
        }
    }
}

/// Who holds a recorded file key, and what the file is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditionFile {
    /// The edition (or branch title) the file belongs to.
    pub owner: FileOwner,
    /// The host upload id that claimed the key last, or [`ACQUIRE_WRITER`].
    pub writer: String,
    /// What the file is.
    pub role: FileRole,
    /// Lowercase hex SHA-256 of the content, when known (host uploads).
    pub sha256: Option<String>,
}

/// A row read back into an [`EditionFile`]; `None` for a row whose
/// `owner_kind` this build does not know, which holds its key against
/// every claim.
fn edition_file(row: edition_files::Model) -> Option<EditionFile> {
    Some(EditionFile {
        owner: FileOwner::from_row(&row.owner_kind, row.owner_id)?,
        writer: row.writer,
        role: FileRole::parse(&row.role),
        sha256: row.sha256,
    })
}

/// The condition matching a row that is still exactly `file`.
fn still(file: &EditionFile) -> Condition {
    Condition::all()
        .add(edition_files::Column::OwnerKind.eq(file.owner.kind()))
        .add(edition_files::Column::OwnerId.eq(file.owner.id()))
        .add(edition_files::Column::Writer.eq(file.writer.as_str()))
}

impl LibraryStore {
    /// The edition a library title (`books.uuid`) resolves to, through its
    /// store listing; `None` when the title is unknown or not resolved yet.
    ///
    /// # Errors
    ///
    /// Returns an error when a read fails.
    pub async fn edition_of_book(&self, book_uuid: &str) -> Result<Option<String>> {
        let Some(book) = books::Entity::find()
            .filter(books::Column::Uuid.eq(book_uuid))
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?
        else {
            return Ok(None);
        };
        Ok(store_listings::Entity::find()
            .filter(store_listings::Column::PluginKey.eq(book.plugin_key))
            .filter(
                store_listings::Column::Marketplace
                    .eq(bookclerk_resolve::stored_marketplace(&book.marketplace)),
            )
            .filter(
                store_listings::Column::ProductId
                    .eq(bookclerk_resolve::stored_product_id(&book.product_id)),
            )
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?
            .map(|listing| listing.edition_id))
    }

    /// The file owner of a library title: its edition
    /// ([`Self::edition_of_book`]).
    ///
    /// # Errors
    ///
    /// Returns an error when a read fails.
    pub async fn file_owner_of_book(&self, book_uuid: &str) -> Result<Option<FileOwner>> {
        Ok(self
            .edition_of_book(book_uuid)
            .await?
            .map(FileOwner::Edition))
    }

    /// Every library title (`books.uuid`) whose listing sells `edition_id`:
    /// the account rows that share the edition's files.
    ///
    /// # Errors
    ///
    /// Returns an error when a read fails.
    pub async fn books_of_edition(&self, edition_id: &str) -> Result<BTreeSet<String>> {
        let listings = store_listings::Entity::find()
            .filter(store_listings::Column::EditionId.eq(edition_id))
            .all(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        let mut uuids = BTreeSet::new();
        for listing in listings {
            let rows = books::Entity::find()
                .filter(books::Column::PluginKey.eq(listing.plugin_key.as_str()))
                .filter(books::Column::ProductId.eq(listing.product_id.as_str()))
                .all(self.db())
                .await
                .map_err(LibraryError::Orm)?;
            uuids.extend(
                rows.into_iter()
                    .filter(|b| {
                        bookclerk_resolve::stored_marketplace(&b.marketplace) == listing.marketplace
                    })
                    .map(|b| b.uuid),
            );
        }
        Ok(uuids)
    }

    /// Records that `owner` writes `key` on `destination` (an acquire run
    /// on this node, just before the write).
    ///
    /// The record is taken when the key is unrecorded, already `owner`'s,
    /// or held by an owner no longer live (a removed edition, or a title a
    /// branch shares here, whose keys an acquire on this node takes over).
    /// Another live edition's key is refused, changing nothing: two
    /// editions whose short ids collide can plan one chapter or sidecar
    /// name the audio-name check does not see, and the write must not
    /// replace that edition's file. Each record is one conditional write,
    /// so of two acquires racing for one key only one wins.
    ///
    /// # Returns
    ///
    /// `true` when the key is now `owner`'s; `false` when another live
    /// edition holds it.
    ///
    /// # Errors
    ///
    /// Returns an error when a write fails.
    pub async fn record_edition_file(
        &self,
        destination: &str,
        key: &str,
        owner: &FileOwner,
        role: FileRole,
    ) -> Result<bool> {
        let now = now_str();
        edition_files::Entity::insert(edition_files::ActiveModel {
            destination: Set(destination.to_string()),
            storage_key: Set(key.to_string()),
            owner_kind: Set(owner.kind().to_string()),
            owner_id: Set(owner.id().to_string()),
            role: Set(role.as_str().to_string()),
            lossless: Set(0),
            bitrate_kbps: Set(None),
            sha256: Set(None),
            writer: Set(ACQUIRE_WRITER.to_string()),
            written_at: Set(now.clone()),
        })
        .on_conflict(
            OnConflict::columns([
                edition_files::Column::Destination,
                edition_files::Column::StorageKey,
            ])
            .do_nothing()
            .to_owned(),
        )
        .exec_without_returning(self.db())
        .await
        .map_err(LibraryError::Orm)?;
        let live_edition = Condition::all()
            .add(edition_files::Column::OwnerKind.eq("edition"))
            .add(
                edition_files::Column::OwnerId.in_subquery(
                    Query::select()
                        .column(editions::Column::Id)
                        .from(editions::Entity)
                        .to_owned(),
                ),
            );
        let mine = Condition::all()
            .add(edition_files::Column::OwnerKind.eq(owner.kind()))
            .add(edition_files::Column::OwnerId.eq(owner.id()));
        let recorded = edition_files::Entity::update_many()
            .col_expr(edition_files::Column::OwnerKind, Expr::value(owner.kind()))
            .col_expr(edition_files::Column::OwnerId, Expr::value(owner.id()))
            .col_expr(edition_files::Column::Role, Expr::value(role.as_str()))
            .col_expr(edition_files::Column::Lossless, Expr::value(0))
            .col_expr(
                edition_files::Column::BitrateKbps,
                Expr::value(Option::<i64>::None),
            )
            .col_expr(
                edition_files::Column::Sha256,
                Expr::value(Option::<String>::None),
            )
            .col_expr(edition_files::Column::Writer, Expr::value(ACQUIRE_WRITER))
            .col_expr(edition_files::Column::WrittenAt, Expr::value(now))
            .filter(edition_files::Column::Destination.eq(destination))
            .filter(edition_files::Column::StorageKey.eq(key))
            .filter(Condition::any().add(mine).add(live_edition.not()))
            .exec(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(recorded.rows_affected > 0)
    }

    /// Claims `key` on `destination` for `owner`, with `writer` (a host
    /// upload id) as its latest writer.
    ///
    /// The claim succeeds when the key is unrecorded, already the owner's
    /// (any account's row of the same edition), or recorded for an owner
    /// that holds no key against this claim; answers false, changing
    /// nothing, when another edition holds it. An edition holds its keys
    /// against every claim while it exists. A title a branch shares here
    /// holds its keys only against another branch title's claim
    /// (`branch_titles_hold`, a catalog upload): a Host job's upload of a
    /// book takes them over, as an acquire on this node would. Each claim
    /// is one conditional write, so of two uploads of different editions
    /// racing for one key only one wins.
    ///
    /// `expected` is the record the caller read before
    /// ([`Self::edition_file`]; `None` when unrecorded), which a refused
    /// claim's release restores: the claim also answers false, changing
    /// nothing, when the record is no longer that one (an acquire recorded
    /// the key in between), so a release never removes a record the claim
    /// did not replace.
    ///
    /// # Arguments
    ///
    /// * `destination`, `key` - The file's place.
    /// * `owner` - Who claims it.
    /// * `writer` - The host upload id.
    /// * `role`, `sha256` - What the file is and its declared digest.
    /// * `expected` - The record read before the claim.
    /// * `branch_titles_hold` - True for a catalog upload's claim.
    /// * `title` - The library title (`books.uuid`) a job upload writes
    ///   for. A key its edition already holds is taken over only when this
    ///   upload or an earlier upload of the same title wrote it, never one
    ///   an acquire or another account's upload wrote: the edition's files
    ///   are shared, so one account's node cannot replace them. `None` for
    ///   a catalog upload, whose branch title holds its keys alone.
    ///
    /// # Errors
    ///
    /// Returns an error when a write fails.
    #[allow(clippy::too_many_arguments)]
    pub async fn claim_edition_file(
        &self,
        destination: &str,
        key: &str,
        owner: &FileOwner,
        writer: &str,
        role: FileRole,
        sha256: Option<&str>,
        expected: Option<&EditionFile>,
        branch_titles_hold: bool,
        title: Option<&str>,
    ) -> Result<bool> {
        let now = now_str();
        // Only a key read as unrecorded is inserted: a claim expecting a
        // record that has since gone must change nothing, so its retry
        // reads the key afresh.
        if expected.is_none() {
            edition_files::Entity::insert(edition_files::ActiveModel {
                destination: Set(destination.to_string()),
                storage_key: Set(key.to_string()),
                owner_kind: Set(owner.kind().to_string()),
                owner_id: Set(owner.id().to_string()),
                role: Set(role.as_str().to_string()),
                lossless: Set(0),
                bitrate_kbps: Set(None),
                sha256: Set(sha256.map(str::to_string)),
                writer: Set(writer.to_string()),
                written_at: Set(now.clone()),
            })
            .on_conflict(
                OnConflict::columns([
                    edition_files::Column::Destination,
                    edition_files::Column::StorageKey,
                ])
                .do_nothing()
                .to_owned(),
            )
            .exec_without_returning(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        }
        // A holder is live while it is in the library: an edition that
        // exists, or, against a catalog upload's claim, a title a branch
        // shares here. Only a holder that is neither is stale.
        let live_edition = Condition::all()
            .add(edition_files::Column::OwnerKind.eq("edition"))
            .add(
                edition_files::Column::OwnerId.in_subquery(
                    Query::select()
                        .column(editions::Column::Id)
                        .from(editions::Entity)
                        .to_owned(),
                ),
            );
        let mut live = Condition::any().add(live_edition);
        if branch_titles_hold {
            live = live.add(
                Condition::all()
                    .add(edition_files::Column::OwnerKind.eq("branch_title"))
                    .add(
                        edition_files::Column::OwnerId.in_subquery(
                            Query::select()
                                .column(branch_titles::Column::Uuid)
                                .from(branch_titles::Entity)
                                .to_owned(),
                        ),
                    ),
            );
        }
        let mut mine = Condition::all()
            .add(edition_files::Column::OwnerKind.eq(owner.kind()))
            .add(edition_files::Column::OwnerId.eq(owner.id()));
        if let Some(title) = title {
            mine = mine.add(
                Condition::any()
                    .add(edition_files::Column::Writer.eq(writer))
                    .add(
                        edition_files::Column::Writer.in_subquery(
                            Query::select()
                                .column(node_upload_sessions::Column::UploadId)
                                .from(node_upload_sessions::Entity)
                                .and_where(node_upload_sessions::Column::BookUuid.eq(title))
                                .to_owned(),
                        ),
                    ),
            );
        }
        let claimed = edition_files::Entity::update_many()
            .col_expr(edition_files::Column::OwnerKind, Expr::value(owner.kind()))
            .col_expr(edition_files::Column::OwnerId, Expr::value(owner.id()))
            .col_expr(edition_files::Column::Role, Expr::value(role.as_str()))
            .col_expr(
                edition_files::Column::Sha256,
                Expr::value(sha256.map(str::to_string)),
            )
            .col_expr(edition_files::Column::Writer, Expr::value(writer))
            .col_expr(edition_files::Column::WrittenAt, Expr::value(now))
            .filter(edition_files::Column::Destination.eq(destination))
            .filter(edition_files::Column::StorageKey.eq(key))
            .filter(Condition::any().add(mine).add(live.not()))
            // Unrecorded before: only the row the insert above created.
            .filter(match expected {
                Some(prior) => still(prior),
                None => Condition::all()
                    .add(edition_files::Column::OwnerKind.eq(owner.kind()))
                    .add(edition_files::Column::OwnerId.eq(owner.id()))
                    .add(edition_files::Column::Writer.eq(writer)),
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
    pub async fn release_edition_file(
        &self,
        destination: &str,
        key: &str,
        writer: &str,
        prior: Option<&EditionFile>,
    ) -> Result<()> {
        let ours = Condition::all()
            .add(edition_files::Column::Destination.eq(destination))
            .add(edition_files::Column::StorageKey.eq(key))
            .add(edition_files::Column::Writer.eq(writer));
        match prior {
            None => {
                edition_files::Entity::delete_many()
                    .filter(ours)
                    .exec(self.db())
                    .await
                    .map_err(LibraryError::Orm)?;
            }
            Some(prior) => {
                edition_files::Entity::update_many()
                    .col_expr(
                        edition_files::Column::OwnerKind,
                        Expr::value(prior.owner.kind()),
                    )
                    .col_expr(
                        edition_files::Column::OwnerId,
                        Expr::value(prior.owner.id()),
                    )
                    .col_expr(
                        edition_files::Column::Role,
                        Expr::value(prior.role.as_str()),
                    )
                    .col_expr(
                        edition_files::Column::Sha256,
                        Expr::value(prior.sha256.clone()),
                    )
                    .col_expr(
                        edition_files::Column::Writer,
                        Expr::value(prior.writer.as_str()),
                    )
                    .filter(ours)
                    .exec(self.db())
                    .await
                    .map_err(LibraryError::Orm)?;
            }
        }
        Ok(())
    }

    /// How many keys `writer` (a host upload id) holds, on every
    /// destination together. An upload id is unique and claims keys for one
    /// owner only, so this is how many keys that upload's title still holds
    /// through it.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn count_edition_files_by(&self, writer: &str) -> Result<u64> {
        use sea_orm::PaginatorTrait as _;
        edition_files::Entity::find()
            .filter(edition_files::Column::Writer.eq(writer))
            .count(self.db())
            .await
            .map_err(LibraryError::Orm)
    }

    /// Who holds `key` on `destination`; `None` when it is unrecorded.
    ///
    /// A row naming an owner kind this build does not know reads as held
    /// by an edition no claim matches, so it is never taken over.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn edition_file(&self, destination: &str, key: &str) -> Result<Option<EditionFile>> {
        let row = edition_files::Entity::find_by_id((destination.to_string(), key.to_string()))
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(row.map(|row| {
            let writer = row.writer.clone();
            edition_file(row).unwrap_or(EditionFile {
                owner: FileOwner::Edition(String::new()),
                writer,
                role: FileRole::Sidecar,
                sha256: None,
            })
        }))
    }

    /// The audio keys (primary audio, parts and chapters) recorded for
    /// `owner` on `destination`, in key order: every file a copy of the
    /// edition's recording there needs.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn audio_keys_of(&self, destination: &str, owner: &FileOwner) -> Result<Vec<String>> {
        let rows = edition_files::Entity::find()
            .filter(edition_files::Column::Destination.eq(destination))
            .filter(edition_files::Column::OwnerKind.eq(owner.kind()))
            .filter(edition_files::Column::OwnerId.eq(owner.id()))
            .filter(edition_files::Column::Role.is_in([
                FileRole::Audio.as_str(),
                FileRole::Part.as_str(),
                FileRole::Chapter.as_str(),
            ]))
            .order_by_asc(edition_files::Column::StorageKey)
            .all(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(rows.into_iter().map(|row| row.storage_key).collect())
    }

    /// True when another live edition's file is recorded, on any
    /// destination, at the name `stem` (a planned key without its
    /// extension): the key `stem` itself, `stem` with an extension in the
    /// same folder (`stem.cue`), or one of its part files
    /// (`stem — Part 1.m4b`). Another edition's files already live under
    /// that name, so an acquire of `owner` is refused rather than overwrite
    /// them. A longer name that only starts with `stem` (`stem Peaks.m4b`)
    /// is not the same name. A branch title's file, or a file of an edition
    /// no longer in the library, does not count: an acquire takes those
    /// keys over, as [`LibraryStore::claim_edition_file`] does.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn name_held_by_another_owner(&self, stem: &str, owner: &FileOwner) -> Result<bool> {
        use sea_orm::sea_query::LikeExpr;
        use sea_orm::PaginatorTrait as _;
        let base = like_escaped(stem);
        let with_extension = Condition::all()
            .add(
                edition_files::Column::StorageKey
                    .like(LikeExpr::new(format!("{base}.%")).escape('\\')),
            )
            .add(
                edition_files::Column::StorageKey
                    .not_like(LikeExpr::new(format!("{base}.%/%")).escape('\\')),
            );
        let part = Condition::all()
            .add(
                edition_files::Column::StorageKey
                    .like(LikeExpr::new(format!("{base} — %")).escape('\\')),
            )
            .add(
                edition_files::Column::StorageKey
                    .not_like(LikeExpr::new(format!("{base} — %/%")).escape('\\')),
            );
        let named = Condition::any()
            .add(edition_files::Column::StorageKey.eq(stem))
            .add(with_extension)
            .add(part);
        let mine = Condition::all()
            .add(edition_files::Column::OwnerKind.eq(owner.kind()))
            .add(edition_files::Column::OwnerId.eq(owner.id()));
        let live_edition = Condition::all()
            .add(edition_files::Column::OwnerKind.eq("edition"))
            .add(
                edition_files::Column::OwnerId.in_subquery(
                    Query::select()
                        .column(editions::Column::Id)
                        .from(editions::Entity)
                        .to_owned(),
                ),
            );
        let others = edition_files::Entity::find()
            .filter(named)
            .filter(mine.not())
            .filter(live_edition)
            .count(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(others > 0)
    }
}

/// `text` with the `LIKE` wildcards (`%`, `_`) and the escape character
/// itself escaped with `\`, for a pattern matched with that escape.
pub(crate) fn like_escaped(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len() + 2);
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_follow_the_extension() {
        assert_eq!(FileRole::for_key("A/T/T.m4b", None), FileRole::Audio);
        assert_eq!(
            FileRole::for_key("A/T/T - 02.mp3", Some("A/T/T - 01.mp3")),
            FileRole::Part
        );
        assert_eq!(FileRole::for_key("A/T/T.PDF", None), FileRole::Pdf);
        assert_eq!(FileRole::for_key("A/T/cover.jpg", None), FileRole::Cover);
        assert_eq!(FileRole::for_key("A/T/T.vtt", None), FileRole::Transcript);
        assert_eq!(FileRole::for_key("A/T/T.epub", None), FileRole::Epub);
        assert_eq!(FileRole::for_key("A/T/T.cue", None), FileRole::Sidecar);
        assert_eq!(FileRole::for_key("A/T/README", None), FileRole::Sidecar);
    }

    #[test]
    fn stored_spellings_round_trip() {
        for role in FileRole::ALL {
            assert_eq!(FileRole::parse(role.as_str()), role);
        }
        assert_eq!(FileRole::parse("unknown"), FileRole::Sidecar);
        assert_eq!(
            FileOwner::from_row("edition", "e1".into()),
            Some(FileOwner::Edition("e1".into()))
        );
        assert_eq!(FileOwner::from_row("x", "e1".into()), None);
    }
}
