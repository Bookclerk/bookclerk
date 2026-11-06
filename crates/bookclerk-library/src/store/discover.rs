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
use sea_orm::{
    ActiveModelTrait, ActiveValue::NotSet, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter,
    QueryOrder, TransactionTrait,
};

use super::{now_str, parse_dt, parse_dt_opt, LibraryStore};
use crate::entities::{discover_candidates, discover_feeds, discover_store_calls};
use crate::error::{LibraryError, Result};
use crate::models::{portal_prefs_key, user_prefs_key, OPERATOR_PREFS_KEY};

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
    /// The delete and inserts run in one transaction, so a reader sees the
    /// store's old rows or its new rows, never a mix, and other stores'
    /// rows for the subject are untouched.
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
    /// Returns the database error; nothing is changed then.
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
        let txn = self.db.begin().await.map_err(LibraryError::Orm)?;
        let written = async {
            discover_candidates::Entity::delete_many()
                .filter(discover_candidates::Column::SubjectKey.eq(subject_key.as_str()))
                .filter(discover_candidates::Column::Source.eq(source))
                .exec(&txn)
                .await
                .map_err(LibraryError::Orm)?;
            for (key, json) in unique {
                discover_candidates::ActiveModel {
                    id: NotSet,
                    subject_key: Set(subject_key.clone()),
                    source: Set(source.to_string()),
                    candidate_key: Set(key.clone()),
                    seed_scope: Set(scope.as_str().to_string()),
                    candidate_json: Set(json.clone()),
                    fetched_at: Set(now.clone()),
                }
                .insert(&txn)
                .await
                .map_err(LibraryError::Orm)?;
            }
            Ok::<(), LibraryError>(())
        }
        .await;
        match written {
            Ok(()) => {
                txn.commit().await.map_err(LibraryError::Orm)?;
                if let Some(fault) = crate::take_txn_fault() {
                    return Err(LibraryError::Orm(sea_orm::DbErr::Custom(fault)));
                }
                Ok(())
            }
            Err(err) => {
                let _ = txn.rollback().await;
                let _ = crate::take_txn_fault();
                Err(err)
            }
        }
    }

    /// Lists every stored candidate of one subject, oldest store fetch first.
    ///
    /// Rows whose scope text is unknown (written by a newer release) are skipped.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn list_discover_candidates(
        &self,
        subject: &DiscoverSubject,
    ) -> Result<Vec<StoredCandidate>> {
        let rows = discover_candidates::Entity::find()
            .filter(discover_candidates::Column::SubjectKey.eq(subject.key()))
            .order_by_asc(discover_candidates::Column::Id)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(rows
            .into_iter()
            .filter_map(|m| {
                Some(StoredCandidate {
                    seed_scope: SeedScope::parse(&m.seed_scope)?,
                    fetched_at: parse_dt(&m.fetched_at),
                    source: m.source,
                    candidate_key: m.candidate_key,
                    candidate_json: m.candidate_json,
                })
            })
            .collect())
    }

    /// Returns when one subject's newest stored candidate was fetched.
    ///
    /// # Returns
    ///
    /// `None` when the subject has no stored candidates.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn newest_discover_candidate_fetch(
        &self,
        subject: &DiscoverSubject,
    ) -> Result<Option<DateTime<Utc>>> {
        let row = discover_candidates::Entity::find()
            .filter(discover_candidates::Column::SubjectKey.eq(subject.key()))
            .order_by_desc(discover_candidates::Column::FetchedAt)
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
        let subject_key = subject.key();
        let fetched = candidates_fetched_at.map(|t| t.to_rfc3339());
        let existing = discover_feeds::Entity::find()
            .filter(discover_feeds::Column::SubjectKey.eq(subject_key.as_str()))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        if let Some(existing) = existing {
            let mut am: discover_feeds::ActiveModel = existing.into();
            am.feed_json = Set(feed_json.to_string());
            am.seed_scope = Set(scope.as_str().to_string());
            am.candidates_fetched_at = Set(fetched);
            am.computed_at = Set(now_str());
            am.update(&self.db).await.map_err(LibraryError::Orm)?;
        } else {
            discover_feeds::ActiveModel {
                id: NotSet,
                subject_key: Set(subject_key),
                feed_json: Set(feed_json.to_string()),
                seed_scope: Set(scope.as_str().to_string()),
                candidates_fetched_at: Set(fetched),
                computed_at: Set(now_str()),
            }
            .insert(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        }
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
        let rows = discover_feeds::Entity::find()
            .order_by_asc(discover_feeds::Column::Id)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(rows
            .iter()
            .filter_map(|m| DiscoverSubject::parse(&m.subject_key))
            .collect())
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
        assert!(store
            .newest_discover_candidate_fetch(&alice)
            .await
            .unwrap()
            .is_some());
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
