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
//!
//! The titles themselves live once, in the shared catalog pool
//! ([`super::catalog_listings`]): a candidate row and a cached store answer
//! refer to `store_listings` rows and keep only what is personal to the
//! subject or to the call.

use chrono::{DateTime, Utc};
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveModelTrait, ActiveValue::NotSet, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect,
};

use bookclerk_plugin_abi::TypedDbStatement;

use super::catalog_listings::CatalogListingRecord;
use super::write_batches::WriteBatches;
use super::{now_str, parse_dt, parse_dt_opt, LibraryStore};
use crate::column_read::SelectColumnExt;
use crate::control_plane::{batch, sea_probe, Bump, LiveTopic};
use crate::entities::{
    discover_candidates, discover_feeds, discover_fetches, discover_store_call_hits,
    discover_store_calls, discover_tastes, portal_identities, users,
};
use crate::error::{LibraryError, Result};
use crate::models::{portal_prefs_key, user_prefs_key, OPERATOR_PREFS_KEY};

/// Rows read per query when a Discover listing walks a table, so no single
/// statement returns an unbounded result set.
///
/// Large enough that a typical subject's candidates arrive in one or two
/// reads. Candidate rows are references (a few hundred bytes); the listing
/// text is read from the pool in its own size-bounded chunks.
pub(super) const DISCOVER_READ_PAGE: u64 = 200;

/// Most hits read back for one cached storefront call; stores answer a few
/// dozen titles per call.
const STORE_CALL_MAX_HITS: u64 = 500;

/// The two statements that upsert one candidate row.
///
/// `UPDATE` first, then `INSERT OR IGNORE`: an existing row is rewritten
/// and the insert is ignored; a new row is missed by the update and
/// inserted. Canonical SQL-v1 has no `ON CONFLICT … DO UPDATE`, and the
/// pair is idempotent in either order across batches.
fn candidate_upsert(
    subject_key: &str,
    source: &str,
    scope: SeedScope,
    row: &CandidateRef,
    fetched_at: &str,
) -> [TypedDbStatement; 2] {
    let context = [
        batch::text(source),
        batch::text(scope.as_str()),
        batch::text(&row.origin),
        batch::text_opt(row.seed_title.as_deref()),
        batch::text_opt(row.seed_categories.as_deref()),
        batch::text_opt(row.starter_genre.as_deref()),
        batch::text(fetched_at),
    ];
    let mut update = context.to_vec();
    update.extend([batch::text(subject_key), batch::text(&row.listing_id)]);
    let mut insert = vec![batch::text(subject_key), batch::text(&row.listing_id)];
    insert.extend(context);
    [
        batch::exec(
            "UPDATE discover_candidates \
             SET source = ?, seed_scope = ?, origin = ?, seed_title = ?, seed_categories = ?, \
             starter_genre = ?, fetched_at = ? \
             WHERE subject_key = ? AND listing_id = ?",
            update,
        ),
        batch::exec(
            "INSERT OR IGNORE INTO discover_candidates \
             (subject_key, listing_id, source, seed_scope, origin, seed_title, seed_categories, \
              starter_genre, fetched_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            insert,
        ),
    ]
}

/// The statements that replace one cached storefront answer, in the order
/// they must run.
struct StoreCallStatements {
    /// Claims the call row for this write (its token, an expired fetch
    /// time) and deletes the previous answer's hits; one atomic group.
    head: Vec<TypedDbStatement>,
    /// One insert per hit, each applied only while the row carries this
    /// write's token.
    hits: Vec<TypedDbStatement>,
    /// Writes the fetch time, only while the row carries this write's token.
    stamp: TypedDbStatement,
}

