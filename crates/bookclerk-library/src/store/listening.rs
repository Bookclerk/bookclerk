//! Batched reads and writes for listening sync (`listening_progress`).
//!
//! An integration's listening sync reports many items at once. Resolving and
//! storing them one at a time cost four sequential round trips per item plus
//! a whole-library read for matching. These helpers do the same work in a
//! few bulk statements:
//!
//! - [`LibraryStore::match_listening_items`] resolves every item's ASIN and
//!   ISBN through the indexed `edition_identifiers` table to a work (and an
//!   edition when exactly one carries the identifier). It never matches by
//!   title. A row is keyed by that work; an item whose identifiers name no
//!   work is not stored.
//! - [`LibraryStore::portal_identity_ids`] reads the portal identities of
//!   every reported user in one lookup per chunk.
//! - [`LibraryStore::upsert_listening_progress_batch`] reads the stored rows
//!   in chunks and writes the new state as atomic host batches sized to the
//!   database plugin's limits.

use std::collections::{BTreeMap, BTreeSet};

use bookclerk_plugin_abi::{DbType, DbValue, TypedDbStatement};
use bookclerk_resolve::{normalize_identifier, IdentifierScheme};
use sea_orm::{ColumnTrait, EntityTrait, FromQueryResult, QueryFilter, QuerySelect};

use super::write_batches::WriteBatches;
use super::{map_listening, now_str, LibraryStore, NewListeningProgress};
use crate::control_plane::batch;
use crate::entities::{edition_identifiers, listening_progress, portal_identities};
use crate::error::{LibraryError, Result};
use crate::models::ListeningProgressRecord;

/// The identifiers a listening item names its book by, as the provider
/// reported them (any case and punctuation; normalised here).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListeningIdentifiers {
    /// Amazon / Audible ASIN, when the provider knows it.
    pub asin: Option<String>,
    /// ISBN-10 or ISBN-13, when the provider knows it.
    pub isbn: Option<String>,
}

/// What one listening item resolved to in the library.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListeningMatch {
    /// Work the item's identifiers resolve to, when they name exactly one.
    pub work_id: Option<String>,
    /// Edition carrying the identifier that decided the work, when exactly
    /// one edition carries it.
    pub edition_id: Option<String>,
}

/// One stored listening row a batched write produced.
#[derive(Debug, Clone)]
pub struct ListeningProgressWrite {
    /// The stored row before this write; `None` when the write inserted it.
    pub before: Option<ListeningProgressRecord>,
    /// The row as written: every report for its key merged over `before`
    /// the way [`LibraryStore::upsert_listening_progress`] merges one.
    pub after: NewListeningProgress,
}

/// One `edition_identifiers` value and the edition carrying it.
///
/// Read by column name: an adapter may return a projection's columns in any
/// order, so positional tuples are not safe here.
#[derive(Debug, FromQueryResult)]
struct EditionByValue {
    /// Normalised identifier value.
    value: String,
    /// Edition carrying it.
    edition_id: String,
}

/// One portal identity's external user id and row id.
#[derive(Debug, FromQueryResult)]
struct IdentityByUser {
    /// User id at the provider.
    external_user_id: String,
    /// `portal_identities.id`.
    id: i64,
}

/// Stored identity of one listening row: provider, external user, item.
type ListeningKey = (String, String, String);

