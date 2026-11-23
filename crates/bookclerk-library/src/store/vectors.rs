//! Text vectors in the `embeddings` table: the one durable copy.
//!
//! A vector belongs to a [`VectorSpace`]: the model that made it and that
//! model's revision. Vectors are compared only inside one space, so every
//! read here names the space it wants. Two kinds of target are stored:
//! works ([`VECTOR_KIND_WORK`], keyed by `works.id`) and plain texts
//! ([`VECTOR_KIND_TEXT`], keyed by the SHA-256 of the text, so every
//! candidate with the same text shares one vector).
//!
//! See `docs/adr/ai-models.md`, *Vectors are stored once and searched in
//! memory*, and `docs/internals/discovery.md`, *Embeddings*.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sea_orm::sea_query::{Expr, OnConflict, Query};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set};

use super::library_model::find_chunked;
use super::{now_str, LibraryStore};
use crate::entities::{embeddings, works};
use crate::error::{LibraryError, Result};

/// `embeddings.target_kind` of a work's vector; the target id is `works.id`.
pub const VECTOR_KIND_WORK: &str = "work";

/// `embeddings.target_kind` of a plain text's vector; the target id is the
/// SHA-256 hex of the text.
pub const VECTOR_KIND_TEXT: &str = "text";

/// What made a set of vectors. Vectors of two different spaces are never
/// compared.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VectorSpace {
    /// What made the vectors: `@<plugin id>/<model id>` for a model a
    /// plugin serves, or the stand-in's own id.
    pub model: String,
    /// The model's digest of everything that decides its output (weights,
    /// tokenizer, language packs); empty for the stand-in.
    pub revision: String,
    /// BCP 47 tags of the languages the model was loaded for, joined by
    /// `,`. Recorded beside the vectors; the revision already covers it,
    /// so it takes no part in comparing two spaces.
    pub packs: String,
}

impl VectorSpace {
    /// Whether `other` is this space: the same model at the same revision.
    #[must_use]
    pub fn same_as(&self, other: &Self) -> bool {
        self.model == other.model && self.revision == other.revision
    }
}

/// How many work vectors one space holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceCoverage {
    /// The space.
    pub space: VectorSpace,
    /// Work vectors stored in it.
    pub works: u64,
    /// When the newest of them was written.
    pub newest: Option<DateTime<Utc>>,
}

/// One vector to store.
#[derive(Debug, Clone, Copy)]
pub struct NewVector<'a> {
    /// Id of the target within its kind.
    pub target_id: &'a str,
    /// The vector as little-endian `f32`s.
    pub vector: &'a [u8],
    /// SHA-256 hex of the text that was embedded.
    pub text_hash: &'a str,
}

/// The rows of `space`.
fn in_space(space: &VectorSpace) -> sea_orm::Condition {
    sea_orm::Condition::all()
        .add(embeddings::Column::Model.eq(space.model.as_str()))
        .add(embeddings::Column::Revision.eq(space.revision.as_str()))
}

