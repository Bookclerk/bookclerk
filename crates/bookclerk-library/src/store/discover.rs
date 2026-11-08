//! Stored Discover data: per-subject storefront candidates, the shared
//! storefront answer cache, and each subject's ranked feed.
//!
//! Discover used to rebuild every shelf inside the page request. These
//! tables let queued jobs do that work instead (`docs/internals/discovery.md`,
//! *Precomputed shelves*): a `discover_candidates` job fetches storefront
//! titles from one subject's own seeds and writes them here store by store,
//! a `discover_feed` job ranks them into shelves, and the page only reads the
//! stored feed. All three tables hold derived data that the jobs can rebuild,
//! so a backend copy (`migrate_library_backend`) leaves them behind.

use chrono::{DateTime, Utc};
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveModelTrait, ActiveValue::NotSet, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect,
};

use super::{now_str, parse_dt, parse_dt_opt, LibraryStore};
use crate::entities::{
    discover_candidates, discover_feeds, discover_fetches, discover_store_calls, portal_identities,
    users,
};
use crate::error::{LibraryError, Result};
use crate::models::{portal_prefs_key, user_prefs_key, OPERATOR_PREFS_KEY};

/// Rows read per query when a Discover listing walks a table, so no single
/// statement returns an unbounded result set.
///
/// Large enough that a typical subject's candidates arrive in one or two
/// reads. A page of real candidates (about 1.3 KB each) can still exceed the
/// sqlite plugin's 256 KiB `maxResultBytes`, so the candidate listing halves
/// its page when a result is refused for size.
const DISCOVER_READ_PAGE: u64 = 200;

/// Whose Discover shelves a stored row belongs to.
///
/// The same key as `user_preferences.subject_key`: the operator, a
/// first-party user, or a portal identity with no user.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DiscoverSubject {
    /// The operator (token, session, or auth disabled); seeded from the household.
    Operator,
    /// A first-party user, by `users.id`.
    User(i64),
    /// A portal identity that has no first-party user, by `portal_identities.id`.
    Portal(i64),
}

impl DiscoverSubject {
    /// Parses a stored subject key (`operator`, `user:<id>`, `portal:<id>`).
    ///
    /// # Returns
    ///
    /// `None` for any other text, including a non-numeric or negative id.
    #[must_use]
    pub fn parse(key: &str) -> Option<Self> {
        if key == OPERATOR_PREFS_KEY {
            return Some(Self::Operator);
        }
        let id = |rest: &str| rest.parse::<i64>().ok().filter(|id| *id >= 0);
        if let Some(rest) = key.strip_prefix("user:") {
            return id(rest).map(Self::User);
        }
        if let Some(rest) = key.strip_prefix("portal:") {
            return id(rest).map(Self::Portal);
        }
        None
    }

    /// The subject's stored key, the inverse of [`Self::parse`].
    #[must_use]
    pub fn key(&self) -> String {
        match self {
            Self::Operator => OPERATOR_PREFS_KEY.to_string(),
            Self::User(id) => user_prefs_key(*id),
            Self::Portal(id) => portal_prefs_key(*id),
        }
    }
}

/// Which seeds a subject's candidates and feed were built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeedScope {
    /// The subject's own books, listening, ratings and wishlist.
    Own,
    /// The whole household library, used while the subject has no taste of
    /// their own (and always for the operator).
    Household,
}

impl SeedScope {
    /// Wire and column text (`own`, `household`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Own => "own",
            Self::Household => "household",
        }
    }

    /// Parses [`Self::as_str`] text; anything else is `None`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "own" => Some(Self::Own),
            "household" => Some(Self::Household),
            _ => None,
        }
    }
}

/// One stored storefront candidate of a subject.
#[derive(Debug, Clone)]
pub struct StoredCandidate {
    /// Storefront plugin id that returned the title.
    pub source: String,
    /// Identity key of the candidate within its subject and store.
    pub candidate_key: String,
    /// Seeds that produced the row.
    pub seed_scope: SeedScope,
    /// The candidate as JSON, written by `bookclerk-discover`.
    pub candidate_json: String,
    /// When the store fetch that produced the row ran.
    pub fetched_at: DateTime<Utc>,
}