/// Builds the statements that replace the answer cached under `call_key`.
///
/// # Arguments
///
/// * `call_key` - The call's key.
/// * `source` - Storefront plugin id that answered.
/// * `writer` - This write's random token, which fences its hits and stamp
///   against a later write of the same call.
/// * `hits` - The listings, in the store's order.
fn store_call_statements(
    call_key: &str,
    source: &str,
    writer: &str,
    hits: &[StoreCallHit],
) -> StoreCallStatements {
    let expired = DateTime::<Utc>::UNIX_EPOCH.to_rfc3339();
    let head = vec![
        batch::exec(
            "UPDATE discover_store_calls SET source = ?, fetched_at = ?, writer = ? \
             WHERE call_key = ?",
            vec![
                batch::text(source),
                batch::text(&expired),
                batch::text(writer),
                batch::text(call_key),
            ],
        ),
        batch::exec(
            "INSERT OR IGNORE INTO discover_store_calls (call_key, source, fetched_at, writer) \
             VALUES (?, ?, ?, ?)",
            vec![
                batch::text(call_key),
                batch::text(source),
                batch::text(&expired),
                batch::text(writer),
            ],
        ),
        batch::exec(
            "DELETE FROM discover_store_call_hits WHERE call_key = ?",
            vec![batch::text(call_key)],
        ),
    ];
    let hits = (0_i64..)
        .zip(hits)
        .map(|(position, hit)| {
            batch::exec(
                "INSERT OR IGNORE INTO discover_store_call_hits \
                 (call_key, position, listing_id, origin, rank_hint) \
                 SELECT ?, ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM discover_store_calls \
                 WHERE call_key = ? AND writer = ?)",
                vec![
                    batch::text(call_key),
                    batch::int(position),
                    batch::text(&hit.listing_id),
                    batch::text(&hit.origin),
                    batch::int_opt(hit.rank_hint.map(i64::from)),
                    batch::text(call_key),
                    batch::text(writer),
                ],
            )
        })
        .collect();
    let stamp = batch::exec(
        "UPDATE discover_store_calls SET fetched_at = ? WHERE call_key = ? AND writer = ?",
        vec![
            batch::text(&now_str()),
            batch::text(call_key),
            batch::text(writer),
        ],
    );
    StoreCallStatements { head, hits, stamp }
}

/// One title a store returned for a subject, as written: the shared
/// listing it is, plus why the subject got it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateRef {
    /// The `store_listings` row.
    pub listing_id: String,
    /// How the store found it (`related to …`, `deals`).
    pub origin: String,
    /// The subject's title that seeded it, when a seed did.
    pub seed_title: Option<String>,
    /// That seed's categories.
    pub seed_categories: Option<String>,
    /// Starter genre it was browsed from, for a thin taste.
    pub starter_genre: Option<String>,
}

/// One listing a cached storefront call answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreCallHit {
    /// The `store_listings` row the hit resolved to.
    pub listing_id: String,
    /// How the store found it for this call.
    pub origin: String,
    /// The store's own 1-based rank for the hit, when it gave one.
    pub rank_hint: Option<u32>,
}

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

/// One stored storefront candidate of a subject: the shared listing joined
/// with what is personal to the subject.
#[derive(Debug, Clone)]
pub struct StoredCandidate {
    /// Storefront plugin id that returned the title (`audible`).
    pub source: String,
    /// The listing, read from the shared pool.
    pub listing: CatalogListingRecord,
    /// Seeds that produced the row.
    pub seed_scope: SeedScope,
    /// How the store found it.
    pub origin: String,
    /// The subject's title that seeded it, when a seed did.
    pub seed_title: Option<String>,
    /// That seed's categories.
    pub seed_categories: Option<String>,
    /// Starter genre it was browsed from, for a thin taste.
    pub starter_genre: Option<String>,
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

/// One subject's stored Discover taste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredDiscoverTaste {
    /// The taste as JSON (`bookclerk_discover::StoredTaste`).
    pub taste_json: String,
    /// Seeds the taste comes from.
    pub seed_scope: SeedScope,
    /// When the taste was computed.
    pub computed_at: DateTime<Utc>,
}