impl LibraryStore {
    /// Stores vectors of one kind in one space, replacing the vector a
    /// target already has there.
    ///
    /// # Arguments
    ///
    /// * `target_kind` - [`VECTOR_KIND_WORK`] or [`VECTOR_KIND_TEXT`].
    /// * `space` - The space the vectors belong to.
    /// * `dims` - Numbers in each vector.
    /// * `vectors` - The vectors, one statement each.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a write fails; the vectors before
    /// it are stored.
    pub async fn put_vectors(
        &self,
        target_kind: &str,
        space: &VectorSpace,
        dims: i64,
        vectors: &[NewVector<'_>],
    ) -> Result<()> {
        for new in vectors {
            let now = now_str();
            let row = embeddings::ActiveModel {
                id: sea_orm::NotSet,
                target_kind: Set(target_kind.to_string()),
                target_id: Set(new.target_id.to_string()),
                model: Set(space.model.clone()),
                revision: Set(space.revision.clone()),
                packs: Set(space.packs.clone()),
                dims: Set(dims),
                vector: Set(new.vector.to_vec()),
                text_hash: Set(new.text_hash.to_string()),
                created_at: Set(now.clone()),
                updated_at: Set(now),
            };
            embeddings::Entity::insert(row)
                .on_conflict(
                    OnConflict::columns([
                        embeddings::Column::TargetKind,
                        embeddings::Column::TargetId,
                        embeddings::Column::Model,
                        embeddings::Column::Revision,
                    ])
                    .update_columns([
                        embeddings::Column::Packs,
                        embeddings::Column::Dims,
                        embeddings::Column::Vector,
                        embeddings::Column::TextHash,
                        embeddings::Column::UpdatedAt,
                    ])
                    .to_owned(),
                )
                .exec_without_returning(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
        }
        Ok(())
    }

    /// Stored text hashes of targets in one space, keyed by target id.
    /// Targets with no vector there are absent.
    ///
    /// Reads in chunks sized from the adapter's limits, so one call
    /// replaces a lookup per target.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn vector_text_hashes(
        &self,
        target_kind: &str,
        space: &VectorSpace,
        target_ids: &[String],
    ) -> Result<HashMap<String, String>> {
        /// One stored hash, decoded by column name: the host database proxy
        /// does not keep the select order, so a positional tuple read could
        /// swap the columns.
        #[derive(Debug, sea_orm::FromQueryResult)]
        struct StoredHash {
            /// `embeddings.target_id`.
            target_id: String,
            /// `embeddings.text_hash`.
            text_hash: String,
        }
        let mut hashes = HashMap::with_capacity(target_ids.len());
        // One row per target in a space: a chunk no wider than one reply's
        // rows fits it.
        let width = self
            .lookup_chunk()
            .min(usize::try_from(super::keyset::result_page(self)).unwrap_or(usize::MAX));
        for chunk in target_ids.chunks(width) {
            let rows = embeddings::Entity::find()
                .select_only()
                .column(embeddings::Column::TargetId)
                .column(embeddings::Column::TextHash)
                .filter(embeddings::Column::TargetKind.eq(target_kind))
                .filter(in_space(space))
                .filter(embeddings::Column::TargetId.is_in(chunk.iter().cloned()))
                .into_model::<StoredHash>()
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
            hashes.extend(rows.into_iter().map(|row| (row.target_id, row.text_hash)));
        }
        Ok(hashes)
    }

    /// Stored vectors of targets in one space, keyed by target id, as
    /// little-endian `f32` bytes. Targets with no vector there are absent.
    ///
    /// Reads in chunks that narrow until each reply fits the adapter's
    /// limits, so a space of long vectors is still read.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn vectors(
        &self,
        target_kind: &str,
        space: &VectorSpace,
        target_ids: &[String],
    ) -> Result<HashMap<String, Vec<u8>>> {
        let rows = find_chunked(self, target_ids, |part| {
            embeddings::Entity::find()
                .filter(embeddings::Column::TargetKind.eq(target_kind))
                .filter(in_space(space))
                .filter(embeddings::Column::TargetId.is_in(part.iter().cloned()))
        })
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| (row.target_id, row.vector))
            .collect())
    }

    /// The spaces that hold work vectors, with how many each holds.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn work_vector_spaces(&self) -> Result<Vec<SpaceCoverage>> {
        /// One space's group, decoded by column name (the host database
        /// proxy does not keep the select order).
        #[derive(Debug, sea_orm::FromQueryResult)]
        struct Group {
            /// `embeddings.model`.
            model: String,
            /// `embeddings.revision`.
            revision: String,
            /// The group's newest `embeddings.packs`.
            packs: Option<String>,
            /// Work vectors in the group.
            n: i64,
            /// The group's newest `embeddings.updated_at`.
            newest: Option<String>,
        }
        // The groups are read in pages of at most one reply's rows, each
        // starting after the last space of the one before.
        let key = [embeddings::Column::Model, embeddings::Column::Revision];
        let page = super::keyset::result_page(self);
        let mut rows: Vec<Group> = Vec::new();
        loop {
            let mut query = embeddings::Entity::find()
                .select_only()
                .column(embeddings::Column::Model)
                .column(embeddings::Column::Revision)
                .column_as(embeddings::Column::Packs.max(), "packs")
                .column_as(embeddings::Column::Id.count(), "n")
                .column_as(embeddings::Column::UpdatedAt.max(), "newest")
                .filter(embeddings::Column::TargetKind.eq(VECTOR_KIND_WORK));
            if let Some(last) = rows.last() {
                query = query.filter(super::keyset::after_key(
                    &key,
                    &[last.model.clone().into(), last.revision.clone().into()],
                ));
            }
            let groups = query
                .group_by(embeddings::Column::Model)
                .group_by(embeddings::Column::Revision)
                .order_by_asc(embeddings::Column::Model)
                .order_by_asc(embeddings::Column::Revision)
                .limit(page)
                .into_model::<Group>()
                .all(&self.db)
                .await
                .map_err(LibraryError::from_db_err)?;
            let full = u64::try_from(groups.len()).unwrap_or(u64::MAX) >= page;
            rows.extend(groups);
            if !full {
                break;
            }
        }
        Ok(rows
            .into_iter()
            .map(|group| SpaceCoverage {
                space: VectorSpace {
                    model: group.model,
                    revision: group.revision,
                    packs: group.packs.unwrap_or_default(),
                },
                works: u64::try_from(group.n).unwrap_or(0),
                newest: group
                    .newest
                    .as_deref()
                    .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
                    .map(|at| at.with_timezone(&Utc)),
            })
            .collect())
    }

    /// How many works the library has.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the read fails.
    pub async fn count_works(&self) -> Result<u64> {
        /// The count, decoded by column name.
        #[derive(Debug, sea_orm::FromQueryResult)]
        struct Count {
            /// Rows of `works`.
            n: i64,
        }
        let count = works::Entity::find()
            .select_only()
            .column_as(works::Column::Id.count(), "n")
            .into_model::<Count>()
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(count.map_or(0, |count| u64::try_from(count.n).unwrap_or(0)))
    }

    /// Deletes work vectors (every space) whose work no longer exists, and
    /// answers how many it removed.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the delete fails.
    pub async fn prune_orphan_work_vectors(&self) -> Result<u64> {
        Ok(embeddings::Entity::delete_many()
            .filter(embeddings::Column::TargetKind.eq(VECTOR_KIND_WORK))
            .filter(
                embeddings::Column::TargetId.not_in_subquery(
                    Query::select()
                        .column(works::Column::Id)
                        .from(works::Entity)
                        .to_owned(),
                ),
            )
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .rows_affected)
    }

    /// Deletes every vector, of any kind, outside `keep` that was last
    /// written or used before `before`, and answers how many it removed.
    ///
    /// Called once `keep` holds a vector for every work, so the vectors of
    /// a model the library moved away from do not stay. The age limit
    /// leaves alone a space another node is still writing.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the delete fails.
    pub async fn prune_vectors_outside(
        &self,
        keep: &VectorSpace,
        before: DateTime<Utc>,
    ) -> Result<u64> {
        Ok(embeddings::Entity::delete_many()
            .filter(
                sea_orm::Condition::any()
                    .add(embeddings::Column::Model.ne(keep.model.as_str()))
                    .add(embeddings::Column::Revision.ne(keep.revision.as_str())),
            )
            .filter(embeddings::Column::UpdatedAt.lt(before.to_rfc3339()))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .rows_affected)
    }

    /// Deletes the text vectors (every space) last used before `before`,
    /// and answers how many it removed.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when the delete fails.
    pub async fn prune_text_vectors(&self, before: DateTime<Utc>) -> Result<u64> {
        Ok(embeddings::Entity::delete_many()
            .filter(embeddings::Column::TargetKind.eq(VECTOR_KIND_TEXT))
            .filter(embeddings::Column::UpdatedAt.lt(before.to_rfc3339()))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .rows_affected)
    }

    /// Marks text vectors of one space as used now, when their last use is
    /// older than `stale`, so [`Self::prune_text_vectors`] keeps what a
    /// ranking still reads without a write per read.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a write fails.
    pub async fn touch_text_vectors(
        &self,
        space: &VectorSpace,
        target_ids: &[String],
        stale: DateTime<Utc>,
    ) -> Result<()> {
        for chunk in target_ids.chunks(self.lookup_chunk()) {
            embeddings::Entity::update_many()
                .col_expr(embeddings::Column::UpdatedAt, Expr::value(now_str()))
                .filter(embeddings::Column::TargetKind.eq(VECTOR_KIND_TEXT))
                .filter(in_space(space))
                .filter(embeddings::Column::TargetId.is_in(chunk.iter().cloned()))
                .filter(embeddings::Column::UpdatedAt.lt(stale.to_rfc3339()))
                .exec(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
        }
        Ok(())
    }
}
