//! A work's long texts (description, subjects, categories) stored in pieces
//! when they do not fit its row on the database plugin.
//!
//! A text goes to `work_text_parts` when it is wider than one piece the
//! plugin carries (the smallest of its cell, statement and result limits)
//! or when keeping it inline would make the work's row wider than one
//! reply. Its own column then holds an empty string, and the row's
//! `text_version` names the pieces. Writes stage the pieces under a new
//! version first, then change the row with a compare-and-swap on its
//! `updated_at` and `text_version` that also requires every staged piece
//! to still be there. The same batch drops the version the row replaced
//! and, when the swap lost, the version it staged, each only while no row
//! names it; a write whose swap fails drops its staged version the same
//! guarded way. Pieces of a named version are never dropped and a swap
//! never names a version missing pieces, so a row never names pieces that
//! are gone, even when a swap the caller gave up on lands after the
//! clean-up, and a write that loses never touches another writer's staged
//! pieces. Readers join the pieces again
//! with [`LibraryStore::complete_work_texts`] and accept them only while
//! the row still names their version.
//!
//! A write that stops between staging and its swap (a crash or a failed
//! batch it could not clean up) leaves its staged pieces unnamed; they
//! take space but are never read.
//!
//! On a plugin without cell or result caps (the first-party SQLite one)
//! every text stays inline and nothing here changes what is stored.

use std::collections::BTreeMap;

use bookclerk_plugin_abi::TypedDbStatement;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use super::library_model::find_chunked_paged;
use super::write_batches::WriteBatches;
use super::{now_str, LibraryStore};
use crate::control_plane::batch;
use crate::entities::{work_text_parts, works};
use crate::error::{LibraryError, Result};

/// Room a work row takes in a reply beside its text columns: the reply's
/// framing and the column names.
const ROW_OVERHEAD: usize = 512;

/// Times a read or a write starts again when a concurrent write of the same
/// work replaced its texts in between.
const ATTEMPTS: usize = 3;

/// The long texts a work row carries, in the order they are placed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TextField {
    /// `works.description`.
    Description,
    /// `works.subjects`.
    Subjects,
    /// `works.categories`.
    Categories,
}

impl TextField {
    /// Every field, in placement order.
    const ALL: [Self; 3] = [Self::Description, Self::Subjects, Self::Categories];

    /// The stored `work_text_parts.field` value.
    fn as_str(self) -> &'static str {
        match self {
            Self::Description => "description",
            Self::Subjects => "subjects",
            Self::Categories => "categories",
        }
    }

    /// Parses a stored `work_text_parts.field` value.
    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|field| field.as_str() == value)
    }

    /// This field's value in `row`, to replace.
    fn of_mut(self, row: &mut works::Model) -> &mut Option<String> {
        match self {
            Self::Description => &mut row.description,
            Self::Subjects => &mut row.subjects,
            Self::Categories => &mut row.categories,
        }
    }
}

/// The values one write gives a work's texts and cover.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WorkTexts {
    /// Blurb or synopsis.
    pub(crate) description: Option<String>,
    /// Subject tags (serialized).
    pub(crate) subjects: Option<String>,
    /// Category labels (serialized).
    pub(crate) categories: Option<String>,
    /// HTTPS cover URL.
    pub(crate) cover_url: Option<String>,
}

impl WorkTexts {
    /// This write's value for `field`.
    fn field(&self, field: TextField) -> &Option<String> {
        match field {
            TextField::Description => &self.description,
            TextField::Subjects => &self.subjects,
            TextField::Categories => &self.categories,
        }
    }
}

/// Where one write puts a work's texts: the column values, the version
/// that names the pieces, and the piece inserts to run first.
struct Placed {
    /// The column values, with an empty string for a text in pieces.
    columns: BTreeMap<TextField, Option<String>>,
    /// The new `text_version`, `None` when every text is inline.
    version: Option<String>,
    /// One insert per piece.
    inserts: Vec<TypedDbStatement>,
}

/// The swap that names a work's new texts, only over the row the write
/// read.
///
/// It names the new version only while all its pieces are stored, so a swap
/// that lands after its own clean-up changes nothing.
const PUBLISH_SQL: &str = "UPDATE works SET description = ?, subjects = ?, categories = ?, \
     cover_url = ?, text_version = ?, updated_at = ? \
     WHERE id = ? AND updated_at = ? AND COALESCE(text_version, '') = ? \
     AND (SELECT COUNT(*) FROM work_text_parts WHERE work_id = ? AND version = ?) = ?";

/// Room in the publication batch for the statement that drops unnamed
/// versions: its SQL and its binds.
const DROP_ROOM: usize = 512;

/// Bytes `value` takes in a cell, unescaped.
fn width(value: Option<&str>) -> usize {
    value.map_or(0, str::len)
}

