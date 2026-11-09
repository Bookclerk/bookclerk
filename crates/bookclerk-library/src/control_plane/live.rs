//! Change revisions for the SPA's live socket (H-4 step 10).
//!
//! Every owner node shares the library database, so a change made on any
//! of them (a job finishing elsewhere, a scan's rows, a heartbeat, a
//! configuration write) shows up here. Each topic has one counter row in
//! `live_topic_revisions`, and every write to the table behind a topic
//! bumps that counter in the same atomic batch as the write
//! ([`LibraryStore::exec_bumped`], [`bump`], [`bump_if_stamped`],
//! [`bump_if_any`]). [`live_revisions`] reads the whole counter table, at
//! most one row per topic, so a poll costs the same however large the
//! library grows. The daemon compares successive reads and tells the SPA
//! which topics changed; the SPA then re-reads them through the ordinary
//! authorized routes. A revision carries no row data.
//!
//! A counter only moves forward. A bump that a write did not need (a
//! compare-and-swap that lost, say) costs the SPA one extra re-read; a
//! missing bump would leave a page stale, so every write path to a live
//! table names its topic.

use bookclerk_plugin_abi::{DbPlanStatementKind, DbResultSelection, DbValue, TypedDbStatement};
use sea_orm::{ConnectionTrait, QueryTrait};

use crate::control_plane::batch::{exec, query, request, rows_affected, text};
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// A topic of the live socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LiveTopic {
    /// The library's titles (`books`).
    Library,
    /// Jobs (`jobs`).
    Jobs,
    /// Event deliveries (`event_deliveries`).
    Deliveries,
    /// Node heartbeats (`node_status`).
    Nodes,
    /// Cluster configuration documents (`configuration_changes`).
    Config,
    /// Owner-plugin consent on this host (`remote_plugin_consents`).
    Consents,
    /// Owner clusters this host joined, and their state (`cluster_links`).
    Clusters,
    /// Users (`users`).
    Users,
    /// Stored Discover shelves (`discover_feeds`): a ranking job wrote or
    /// dropped someone's shelves.
    Discover,
}

impl LiveTopic {
    /// Every topic.
    pub const ALL: [Self; 9] = [
        Self::Library,
        Self::Jobs,
        Self::Deliveries,
        Self::Nodes,
        Self::Config,
        Self::Consents,
        Self::Clusters,
        Self::Users,
        Self::Discover,
    ];

    /// The topic's wire name, also its key in `live_topic_revisions`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Library => "library",
            Self::Jobs => "jobs",
            Self::Deliveries => "deliveries",
            Self::Nodes => "nodes",
            Self::Config => "config",
            Self::Consents => "consents",
            Self::Clusters => "clusters",
            Self::Users => "users",
            Self::Discover => "discover",
        }
    }

    /// The topic for a wire name, or `None` for a name no topic has.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|topic| topic.as_str() == name)
    }

    /// The table behind the topic and its change-stamp column, which every
    /// write to the table sets to the time of the write.
    ///
    /// `None` for `cluster_links`, which has no change stamp, and for
    /// `configuration_changes`, an append-only log whose stamp is not
    /// indexed; their writes bump on a probe instead.
    #[must_use]
    pub fn stamp_column(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Library => Some(("books", "updated_at")),
            Self::Jobs => Some(("jobs", "updated_at")),
            Self::Deliveries => Some(("event_deliveries", "updated_at")),
            Self::Nodes => Some(("node_status", "heartbeat_at")),
            Self::Config => None,
            Self::Consents => Some(("remote_plugin_consents", "updated_at")),
            Self::Clusters => None,
            Self::Users => Some(("users", "updated_at")),
            Self::Discover => Some(("discover_feeds", "computed_at")),
        }
    }
}

/// One topic's revision: a counter that every write to the topic's table
/// moves forward by at least one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LiveRevision(pub i64);

/// Every topic's revision, in [`LiveTopic::ALL`] order.
pub type LiveRevisions = Vec<(LiveTopic, LiveRevision)>;

