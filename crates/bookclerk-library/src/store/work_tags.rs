//! Library tags (`work_tags`): labels on a top-level work that everyone
//! sees, set by an Administrator or read from a sidecar file.
//!
//! A person's own tags (`user_work_tags`) are separate and private. Library
//! tags reach the page index as facet `tag`, so the Library filters by them
//! and a patron scope can include or exclude a tag (see
//! `docs/adr/patron-scopes.md`).

use chrono::Utc;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use serde::{Deserialize, Serialize};

use super::library_index::facet_key;
use super::LibraryStore;
use crate::control_plane::batch;
use crate::entities::{work_tags, works};
use crate::error::{EntityKind, LibraryError, Result};

/// Most tags an Administrator may set on one work. With the delete this
/// fits one batch of the smallest statement count a database plugin may
/// advertise (40).
pub const MAX_WORK_TAGS: usize = 32;

/// Longest tag, in UTF-8 bytes after trimming: the longest value the page
/// index keeps under a facet, and the longest value a scope rule matches.
pub const MAX_WORK_TAG_BYTES: usize = 256;

/// Where a library tag came from (`work_tags.source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkTagSource {
    /// Set in Bookclerk by an Owner, an Administrator or the operator.
    Admin,
    /// Read from a metadata file beside the book.
    Sidecar,
}

impl WorkTagSource {
    /// The stored value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Sidecar => "sidecar",
        }
    }

    /// Parses a stored value.
    fn parse(stored: &str) -> Option<Self> {
        match stored {
            "admin" => Some(Self::Admin),
            "sidecar" => Some(Self::Sidecar),
            _ => None,
        }
    }
}

/// One library tag on a work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct WorkTag {
    /// The tag as written.
    #[schema(example = "Kids")]
    pub value: String,
    /// Where it came from.
    pub source: WorkTagSource,
}

/// Trims, drops empties and duplicates (by [`facet_key`], first spelling
/// kept) and checks the bounds.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for more than [`MAX_WORK_TAGS`] tags or
/// a tag longer than [`MAX_WORK_TAG_BYTES`].
pub fn normalize_work_tags(values: &[String]) -> Result<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();
    for value in values {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        if value.len() > MAX_WORK_TAG_BYTES {
            return Err(LibraryError::Invalid(format!(
                "a library tag is at most {MAX_WORK_TAG_BYTES} bytes"
            )));
        }
        let key = facet_key(value);
        if !out.iter().any(|(k, _)| *k == key) {
            out.push((key, value.to_string()));
        }
    }
    if out.len() > MAX_WORK_TAGS {
        return Err(LibraryError::Invalid(format!(
            "a work has at most {MAX_WORK_TAGS} library tags"
        )));
    }
    Ok(out)
}

/// `PUT /api/library/works/{id}/tags` body: the tags an Administrator sets.
#[derive(Debug, utoipa::ToSchema)]
// Read by the schema only (no unknown fields); `WorkTagsFields` enforces it.
#[serde(deny_unknown_fields)]
pub struct WorkTagsBody {
    /// The tags, replacing the ones set in Bookclerk (sidecar tags stay).
    /// Blanks and repeats, ignoring case, are dropped; at most 32 tags of
    /// at most 256 bytes each.
    #[schema(example = json!(["Kids", "Bedtime"]), max_items = 32)]
    tags: Vec<String>,
}

/// [`WorkTagsBody`]'s fields as JSON carries them; read only from a JSON
/// object ([`WorkTagsBody`]'s `Deserialize`), never from an array.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkTagsFields {
    /// See [`WorkTagsBody::tags`].
    tags: Vec<String>,
}

impl<'de> Deserialize<'de> for WorkTagsBody {
    /// Reads the body from a JSON object only.
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let fields: WorkTagsFields =
            crate::control_plane::from_object(deserializer, "a JSON object with tags")?;
        Ok(Self { tags: fields.tags })
    }
}

/// A validated [`WorkTagsBody`]: at most [`MAX_WORK_TAGS`] distinct tags
/// (by [`facet_key`]), trimmed, each 1 to [`MAX_WORK_TAG_BYTES`] bytes, in
/// the order sent with the first spelling kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkTagsRequest {
    /// The tags as written.
    tags: Vec<String>,
}