impl LibraryStore {
    /// Replaces every candidate one store returned for one subject.
    ///
    /// Each row is upserted with this call's fetch time, then the store's
    /// rows for the subject that this call did not write are deleted. Other
    /// stores' rows for the subject are untouched.
    ///
    /// The writes go to the database plugin as canonical host batches, each
    /// one atomic request (one transaction) holding as many rows as the
    /// plugin's `maxStatements` and `maxRequestBytes` allow, so a few hundred
    /// candidates cost a handful of round trips instead of one per row. No
    /// interactive transaction is used: the external database adapters only
    /// take single statements and atomic batches, so `begin` is refused
    /// there. The delete rides in the last batch. A reader between two
    /// batches can see part of the new set next to old rows that are not
    /// deleted yet.
    ///
    /// # Arguments
    ///
    /// * `subject` - Whose candidates these are.
    /// * `source` - Storefront plugin id that returned them.
    /// * `scope` - Seeds that produced them.
    /// * `rows` - The candidates, as listing references; a listing named
    ///   twice keeps the last.
    ///
    /// # Errors
    ///
    /// Returns the database error, including when a single row is larger
    /// than one request may carry. Batches written before it stay; the old
    /// rows are only deleted once every new row is stored.
    pub async fn replace_discover_candidates(
        &self,
        subject: &DiscoverSubject,
        source: &str,
        scope: SeedScope,
        rows: &[CandidateRef],
    ) -> Result<()> {
        let subject_key = subject.key();
        let now = now_str();
        let mut unique: Vec<&CandidateRef> = Vec::with_capacity(rows.len());
        for row in rows {
            if let Some(pos) = unique
                .iter()
                .position(|seen| seen.listing_id == row.listing_id)
            {
                unique.remove(pos);
            }
            unique.push(row);
        }
        let mut batches = WriteBatches::new(self.db_capabilities(), "discover candidate");
        for row in unique {
            let pair = candidate_upsert(&subject_key, source, scope, row, &now);
            if let Some(full) = batches.push_group(pair.to_vec())? {
                self.run_candidate_batch(full).await?;
            }
        }
        let stale = batch::exec(
            "DELETE FROM discover_candidates \
             WHERE subject_key = ? AND source = ? AND fetched_at <> ?",
            vec![
                batch::text(&subject_key),
                batch::text(source),
                batch::text(&now),
            ],
        );
        if let Some(full) = batches.push(stale)? {
            self.run_candidate_batch(full).await?;
        }
        if let Some(last) = batches.finish() {
            self.run_candidate_batch(last).await?;
        }
        Ok(())
    }

    /// Sends one chunk of candidate writes as one atomic host batch.
    ///
    /// # Errors
    ///
    /// Returns the database error; nothing in the chunk is kept then.
    async fn run_candidate_batch(&self, statements: Vec<TypedDbStatement>) -> Result<()> {
        let operation_id = format!("discover-candidates-{}", uuid::Uuid::new_v4());
        self.execute_host_batch(batch::request(&operation_id, statements))
            .await?;
        Ok(())
    }