/// The counter bump, unconditional: for inserts, deletes of a known row,
/// and writes that always change a row.
#[must_use]
pub(crate) fn bump(topic: LiveTopic) -> TypedDbStatement {
    exec(
        "UPDATE live_topic_revisions SET revision = revision + 1 WHERE topic = ?",
        vec![text(topic.as_str())],
    )
}

/// Runs [`bump`] on `conn`, for the in-process fallbacks that write inside
/// an open local transaction (local tools and tests without a database
/// plugin); the bump commits or rolls back with that transaction.
///
/// # Errors
///
/// Returns the write error.
pub(crate) async fn bump_on<C: sea_orm::ConnectionTrait>(conn: &C, topic: LiveTopic) -> Result<()> {
    conn.execute_raw(bookclerk_db_exec::canonical_statement(
        "UPDATE live_topic_revisions SET revision = revision + 1 WHERE topic = ?",
        [sea_orm::Value::from(topic.as_str())],
    ))
    .await
    .map_err(LibraryError::Orm)?;
    Ok(())
}

/// Runs `write` and its topic's bump through the store as one batch
/// ([`LibraryStore::exec_bumped`]) when `store` is given, or on `conn`, an
/// open local transaction, followed by [`bump_on`] when the write changed a
/// row (the transaction makes the pair atomic).
///
/// For write helpers shared by the store's own paths and the in-process
/// fallbacks that run inside a local `begin()` (local tools and tests).
///
/// # Returns
///
/// The rows the write changed.
///
/// # Errors
///
/// Returns the write error.
pub(crate) async fn exec_bumped_via<C, Q>(
    conn: &C,
    store: Option<&LibraryStore>,
    write: Q,
    topic: LiveTopic,
    when: Bump,
) -> Result<u64>
where
    C: sea_orm::ConnectionTrait,
    Q: QueryTrait,
{
    if let Some(store) = store {
        return store.exec_bumped(write, topic, when).await;
    }
    let changed = conn
        .execute_raw(bookclerk_db_exec::build_canonical(&write))
        .await
        .map_err(LibraryError::Orm)?
        .rows_affected();
    if changed > 0 {
        bump_on(conn, topic).await?;
    }
    Ok(changed)
}

/// The counter bump, taken only when a row of the topic's table carries
/// `stamp` in its change-stamp column. Placed after a write that sets the
/// stamp, it bumps exactly when the write changed a row (the stamp columns
/// are indexed or the tables small, so the check is one seek).
///
/// # Errors
///
/// Returns [`LibraryError::Other`] for a topic without a change stamp
/// (`clusters`).
pub(crate) fn bump_if_stamped(topic: LiveTopic, stamp: &str) -> Result<TypedDbStatement> {
    let (table, column) = topic.stamp_column().ok_or_else(|| {
        LibraryError::Other(anyhow::anyhow!(
            "live topic `{}` has no change stamp",
            topic.as_str()
        ))
    })?;
    Ok(exec(
        &format!(
            "UPDATE live_topic_revisions SET revision = revision + 1 \
             WHERE topic = ? AND EXISTS (SELECT 1 FROM {table} WHERE {column} = ?)"
        ),
        vec![text(topic.as_str()), text(stamp)],
    ))
}

/// The counter bump, taken only when `probe` (a `SELECT`) finds a row.
/// Placed before a delete or sweep with the same filter, it bumps exactly
/// when the write is about to change a row.
#[must_use]
pub(crate) fn bump_if_any(topic: LiveTopic, probe: TypedDbStatement) -> TypedDbStatement {
    let mut parameters = vec![text(topic.as_str())];
    parameters.extend(probe.parameters);
    exec(
        &format!(
            "UPDATE live_topic_revisions SET revision = revision + 1 \
             WHERE topic = ? AND EXISTS ({})",
            probe.sql
        ),
        parameters,
    )
}