impl LibraryStore {
    /// Resolves many listening items to works in a few bulk lookups.
    ///
    /// The item's ASIN is tried first, then its ISBN; the first one that
    /// names a work decides it. For that identifier, the work comes from the
    /// editions whose `edition_identifiers` carry it (an ASIN in any
    /// marketplace), used only when those editions agree on one work; the
    /// edition is kept when exactly one carries it. Titles are never
    /// compared.
    ///
    /// # Arguments
    ///
    /// * `items` - Each item's identifiers.
    ///
    /// # Returns
    ///
    /// One match per item, in the same order; an item no identifier
    /// resolves has neither a work nor an edition.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn match_listening_items(
        &self,
        items: &[ListeningIdentifiers],
    ) -> Result<Vec<ListeningMatch>> {
        let normalised: Vec<(Option<String>, Option<String>)> = items
            .iter()
            .map(|item| {
                (
                    item.asin
                        .as_deref()
                        .and_then(|raw| normalize_identifier(IdentifierScheme::Asin, raw)),
                    item.isbn
                        .as_deref()
                        .and_then(|raw| normalize_identifier(IdentifierScheme::Isbn, raw)),
                )
            })
            .collect();
        let asins: BTreeSet<String> = normalised.iter().filter_map(|n| n.0.clone()).collect();
        let isbns: BTreeSet<String> = normalised.iter().filter_map(|n| n.1.clone()).collect();

        let editions_by_asin = self
            .editions_by_identifier(IdentifierScheme::Asin, &asins)
            .await?;
        let editions_by_isbn = self
            .editions_by_identifier(IdentifierScheme::Isbn, &isbns)
            .await?;
        let work_of_edition = self
            .works_of_editions(
                editions_by_asin
                    .values()
                    .chain(editions_by_isbn.values())
                    .flatten()
                    .cloned()
                    .collect(),
            )
            .await?;

        let resolve = |editions: Option<&BTreeSet<String>>| -> Option<ListeningMatch> {
            let editions = editions?;
            let works: BTreeSet<&String> = editions
                .iter()
                .filter_map(|edition| work_of_edition.get(edition))
                .collect();
            if works.len() != 1 {
                return None;
            }
            Some(ListeningMatch {
                work_id: works.into_iter().next().cloned(),
                edition_id: match editions.len() {
                    1 => editions.iter().next().cloned(),
                    _ => None,
                },
            })
        };
        Ok(normalised
            .iter()
            .map(|(asin, isbn)| {
                resolve(asin.as_ref().and_then(|a| editions_by_asin.get(a)))
                    .or_else(|| resolve(isbn.as_ref().and_then(|i| editions_by_isbn.get(i))))
                    .unwrap_or_default()
            })
            .collect())
    }

    /// Editions carrying each identifier value of `scheme`, in any
    /// marketplace.
    ///
    /// # Arguments
    ///
    /// * `scheme` - Identifier scheme of `values`.
    /// * `values` - Normalised identifier values.
    ///
    /// # Returns
    ///
    /// Edition ids keyed by identifier value; values no edition carries are
    /// absent.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    async fn editions_by_identifier(
        &self,
        scheme: IdentifierScheme,
        values: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, BTreeSet<String>>> {
        let values: Vec<String> = values.iter().cloned().collect();
        let mut found: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for part in values.chunks(self.lookup_chunk()) {
            let rows = edition_identifiers::Entity::find()
                .select_only()
                .column(edition_identifiers::Column::Value)
                .column(edition_identifiers::Column::EditionId)
                .filter(edition_identifiers::Column::Scheme.eq(scheme.as_str()))
                .filter(edition_identifiers::Column::Value.is_in(part.iter().cloned()))
                .into_model::<EditionByValue>()
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
            for row in rows {
                found.entry(row.value).or_default().insert(row.edition_id);
            }
        }
        Ok(found)
    }

    /// Portal identity ids of many external users of one provider.
    ///
    /// # Arguments
    ///
    /// * `provider` - Identity provider / integration id.
    /// * `external_user_ids` - User ids at that provider.
    ///
    /// # Returns
    ///
    /// `portal_identities.id` keyed by external user id; users with no
    /// identity are absent.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn portal_identity_ids(
        &self,
        provider: &str,
        external_user_ids: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, i64>> {
        let users: Vec<String> = external_user_ids.iter().cloned().collect();
        let mut found = BTreeMap::new();
        for part in users.chunks(self.lookup_chunk()) {
            let rows = portal_identities::Entity::find()
                .select_only()
                .column(portal_identities::Column::ExternalUserId)
                .column(portal_identities::Column::Id)
                .filter(portal_identities::Column::Provider.eq(provider))
                .filter(portal_identities::Column::ExternalUserId.is_in(part.iter().cloned()))
                .into_model::<IdentityByUser>()
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
            found.extend(rows.into_iter().map(|r| (r.external_user_id, r.id)));
        }
        Ok(found)
    }

    /// Stores many listening rows in a few chunked reads and atomic batches.
    ///
    /// Reports for one `(provider, external_user_id, external_item_id)` are
    /// merged in order over the stored row, exactly as successive calls to
    /// [`Self::upsert_listening_progress`] would merge them: optional text
    /// and the identity keep their stored value when a report leaves them
    /// out, progress fields take the report's. Each row is written once.
    ///
    /// The writes are split into atomic batches under the database plugin's
    /// `maxStatements` and `maxRequestBytes`; a row's two statements always
    /// share a batch. No interactive transaction is used, so a reader
    /// between two batches can see part of the sync.
    ///
    /// # Arguments
    ///
    /// * `rows` - Reports to store, in the order the provider sent them.
    ///
    /// # Returns
    ///
    /// One write per distinct row key, in the order the keys first appear.
    ///
    /// # Errors
    ///
    /// Returns the database error. Batches sent before a failing one stay
    /// written.
    pub async fn upsert_listening_progress_batch(
        &self,
        rows: &[NewListeningProgress],
    ) -> Result<Vec<ListeningProgressWrite>> {
        let mut order: Vec<ListeningKey> = Vec::new();
        let mut reports: BTreeMap<ListeningKey, Vec<&NewListeningProgress>> = BTreeMap::new();
        for row in rows {
            let key = (
                row.provider.clone(),
                row.external_user_id.clone(),
                row.external_item_id.clone(),
            );
            reports
                .entry(key.clone())
                .or_insert_with(|| {
                    order.push(key);
                    Vec::new()
                })
                .push(row);
        }
        let mut stored = self.stored_listening_rows(&order).await?;

        let now = now_str();
        let mut batches = WriteBatches::new(self.db_capabilities(), "listening progress");
        let mut writes = Vec::with_capacity(order.len());
        for key in order {
            let before = stored.remove(&key);
            let mut merged = before.as_ref().map(written_row);
            for report in reports.remove(&key).unwrap_or_default() {
                merged = Some(merge_listening(merged.as_ref(), report));
            }
            let Some(after) = merged else { continue };
            if let Some(full) = batches.push_group(listening_upsert(&after, &now).to_vec())? {
                self.run_listening_batch(full).await?;
            }
            writes.push(ListeningProgressWrite { before, after });
        }
        if let Some(last) = batches.finish() {
            self.run_listening_batch(last).await?;
        }
        Ok(writes)
    }

    /// Stored listening rows for `keys`, read per provider and user with
    /// the items chunked.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    async fn stored_listening_rows(
        &self,
        keys: &[ListeningKey],
    ) -> Result<BTreeMap<ListeningKey, ListeningProgressRecord>> {
        let mut items_of: BTreeMap<(&str, &str), Vec<String>> = BTreeMap::new();
        for (provider, user, item) in keys {
            items_of
                .entry((provider.as_str(), user.as_str()))
                .or_default()
                .push(item.clone());
        }
        let mut found = BTreeMap::new();
        for ((provider, user), items) in items_of {
            for part in items.chunks(self.lookup_chunk()) {
                for model in listening_progress::Entity::find()
                    .filter(listening_progress::Column::Provider.eq(provider))
                    .filter(listening_progress::Column::ExternalUserId.eq(user))
                    .filter(listening_progress::Column::ExternalItemId.is_in(part.iter().cloned()))
                    .all(&self.db)
                    .await
                    .map_err(LibraryError::Orm)?
                {
                    let record = map_listening(model);
                    found.insert(
                        (
                            record.provider.clone(),
                            record.external_user_id.clone(),
                            record.external_item_id.clone(),
                        ),
                        record,
                    );
                }
            }
        }
        Ok(found)
    }

    /// Sends one chunk of listening writes as one atomic host batch.
    ///
    /// # Errors
    ///
    /// Returns the database error; nothing in the chunk is kept then.
    async fn run_listening_batch(&self, statements: Vec<TypedDbStatement>) -> Result<()> {
        let operation_id = format!("listening-progress-{}", uuid::Uuid::new_v4());
        self.execute_host_batch(batch::request(&operation_id, statements))
            .await?;
        Ok(())
    }
}