/// One subject's stored, ranked Discover feed.
#[derive(Debug, Clone)]
pub struct StoredFeed {
    /// The feed as JSON, before hidden shelves are filtered out.
    pub feed_json: String,
    /// Seeds the feed was ranked from.
    pub seed_scope: SeedScope,
    /// Newest candidate fetch the feed used, when it used any.
    pub candidates_fetched_at: Option<DateTime<Utc>>,
    /// When the feed was ranked.
    pub computed_at: DateTime<Utc>,
}

impl LibraryStore {
    /// Replaces every candidate one store returned for one subject.
    ///
    /// Each row is upserted with this call's fetch time, then the store's
    /// rows for the subject that this call did not write are deleted. Other
    /// stores' rows for the subject are untouched. No interactive
    /// transaction is used: the external database adapters only take
    /// single statements and atomic batches, so `begin` is refused there.
    /// A reader between the two steps can see the store's new rows next to
    /// its old ones, never fewer than either set.
    ///
    /// # Arguments
    ///
    /// * `subject` - Whose candidates these are.
    /// * `source` - Storefront plugin id that returned them.
    /// * `scope` - Seeds that produced them.
    /// * `rows` - `(candidate_key, candidate_json)` pairs; duplicate keys keep the last.
    ///
    /// # Errors
    ///
    /// Returns the database error. Rows written before it stay; the old rows
    /// are only deleted once every new row is stored.
    pub async fn replace_discover_candidates(
        &self,
        subject: &DiscoverSubject,
        source: &str,
        scope: SeedScope,
        rows: &[(String, String)],
    ) -> Result<()> {
        let subject_key = subject.key();
        let now = now_str();
        let mut unique: Vec<&(String, String)> = Vec::with_capacity(rows.len());
        for row in rows {
            if let Some(pos) = unique.iter().position(|seen| seen.0 == row.0) {
                unique.remove(pos);
            }
            unique.push(row);
        }
        for (key, json) in unique {
            let row = discover_candidates::ActiveModel {
                id: NotSet,
                subject_key: Set(subject_key.clone()),
                source: Set(source.to_string()),
                candidate_key: Set(key.clone()),
                seed_scope: Set(scope.as_str().to_string()),
                candidate_json: Set(json.clone()),
                fetched_at: Set(now.clone()),
            };
            discover_candidates::Entity::insert(row)
                .on_conflict(
                    OnConflict::columns([
                        discover_candidates::Column::SubjectKey,
                        discover_candidates::Column::Source,
                        discover_candidates::Column::CandidateKey,
                    ])
                    .update_columns([
                        discover_candidates::Column::SeedScope,
                        discover_candidates::Column::CandidateJson,
                        discover_candidates::Column::FetchedAt,
                    ])
                    .to_owned(),
                )
                .exec_without_returning(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
        }
        discover_candidates::Entity::delete_many()
            .filter(discover_candidates::Column::SubjectKey.eq(subject_key.as_str()))
            .filter(discover_candidates::Column::Source.eq(source))
            .filter(discover_candidates::Column::FetchedAt.ne(now.as_str()))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Lists every stored candidate of one subject, oldest store fetch first.
    ///
    /// Rows are read in id pages, then ordered by fetch time (ties keep id
    /// order). A refreshed candidate keeps its id, so id order alone would
    /// not be fetch order. A page whose result would exceed the database
    /// plugin's `maxResultBytes` is halved and read again; the smaller width
    /// is kept for the rest of the walk. Rows whose scope text is unknown
    /// (written by a newer release) are skipped.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn list_discover_candidates(
        &self,
        subject: &DiscoverSubject,
    ) -> Result<Vec<StoredCandidate>> {
        let key = subject.key();
        let mut out = Vec::new();
        let mut after: i64 = 0;
        let mut width = DISCOVER_READ_PAGE;
        loop {
            let rows = loop {
                let read = discover_candidates::Entity::find()
                    .filter(discover_candidates::Column::SubjectKey.eq(key.as_str()))
                    .filter(discover_candidates::Column::Id.gt(after))
                    .order_by_asc(discover_candidates::Column::Id)
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
            after = last.id;
            let full = u64::try_from(rows.len()).is_ok_and(|n| n >= width);
            out.extend(rows.into_iter().filter_map(|m| {
                Some(StoredCandidate {
                    seed_scope: SeedScope::parse(&m.seed_scope)?,
                    fetched_at: parse_dt(&m.fetched_at),
                    source: m.source,
                    candidate_key: m.candidate_key,
                    candidate_json: m.candidate_json,
                })
            }));
            if !full {
                break;
            }
        }
        out.sort_by_key(|c| c.fetched_at);
        Ok(out)
    }

    /// Reports whether a Discover subject still exists.
    ///
    /// The operator always exists; a user or portal subject exists while
    /// its `users` or `portal_identities` row does. Jobs check this so a
    /// fetch or ranking queued before the subject was deleted does not
    /// write its rows back.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn discover_subject_exists(&self, subject: &DiscoverSubject) -> Result<bool> {
        let found = match subject {
            DiscoverSubject::Operator => return Ok(true),
            DiscoverSubject::User(id) => users::Entity::find_by_id(*id)
                .select_only()
                .column(users::Column::Id)
                .into_tuple::<i64>()
                .one(&self.db)
                .await
                .map_err(LibraryError::Orm)?,
            DiscoverSubject::Portal(id) => portal_identities::Entity::find_by_id(*id)
                .select_only()
                .column(portal_identities::Column::Id)
                .into_tuple::<i64>()
                .one(&self.db)
                .await
                .map_err(LibraryError::Orm)?,
        };
        Ok(found.is_some())
    }

    /// Deletes every stored Discover row of one subject (candidates, feed
    /// and fetch marker).
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn forget_discover_subject(&self, subject: &DiscoverSubject) -> Result<()> {
        let key = subject.key();
        discover_candidates::Entity::delete_many()
            .filter(discover_candidates::Column::SubjectKey.eq(key.clone()))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        discover_feeds::Entity::delete_many()
            .filter(discover_feeds::Column::SubjectKey.eq(key.clone()))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        discover_fetches::Entity::delete_many()
            .filter(discover_fetches::Column::SubjectKey.eq(key))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Records that one subject's candidate fetch finished at `at`.
    ///
    /// Recorded separately from the candidate rows, so a fetch that found
    /// nothing new still counts as fresh for a day.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn mark_discover_fetch(
        &self,
        subject: &DiscoverSubject,
        at: DateTime<Utc>,
    ) -> Result<()> {
        let key = subject.key();
        let existing = discover_fetches::Entity::find()
            .filter(discover_fetches::Column::SubjectKey.eq(key.clone()))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        match existing {
            Some(model) => {
                let mut am: discover_fetches::ActiveModel = model.into();
                am.fetched_at = Set(at.to_rfc3339());
                am.update(&self.db).await.map_err(LibraryError::Orm)?;
            }
            None => {
                discover_fetches::ActiveModel {
                    id: NotSet,
                    subject_key: Set(key),
                    fetched_at: Set(at.to_rfc3339()),
                }
                .insert(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
            }
        }
        Ok(())
    }

    /// Returns when one subject's last candidate fetch finished.
    ///
    /// # Returns
    ///
    /// `None` when no fetch has finished for the subject yet.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn newest_discover_candidate_fetch(
        &self,
        subject: &DiscoverSubject,
    ) -> Result<Option<DateTime<Utc>>> {
        let row = discover_fetches::Entity::find()
            .filter(discover_fetches::Column::SubjectKey.eq(subject.key()))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(row.map(|m| parse_dt(&m.fetched_at)))
    }

    /// Returns a cached storefront answer no older than `max_age`.
    ///
    /// # Arguments
    ///
    /// * `call_key` - Key the caller derived from the store, call kind and query.
    /// * `max_age` - Oldest answer still worth reusing.
    ///
    /// # Returns
    ///
    /// The cached JSON, or `None` when absent or too old.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn get_discover_store_call(
        &self,
        call_key: &str,
        max_age: chrono::Duration,
    ) -> Result<Option<String>> {
        let row = discover_store_calls::Entity::find()
            .filter(discover_store_calls::Column::CallKey.eq(call_key))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(row
            .filter(|m| parse_dt(&m.fetched_at) + max_age > Utc::now())
            .map(|m| m.response_json))
    }