    /// Lists every stored candidate of one subject, oldest store fetch first,
    /// each joined with its listing from the shared pool.
    ///
    /// Candidate rows are read in id pages, then their listings in chunks
    /// that stay under the database plugin's result limit
    /// ([`LibraryStore::catalog_listings`]), then the rows are ordered by
    /// fetch time (ties keep id order). Rows whose scope text is unknown
    /// (written by a newer release), or whose listing is gone, are skipped.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn list_discover_candidates(
        &self,
        subject: &DiscoverSubject,
    ) -> Result<Vec<StoredCandidate>> {
        let key = subject.key();
        let mut rows = Vec::new();
        let mut after: i64 = 0;
        loop {
            let page = discover_candidates::Entity::find()
                .filter(discover_candidates::Column::SubjectKey.eq(key.as_str()))
                .filter(discover_candidates::Column::Id.gt(after))
                .order_by_asc(discover_candidates::Column::Id)
                .limit(DISCOVER_READ_PAGE)
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
            let Some(last) = page.last() else { break };
            after = last.id;
            let full = u64::try_from(page.len()).is_ok_and(|n| n >= DISCOVER_READ_PAGE);
            rows.extend(page);
            if !full {
                break;
            }
        }
        let ids: Vec<String> = rows.iter().map(|m| m.listing_id.clone()).collect();
        let mut listings: std::collections::HashMap<String, CatalogListingRecord> = self
            .catalog_listings(&ids)
            .await?
            .into_iter()
            .map(|l| (l.id.clone(), l))
            .collect();
        let mut out: Vec<StoredCandidate> = rows
            .into_iter()
            .filter_map(|m| {
                let seed_scope = SeedScope::parse(&m.seed_scope)?;
                // Two rows never share a listing within one subject, so the
                // listing moves into its row.
                let listing = listings.remove(&m.listing_id)?;
                Some(StoredCandidate {
                    source: m.source,
                    listing,
                    seed_scope,
                    origin: m.origin,
                    seed_title: m.seed_title,
                    seed_categories: m.seed_categories,
                    starter_genre: m.starter_genre,
                    fetched_at: parse_dt(&m.fetched_at),
                })
            })
            .collect();
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
                .into_column::<i64, _>(users::Column::Id)
                .one(&self.db)
                .await
                .map_err(LibraryError::Orm)?,
            DiscoverSubject::Portal(id) => portal_identities::Entity::find_by_id(*id)
                .into_column::<i64, _>(portal_identities::Column::Id)
                .one(&self.db)
                .await
                .map_err(LibraryError::Orm)?,
        };
        Ok(found.is_some())
    }

    /// Deletes every stored Discover row of one subject (candidates, feed,
    /// taste and fetch marker).
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
        let probe = discover_feeds::Entity::find()
            .filter(discover_feeds::Column::SubjectKey.eq(key.clone()));
        self.exec_bumped(
            discover_feeds::Entity::delete_many()
                .filter(discover_feeds::Column::SubjectKey.eq(key.clone())),
            LiveTopic::Discover,
            Bump::IfAny(sea_probe(&probe)?),
        )
        .await?;
        discover_tastes::Entity::delete_many()
            .filter(discover_tastes::Column::SubjectKey.eq(key.clone()))
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

    /// Stores one subject's taste, replacing the previous one.
    ///
    /// # Arguments
    ///
    /// * `subject` - Whose taste it is.
    /// * `taste_json` - The taste, as the Discover crate encodes it.
    /// * `scope` - Seeds it comes from.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn put_discover_taste(
        &self,
        subject: &DiscoverSubject,
        taste_json: &str,
        scope: SeedScope,
    ) -> Result<()> {
        let row = discover_tastes::ActiveModel {
            subject_key: Set(subject.key()),
            taste_json: Set(taste_json.to_string()),
            seed_scope: Set(scope.as_str().to_string()),
            computed_at: Set(now_str()),
        };
        discover_tastes::Entity::insert(row)
            .on_conflict(
                OnConflict::column(discover_tastes::Column::SubjectKey)
                    .update_columns([
                        discover_tastes::Column::TasteJson,
                        discover_tastes::Column::SeedScope,
                        discover_tastes::Column::ComputedAt,
                    ])
                    .to_owned(),
            )
            .exec(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(())
    }

    /// Returns one subject's stored taste, when it has one.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn get_discover_taste(
        &self,
        subject: &DiscoverSubject,
    ) -> Result<Option<StoredDiscoverTaste>> {
        let row = discover_tastes::Entity::find()
            .filter(discover_tastes::Column::SubjectKey.eq(subject.key()))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(row.and_then(|m| {
            Some(StoredDiscoverTaste {
                seed_scope: SeedScope::parse(&m.seed_scope)?,
                computed_at: parse_dt(&m.computed_at),
                taste_json: m.taste_json,
            })
        }))
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
    /// The listings the call answered, in the store's order, or `None`
    /// when absent or too old.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn get_discover_store_call(
        &self,
        call_key: &str,
        max_age: chrono::Duration,
    ) -> Result<Option<Vec<StoreCallHit>>> {
        let row = discover_store_calls::Entity::find()
            .filter(discover_store_calls::Column::CallKey.eq(call_key))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        if !row.is_some_and(|m| parse_dt(&m.fetched_at) + max_age > Utc::now()) {
            return Ok(None);
        }
        let hits = discover_store_call_hits::Entity::find()
            .filter(discover_store_call_hits::Column::CallKey.eq(call_key))
            .order_by_asc(discover_store_call_hits::Column::Position)
            .limit(STORE_CALL_MAX_HITS)
            .all(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        Ok(Some(
            hits.into_iter()
                .map(|m| StoreCallHit {
                    listing_id: m.listing_id,
                    origin: m.origin,
                    rank_hint: m.rank_hint.and_then(|r| u32::try_from(r).ok()),
                })
                .collect(),
        ))
    }

    /// Stores (or refreshes) one cached storefront answer: the call row and
    /// the listings it answered, replacing the previous answer's hits.
    ///
    /// The writes go as atomic host batches sized from the database
    /// plugin's limits. The first batch marks the call as fetched long ago
    /// and deletes the old hits; the new fetch time is written last, in the
    /// final batch. A reader between batches, or after a batch fails, sees
    /// an expired answer and asks the store again, never a fresh answer
    /// with only part of its hits. Each write claims the call row with its
    /// own random token, and its hits and fetch time apply only while the
    /// row still carries that token, so two concurrent answers to one call
    /// are never mixed: the later claim wins, and the earlier write adds
    /// nothing after it.
    ///
    /// # Arguments
    ///
    /// * `call_key` - The call's key.
    /// * `source` - Storefront plugin id that answered.
    /// * `hits` - The listings, in the store's order.
    ///
    /// # Errors
    ///
    /// Returns the database error.
    pub async fn put_discover_store_call(
        &self,
        call_key: &str,
        source: &str,
        hits: &[StoreCallHit],
    ) -> Result<()> {
        let writer = uuid::Uuid::new_v4().simple().to_string();
        let parts = store_call_statements(call_key, source, &writer, hits);
        let mut batches = WriteBatches::new(self.db_capabilities(), "discover store call");
        if let Some(full) = batches.push_group(parts.head)? {
            self.run_candidate_batch(full).await?;
        }
        for stmt in parts.hits {
            if let Some(full) = batches.push(stmt)? {
                self.run_candidate_batch(full).await?;
            }
        }
        if let Some(full) = batches.push(parts.stamp)? {
            self.run_candidate_batch(full).await?;
        }
        if let Some(last) = batches.finish() {
            self.run_candidate_batch(last).await?;
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
        // The hits first, so a backend that does not enforce the cascade
        // keeps no orphans either.
        let cutoff_text = cutoff.to_rfc3339();
        let operation_id = format!("discover-store-call-prune-{}", uuid::Uuid::new_v4());
        self.execute_host_batch(batch::request(
            &operation_id,
            vec![batch::exec(
                "DELETE FROM discover_store_call_hits WHERE call_key IN \
                 (SELECT call_key FROM discover_store_calls WHERE fetched_at < ?)",
                vec![batch::text(&cutoff_text)],
            )],
        ))
        .await?;
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
        let upsert = discover_feeds::Entity::insert(row).on_conflict(
            OnConflict::column(discover_feeds::Column::SubjectKey)
                .update_columns([
                    discover_feeds::Column::FeedJson,
                    discover_feeds::Column::SeedScope,
                    discover_feeds::Column::CandidatesFetchedAt,
                    discover_feeds::Column::ComputedAt,
                ])
                .to_owned(),
        );
        self.exec_bumped(upsert, LiveTopic::Discover, Bump::Always)
            .await?;
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
            let rows = discover_feeds::Entity::find()
                .select_only()
                .column(discover_feeds::Column::Id)
                .column(discover_feeds::Column::SubjectKey)
                .filter(discover_feeds::Column::Id.gt(after))
                .order_by_asc(discover_feeds::Column::Id)
                .limit(DISCOVER_READ_PAGE)
                .into_model::<FeedSubjectRow>()
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
            let Some(last) = rows.last() else { break };
            after = last.id;
            let full = u64::try_from(rows.len()).is_ok_and(|n| n >= DISCOVER_READ_PAGE);
            out.extend(
                rows.iter()
                    .filter_map(|row| DiscoverSubject::parse(&row.subject_key)),
            );
            if !full {
                break;
            }
        }
        Ok(out)
    }
}