/// A SeaORM query as a canonical batch statement of `Execute` kind.
///
/// SeaORM renders SQLite-shaped SQL; its quoted identifiers are dropped as
/// the plugin host's SeaORM proxy drops them, and adapters lower the rest.
///
/// # Errors
///
/// Returns [`LibraryError::Other`] when a bind has no canonical type.
pub(crate) fn sea_exec<Q: QueryTrait>(statement: &Q) -> Result<TypedDbStatement> {
    let built = bookclerk_db_exec::build_canonical(statement);
    let parameters = built
        .values
        .map(|values| values.0)
        .unwrap_or_default()
        .iter()
        .map(bookclerk_db_exec::db_value_from_sea)
        .collect::<std::result::Result<Vec<DbValue>, String>>()
        .map_err(|err| LibraryError::Other(anyhow::anyhow!("live bump bind: {err}")))?;
    Ok(exec(
        &crate::unquote_sea_identifiers(&built.sql),
        parameters,
    ))
}

/// [`sea_exec`] for a probe: a SeaORM `SELECT` to pass to [`bump_if_any`].
///
/// # Errors
///
/// Returns [`LibraryError::Other`] when a bind has no canonical type.
pub(crate) fn sea_probe<Q: QueryTrait>(select: &Q) -> Result<TypedDbStatement> {
    sea_exec(select)
}

/// When a write made through [`LibraryStore::exec_bumped`] bumps its topic.
#[derive(Debug, Clone)]
pub(crate) enum Bump {
    /// Whenever the batch commits.
    Always,
    /// When a row carries this change stamp after the write
    /// ([`bump_if_stamped`]).
    Stamped(String),
    /// When this probe finds a row before the write ([`bump_if_any`]).
    IfAny(TypedDbStatement),
}

impl LibraryStore {
    /// Runs one SeaORM write and its topic's counter bump as one atomic
    /// batch.
    ///
    /// # Arguments
    ///
    /// * `write` - The write (`insert`, `update_many`, `delete_many`, a
    ///   validated `update`, …).
    /// * `topic` - The live topic of the table it writes.
    /// * `when` - When the counter moves.
    ///
    /// A write too large for one batch statement (past `maxBinds` or
    /// `maxPayloadBytes`) runs on the store connection instead and bumps
    /// afterwards ([`Self::bump_after_oversized_write`]).
    ///
    /// # Returns
    ///
    /// The rows the write changed.
    ///
    /// # Errors
    ///
    /// Returns the batch's typecheck, transport or engine error; nothing is
    /// written then.
    pub(crate) async fn exec_bumped<Q: QueryTrait>(
        &self,
        write: Q,
        topic: LiveTopic,
        when: Bump,
    ) -> Result<u64> {
        let built = sea_exec(&write)?;
        if !self.admits_statement(&built) {
            let changed = self
                .db()
                .execute_raw(bookclerk_db_exec::build_canonical(&write))
                .await
                .map_err(LibraryError::Orm)?
                .rows_affected();
            if changed > 0 {
                self.bump_after_oversized_write(topic).await;
            }
            return Ok(changed);
        }
        let write = built;
        let (statements, index) = match when {
            Bump::Always => (vec![write, bump(topic)], 0),
            Bump::Stamped(stamp) => (vec![write, bump_if_stamped(topic, &stamp)?], 0),
            Bump::IfAny(probe) => (vec![bump_if_any(topic, probe), write], 1),
        };
        let reply = self
            .execute_host_batch(request(
                &format!("live.{}:{}", topic.as_str(), uuid::Uuid::new_v4()),
                statements,
            ))
            .await?;
        Ok(rows_affected(&reply, index))
    }