/// A stored row as the input that would write it.
fn written_row(row: &ListeningProgressRecord) -> NewListeningProgress {
    NewListeningProgress {
        identity_id: row.identity_id,
        provider: row.provider.clone(),
        external_user_id: row.external_user_id.clone(),
        work_id: row.work_id.clone(),
        edition_id: row.edition_id.clone(),
        external_item_id: row.external_item_id.clone(),
        progress: row.progress,
        current_time_seconds: row.current_time_seconds,
        duration_seconds: row.duration_seconds,
        is_finished: row.is_finished,
        last_listened_at: row.last_listened_at,
    }
}

/// One report merged over the current row, as
/// [`LibraryStore::upsert_listening_progress`] merges it: the work is the
/// report's (its identifiers resolved it just now); the identity, edition
/// and last-listened time keep the current value when the report has none;
/// progress fields take the report's.
/// A non-finite number is stored as unknown.
///
/// # Arguments
///
/// * `current` - The row so far, when it exists.
/// * `report` - The next report for the same key.
fn merge_listening(
    current: Option<&NewListeningProgress>,
    report: &NewListeningProgress,
) -> NewListeningProgress {
    let keep = |new: &Option<String>, old: Option<&Option<String>>| {
        new.clone().or_else(|| old.cloned().flatten())
    };
    let finite = |v: Option<f64>| v.filter(|x| x.is_finite());
    NewListeningProgress {
        identity_id: report.identity_id.or(current.and_then(|c| c.identity_id)),
        provider: report.provider.clone(),
        external_user_id: report.external_user_id.clone(),
        work_id: report.work_id.clone(),
        edition_id: keep(&report.edition_id, current.map(|c| &c.edition_id)),
        external_item_id: report.external_item_id.clone(),
        progress: finite(report.progress),
        current_time_seconds: finite(report.current_time_seconds),
        duration_seconds: finite(report.duration_seconds),
        is_finished: report.is_finished,
        last_listened_at: report
            .last_listened_at
            .or(current.and_then(|c| c.last_listened_at)),
    }
}