    /// Stores (or refreshes) one cached storefront answer.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn put_discover_store_call(
        &self,
        call_key: &str,
        source: &str,
        response_json: &str,
    ) -> Result<()> {
        let now = now_str();
        let existing = discover_store_calls::Entity::find()
            .filter(discover_store_calls::Column::CallKey.eq(call_key))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        if let Some(existing) = existing {
            let mut am: discover_store_calls::ActiveModel = existing.into();
            am.source = Set(source.to_string());
            am.response_json = Set(response_json.to_string());
            am.fetched_at = Set(now);
            am.update(&self.db).await.map_err(LibraryError::Orm)?;
        } else {
            discover_store_calls::ActiveModel {
                id: NotSet,
                call_key: Set(call_key.to_string()),
                source: Set(source.to_string()),
                response_json: Set(response_json.to_string()),
                fetched_at: Set(now),
            }
            .insert(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        }
        Ok(())
    }

    /// Deletes cached storefront answers fetched before `cutoff`.
    ///
    /// # Returns
    ///
    /// How many answers were deleted.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn prune_discover_store_calls(&self, cutoff: DateTime<Utc>) -> Result<u64> {
        let res = discover_store_calls::Entity::delete_many()
            .filter(discover_store_calls::Column::FetchedAt.lt(cutoff.to_rfc3339()))
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(res.rows_affected)
    }

    /// Stores (or replaces) one subject's ranked feed.
    ///
    /// One upsert writes the row without reading the old one first, so a
    /// stored feed too large to read back cannot block its replacement.
    ///
    /// # Arguments
    ///
    /// * `subject` - Whose feed it is.
    /// * `feed_json` - The feed, before hidden shelves are filtered out.
    /// * `scope` - Seeds it was ranked from.
    /// * `candidates_fetched_at` - Newest candidate fetch it used, when any.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn put_discover_feed(
        &self,
        subject: &DiscoverSubject,
        feed_json: &str,
        scope: SeedScope,
        candidates_fetched_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        let row = discover_feeds::ActiveModel {
            id: NotSet,
            subject_key: Set(subject.key()),
            feed_json: Set(feed_json.to_string()),
            seed_scope: Set(scope.as_str().to_string()),
            candidates_fetched_at: Set(candidates_fetched_at.map(|t| t.to_rfc3339())),
            computed_at: Set(now_str()),
        };
        discover_feeds::Entity::insert(row)
            .on_conflict(
                OnConflict::column(discover_feeds::Column::SubjectKey)
                    .update_columns([
                        discover_feeds::Column::FeedJson,
                        discover_feeds::Column::SeedScope,
                        discover_feeds::Column::CandidatesFetchedAt,
                        discover_feeds::Column::ComputedAt,
                    ])
                    .to_owned(),
            )
            .exec_without_returning(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Returns one subject's stored feed, when it has one.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn get_discover_feed(&self, subject: &DiscoverSubject) -> Result<Option<StoredFeed>> {
        let row = discover_feeds::Entity::find()
            .filter(discover_feeds::Column::SubjectKey.eq(subject.key()))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(row.and_then(|m| {
            Some(StoredFeed {
                seed_scope: SeedScope::parse(&m.seed_scope)?,
                candidates_fetched_at: parse_dt_opt(m.candidates_fetched_at.as_deref()),
                computed_at: parse_dt(&m.computed_at),
                feed_json: m.feed_json,
            })
        }))
    }

    /// Lists every subject that has a stored feed, so a shared change (the
    /// request queue) can re-rank them all.
    ///
    /// Keys that do not parse as a [`DiscoverSubject`] are skipped.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn list_discover_feed_subjects(&self) -> Result<Vec<DiscoverSubject>> {
        let mut out = Vec::new();
        let mut after: i64 = 0;
        loop {
            // Only the id and key: a feed body can be large, and this walk
            // touches every subject.
            let rows: Vec<(i64, String)> = discover_feeds::Entity::find()
                .select_only()
                .column(discover_feeds::Column::Id)
                .column(discover_feeds::Column::SubjectKey)
                .filter(discover_feeds::Column::Id.gt(after))
                .order_by_asc(discover_feeds::Column::Id)
                .limit(DISCOVER_READ_PAGE)
                .into_tuple()
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
            let Some((last, _)) = rows.last() else { break };
            after = *last;
            let full = u64::try_from(rows.len()).is_ok_and(|n| n >= DISCOVER_READ_PAGE);
            out.extend(rows.iter().filter_map(|(_, k)| DiscoverSubject::parse(k)));
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

    #[test]
    fn subject_keys_round_trip() {
        for subject in [
            DiscoverSubject::Operator,
            DiscoverSubject::User(7),
            DiscoverSubject::Portal(0),
        ] {
            assert_eq!(DiscoverSubject::parse(&subject.key()), Some(subject));
        }
        for bad in [
            "",
            "user:",
            "user:-1",
            "user:x",
            "portal:1.5",
            "other:1",
            "Operator",
        ] {
            assert_eq!(DiscoverSubject::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn seed_scope_round_trips() {
        for scope in [SeedScope::Own, SeedScope::Household] {
            assert_eq!(SeedScope::parse(scope.as_str()), Some(scope));
        }
        assert_eq!(SeedScope::parse("mine"), None);
    }

    /// An empty in-memory library with the host schema.
    async fn store() -> LibraryStore {
        LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        )
    }

    /// `(key, json)` rows for [`LibraryStore::replace_discover_candidates`].
    fn rows(keys: &[&str]) -> Vec<(String, String)> {
        keys.iter()
            .map(|k| ((*k).to_string(), format!("{{\"k\":\"{k}\"}}")))
            .collect()
    }

    #[tokio::test]
    async fn candidates_replace_per_store_and_subject() {
        let store = store().await;
        let alice = DiscoverSubject::User(1);
        let bob = DiscoverSubject::User(2);
        assert!(store
            .newest_discover_candidate_fetch(&alice)
            .await
            .unwrap()
            .is_none());
        store
            .replace_discover_candidates(&alice, "libro", SeedScope::Own, &rows(&["a", "b"]))
            .await
            .unwrap();
        store
            .replace_discover_candidates(&alice, "chirp", SeedScope::Own, &rows(&["c"]))
            .await
            .unwrap();
        store
            .replace_discover_candidates(&bob, "libro", SeedScope::Household, &rows(&["z"]))
            .await
            .unwrap();
        // A new answer from one store replaces only that store's rows.
        store
            .replace_discover_candidates(&alice, "libro", SeedScope::Own, &rows(&["d"]))
            .await
            .unwrap();

        let mut keys: Vec<(String, String)> = store
            .list_discover_candidates(&alice)
            .await
            .unwrap()
            .into_iter()
            .map(|c| (c.source, c.candidate_key))
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                (String::from("chirp"), String::from("c")),
                (String::from("libro"), String::from("d")),
            ]
        );
        let bob_rows = store.list_discover_candidates(&bob).await.unwrap();
        assert_eq!(bob_rows.len(), 1);
        assert_eq!(bob_rows[0].seed_scope, SeedScope::Household);
        assert_eq!(bob_rows[0].candidate_json, "{\"k\":\"z\"}");
        // Candidate rows alone do not mark a fetch; finishing one does, even
        // when it stored nothing, and a later finish moves the mark.
        assert!(store
            .newest_discover_candidate_fetch(&alice)
            .await
            .unwrap()
            .is_none());
        let first = Utc::now() - chrono::Duration::hours(2);
        store.mark_discover_fetch(&alice, first).await.unwrap();
        let later = Utc::now();
        store.mark_discover_fetch(&alice, later).await.unwrap();
        assert_eq!(
            store
                .newest_discover_candidate_fetch(&alice)
                .await
                .unwrap()
                .map(|t| t.timestamp()),
            Some(later.timestamp())
        );
        assert!(store
            .newest_discover_candidate_fetch(&bob)
            .await
            .unwrap()
            .is_none());
    }

    /// External database adapters refuse interactive transactions, so the
    /// write must not open one: a refused `begin` leaves it working.
    #[tokio::test]
    async fn candidates_replace_without_an_interactive_transaction() {
        let store = store().await;
        let alice = DiscoverSubject::User(1);
        store
            .replace_discover_candidates(&alice, "libro", SeedScope::Own, &rows(&["a", "b"]))
            .await
            .unwrap();
        crate::inject_begin_failures(1);
        let replaced = store
            .replace_discover_candidates(
                &alice,
                "libro",
                SeedScope::Household,
                &[
                    ("b".to_string(), "{\"k\":\"b2\"}".to_string()),
                    ("c".to_string(), "{\"k\":\"c\"}".to_string()),
                    ("c".to_string(), "{\"k\":\"c2\"}".to_string()),
                ],
            )
            .await;
        let unused_injection = crate::consume_begin_injection();
        replaced.unwrap();
        assert!(unused_injection, "the write opened a transaction");
        let mut got: Vec<(String, String, SeedScope)> = store
            .list_discover_candidates(&alice)
            .await
            .unwrap()
            .into_iter()
            .map(|c| (c.candidate_key, c.candidate_json, c.seed_scope))
            .collect();
        got.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            got,
            vec![
                (
                    String::from("b"),
                    String::from("{\"k\":\"b2\"}"),
                    SeedScope::Household
                ),
                (
                    String::from("c"),
                    String::from("{\"k\":\"c2\"}"),
                    SeedScope::Household
                ),
            ]
        );
    }

    /// A full read page of real-sized candidates is larger than the sqlite
    /// guest's `maxResultBytes`, so the listing has to shrink its pages.
    #[tokio::test]
    async fn large_candidates_list_under_the_result_cap() {
        let store = store().await;
        let alice = DiscoverSubject::User(1);
        let body = "x".repeat(1_400);
        let count = usize::try_from(DISCOVER_READ_PAGE).unwrap() + 37;
        let rows: Vec<(String, String)> = (0..count)
            .map(|i| {
                (
                    format!("k{i:04}"),
                    format!("{{\"k\":\"{i}\",\"d\":\"{body}\"}}"),
                )
            })
            .collect();
        store
            .replace_discover_candidates(&alice, "libro", SeedScope::Own, &rows)
            .await
            .unwrap();
        let listed = store.list_discover_candidates(&alice).await.unwrap();
        assert_eq!(listed.len(), count);
    }

    /// A stored feed larger than one inline result reads back whole and is
    /// still replaced, since the write does not load the old row.
    #[tokio::test]
    async fn a_large_feed_reads_back_and_is_replaced() {
        let store = store().await;
        let alice = DiscoverSubject::User(1);
        let quoted = "\"".repeat(150_000);
        store
            .put_discover_feed(&alice, &quoted, SeedScope::Own, None)
            .await
            .unwrap();
        let feed = store.get_discover_feed(&alice).await.unwrap().unwrap();
        assert_eq!(feed.feed_json.len(), 150_000);
        store
            .put_discover_feed(&alice, "{}", SeedScope::Own, None)
            .await
            .unwrap();
        let feed = store.get_discover_feed(&alice).await.unwrap().unwrap();
        assert_eq!(feed.feed_json, "{}");
    }

    #[tokio::test]
    async fn a_refreshed_store_lists_after_an_older_fetch() {
        let store = store().await;
        let alice = DiscoverSubject::User(1);
        store
            .replace_discover_candidates(&alice, "libro", SeedScope::Own, &rows(&["a"]))
            .await
            .unwrap();
        store
            .replace_discover_candidates(&alice, "chirp", SeedScope::Own, &rows(&["b"]))
            .await
            .unwrap();
        store
            .replace_discover_candidates(&alice, "libro", SeedScope::Own, &rows(&["a"]))
            .await
            .unwrap();
        let order: Vec<String> = store
            .list_discover_candidates(&alice)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.source)
            .collect();
        assert_eq!(order, vec![String::from("chirp"), String::from("libro")]);
    }

    #[tokio::test]
    async fn store_calls_are_cached_refreshed_and_pruned() {
        let store = store().await;
        let day = chrono::Duration::hours(24);
        assert!(store
            .get_discover_store_call("k1", day)
            .await
            .unwrap()
            .is_none());
        store
            .put_discover_store_call("k1", "libro", "[1]")
            .await
            .unwrap();
        assert_eq!(
            store.get_discover_store_call("k1", day).await.unwrap(),
            Some(String::from("[1]"))
        );
        // A newer answer for the same call replaces the old one.
        store
            .put_discover_store_call("k1", "libro", "[2]")
            .await
            .unwrap();
        assert_eq!(
            store.get_discover_store_call("k1", day).await.unwrap(),
            Some(String::from("[2]"))
        );
        // Too old to reuse.
        assert!(store
            .get_discover_store_call("k1", chrono::Duration::seconds(-1))
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            store
                .prune_discover_store_calls(Utc::now() - day)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .prune_discover_store_calls(Utc::now() + day)
                .await
                .unwrap(),
            1
        );
        assert!(store
            .get_discover_store_call("k1", day)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn a_deleted_subject_is_missing_and_forgotten() {
        let store = store().await;
        assert!(store
            .discover_subject_exists(&DiscoverSubject::Operator)
            .await
            .unwrap());
        assert!(!store
            .discover_subject_exists(&DiscoverSubject::User(41))
            .await
            .unwrap());
        assert!(!store
            .discover_subject_exists(&DiscoverSubject::Portal(42))
            .await
            .unwrap());
        let user = store
            .create_user(crate::models::UserRole::Member, Some("Ann"), None)
            .await
            .unwrap();
        let identity = store
            .upsert_portal_identity("oidc", "ann", Some("Ann"))
            .await
            .unwrap();
        assert!(store
            .discover_subject_exists(&DiscoverSubject::User(user.id))
            .await
            .unwrap());
        assert!(store
            .discover_subject_exists(&DiscoverSubject::Portal(identity.id))
            .await
            .unwrap());

        let gone = DiscoverSubject::User(41);
        let kept = DiscoverSubject::User(user.id);
        for subject in [&gone, &kept] {
            store
                .replace_discover_candidates(subject, "libro", SeedScope::Own, &rows(&["k"]))
                .await
                .unwrap();
            store
                .put_discover_feed(subject, "{}", SeedScope::Own, None)
                .await
                .unwrap();
            store
                .mark_discover_fetch(subject, Utc::now())
                .await
                .unwrap();
        }
        store.forget_discover_subject(&gone).await.unwrap();
        assert!(store
            .list_discover_candidates(&gone)
            .await
            .unwrap()
            .is_empty());
        assert!(store.get_discover_feed(&gone).await.unwrap().is_none());
        assert!(store
            .newest_discover_candidate_fetch(&gone)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            store.list_discover_candidates(&kept).await.unwrap().len(),
            1
        );
        assert!(store.get_discover_feed(&kept).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn listings_walk_past_one_read_page() {
        let store = store().await;
        let alice = DiscoverSubject::User(1);
        let count = usize::try_from(DISCOVER_READ_PAGE).unwrap() * 2 + 3;
        let keys: Vec<String> = (0..count).map(|i| format!("k{i}")).collect();
        let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        store
            .replace_discover_candidates(&alice, "libro", SeedScope::Own, &rows(&key_refs))
            .await
            .unwrap();
        let listed = store.list_discover_candidates(&alice).await.unwrap();
        assert_eq!(listed.len(), count);
        assert_eq!(listed[0].candidate_key, "k0");
        assert_eq!(listed[count - 1].candidate_key, format!("k{}", count - 1));

        for id in 0..count {
            store
                .put_discover_feed(
                    &DiscoverSubject::User(i64::try_from(id).unwrap()),
                    "{}",
                    SeedScope::Own,
                    None,
                )
                .await
                .unwrap();
        }
        assert_eq!(
            store.list_discover_feed_subjects().await.unwrap().len(),
            count
        );
    }

    #[tokio::test]
    async fn feeds_are_stored_per_subject_and_listed() {
        let store = store().await;
        assert!(store
            .list_discover_feed_subjects()
            .await
            .unwrap()
            .is_empty());
        assert!(store
            .get_discover_feed(&DiscoverSubject::Operator)
            .await
            .unwrap()
            .is_none());
        let fetched = Utc::now() - chrono::Duration::minutes(5);
        store
            .put_discover_feed(&DiscoverSubject::Operator, "{}", SeedScope::Household, None)
            .await
            .unwrap();
        store
            .put_discover_feed(
                &DiscoverSubject::Portal(4),
                "{\"shelves\":[]}",
                SeedScope::Own,
                Some(fetched),
            )
            .await
            .unwrap();
        // Ranking again replaces the subject's feed.
        store
            .put_discover_feed(
                &DiscoverSubject::Operator,
                "{\"v\":2}",
                SeedScope::Household,
                Some(fetched),
            )
            .await
            .unwrap();

        let operator = store
            .get_discover_feed(&DiscoverSubject::Operator)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(operator.feed_json, "{\"v\":2}");
        assert_eq!(operator.seed_scope, SeedScope::Household);
        assert_eq!(
            operator.candidates_fetched_at.map(|t| t.timestamp()),
            Some(fetched.timestamp())
        );
        let mut subjects: Vec<String> = store
            .list_discover_feed_subjects()
            .await
            .unwrap()
            .iter()
            .map(DiscoverSubject::key)
            .collect();
        subjects.sort();
        assert_eq!(subjects, vec!["operator", "portal:4"]);
    }
}