/// Bytes `value` takes in a reply, where a read counts each text as an
/// escaped JSON string: a quote or backslash costs two bytes and a control
/// character up to six.
fn encoded_width(value: Option<&str>) -> usize {
    value.map_or(0, |text| {
        text.chars()
            .map(|c| match c {
                '"' | '\\' | '\n' | '\r' | '\t' | '\u{08}' | '\u{0c}' => 2,
                c if u32::from(c) < 0x20 => 6,
                c => c.len_utf8(),
            })
            .sum::<usize>()
            .saturating_add(2)
    })
}

impl LibraryStore {
    /// Replaces the long texts of every row in `rows` that keeps them in
    /// pieces with the joined text.
    ///
    /// Pieces are read in keyset pages sized to the database plugin's
    /// result limits, then the rows are read again: a row's pieces are
    /// taken only while it still names their version, so pages read across
    /// a concurrent swap are never mixed. A row a concurrent write changed
    /// is completed from its new version, up to a few times.
    ///
    /// # Errors
    ///
    /// Returns the database error, or [`LibraryError::Unavailable`] when
    /// concurrent writes kept replacing a work's texts while it was read.
    pub(crate) async fn complete_work_texts(&self, rows: &mut [works::Model]) -> Result<()> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let pieced: Vec<usize> = (0..rows.len())
                .filter(|&at| rows[at].text_version.is_some())
                .collect();
            if pieced.is_empty() {
                return Ok(());
            }
            let ids: Vec<String> = pieced
                .iter()
                .map(|&at| rows[at].id.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            let mut by_version: BTreeMap<(String, String), Vec<work_text_parts::Model>> =
                BTreeMap::new();
            for piece in self.read_text_parts(&ids).await? {
                by_version
                    .entry((piece.work_id.clone(), piece.version.clone()))
                    .or_default()
                    .push(piece);
            }
            // Read the rows again: pieces count only for the version a row
            // still names, since a named version's pieces are never dropped.
            let fresh: BTreeMap<String, works::Model> =
                super::library_model::find_chunked::<works::Entity, _>(self, &ids, |part| {
                    works::Entity::find().filter(works::Column::Id.is_in(part.iter().cloned()))
                })
                .await?
                .into_iter()
                .map(|row| (row.id.clone(), row))
                .collect();
            let mut stale: Vec<usize> = Vec::new();
            for at in pieced {
                let row = &mut rows[at];
                let version = row.text_version.clone().unwrap_or_default();
                let named = fresh
                    .get(&row.id)
                    .is_some_and(|now| now.text_version.as_deref() == Some(version.as_str()));
                let texts = by_version
                    .get(&(row.id.clone(), version))
                    .filter(|_| named)
                    .and_then(|run| joined(run));
                match texts {
                    Some(texts) if !texts.is_empty() => {
                        for (field, text) in texts {
                            *field.of_mut(row) = Some(text);
                        }
                        row.text_version = None;
                    }
                    _ => stale.push(at),
                }
            }
            if stale.is_empty() {
                return Ok(());
            }
            if attempt >= ATTEMPTS {
                return Err(LibraryError::Unavailable(format!(
                    "the texts of work {} kept changing while they were read",
                    rows[stale[0]].id
                )));
            }
            // A later write replaced these texts: start from the new rows.
            for at in stale {
                if let Some(row) = fresh.get(&rows[at].id) {
                    rows[at] = row.clone();
                }
            }
        }
    }

    /// Every stored piece of the works `ids`, in key order.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    async fn read_text_parts(&self, ids: &[String]) -> Result<Vec<work_text_parts::Model>> {
        find_chunked_paged(
            self,
            ids,
            &[
                work_text_parts::Column::WorkId,
                work_text_parts::Column::Version,
                work_text_parts::Column::Field,
                work_text_parts::Column::Seq,
            ],
            |part| {
                work_text_parts::Entity::find()
                    .filter(work_text_parts::Column::WorkId.is_in(part.iter().cloned()))
            },
        )
        .await
    }

    /// Rewrites one work's texts and cover to what `merge` makes of the
    /// stored ones, placing each text inline or in pieces for the database
    /// plugin.
    ///
    /// `merge` sees the work's row with its texts completed and answers the
    /// values to store, or `None` to leave the work as it is. The row
    /// changes only if no other write changed it since it was read; when
    /// one did, the work is read and merged again, up to a few times.
    ///
    /// # Arguments
    ///
    /// * `work_id` - The work to rewrite.
    /// * `merge` - The values to store, given the stored row.
    ///
    /// # Returns
    ///
    /// `false` when the work does not exist.
    ///
    /// # Errors
    ///
    /// Returns the database error, or [`LibraryError::Conflict`] when other
    /// writes kept changing the work.
    pub(crate) async fn update_work_texts<F>(&self, work_id: &str, merge: F) -> Result<bool>
    where
        F: Fn(&works::Model) -> Option<WorkTexts>,
    {
        for _ in 0..ATTEMPTS {
            let Some(row) = works::Entity::find_by_id(work_id.to_string())
                .one(&self.db)
                .await
                .map_err(LibraryError::from_db_err)?
            else {
                return Ok(false);
            };
            let mut completed = [row.clone()];
            self.complete_work_texts(&mut completed).await?;
            let [completed] = completed;
            let Some(texts) = merge(&completed) else {
                return Ok(true);
            };
            if self.swap_work_texts(&row, &texts).await? {
                return Ok(true);
            }
        }
        Err(LibraryError::Conflict(format!(
            "work {work_id} kept changing while its texts were written"
        )))
    }

    /// Writes `texts` over `row`, as stored when read, if it is unchanged.
    ///
    /// # Returns
    ///
    /// `false` when another write changed the row first; the pieces this
    /// write staged are dropped.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    async fn swap_work_texts(&self, row: &works::Model, texts: &WorkTexts) -> Result<bool> {
        let placed = self.place_work_texts(row, texts);
        let mut batches = WriteBatches::new(self.db_capabilities(), "work text piece");
        let mut staged = Vec::new();
        let pieces = placed.inserts.len();
        for insert in placed.inserts {
            staged.extend(batches.push(insert)?);
        }
        staged.extend(batches.finish());
        for statements in staged {
            let operation_id = format!("work-text-parts-{}", uuid::Uuid::new_v4());
            if let Err(err) = self
                .execute_host_batch(batch::request(&operation_id, statements))
                .await
            {
                self.drop_staged_version(row, placed.version.as_deref())
                    .await;
                return Err(err);
            }
        }
        let column = |field: TextField| {
            batch::text_opt(
                placed
                    .columns
                    .get(&field)
                    .and_then(|value| value.as_deref()),
            )
        };
        let operation_id = format!("work-texts-{}", uuid::Uuid::new_v4());
        let mut statements: Vec<TypedDbStatement> = lock_versions(
            &row.id,
            &placed.version.as_deref().into_iter().collect::<Vec<_>>(),
        )
        .into_iter()
        .collect();
        let swap_at = statements.len();
        statements.push(batch::exec(
            PUBLISH_SQL,
            vec![
                column(TextField::Description),
                column(TextField::Subjects),
                column(TextField::Categories),
                batch::text_opt(texts.cover_url.as_deref()),
                batch::text_opt(placed.version.as_deref()),
                batch::text(&now_str()),
                batch::text(&row.id),
                batch::text(&row.updated_at),
                batch::text(row.text_version.as_deref().unwrap_or_default()),
                batch::text(&row.id),
                batch::text(placed.version.as_deref().unwrap_or_default()),
                batch::int(i64::try_from(pieces).unwrap_or(i64::MAX)),
            ],
        ));
        let versions: Vec<&str> = [row.text_version.as_deref(), placed.version.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        statements.extend(drop_unnamed_versions(&row.id, &versions));
        match self
            .execute_host_batch(batch::request(&operation_id, statements))
            .await
        {
            Ok(reply) => Ok(batch::rows_affected(&reply, swap_at) > 0),
            Err(err) => {
                // The publication either never applied or applied with its
                // reply lost; the guarded drop keeps the version only in the
                // second case, where the row names it.
                self.drop_staged_version(row, placed.version.as_deref())
                    .await;
                Err(err)
            }
        }
    }

    /// Drops the pieces this write staged under `version` unless a row now
    /// names them, on a best-effort basis.
    ///
    /// A failed clean-up leaves only unnamed pieces, which are never read.
    ///
    /// # Arguments
    ///
    /// * `row` - The work row the write targeted.
    /// * `version` - The version the write staged, if its texts were pieced.
    async fn drop_staged_version(&self, row: &works::Model, version: Option<&str>) {
        let Some(version) = version else {
            return;
        };
        let operation_id = format!("work-text-parts-drop-{}", uuid::Uuid::new_v4());
        let _ = self
            .execute_host_batch(batch::request(
                &operation_id,
                drop_unnamed_versions(&row.id, &[version]),
            ))
            .await;
    }

    /// Places `texts` for `row`: inline while each fits one piece and the
    /// row, with its texts escaped as a reply carries them, still fits one
    /// reply; the rest in pieces under a new version.
    fn place_work_texts(&self, row: &works::Model, texts: &WorkTexts) -> Placed {
        let caps = self.db_capabilities();
        let piece = crate::control_plane::chunk_bytes(caps);
        // The other columns, the version a pieced row names and its new
        // `updated_at` (64 bytes each at most), and the reply's framing.
        let fixed = [
            row.id.as_str(),
            row.kind.as_str(),
            row.title.as_str(),
            row.match_key.as_str(),
            row.parent_work_id.as_deref().unwrap_or_default(),
            texts.cover_url.as_deref().unwrap_or_default(),
            row.created_at.as_str(),
            row.updated_at.as_str(),
        ]
        .iter()
        .map(|value| encoded_width(Some(value)))
        .sum::<usize>()
        .saturating_add(2 * 64 + ROW_OVERHEAD);
        // The inline texts travel escaped twice: in the swap statement,
        // under the payload and request caps, and in every later read of
        // the row, under the result cap. The host refuses a plugin whose
        // caps are below its minimums, so none is zero.
        let cap = |limit: u32, used: usize| {
            usize::try_from(limit)
                .unwrap_or(usize::MAX)
                .saturating_sub(used)
        };
        let mut budget = [
            cap(caps.max_result_bytes, 0),
            cap(caps.max_payload_bytes, PUBLISH_SQL.len()),
            cap(caps.max_request_bytes, PUBLISH_SQL.len() + DROP_ROOM),
        ]
        .into_iter()
        .fold(usize::MAX, usize::min)
        .saturating_sub(fixed);
        // The narrowest texts are kept inline first, so as many as fit stay
        // in the row.
        let mut order = TextField::ALL;
        order.sort_by_key(|&field| encoded_width(texts.field(field).as_deref()));
        let mut columns = BTreeMap::new();
        let mut pieced = Vec::new();
        for field in order {
            let value = texts.field(field);
            // The cell cap counts the text as stored; the reply cap counts
            // it escaped.
            let encoded = encoded_width(value.as_deref());
            if width(value.as_deref()) <= piece && encoded <= budget {
                budget -= encoded;
                columns.insert(field, value.clone());
            } else {
                columns.insert(field, Some(String::new()));
                pieced.push(field);
            }
        }
        if pieced.is_empty() {
            return Placed {
                columns,
                version: None,
                inserts: Vec::new(),
            };
        }
        let version = uuid::Uuid::new_v4().simple().to_string();
        let mut inserts = Vec::new();
        for field in pieced {
            let text = texts.field(field).as_deref().unwrap_or_default();
            let cut = crate::control_plane::cut_text(text, piece);
            let parts = i64::try_from(cut.len()).unwrap_or(i64::MAX);
            for (seq, body) in (0_i64..).zip(cut) {
                inserts.push(batch::exec(
                    "INSERT OR IGNORE INTO work_text_parts \
                     (work_id, version, field, seq, parts, body) VALUES (?, ?, ?, ?, ?, ?)",
                    vec![
                        batch::text(&row.id),
                        batch::text(&version),
                        batch::text(field.as_str()),
                        batch::int(seq),
                        batch::int(parts),
                        batch::text(body),
                    ],
                ));
            }
        }
        Placed {
            columns,
            version: Some(version),
            inserts,
        }
    }
}