/// One stored feed's id and subject key, read by column name (#854).
#[derive(Debug, sea_orm::FromQueryResult)]
struct FeedSubjectRow {
    /// `discover_feeds.id`: the page cursor.
    id: i64,
    /// `discover_feeds.subject_key`: a [`DiscoverSubject`] key.
    subject_key: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_plugin_abi::DbCapabilities;

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

    /// Catalog listings for `products` (one store, `us`), recorded into the
    /// pool, as candidate references whose origin is the product id.
    async fn refs(store: &LibraryStore, products: &[&str]) -> Vec<CandidateRef> {
        let listings: Vec<crate::CatalogListing> = products
            .iter()
            .map(|p| crate::CatalogListing {
                draft: bookclerk_resolve::ListingDraft {
                    plugin_key: String::from("libro"),
                    marketplace: Some(String::from("us")),
                    product_id: (*p).to_string(),
                    title: format!("Title {p}"),
                    credits: vec![(
                        format!("Author {p}"),
                        bookclerk_resolve::CreditRole::Author,
                        None,
                    )],
                    ..bookclerk_resolve::ListingDraft::default()
                },
                detail: crate::ListingDetail::default(),
                offers: Vec::new(),
            })
            .collect();
        store
            .record_catalog_listings(&listings)
            .await
            .unwrap()
            .ids
            .into_iter()
            .zip(products)
            .map(|(id, p)| CandidateRef {
                listing_id: id.unwrap(),
                origin: (*p).to_string(),
                seed_title: None,
                seed_categories: None,
                starter_genre: None,
            })
            .collect()
    }

