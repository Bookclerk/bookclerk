//! Change revisions for the SPA's live socket (H-4 step 10).
//!
//! Every owner node shares the library database, so a change made on any
//! of them (a job finishing elsewhere, a scan's rows, a heartbeat, a
//! configuration write) shows up here. [`live_revisions`] reads a cheap
//! revision per topic: the row count and the newest change stamp of the
//! table behind it (for `cluster_links`, which has no change stamp, a
//! digest of its few rows). The daemon compares successive reads and tells the
//! SPA which topics changed; the SPA then re-reads them through the
//! ordinary authorized routes. A revision carries no row data.

use sea_orm::{EntityTrait, PaginatorTrait, QueryOrder, QuerySelect};
use sha2::{Digest as _, Sha256};

use crate::entities::{
    books, cluster_links, configuration_changes, event_deliveries, jobs, node_status,
    remote_plugin_consents, users,
};
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
}

impl LiveTopic {
    /// Every topic.
    pub const ALL: [Self; 8] = [
        Self::Library,
        Self::Jobs,
        Self::Deliveries,
        Self::Nodes,
        Self::Config,
        Self::Consents,
        Self::Clusters,
        Self::Users,
    ];

    /// The topic's wire name.
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
        }
    }
}

/// One topic's revision: its row count and newest change stamp.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveRevision {
    /// Rows behind the topic.
    pub rows: u64,
    /// The newest change stamp (RFC 3339 text or a sequence number), or a
    /// digest of the rows for a table without one.
    pub newest: Option<String>,
}

/// Every topic's revision, in [`LiveTopic::ALL`] order.
pub type LiveRevisions = Vec<(LiveTopic, LiveRevision)>;

/// Count and newest value of one text column.
async fn revision<E, C>(store: &LibraryStore, column: C) -> Result<LiveRevision>
where
    E: EntityTrait,
    E::Model: Sync,
    C: sea_orm::ColumnTrait,
{
    let rows = E::find()
        .count(store.db())
        .await
        .map_err(LibraryError::from_db_err)?;
    let newest: Option<String> = E::find()
        .select_only()
        .column(column)
        .order_by_desc(column)
        .limit(1)
        .into_tuple()
        .one(store.db())
        .await
        .map_err(LibraryError::from_db_err)?;
    Ok(LiveRevision { rows, newest })
}

/// Revision of `cluster_links`: its row count and a digest of every row.
///
/// The table has no change stamp, and a join, a leave or a revoke changes
/// a row's state in place, so the newest enrollment time alone would miss
/// it. It holds one row per owner cluster joined, so reading it whole is
/// cheap.
async fn cluster_links_revision(store: &LibraryStore) -> Result<LiveRevision> {
    let rows = cluster_links::Entity::find()
        .order_by_asc(cluster_links::Column::OwnerClusterId)
        .all(store.db())
        .await
        .map_err(LibraryError::from_db_err)?;
    let mut digest = Sha256::new();
    for row in &rows {
        for field in [
            row.owner_cluster_id.as_str(),
            row.owner_origin.as_str(),
            row.remote_node_id.as_str(),
            row.spki_pin.as_deref().unwrap_or(""),
            row.state.as_str(),
            row.enrolled_at.as_str(),
        ] {
            // Length-prefixed, so field boundaries cannot shift.
            digest.update((field.len() as u64).to_be_bytes());
            digest.update(field.as_bytes());
        }
    }
    let newest = (!rows.is_empty()).then(|| hex::encode(digest.finalize()));
    Ok(LiveRevision {
        rows: rows.len() as u64,
        newest,
    })
}

/// Reads every topic's revision.
///
/// # Errors
///
/// Returns the read error.
pub async fn live_revisions(store: &LibraryStore) -> Result<LiveRevisions> {
    let mut out = Vec::with_capacity(LiveTopic::ALL.len());
    for topic in LiveTopic::ALL {
        let value = match topic {
            LiveTopic::Library => {
                revision::<books::Entity, _>(store, books::Column::UpdatedAt).await?
            }
            LiveTopic::Jobs => revision::<jobs::Entity, _>(store, jobs::Column::UpdatedAt).await?,
            LiveTopic::Deliveries => {
                revision::<event_deliveries::Entity, _>(store, event_deliveries::Column::UpdatedAt)
                    .await?
            }
            LiveTopic::Nodes => {
                revision::<node_status::Entity, _>(store, node_status::Column::HeartbeatAt).await?
            }
            LiveTopic::Config => {
                let rows = configuration_changes::Entity::find()
                    .count(store.db())
                    .await
                    .map_err(LibraryError::from_db_err)?;
                let newest: Option<i64> = configuration_changes::Entity::find()
                    .select_only()
                    .column(configuration_changes::Column::Id)
                    .order_by_desc(configuration_changes::Column::Id)
                    .limit(1)
                    .into_tuple()
                    .one(store.db())
                    .await
                    .map_err(LibraryError::from_db_err)?;
                LiveRevision {
                    rows,
                    newest: newest.map(|id| id.to_string()),
                }
            }
            LiveTopic::Consents => {
                revision::<remote_plugin_consents::Entity, _>(
                    store,
                    remote_plugin_consents::Column::UpdatedAt,
                )
                .await?
            }
            LiveTopic::Clusters => cluster_links_revision(store).await?,
            LiveTopic::Users => {
                revision::<users::Entity, _>(store, users::Column::UpdatedAt).await?
            }
        };
        out.push((topic, value));
    }
    Ok(out)
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
        assert_eq!(jobs.1.rows, 1);
        assert!(jobs.1.newest.is_some());
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
            set_cluster_link_state(&store, "cluster-1", CLUSTER_LINK_REVOKED)
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

        leave_cluster_link(&store, "cluster-1")
            .await
            .unwrap()
            .expect("the link");
        let left = live_revisions(&store).await.unwrap();
        assert_eq!(changed_topics(&rejoined, &left), vec![LiveTopic::Clusters]);
        let again = live_revisions(&store).await.unwrap();
        assert!(changed_topics(&left, &again).is_empty());
    }
}