    /// Inserts one row through SeaORM and bumps `topic`, as one atomic
    /// batch, and answers the new row's identity (the table's
    /// `INTEGER PRIMARY KEY`). An insert too large for one batch statement
    /// runs on the store connection and bumps afterwards, as
    /// [`Self::exec_bumped`] does.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Other`] when the insert is not a single-row
    /// insert into a table with an identity column, and the batch's error.
    pub(crate) async fn insert_bumped<Q: QueryTrait>(
        &self,
        insert: Q,
        topic: LiveTopic,
    ) -> Result<i64> {
        let mut write = sea_exec(&insert)?;
        if !self.admits_statement(&write) {
            let inserted = self
                .db()
                .execute_raw(bookclerk_db_exec::build_canonical(&insert))
                .await
                .map_err(LibraryError::Orm)?;
            let id = i64::try_from(inserted.last_insert_id()).map_err(|_| {
                LibraryError::Other(anyhow::anyhow!("live insert identity is out of range"))
            })?;
            self.bump_after_oversized_write(topic).await;
            return Ok(id);
        }
        write.sql = crate::proxy_insert_returning_identity(
            &write.sql,
            &crate::migrations::host_sql_type_env(),
        )
        .ok_or_else(|| {
            LibraryError::Other(anyhow::anyhow!(
                "live insert is not a single-row insert into a table with an identity"
            ))
        })?;
        write.kind = DbPlanStatementKind::Returning;
        write.max_rows = 1;
        write.result_selection = DbResultSelection::Rows;
        let reply = self
            .execute_host_batch(request(
                &format!("live.{}:{}", topic.as_str(), uuid::Uuid::new_v4()),
                vec![write, bump(topic)],
            ))
            .await?;
        match reply
            .statements
            .first()
            .and_then(|stmt| stmt.rows.first())
            .and_then(|row| row.values.first())
        {
            Some(DbValue::Int64(id)) => Ok(*id),
            other => Err(LibraryError::Other(anyhow::anyhow!(
                "live insert returned no identity: {other:?}"
            ))),
        }
    }

    /// Whether `statement` fits the adapter's negotiated per-statement
    /// limits (`maxBinds`, `maxPayloadBytes`, statement size) on its own.
    pub(crate) fn admits_statement(&self, statement: &TypedDbStatement) -> bool {
        crate::sql_plan::validate_execute_request(
            &request("live.admit", vec![statement.clone()]),
            self.db_capabilities(),
        )
        .is_ok()
    }

    /// Bumps `topic` after a write too large for a host batch, which ran
    /// on the store connection as it did before the counters existed.
    ///
    /// The write has committed, so a failed bump is logged rather than
    /// returned: it costs one missed notice, which the web app's periodic
    /// re-reads cover, and must not report the write as failed.
    pub(crate) async fn bump_after_oversized_write(&self, topic: LiveTopic) {
        if let Err(err) = self.bump_live_topic(topic).await {
            tracing::warn!(
                topic = topic.as_str(),
                error = %err,
                "live counter bump after an oversized write failed"
            );
        }
    }

    /// Bumps `topic` on its own, for a write the counter cannot join (one
    /// that runs on a connection of its own, such as a schema reset).
    ///
    /// # Errors
    ///
    /// Returns the write error.
    pub async fn bump_live_topic(&self, topic: LiveTopic) -> Result<()> {
        self.execute_host_batch(request(
            &format!("live.{}:{}", topic.as_str(), uuid::Uuid::new_v4()),
            vec![bump(topic)],
        ))
        .await?;
        Ok(())
    }
}

/// Reads every topic's revision: one batch of one-row reads of
/// `live_topic_revisions`, one per topic, so it fits an adapter that
/// returns a single row per statement.
///
/// A topic without a counter row (a database the seed has not reached yet)
/// reads as revision 0.
///
/// # Errors
///
/// Returns the read error.
pub async fn live_revisions(store: &LibraryStore) -> Result<LiveRevisions> {
    let reads = LiveTopic::ALL
        .iter()
        .map(|topic| {
            query(
                "SELECT revision FROM live_topic_revisions WHERE topic = ?",
                vec![text(topic.as_str())],
                1,
            )
        })
        .collect();
    let reply = store
        .execute_host_batch(request(
            &format!("live.read:{}", uuid::Uuid::new_v4()),
            reads,
        ))
        .await?;
    Ok(LiveTopic::ALL
        .into_iter()
        .enumerate()
        .map(|(index, topic)| {
            let revision = reply
                .statements
                .get(index)
                .and_then(|stmt| stmt.rows.first())
                .and_then(|row| row.values.first());
            match revision {
                Some(DbValue::Int64(revision)) => (topic, LiveRevision(*revision)),
                _ => (topic, LiveRevision::default()),
            }
        })
        .collect())
}