/// Float bind, or a typed NULL when unknown.
fn float_opt(value: Option<f64>) -> DbValue {
    match value {
        Some(value) => DbValue::Float64(value),
        None => DbValue::Null(DbType::Float64),
    }
}

/// The two statements that store one merged listening row.
///
/// `UPDATE` first, then `INSERT OR IGNORE` (canonical SQL-v1 has no
/// `ON CONFLICT … DO UPDATE`): an existing row is rewritten and the insert
/// is ignored; a new row is missed by the update and inserted. The update
/// keeps a stored value with `COALESCE` where the merge would, so a sync
/// running beside another never blanks a field the other filled.
///
/// # Arguments
///
/// * `row` - The merged row.
/// * `now` - `updated_at` for the write (RFC 3339).
fn listening_upsert(row: &NewListeningProgress, now: &str) -> [TypedDbStatement; 2] {
    let last = row.last_listened_at.map(|d| d.to_rfc3339());
    let finished = batch::int(i64::from(row.is_finished));
    [
        batch::exec(
            "UPDATE listening_progress SET \
             identity_id = COALESCE(?, identity_id), \
             work_id = ?, \
             edition_id = COALESCE(?, edition_id), \
             progress = ?, current_time_seconds = ?, duration_seconds = ?, \
             is_finished = ?, \
             last_listened_at = COALESCE(?, last_listened_at), \
             updated_at = ? \
             WHERE provider = ? AND external_user_id = ? AND external_item_id = ?",
            vec![
                batch::int_opt(row.identity_id),
                batch::text(&row.work_id),
                batch::text_opt(row.edition_id.as_deref()),
                float_opt(row.progress),
                float_opt(row.current_time_seconds),
                float_opt(row.duration_seconds),
                finished.clone(),
                batch::text_opt(last.as_deref()),
                batch::text(now),
                batch::text(&row.provider),
                batch::text(&row.external_user_id),
                batch::text(&row.external_item_id),
            ],
        ),
        batch::exec(
            "INSERT OR IGNORE INTO listening_progress \
             (identity_id, provider, external_user_id, work_id, edition_id, external_item_id, \
              progress, current_time_seconds, duration_seconds, is_finished, last_listened_at, \
              updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            vec![
                batch::int_opt(row.identity_id),
                batch::text(&row.provider),
                batch::text(&row.external_user_id),
                batch::text(&row.work_id),
                batch::text_opt(row.edition_id.as_deref()),
                batch::text(&row.external_item_id),
                float_opt(row.progress),
                float_opt(row.current_time_seconds),
                float_opt(row.duration_seconds),
                finished,
                batch::text_opt(last.as_deref()),
                batch::text(now),
            ],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NewBook;
    use crate::test_support::{sqlite, Profile};

    /// A report of `item` for `user` on the `abs` provider at `progress`,
    /// about `work`.
    fn report(user: &str, item: &str, work: &str, progress: f64) -> NewListeningProgress {
        NewListeningProgress {
            identity_id: None,
            provider: "abs".into(),
            external_user_id: user.into(),
            work_id: work.into(),
            edition_id: None,
            external_item_id: item.into(),
            progress: Some(progress),
            current_time_seconds: None,
            duration_seconds: None,
            is_finished: false,
            last_listened_at: None,
        }
    }

    /// A catalog work for `title`, made the way a wish makes one.
    async fn work(store: &LibraryStore, title: &str) -> String {
        store
            .work_for_wish(&crate::TitleQuery {
                title,
                authors: Some("Frank Herbert"),
                ..crate::TitleQuery::default()
            })
            .await
            .unwrap()
            .0
    }

    /// The edition a library title with `uuid` reaches.
    async fn edition_of(store: &LibraryStore, uuid: &str) -> Option<String> {
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
        let book = crate::entities::books::Entity::find()
            .filter(crate::entities::books::Column::Uuid.eq(uuid))
            .one(store.db())
            .await
            .unwrap()
            .unwrap();
        store.edition_id_for_book(&book).await.unwrap()
    }

    /// Identifiers with an optional ASIN and ISBN.
    fn ids(asin: Option<&str>, isbn: Option<&str>) -> ListeningIdentifiers {
        ListeningIdentifiers {
            asin: asin.map(str::to_string),
            isbn: isbn.map(str::to_string),
        }
    }

    #[tokio::test]
    async fn items_resolve_by_identifier_and_never_by_title() {
        for profile in Profile::ALL {
            let kit = sqlite::memory(profile).await;
            let store = kit.store();
            let name = profile.name();
            store
                .upsert_account("audible", "a1", "us", None, true, "audible")
                .await
                .unwrap();
            let mut dune = NewBook::minimal("B00DUNE001", "a1", "US", "Dune");
            dune.isbn = Some("9780441172719".into());
            let dune = store.upsert_book(&dune).await.unwrap();
            let other = store
                .upsert_book(&NewBook::minimal("B00OTHER01", "a1", "US", "Other"))
                .await
                .unwrap();
            let dune_work = store.work_id_for_book(&dune.uuid).await.unwrap();
            let other_work = store.work_id_for_book(&other.uuid).await.unwrap();
            let dune_edition = edition_of(store, &dune.uuid).await;
            let other_edition = edition_of(store, &other.uuid).await;
            assert!(dune_work.is_some(), "{name}");
            assert!(dune_edition.is_some(), "{name}");
            assert_ne!(dune_work, other_work, "{name}");

            let matches = store
                .match_listening_items(&[
                    // Any case of the ASIN.
                    ids(Some(" b00dune001 "), None),
                    // A hyphenated ISBN matches its normalised spelling.
                    ids(None, Some("978-0-441-17271-9")),
                    // The ASIN wins over a conflicting ISBN.
                    ids(Some("B00OTHER01"), Some("9780441172719")),
                    // An unknown ASIN falls through to the ISBN.
                    ids(Some("B0UNKNOWN1"), Some("9780441172719")),
                    // Nothing to go on: titles are never compared.
                    ids(None, None),
                    ids(Some("B0UNKNOWN1"), Some("")),
                ])
                .await
                .unwrap();
            let want_dune = ListeningMatch {
                work_id: dune_work.clone(),
                edition_id: dune_edition.clone(),
            };
            let want_other = ListeningMatch {
                work_id: other_work.clone(),
                edition_id: other_edition.clone(),
            };
            assert_eq!(
                matches,
                vec![
                    want_dune.clone(),
                    want_dune.clone(),
                    want_other,
                    want_dune,
                    ListeningMatch::default(),
                    ListeningMatch::default(),
                ],
                "{name}"
            );
            assert!(store.match_listening_items(&[]).await.unwrap().is_empty());
            assert_eq!(kit.refused_begins(), 0, "{name}");
        }
    }

    #[tokio::test]
    async fn an_identifier_on_two_works_names_neither() {
        for profile in Profile::ALL {
            let kit = sqlite::memory(profile).await;
            let store = kit.store();
            store
                .upsert_account("audible", "a1", "us", None, true, "audible")
                .await
                .unwrap();
            // Two different books that a store gave the same ISBN.
            for (product, title) in [("B00DUNE001", "Dune"), ("B00OTHER01", "Other")] {
                let mut book = NewBook::minimal(product, "a1", "US", title);
                book.isbn = Some("9780441172719".into());
                store.upsert_book(&book).await.unwrap();
            }
            let matches = store
                .match_listening_items(&[ids(None, Some("9780441172719"))])
                .await
                .unwrap();
            assert_eq!(
                matches,
                vec![ListeningMatch::default()],
                "{}",
                profile.name()
            );
        }
    }

    #[tokio::test]
    async fn identities_are_read_for_many_users_at_once() {
        for profile in Profile::ALL {
            let kit = sqlite::memory(profile).await;
            let store = kit.store();
            let dana = store
                .upsert_portal_identity("abs", "dana", Some("Dana"))
                .await
                .unwrap();
            store
                .upsert_portal_identity("other", "lee", Some("Lee"))
                .await
                .unwrap();
            let users: BTreeSet<String> = ["dana", "lee", "nobody"]
                .into_iter()
                .map(str::to_string)
                .collect();
            let found = store.portal_identity_ids("abs", &users).await.unwrap();
            assert_eq!(
                found,
                BTreeMap::from([("dana".to_string(), dana.id)]),
                "{}",
                profile.name()
            );
        }
    }

    #[tokio::test]
    async fn a_sync_merges_reports_and_writes_each_row_once_in_batches() {
        for profile in Profile::ALL {
            let kit = sqlite::memory(profile).await;
            let store = kit.store();
            let name = profile.name();
            let dune = work(store, "Dune").await;
            let messiah = work(store, "Dune Messiah").await;
            store
                .upsert_account("audible", "a1", "us", None, true, "audible")
                .await
                .unwrap();
            let book = store
                .upsert_book(&NewBook::minimal(
                    "B00DUNE001",
                    "a1",
                    "US",
                    "Children of Dune",
                ))
                .await
                .unwrap();
            let edition = edition_of(store, &book.uuid).await;
            let mut seeded = report("u1", "kept", &dune, 0.1);
            seeded.edition_id.clone_from(&edition);
            seeded.last_listened_at = Some(chrono::Utc::now());
            let stored = store.upsert_listening_progress(&seeded).await.unwrap();
            let sent_before = kit.batch_sizes().len();

            // The stored row: the edition left out keeps its value, the
            // work and progress move.
            let mut kept = report("u1", "kept", &messiah, 0.4);
            kept.is_finished = true;
            // A new row reported twice: the second report wins where it
            // speaks and keeps the first's edition where it does not.
            let mut first = report("u2", "new", &dune, 0.2);
            first.edition_id.clone_from(&edition);
            let mut second = report("u2", "new", &dune, 0.3);
            second.progress = Some(f64::NAN);
            second.current_time_seconds = Some(90.0);
            let mut rows = vec![kept, first, second];
            // Enough rows to need several batches under every profile.
            rows.extend((0..40).map(|i| report("u3", &format!("bulk{i:02}"), &dune, 0.5)));

            let writes = store.upsert_listening_progress_batch(&rows).await.unwrap();
            assert_eq!(writes.len(), 42, "{name}");

            let kept = &writes[0];
            assert_eq!(kept.before.as_ref().map(|b| b.id), Some(stored.id));
            assert_eq!(kept.after.work_id, messiah, "{name}");
            assert_eq!(kept.after.edition_id, edition, "{name}");
            assert_eq!(kept.after.progress, Some(0.4));
            assert!(kept.after.is_finished);
            assert_eq!(
                kept.after.last_listened_at.map(|d| d.timestamp()),
                seeded.last_listened_at.map(|d| d.timestamp())
            );

            let new = &writes[1];
            assert!(new.before.is_none(), "{name}");
            assert_eq!(new.after.edition_id, edition);
            assert_eq!(new.after.work_id, dune);
            assert_eq!(new.after.progress, None, "a NaN is stored as unknown");
            assert_eq!(new.after.current_time_seconds, Some(90.0));

            // What was written is what is stored.
            for write in &writes {
                let row = store
                    .get_listening_progress(
                        &write.after.provider,
                        &write.after.external_user_id,
                        &write.after.external_item_id,
                    )
                    .await
                    .unwrap()
                    .unwrap_or_else(|| panic!("{name}: row stored"));
                let row = written_row(&row);
                assert_eq!(row.identity_id, write.after.identity_id);
                assert_eq!(row.work_id, write.after.work_id);
                assert_eq!(row.edition_id, write.after.edition_id);
                assert_eq!(row.progress, write.after.progress);
                assert_eq!(row.current_time_seconds, write.after.current_time_seconds);
                assert_eq!(row.is_finished, write.after.is_finished);
            }

            // 42 rows are 84 statements, split under the profile's cap.
            let caps = profile.capabilities();
            let sizes = kit.batch_sizes()[sent_before..].to_vec();
            assert_eq!(sizes.iter().sum::<usize>(), 84, "{name}: {sizes:?}");
            assert!(
                sizes.iter().all(|n| n % 2 == 0),
                "{name}: a row's two statements share a batch: {sizes:?}"
            );
            if caps.max_statements < 84 {
                assert!(sizes.len() > 1, "{name}: {sizes:?}");
            }
            assert!(
                sizes
                    .iter()
                    .all(|n| u32::try_from(*n).unwrap() <= caps.max_statements),
                "{name}: {sizes:?}"
            );
            assert_eq!(kit.refused_begins(), 0, "{name}");

            // An empty sync writes nothing.
            assert!(store
                .upsert_listening_progress_batch(&[])
                .await
                .unwrap()
                .is_empty());
        }
    }
}