    /// Store call hits for listing references, in order.
    fn hits(refs: &[CandidateRef]) -> Vec<StoreCallHit> {
        refs.iter()
            .map(|r| StoreCallHit {
                listing_id: r.listing_id.clone(),
                origin: r.origin.clone(),
                rank_hint: None,
            })
            .collect()
    }

    /// Stands in for the database plugin's atomic batch: applies each host
    /// batch on the open connection without `BEGIN`, and records how many
    /// statements every batch carried.
    struct RecordingBatches {
        /// The store's own connection.
        db: sea_orm::DatabaseConnection,
        /// Statement count of each batch, in the order they arrived.
        sizes: std::sync::Arc<std::sync::Mutex<Vec<usize>>>,
    }

    #[async_trait::async_trait]
    impl crate::TypedAtomicExec for RecordingBatches {
        async fn execute_typed(
            &self,
            envelope: bookclerk_db_exec::AdapterExecuteRequest,
        ) -> std::result::Result<
            bookclerk_plugin_abi::ExecuteReply,
            bookclerk_plugin_abi::PluginError,
        > {
            self.sizes
                .lock()
                .unwrap()
                .push(envelope.request.statements.len());
            bookclerk_db_exec::execute_typed_on_open_connection(
                &self.db,
                &envelope,
                bookclerk_db_exec::ExecCaps::from(8u32),
                bookclerk_db_exec::AtomicSession::from_deadline(None),
                None,
            )
            .await
            .map_err(|err| bookclerk_plugin_abi::PluginError::unavailable(err.to_string()))
        }
    }

    /// A store whose host batches go through [`RecordingBatches`], with
    /// `caps` as the plugin's advertised limits.
    async fn batch_store(
        caps: DbCapabilities,
    ) -> (LibraryStore, std::sync::Arc<std::sync::Mutex<Vec<usize>>>) {
        let store = store().await.with_db_capabilities(caps);
        let sizes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let exec = RecordingBatches {
            db: store.db().clone(),
            sizes: sizes.clone(),
        };
        (store.with_typed_exec(std::sync::Arc::new(exec)), sizes)
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
            .replace_discover_candidates(
                &alice,
                "libro",
                SeedScope::Own,
                &refs(&store, &["a", "b"]).await,
            )
            .await
            .unwrap();
        store
            .replace_discover_candidates(
                &alice,
                "chirp",
                SeedScope::Own,
                &refs(&store, &["c"]).await,
            )
            .await
            .unwrap();
        store
            .replace_discover_candidates(
                &bob,
                "libro",
                SeedScope::Household,
                &refs(&store, &["z"]).await,
            )
            .await
            .unwrap();
        // A new answer from one store replaces only that store's rows.
        store
            .replace_discover_candidates(
                &alice,
                "libro",
                SeedScope::Own,
                &refs(&store, &["d"]).await,
            )
            .await
            .unwrap();

        let mut keys: Vec<(String, String)> = store
            .list_discover_candidates(&alice)
            .await
            .unwrap()
            .into_iter()
            .map(|c| (c.source, c.listing.product_id))
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
        assert_eq!(bob_rows[0].origin, "z");
        assert_eq!(bob_rows[0].listing.title, "Title z");
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
    /// write must not open one: a refused `begin` leaves it working. The
    /// writes go to the plugin as atomic batches instead.
    #[tokio::test]
    async fn candidates_replace_without_an_interactive_transaction() {
        let (store, sizes) = batch_store(DbCapabilities::advertised_d1()).await;
        let alice = DiscoverSubject::User(1);
        let pool = refs(&store, &["a", "b", "c"]).await;
        store
            .replace_discover_candidates(&alice, "libro", SeedScope::Own, &pool[..2])
            .await
            .unwrap();
        let mut again = vec![pool[1].clone(), pool[2].clone(), pool[2].clone()];
        again[0].origin = String::from("b2");
        again[2].origin = String::from("c2");
        sizes.lock().unwrap().clear();
        crate::inject_begin_failures(1);
        let replaced = store
            .replace_discover_candidates(&alice, "libro", SeedScope::Household, &again)
            .await;
        let unused_injection = crate::consume_begin_injection();
        replaced.unwrap();
        assert!(unused_injection, "the write opened a transaction");
        // Two upsert pairs plus the delete, in one batch.
        assert_eq!(*sizes.lock().unwrap(), vec![5]);
        let mut got: Vec<(String, String, SeedScope)> = store
            .list_discover_candidates(&alice)
            .await
            .unwrap()
            .into_iter()
            .map(|c| (c.listing.product_id, c.origin, c.seed_scope))
            .collect();
        got.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            got,
            vec![
                (String::from("b"), String::from("b2"), SeedScope::Household),
                (String::from("c"), String::from("c2"), SeedScope::Household),
            ]
        );
    }