/// Drops the pieces of `work_id`'s texts under `versions` that its row
/// does not name: the version a swap replaced, and the version a write
/// staged when its swap lost. A version the row names is kept, and so is
/// every other version, which another write may be staging.
///
/// # Returns
///
/// The statements that lock the versions' pieces and then drop the unnamed
/// ones; none when there is no version to drop.
fn drop_unnamed_versions(work_id: &str, versions: &[&str]) -> Vec<TypedDbStatement> {
    let Some(lock) = lock_versions(work_id, versions) else {
        return Vec::new();
    };
    let mut binds = vec![batch::text(work_id)];
    binds.extend(versions.iter().map(|version| batch::text(version)));
    binds.push(batch::text(work_id));
    let marks = vec!["?"; versions.len()].join(", ");
    vec![
        lock,
        batch::exec(
            &format!(
                "DELETE FROM work_text_parts WHERE work_id = ? AND version IN ({marks}) \
                 AND version NOT IN \
                 (SELECT text_version FROM works WHERE id = ? AND text_version IS NOT NULL)"
            ),
            binds,
        ),
    ]
}

/// Locks the pieces of `versions` for the rest of the batch, so a swap
/// naming one of them and a drop of it never overlap.
///
/// The swap and the drop each take these locks in a statement of their own
/// before the statement that decides, so on an engine that reads each
/// statement as of its start (PostgreSQL at `READ COMMITTED`) the deciding
/// statement starts after the other batch committed and sees its outcome:
/// a drop that went first leaves the swap counting missing pieces, and a
/// swap that went first leaves the drop seeing the row name its version.
///
/// # Returns
///
/// `None` when there is no version to lock.
fn lock_versions(work_id: &str, versions: &[&str]) -> Option<TypedDbStatement> {
    if versions.is_empty() {
        return None;
    }
    let mut binds = vec![batch::text(work_id)];
    binds.extend(versions.iter().map(|version| batch::text(version)));
    let marks = vec!["?"; versions.len()].join(", ");
    Some(batch::exec(
        &format!(
            "UPDATE work_text_parts SET body = body \
             WHERE work_id = ? AND version IN ({marks})"
        ),
        binds,
    ))
}