/// The topics whose revision differs between `before` and `after`.
#[must_use]
pub fn changed_topics(before: &LiveRevisions, after: &LiveRevisions) -> Vec<LiveTopic> {
    after
        .iter()
        .filter(|(topic, value)| {
            before
                .iter()
                .find(|(seen, _)| seen == topic)
                .is_none_or(|(_, old)| old != value)
        })
        .map(|(topic, _)| *topic)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{EnqueueJobSpec, JobKind, JobPayload};

    async fn store() -> LibraryStore {
        let db = bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap();
        crate::apply_host_schema(&db).await.unwrap();
        LibraryStore::from_connection(db)
    }

    /// A queued job changes only the jobs topic; an unchanged database
    /// changes nothing; a first read reports every topic.
    #[tokio::test]
    async fn a_new_job_changes_only_the_jobs_topic() {
        let store = store().await;
        let first = live_revisions(&store).await.unwrap();
        assert_eq!(first.len(), LiveTopic::ALL.len());
        assert_eq!(
            changed_topics(&Vec::new(), &first),
            LiveTopic::ALL.to_vec(),
            "nothing seen yet: every topic is news"
        );
        let again = live_revisions(&store).await.unwrap();
        assert!(changed_topics(&first, &again).is_empty());
        store
            .enqueue_job(EnqueueJobSpec {
                kind: JobKind::Scan,
                payload: JobPayload::default(),
                priority: 0,
                max_attempts: 3,
                max_pending: 16,
                run_after: None,
            })
            .await
            .unwrap();
        let after = live_revisions(&store).await.unwrap();
        assert_eq!(changed_topics(&again, &after), vec![LiveTopic::Jobs]);
        let jobs = after
            .iter()
            .find(|(topic, _)| *topic == LiveTopic::Jobs)
            .unwrap();
        assert_eq!(jobs.1, LiveRevision(1), "one enqueue, one bump");
    }

    /// Writing someone's shelves, re-ranking them and dropping them each
    /// change only the discover topic.
    #[tokio::test]
    async fn stored_shelves_change_the_discover_topic() {
        use crate::{DiscoverSubject, SeedScope};
        let store = store().await;
        let before = live_revisions(&store).await.unwrap();
        let subject = DiscoverSubject::Operator;
        store
            .put_discover_feed(&subject, "{}", SeedScope::Own, None)
            .await
            .unwrap();
        let written = live_revisions(&store).await.unwrap();
        assert_eq!(changed_topics(&before, &written), vec![LiveTopic::Discover]);
        // A later ranking of the same person moves the change stamp.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        store
            .put_discover_feed(&subject, "{}", SeedScope::Own, None)
            .await
            .unwrap();
        let reranked = live_revisions(&store).await.unwrap();
        assert_eq!(
            changed_topics(&written, &reranked),
            vec![LiveTopic::Discover]
        );
        store.forget_discover_subject(&subject).await.unwrap();
        let dropped = live_revisions(&store).await.unwrap();
        assert_eq!(
            changed_topics(&reranked, &dropped),
            vec![LiveTopic::Discover]
        );
        assert_eq!(LiveTopic::Discover.as_str(), "discover");
    }

    /// Joining a cluster, a state change (revoked by the owner) and leaving
    /// each change only the clusters topic, although `cluster_links` has no
    /// change stamp and a state change keeps the row count.
    #[tokio::test]
    async fn joining_revoking_and_leaving_change_the_clusters_topic() {
        use crate::control_plane::{
            leave_cluster_link, record_cluster_link, set_cluster_link_state, ClusterLinkKeys,
            NewClusterLink, CLUSTER_LINK_REVOKED,
        };
        let _lock = crate::master_key::master_key_test_lock_async().await;
        let dir = tempfile::tempdir().unwrap();
        let db = bookclerk_plugin_database_sqlite::open(&dir.path().join("library.db"))
            .await
            .unwrap();
        crate::apply_host_schema(&db).await.unwrap();
        crate::configure_master_key(dir.path()).unwrap();
        let store = LibraryStore::from_connection(db);
        let link = NewClusterLink {
            owner_cluster_id: "cluster-1".into(),
            owner_origin: "https://owner.example".into(),
            remote_node_id: "node-1".into(),
            spki_pin: None,
        };
        let keys = ClusterLinkKeys::from_bytes(&[1; 32], &[2; 32]);

        let before = live_revisions(&store).await.unwrap();
        record_cluster_link(&store, &link, &keys).await.unwrap();
        let joined = live_revisions(&store).await.unwrap();
        assert_eq!(changed_topics(&before, &joined), vec![LiveTopic::Clusters]);

        assert!(
            set_cluster_link_state(&store, "cluster-1", CLUSTER_LINK_REVOKED, None)
                .await
                .unwrap()
        );
        let revoked = live_revisions(&store).await.unwrap();
        assert_eq!(changed_topics(&joined, &revoked), vec![LiveTopic::Clusters]);

        // Re-enrolling makes it active again.
        record_cluster_link(&store, &link, &keys).await.unwrap();
        let rejoined = live_revisions(&store).await.unwrap();
        assert_eq!(
            changed_topics(&revoked, &rejoined),
            vec![LiveTopic::Clusters]
        );

        leave_cluster_link(&store, "cluster-1", None)
            .await
            .unwrap()
            .expect("the link");
        let left = live_revisions(&store).await.unwrap();
        assert_eq!(changed_topics(&rejoined, &left), vec![LiveTopic::Clusters]);
        let again = live_revisions(&store).await.unwrap();
        assert!(changed_topics(&left, &again).is_empty());
    }

    /// The revision `topic` reads at in `revisions`.
    fn revision_of(revisions: &LiveRevisions, topic: LiveTopic) -> LiveRevision {
        revisions
            .iter()
            .find(|(seen, _)| *seen == topic)
            .map(|(_, revision)| *revision)
            .expect("every topic is read")
    }

    /// A fresh schema seeds one counter row per topic, every row names a
    /// known topic, and all of them start at zero.
    #[tokio::test]
    async fn the_schema_seeds_a_counter_for_every_topic() {
        const MAX_TOPIC_ROWS: u32 = 64;
        let store = store().await;
        let rows = store
            .execute_host_batch_limited(
                request(
                    "live.test",
                    vec![query(
                        "SELECT topic, revision FROM live_topic_revisions",
                        vec![],
                        MAX_TOPIC_ROWS,
                    )],
                ),
                MAX_TOPIC_ROWS,
            )
            .await
            .unwrap();
        let mut names: Vec<String> = rows.statements[0]
            .rows
            .iter()
            .map(|row| match &row.values[0] {
                DbValue::Text(name) => name.clone(),
                other => panic!("topic is text, got {other:?}"),
            })
            .collect();
        names.sort();
        let mut expected: Vec<String> = LiveTopic::ALL
            .iter()
            .map(|topic| topic.as_str().to_owned())
            .collect();
        expected.sort();
        assert_eq!(names, expected);
        for name in &names {
            assert!(LiveTopic::parse(name).is_some(), "{name} parses");
        }
        assert!(LiveTopic::parse("nope").is_none());
        let first = live_revisions(&store).await.unwrap();
        assert!(first
            .iter()
            .all(|(_, revision)| *revision == LiveRevision(0)));
    }

    /// Claiming a job bumps the jobs topic; a claim, reclaim or prune that
    /// finds nothing to change leaves every counter alone.
    #[tokio::test]
    async fn claims_bump_and_empty_sweeps_do_not() {
        use crate::models::JobResourceClass;
        let store = store().await;
        let idle = live_revisions(&store).await.unwrap();
        assert!(store
            .claim_next_job(JobResourceClass::Network, "node-a", 60, "op-empty")
            .await
            .unwrap()
            .is_none());
        assert_eq!(store.reclaim_expired_event_deliveries().await.unwrap(), 0);
        assert_eq!(store.prune_terminal_jobs(7).await.unwrap(), 0);
        let after_sweeps = live_revisions(&store).await.unwrap();
        assert!(
            changed_topics(&idle, &after_sweeps).is_empty(),
            "no-op sweeps bump nothing"
        );

        store
            .enqueue_job(EnqueueJobSpec {
                kind: JobKind::Scan,
                payload: JobPayload::default(),
                priority: 0,
                max_attempts: 3,
                max_pending: 16,
                run_after: None,
            })
            .await
            .unwrap();
        let queued = live_revisions(&store).await.unwrap();
        let claimed_job = store
            .claim_next_job(JobResourceClass::Network, "node-a", 60, "op-claim")
            .await
            .unwrap();
        assert!(claimed_job.is_some());
        let claimed = live_revisions(&store).await.unwrap();
        assert_eq!(changed_topics(&queued, &claimed), vec![LiveTopic::Jobs]);
        assert_eq!(
            revision_of(&claimed, LiveTopic::Jobs),
            LiveRevision(revision_of(&queued, LiveTopic::Jobs).0 + 1)
        );
    }

    /// Creating a user and changing their status each change only the
    /// users topic.
    #[tokio::test]
    async fn user_writes_change_the_users_topic() {
        use crate::models::{UserRole, UserStatus};
        let store = store().await;
        let before = live_revisions(&store).await.unwrap();
        // Two owners, so disabling one is allowed.
        store
            .create_user_with_profile(UserRole::Owner, Some("A"), Some("a"), None, None)
            .await
            .unwrap();
        let user = store
            .create_user_with_profile(UserRole::Owner, Some("B"), Some("b"), None, None)
            .await
            .unwrap();
        let created = live_revisions(&store).await.unwrap();
        assert_eq!(changed_topics(&before, &created), vec![LiveTopic::Users]);
        store
            .set_user_status(user.id, UserStatus::Disabled)
            .await
            .unwrap();
        let disabled = live_revisions(&store).await.unwrap();
        assert_eq!(changed_topics(&created, &disabled), vec![LiveTopic::Users]);
    }

    /// An explicit bump moves only the named topic.
    #[tokio::test]
    async fn an_explicit_bump_moves_one_topic() {
        let store = store().await;
        let before = live_revisions(&store).await.unwrap();
        store.bump_live_topic(LiveTopic::Config).await.unwrap();
        let after = live_revisions(&store).await.unwrap();
        assert_eq!(changed_topics(&before, &after), vec![LiveTopic::Config]);
    }

    /// Under every database profile the counters read back, and a book
    /// write (too large for one batch statement under the smallest
    /// profile, so it takes the oversized-write path there) bumps only the
    /// library topic.
    #[tokio::test]
    async fn counters_work_under_every_profile() {
        use crate::NewBook;
        for profile in crate::test_support::Profile::ALL {
            let kit = crate::test_support::sqlite::memory(profile).await;
            let store = kit.store();
            store
                .upsert_account("audible", "a1", "us", Some("a1"), true, "audible")
                .await
                .unwrap();
            let before = live_revisions(store).await.unwrap();
            store
                .upsert_book(&NewBook::minimal("B00001", "a1", "us", "Title"))
                .await
                .unwrap();
            let after = live_revisions(store).await.unwrap();
            assert_eq!(
                changed_topics(&before, &after),
                vec![LiveTopic::Library],
                "{}",
                profile.name()
            );
            assert_eq!(kit.refused_begins(), 0, "{}", profile.name());
        }
    }
}