    /// A list longer than one batch is split by the plugin's statement cap,
    /// and the replace still keeps only the new rows of that store.
    #[tokio::test]
    async fn candidates_replace_in_batches_past_the_statement_cap() {
        let mut caps = DbCapabilities::advertised_d1();
        caps.max_statements = bookclerk_plugin_abi::HOST_MIN_STATEMENTS;
        let (store, sizes) = batch_store(caps).await;
        let alice = DiscoverSubject::User(1);
        store
            .replace_discover_candidates(
                &alice,
                "libro",
                SeedScope::Own,
                &refs(&store, &["old", "kept"]).await,
            )
            .await
            .unwrap();
        store
            .replace_discover_candidates(
                &alice,
                "chirp",
                SeedScope::Own,
                &refs(&store, &["other"]).await,
            )
            .await
            .unwrap();
        let names: Vec<String> = (0..60).map(|i| format!("n{i:02}")).collect();
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut fresh = refs(&store, &["kept"]).await;
        fresh.extend(refs(&store, &name_refs).await);
        sizes.lock().unwrap().clear();
        store
            .replace_discover_candidates(&alice, "libro", SeedScope::Household, &fresh)
            .await
            .unwrap();

        // 61 upsert pairs and the delete: 123 statements in batches of at most 40.
        let sent = sizes.lock().unwrap().clone();
        assert_eq!(sent.iter().sum::<usize>(), 123);
        assert!(sent.len() >= 4, "{sent:?}");
        assert!(sent.iter().all(|n| *n <= 40), "{sent:?}");
        let listed = store.list_discover_candidates(&alice).await.unwrap();
        let mut libro: Vec<&str> = listed
            .iter()
            .filter(|c| c.source == "libro")
            .map(|c| c.listing.product_id.as_str())
            .collect();
        libro.sort_unstable();
        assert_eq!(libro.len(), 61);
        assert!(!libro.contains(&"old"));
        assert!(libro.contains(&"kept"));
        assert!(listed
            .iter()
            .filter(|c| c.source == "libro")
            .all(|c| c.seed_scope == SeedScope::Household));
        assert_eq!(
            listed.iter().filter(|c| c.source == "chirp").count(),
            1,
            "another store's rows are untouched"
        );
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
        assert_eq!(feed.feed_json, quoted);
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
            .replace_discover_candidates(
                &alice,
                "libro",
                SeedScope::Own,
                &refs(&store, &["a"]).await,
            )
            .await
            .unwrap();
        store
            .replace_discover_candidates(
                &alice,
                "chirp",
                SeedScope::Own,
                &refs(&store, &["b"]).await,
            )
            .await
            .unwrap();
        store
            .replace_discover_candidates(
                &alice,
                "libro",
                SeedScope::Own,
                &refs(&store, &["a"]).await,
            )
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
    async fn concurrent_answers_to_one_call_are_never_mixed() {
        let store = store().await;
        let day = chrono::Duration::hours(24);
        let pool = refs(&store, &["a", "b", "c", "d"]).await;
        let run = |statements: Vec<TypedDbStatement>| {
            let store = &store;
            async move { store.run_candidate_batch(statements).await.unwrap() }
        };
        let first = super::store_call_statements("k1", "libro", "w1", &hits(&pool[..2]));
        let second = super::store_call_statements("k1", "libro", "w2", &hits(&pool[2..]));
        // The second write claims the call while the first is between
        // batches: the first write's hits and stamp no longer apply.
        run(first.head).await;
        run(second.head).await;
        run(first.hits).await;
        run(vec![first.stamp]).await;
        assert!(
            store
                .get_discover_store_call("k1", day)
                .await
                .unwrap()
                .is_none(),
            "a taken-over write never marks the answer fresh"
        );
        run(second.hits).await;
        run(vec![second.stamp]).await;
        assert_eq!(
            store.get_discover_store_call("k1", day).await.unwrap(),
            Some(hits(&pool[2..]))
        );
        // A write taken over after its hits landed loses them to the new
        // claim, and its late stamp changes nothing.
        let third = super::store_call_statements("k1", "libro", "w3", &hits(&pool[..1]));
        let fourth = super::store_call_statements("k1", "libro", "w4", &hits(&pool[1..2]));
        run(third.head).await;
        run(third.hits).await;
        run(fourth.head).await;
        run(vec![third.stamp]).await;
        assert!(store
            .get_discover_store_call("k1", day)
            .await
            .unwrap()
            .is_none());
        run(fourth.hits).await;
        run(vec![fourth.stamp]).await;
        assert_eq!(
            store.get_discover_store_call("k1", day).await.unwrap(),
            Some(hits(&pool[1..2]))
        );
    }

    #[tokio::test]
    async fn store_calls_are_cached_refreshed_and_pruned() {
        let store = store().await;
        let day = chrono::Duration::hours(24);
        let pool = refs(&store, &["a", "b", "c"]).await;
        assert!(store
            .get_discover_store_call("k1", day)
            .await
            .unwrap()
            .is_none());
        store
            .put_discover_store_call("k1", "libro", &hits(&pool[..2]))
            .await
            .unwrap();
        assert_eq!(
            store.get_discover_store_call("k1", day).await.unwrap(),
            Some(hits(&pool[..2]))
        );
        // A newer answer for the same call replaces the old one, in its order.
        let newer = vec![pool[2].clone(), pool[0].clone()];
        store
            .put_discover_store_call("k1", "libro", &hits(&newer))
            .await
            .unwrap();
        assert_eq!(
            store.get_discover_store_call("k1", day).await.unwrap(),
            Some(hits(&newer))
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
        use sea_orm::PaginatorTrait as _;
        assert_eq!(
            discover_store_call_hits::Entity::find()
                .count(store.db())
                .await
                .unwrap(),
            0,
            "the pruned call's hits went with it"
        );
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
                .replace_discover_candidates(
                    subject,
                    "libro",
                    SeedScope::Own,
                    &refs(&store, &["k"]).await,
                )
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
            .replace_discover_candidates(
                &alice,
                "libro",
                SeedScope::Own,
                &refs(&store, &key_refs).await,
            )
            .await
            .unwrap();
        let listed = store.list_discover_candidates(&alice).await.unwrap();
        assert_eq!(listed.len(), count);
        assert_eq!(listed[0].listing.product_id, "k0");
        assert_eq!(
            listed[count - 1].listing.product_id,
            format!("k{}", count - 1)
        );

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

    #[tokio::test]
    async fn a_stored_taste_is_replaced_and_forgotten_with_its_subject() {
        use crate::test_support::{sqlite, Profile};
        for profile in Profile::ALL {
            let kit = sqlite::memory(profile).await;
            let store = kit.store();
            let name = profile.name();
            let alice = DiscoverSubject::User(1);
            let bob = DiscoverSubject::User(2);
            assert_eq!(
                store.get_discover_taste(&alice).await.unwrap(),
                None,
                "{name}"
            );
            store
                .put_discover_taste(&alice, "{\"v\":1}", SeedScope::Household)
                .await
                .unwrap();
            store
                .put_discover_taste(&alice, "{\"v\":2}", SeedScope::Own)
                .await
                .unwrap();
            store
                .put_discover_taste(&bob, "{}", SeedScope::Household)
                .await
                .unwrap();
            let got = store.get_discover_taste(&alice).await.unwrap().unwrap();
            assert_eq!(got.taste_json, "{\"v\":2}", "{name}");
            assert_eq!(got.seed_scope, SeedScope::Own, "{name}");
            assert!(got.computed_at <= Utc::now(), "{name}");

            store.forget_discover_subject(&alice).await.unwrap();
            assert_eq!(
                store.get_discover_taste(&alice).await.unwrap(),
                None,
                "{name}"
            );
            assert!(
                store.get_discover_taste(&bob).await.unwrap().is_some(),
                "{name}"
            );
        }
    }
}