/// The texts one version's `pieces` join into, or `None` when a text's
/// pieces are not a gapless run of its piece count from 0.
fn joined(pieces: &[work_text_parts::Model]) -> Option<BTreeMap<TextField, String>> {
    let mut by_field: BTreeMap<TextField, Vec<&work_text_parts::Model>> = BTreeMap::new();
    for piece in pieces {
        by_field
            .entry(TextField::parse(&piece.field)?)
            .or_default()
            .push(piece);
    }
    let mut out = BTreeMap::new();
    for (field, mut run) in by_field {
        run.sort_by_key(|piece| piece.seq);
        let count = run.first().map(|piece| piece.parts)?;
        let gapless = i64::try_from(run.len()).ok() == Some(count)
            && (0_i64..)
                .zip(&run)
                .all(|(seq, piece)| piece.seq == seq && piece.parts == count);
        if !gapless {
            return None;
        }
        out.insert(
            field,
            run.into_iter().map(|piece| piece.body.as_str()).collect(),
        );
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use sea_orm::EntityTrait;

    use super::*;
    use crate::test_support::{sqlite, uncapped, Profile};
    use crate::{NewBook, WorkEnrichment};

    /// Text of about `bytes` bytes, multibyte so a piece's byte and
    /// character counts differ.
    fn long_text(bytes: usize) -> String {
        let mut out = String::new();
        while out.len() < bytes {
            out.push_str("é—x ");
        }
        out
    }

    /// A store under `profile` holding one work; answers the work's id.
    async fn one_work(profile: Profile) -> (LibraryStore, String) {
        let store = sqlite::memory(profile).await.into_store();
        store
            .upsert_account("audible", "a1", "us", None, true, "audible")
            .await
            .unwrap();
        let book = NewBook::minimal("B00X", "a1", "US", "Dune");
        let record = store.upsert_book(&book).await.unwrap();
        let work_id = store
            .work_of_holding(&record.holding_id)
            .await
            .unwrap()
            .unwrap();
        (store, work_id)
    }

    /// Every stored piece, in key order.
    async fn pieces(store: &LibraryStore) -> Vec<work_text_parts::Model> {
        let mut rows = uncapped(work_text_parts::Entity::find().all(store.db()))
            .await
            .unwrap();
        rows.sort_by(|a, b| (&a.version, &a.field, a.seq).cmp(&(&b.version, &b.field, b.seq)));
        rows
    }

    #[tokio::test]
    async fn long_work_texts_are_stored_in_pieces_under_every_profile() {
        for profile in Profile::ALL {
            let name = format!("{profile:?}");
            let (store, work_id) = one_work(profile).await;
            let piece = crate::control_plane::chunk_bytes(store.db_capabilities());
            let description = long_text(piece.saturating_mul(3).min(12_000));
            let subjects = long_text(piece.min(900));
            let categories = String::from("Science Fiction");
            store
                .set_work_enrichment(
                    &work_id,
                    &WorkEnrichment {
                        description: Some(description.clone()),
                        subjects: Some(subjects.clone()),
                        categories: Some(categories.clone()),
                        cover_url: Some("https://covers.example/dune.jpg".into()),
                    },
                )
                .await
                .unwrap();
            let work = store.get_work(&work_id).await.unwrap().unwrap();
            assert_eq!(
                work.description.as_deref(),
                Some(description.as_str()),
                "{name}"
            );
            assert_eq!(work.subjects.as_deref(), Some(subjects.as_str()), "{name}");
            assert_eq!(
                work.categories.as_deref(),
                Some(categories.as_str()),
                "{name}"
            );
            let row = uncapped(works::Entity::find_by_id(work_id.clone()).one(store.db()))
                .await
                .unwrap()
                .unwrap();
            let stored = pieces(&store).await;
            if description.len() > piece {
                assert_eq!(row.description.as_deref(), Some(""), "{name}");
                assert!(row.text_version.is_some(), "{name}");
                assert!(stored.iter().all(|p| p.body.len() <= piece), "{name}");
                assert!(
                    stored
                        .iter()
                        .all(|p| Some(&p.version) == row.text_version.as_ref()),
                    "{name}"
                );
            } else {
                assert!(row.text_version.is_none(), "{name}");
                assert!(stored.is_empty(), "{name}");
            }
            // The row fits one reply under the profile.
            assert!(
                store.list_works_page(None, 10).await.unwrap().len() == 1,
                "{name}"
            );

            // A short description takes the texts inline again and drops
            // every piece.
            store
                .set_work_enrichment(
                    &work_id,
                    &WorkEnrichment {
                        description: Some("Spice.".into()),
                        subjects: Some("Desert".into()),
                        ..WorkEnrichment::default()
                    },
                )
                .await
                .unwrap();
            let work = store.get_work(&work_id).await.unwrap().unwrap();
            assert_eq!(work.description.as_deref(), Some("Spice."), "{name}");
            assert_eq!(
                work.categories.as_deref(),
                Some(categories.as_str()),
                "{name}"
            );
            assert!(pieces(&store).await.is_empty(), "{name}");
        }
    }

    #[tokio::test]
    async fn a_write_over_a_changed_row_drops_what_it_staged() {
        let (store, work_id) = one_work(Profile::Minimum).await;
        let piece = crate::control_plane::chunk_bytes(store.db_capabilities());
        let stale = uncapped(works::Entity::find_by_id(work_id.clone()).one(store.db()))
            .await
            .unwrap()
            .unwrap();
        store
            .set_work_enrichment(
                &work_id,
                &WorkEnrichment {
                    description: Some("Spice.".into()),
                    ..WorkEnrichment::default()
                },
            )
            .await
            .unwrap();
        let applied = store
            .swap_work_texts(
                &stale,
                &WorkTexts {
                    description: Some(long_text(piece * 2)),
                    ..WorkTexts::default()
                },
            )
            .await
            .unwrap();
        assert!(!applied);
        assert!(pieces(&store).await.is_empty());
        let work = store.get_work(&work_id).await.unwrap().unwrap();
        assert_eq!(work.description.as_deref(), Some("Spice."));
        assert!(!store
            .update_work_texts("no-such-work", |_| None)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn long_categories_are_stored_in_pieces_and_read_back() {
        let (store, work_id) = one_work(Profile::Minimum).await;
        let piece = crate::control_plane::chunk_bytes(store.db_capabilities());
        let categories = long_text(piece * 2);
        store
            .set_work_enrichment(
                &work_id,
                &WorkEnrichment {
                    categories: Some(categories.clone()),
                    ..WorkEnrichment::default()
                },
            )
            .await
            .unwrap();
        let stored = pieces(&store).await;
        assert!(!stored.is_empty());
        assert!(stored.iter().all(|p| p.field == "categories"));
        let work = store.get_work(&work_id).await.unwrap().unwrap();
        assert_eq!(work.categories.as_deref(), Some(categories.as_str()));
    }

    #[tokio::test]
    async fn a_row_read_before_its_texts_were_replaced_is_read_again() {
        let (store, work_id) = one_work(Profile::Minimum).await;
        let piece = crate::control_plane::chunk_bytes(store.db_capabilities());
        let enrich = |description: String| WorkEnrichment {
            description: Some(description),
            ..WorkEnrichment::default()
        };
        store
            .set_work_enrichment(&work_id, &enrich(long_text(piece * 2)))
            .await
            .unwrap();
        let stale = uncapped(works::Entity::find_by_id(work_id.clone()).one(store.db()))
            .await
            .unwrap()
            .unwrap();
        let replacement = format!("{} Arrakis.", long_text(piece * 2));
        store
            .set_work_enrichment(&work_id, &enrich(replacement.clone()))
            .await
            .unwrap();
        let mut rows = [stale];
        store.complete_work_texts(&mut rows).await.unwrap();
        let [row] = rows;
        assert_eq!(row.description.as_deref(), Some(replacement.as_str()));
        assert!(row.text_version.is_none());
    }

    #[tokio::test]
    async fn a_work_removed_while_its_texts_were_read_is_refused() {
        let (store, work_id) = one_work(Profile::Minimum).await;
        let piece = crate::control_plane::chunk_bytes(store.db_capabilities());
        store
            .set_work_enrichment(
                &work_id,
                &WorkEnrichment {
                    description: Some(long_text(piece * 2)),
                    ..WorkEnrichment::default()
                },
            )
            .await
            .unwrap();
        let read = uncapped(works::Entity::find_by_id(work_id.clone()).one(store.db()))
            .await
            .unwrap()
            .unwrap();
        uncapped(work_text_parts::Entity::delete_many().exec(store.db()))
            .await
            .unwrap();
        uncapped(works::Entity::delete_by_id(work_id.clone()).exec(store.db()))
            .await
            .unwrap();
        let mut rows = [read];
        let err = store.complete_work_texts(&mut rows).await.unwrap_err();
        assert!(matches!(err, LibraryError::Unavailable(_)), "{err:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_work_that_keeps_changing_is_refused() {
        use sea_orm::sea_query::Expr;

        let (store, work_id) = one_work(Profile::Minimum).await;
        let db = store.db().clone();
        let err = store
            .update_work_texts(&work_id, |row| {
                // Another write changes the row after every read.
                let bumped = format!("{}+", row.updated_at);
                let id = row.id.clone();
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(async {
                        works::Entity::update_many()
                            .col_expr(works::Column::UpdatedAt, Expr::value(bumped))
                            .filter(works::Column::Id.eq(id))
                            .exec(&db)
                            .await
                            .unwrap();
                    });
                });
                Some(WorkTexts {
                    description: Some("Spice.".into()),
                    ..WorkTexts::default()
                })
            })
            .await
            .unwrap_err();
        assert!(matches!(err, LibraryError::Conflict(_)), "{err:?}");
        let work = store.get_work(&work_id).await.unwrap().unwrap();
        assert_ne!(work.description.as_deref(), Some("Spice."));
    }

    #[tokio::test]
    async fn a_failed_staging_batch_drops_what_it_staged() {
        let kit = sqlite::memory(Profile::Minimum).await;
        let store = kit.store();
        store
            .upsert_account("audible", "a1", "us", None, true, "audible")
            .await
            .unwrap();
        let record = store
            .upsert_book(&NewBook::minimal("B00X", "a1", "US", "Dune"))
            .await
            .unwrap();
        let work_id = store
            .work_of_holding(&record.holding_id)
            .await
            .unwrap()
            .unwrap();
        let piece = crate::control_plane::chunk_bytes(store.db_capabilities());
        // The first staging batch is applied but its reply is lost.
        kit.lose_next_replies(1);
        let err = store
            .set_work_enrichment(
                &work_id,
                &WorkEnrichment {
                    description: Some(long_text(piece * 40)),
                    ..WorkEnrichment::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, LibraryError::Unavailable(_)), "{err:?}");
        assert!(pieces(store).await.is_empty());
        let work = store.get_work(&work_id).await.unwrap().unwrap();
        assert!(work.description.is_none());
    }

    #[tokio::test]
    async fn texts_that_escape_wider_still_fit_one_reply_under_every_profile() {
        for profile in Profile::ALL {
            let name = format!("{profile:?}");
            let (store, work_id) = one_work(profile).await;
            let piece = crate::control_plane::chunk_bytes(store.db_capabilities());
            // Each text fits one cell as stored but takes twice its bytes,
            // or more, once a reply escapes it.
            let escaped = |fill: &str| {
                let mut text = String::from("\u{1}\"");
                while text.len() < piece.min(1_500) {
                    text.push_str(fill);
                }
                text.truncate(piece.min(1_500));
                text
            };
            let texts = WorkEnrichment {
                description: Some(escaped("\\")),
                subjects: Some(escaped("\"")),
                categories: Some(escaped("\n")),
                cover_url: None,
            };
            store.set_work_enrichment(&work_id, &texts).await.unwrap();
            let work = store.get_work(&work_id).await.unwrap().unwrap();
            assert_eq!(work.description, texts.description, "{name}");
            assert_eq!(work.subjects, texts.subjects, "{name}");
            assert_eq!(work.categories, texts.categories, "{name}");
            let listed = store.list_works_page(None, 10).await.unwrap();
            assert_eq!(listed.len(), 1, "{name}");
        }
    }

    #[tokio::test]
    async fn a_swap_never_names_a_version_missing_pieces() {
        for profile in Profile::ALL {
            let name = format!("{profile:?}");
            let (store, work_id) = one_work(profile).await;
            let row = uncapped(works::Entity::find_by_id(work_id.clone()).one(store.db()))
                .await
                .unwrap()
                .unwrap();
            // A swap that lands after its own clean-up dropped its pieces.
            let reply = store
                .execute_host_batch(batch::request(
                    "work-texts-late",
                    vec![batch::exec(
                        PUBLISH_SQL,
                        vec![
                            batch::text(""),
                            batch::text_opt(None),
                            batch::text_opt(None),
                            batch::text_opt(None),
                            batch::text("gone"),
                            batch::text(&now_str()),
                            batch::text(&row.id),
                            batch::text(&row.updated_at),
                            batch::text(""),
                            batch::text(&row.id),
                            batch::text("gone"),
                            batch::int(2),
                        ],
                    )],
                ))
                .await
                .unwrap();
            assert_eq!(batch::rows_affected(&reply, 0), 0, "{name}");
            let after = uncapped(works::Entity::find_by_id(work_id.clone()).one(store.db()))
                .await
                .unwrap()
                .unwrap();
            assert!(after.text_version.is_none(), "{name}");
            assert!(store.get_work(&work_id).await.is_ok(), "{name}");
        }
    }

    #[tokio::test]
    async fn a_failed_publication_keeps_only_what_a_row_names() {
        for profile in Profile::ALL {
            let name = format!("{profile:?}");
            for lost_reply in [false, true] {
                let kit = sqlite::memory(profile).await;
                let store = kit.store();
                store
                    .upsert_account("audible", "a1", "us", None, true, "audible")
                    .await
                    .unwrap();
                let record = store
                    .upsert_book(&NewBook::minimal("B00X", "a1", "US", "Dune"))
                    .await
                    .unwrap();
                let work_id = store
                    .work_of_holding(&record.holding_id)
                    .await
                    .unwrap()
                    .unwrap();
                let piece = crate::control_plane::chunk_bytes(store.db_capabilities());
                let description = long_text(piece.saturating_mul(3).min(12_000));
                // Staging succeeds; only the batch that names the pieces
                // fails, before it applies or with its reply lost.
                kit.aim_faults_at(Some("UPDATE works SET description"));
                if lost_reply {
                    kit.lose_next_replies(1);
                } else {
                    kit.lock_next_requests(1);
                }
                let err = store
                    .set_work_enrichment(
                        &work_id,
                        &WorkEnrichment {
                            description: Some(description.clone()),
                            ..WorkEnrichment::default()
                        },
                    )
                    .await
                    .unwrap_err();
                kit.aim_faults_at(None);
                assert!(
                    matches!(err, LibraryError::Unavailable(_)),
                    "{name}: {err:?}"
                );
                let work = store.get_work(&work_id).await.unwrap().unwrap();
                let row = uncapped(works::Entity::find_by_id(work_id.clone()).one(store.db()))
                    .await
                    .unwrap()
                    .unwrap();
                let stored = pieces(store).await;
                if lost_reply {
                    // The publication applied: its pieces stay and read back.
                    assert_eq!(
                        work.description.as_deref(),
                        Some(description.as_str()),
                        "{name}"
                    );
                    if description.len() > piece {
                        assert!(!stored.is_empty(), "{name}");
                    }
                    assert!(
                        stored
                            .iter()
                            .all(|p| Some(&p.version) == row.text_version.as_ref()),
                        "{name}"
                    );
                } else {
                    // Nothing was published: the staged pieces are gone.
                    assert!(work.description.is_none(), "{name}");
                    assert!(row.text_version.is_none(), "{name}");
                    assert!(stored.is_empty(), "{name}");
                }
            }
        }
    }

    #[tokio::test]
    async fn a_losing_write_keeps_pieces_another_write_is_staging() {
        use sea_orm::{ActiveModelTrait, ActiveValue::Set};

        let (store, work_id) = one_work(Profile::Minimum).await;
        let piece = crate::control_plane::chunk_bytes(store.db_capabilities());
        let stale = uncapped(works::Entity::find_by_id(work_id.clone()).one(store.db()))
            .await
            .unwrap()
            .unwrap();
        store
            .set_work_enrichment(
                &work_id,
                &WorkEnrichment {
                    description: Some("Spice.".into()),
                    ..WorkEnrichment::default()
                },
            )
            .await
            .unwrap();
        // Another write has staged its pieces and not swapped yet.
        work_text_parts::ActiveModel {
            work_id: Set(work_id.clone()),
            version: Set("staging".into()),
            field: Set("description".into()),
            seq: Set(0),
            parts: Set(1),
            body: Set("Arrakis.".into()),
        }
        .insert(store.db())
        .await
        .unwrap();
        let applied = store
            .swap_work_texts(
                &stale,
                &WorkTexts {
                    description: Some(long_text(piece * 2)),
                    ..WorkTexts::default()
                },
            )
            .await
            .unwrap();
        assert!(!applied);
        let left = pieces(&store).await;
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].version, "staging");
    }

    #[tokio::test]
    async fn the_same_work_twice_is_completed_twice() {
        let (store, work_id) = one_work(Profile::Minimum).await;
        let piece = crate::control_plane::chunk_bytes(store.db_capabilities());
        let description = long_text(piece * 2);
        store
            .set_work_enrichment(
                &work_id,
                &WorkEnrichment {
                    description: Some(description.clone()),
                    ..WorkEnrichment::default()
                },
            )
            .await
            .unwrap();
        let row = uncapped(works::Entity::find_by_id(work_id.clone()).one(store.db()))
            .await
            .unwrap()
            .unwrap();
        let mut rows = [row.clone(), row.clone()];
        store.complete_work_texts(&mut rows).await.unwrap();
        for row in rows {
            assert_eq!(row.description.as_deref(), Some(description.as_str()));
        }
        // More copies than one lookup takes: the pieces are still read once.
        let mut rows = vec![row; store.lookup_chunk() * 2 + 1];
        store.complete_work_texts(&mut rows).await.unwrap();
        for row in rows {
            assert_eq!(row.description.as_deref(), Some(description.as_str()));
        }
    }

    #[tokio::test]
    async fn a_work_missing_a_piece_is_read_again_then_refused() {
        let (store, work_id) = one_work(Profile::Minimum).await;
        let piece = crate::control_plane::chunk_bytes(store.db_capabilities());
        store
            .set_work_enrichment(
                &work_id,
                &WorkEnrichment {
                    description: Some(long_text(piece * 3)),
                    ..WorkEnrichment::default()
                },
            )
            .await
            .unwrap();
        uncapped(
            work_text_parts::Entity::delete_many()
                .filter(work_text_parts::Column::Seq.eq(1))
                .exec(store.db()),
        )
        .await
        .unwrap();
        let err = store.get_work(&work_id).await.unwrap_err();
        assert!(matches!(err, LibraryError::Unavailable(_)), "{err:?}");
    }
}