impl WorkTagsRequest {
    /// The tags as written, trimmed and distinct.
    #[must_use]
    pub fn tags(&self) -> &[String] {
        &self.tags
    }
}

impl WorkTagsBody {
    /// Checks the body ([`normalize_work_tags`]).
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Invalid`] for too many or too long tags.
    pub fn validate(self) -> Result<WorkTagsRequest> {
        Ok(WorkTagsRequest {
            tags: normalize_work_tags(&self.tags)?
                .into_iter()
                .map(|(_, value)| value)
                .collect(),
        })
    }
}

/// Parses and validates a `PUT /api/library/works/{id}/tags` body from its
/// raw bytes, sans IO, the way the route reads it: JSON into
/// [`WorkTagsBody`], then [`WorkTagsBody::validate`].
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] for bytes that are not the body's JSON
/// shape (unknown or repeated fields, wrong types, a missing field, an
/// array instead of an object) or tags outside the bounds.
pub fn parse_work_tags_request(body: &[u8]) -> Result<WorkTagsRequest> {
    serde_json::from_slice::<WorkTagsBody>(body)
        .map_err(|err| LibraryError::Invalid(format!("tags body: {err}")))?
        .validate()
}

impl LibraryStore {
    /// The top-level work `work_id`, or a not-found error for a missing
    /// work or a podcast episode (tags go on its show).
    async fn tag_target(&self, work_id: &str) -> Result<works::Model> {
        let work = works::Entity::find_by_id(work_id.to_string())
            .one(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        match work {
            Some(work) if work.parent_work_id.is_none() => Ok(work),
            _ => Err(LibraryError::not_found(EntityKind::Work, work_id)),
        }
    }

    /// A work's library tags, every source, in key order (a key two sources
    /// share is listed once per source).
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::NotFound`] for a missing work or an episode,
    /// or the read error.
    pub async fn work_tags(&self, work_id: &str) -> Result<Vec<WorkTag>> {
        self.tag_target(work_id).await?;
        let rows = work_tags::Entity::find()
            .filter(work_tags::Column::WorkId.eq(work_id))
            .order_by_asc(work_tags::Column::Key)
            .order_by_asc(work_tags::Column::Source)
            .all(self.db())
            .await
            .map_err(LibraryError::Orm)?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                WorkTagSource::parse(&row.source).map(|source| WorkTag {
                    value: row.value,
                    source,
                })
            })
            .collect())
    }

    /// Replaces the tags an Administrator set on a work (sidecar tags stay)
    /// and refreshes the work's page index row, so the Library filter and
    /// patron scopes see the change at once.
    ///
    /// The replacement is one atomic batch. The caller checks who may set
    /// tags.
    ///
    /// # Arguments
    ///
    /// * `work_id` - A top-level work (`works.id`).
    /// * `values` - The tags; blanks and repeats (ignoring case) are
    ///   dropped.
    ///
    /// # Returns
    ///
    /// The work's tags afterwards, every source.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::NotFound`] for a missing work or an episode,
    /// [`LibraryError::Invalid`] for too many or too long tags, or the
    /// read or write error.
    pub async fn set_work_tags(&self, work_id: &str, values: &[String]) -> Result<Vec<WorkTag>> {
        let tags = normalize_work_tags(values)?;
        self.tag_target(work_id).await?;
        let now = Utc::now().to_rfc3339();
        let source = WorkTagSource::Admin.as_str();
        let mut statements = vec![batch::exec(
            "DELETE FROM work_tags WHERE work_id = ? AND source = ?",
            vec![batch::text(work_id), batch::text(source)],
        )];
        for (key, value) in &tags {
            statements.push(batch::exec(
                "INSERT OR IGNORE INTO work_tags (work_id, source, key, value, created_at) \
                 SELECT ?, ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM works WHERE id = ?)",
                vec![
                    batch::text(work_id),
                    batch::text(source),
                    batch::text(key),
                    batch::text(value),
                    batch::text(&now),
                    batch::text(work_id),
                ],
            ));
        }
        let operation_id = format!("work-tags-{}", uuid::Uuid::new_v4().simple());
        self.execute_host_batch(batch::request(&operation_id, statements))
            .await?;
        self.refresh_library_works(&[work_id.to_string()]).await?;
        self.work_tags(work_id).await
    }
}

#[cfg(test)]
#[path = "work_tags_tests.rs"]
mod tests;
