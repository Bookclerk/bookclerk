//! Branches of branches on the library side: titles a branch shares land
//! as its owner's, deduplicated by provenance, store and product id; a
//! stale fence, a branch that stopped sharing, left or is no longer
//! trusted records nothing; titles the library already holds for that
//! user are not echoed back; a full pass withdraws what it no longer
//! names; and catalog uploads commit only under the live fence.

use bookclerk_node_proto::{
    CatalogFile, CatalogMetadata, CatalogRef, CatalogTitle, FileCopy, UploadFile, CATALOG_ITEM,
};
use sea_orm::{ActiveModelTrait, ActiveValue::Set};

use super::*;
use crate::control_plane::*;
use crate::entities::{node_credentials, nodes};
use crate::master_key::master_key_test_lock_async;
use crate::models::UserRole;
use crate::store::{LibraryStore, NewBook};

const PLUGIN: &str = "path:file:///tmp/store#store";
const BRANCH: &str = "branch-b";

async fn store_in(dir: &std::path::Path) -> LibraryStore {
    let db = bookclerk_plugin_database_sqlite::open(&dir.join("library.db"))
        .await
        .expect("open sqlite file");
    crate::apply_host_schema(&db).await.expect("schema");
    crate::configure_master_key(dir).expect("master key");
    LibraryStore::from_connection(db)
}

async fn add_node(store: &LibraryStore, node_id: &str, kind: &str, owner: Option<i64>) {
    let now = chrono::Utc::now().to_rfc3339();
    nodes::ActiveModel {
        node_id: Set(node_id.into()),
        cluster_id: Set("cluster".into()),
        created_at: Set(now.clone()),
        incarnation: Set("test".into()),
        heartbeat_at: Set(now.clone()),
        software_version: Set("1.0.0".into()),
        schema_state: Set("current".into()),
        compatible: Set(1),
        target: Set(String::new()),
        labels_json: Set("[]".into()),
        kind: Set(kind.into()),
        owner_user_id: Set(owner),
        ..Default::default()
    }
    .insert(store.db())
    .await
    .unwrap();
    if kind == "host" {
        node_credentials::ActiveModel {
            node_id: Set(node_id.into()),
            sign_public_key: Set(vec![1; 32]),
            box_public_key: Set(vec![2; 32]),
            link_id: Set(format!("link-{node_id}")),
            enrolled_at: Set(now),
            revoked_at: Set(None),
            last_connected_at: Set(None),
        }
        .insert(store.db())
        .await
        .unwrap();
    }
}

/// An owner library with a member who owns the branch node.
async fn world(dir: &std::path::Path) -> (LibraryStore, i64) {
    let store = store_in(dir).await;
    let member = store
        .create_user(UserRole::Member, Some("Riley"), None)
        .await
        .unwrap();
    add_node(&store, BRANCH, "host", Some(member.id)).await;
    (store, member.id)
}

fn title(product: &str, size: u64) -> CatalogTitle {
    let key = format!("Author/{product}/{product}.m4b");
    CatalogTitle {
        store: "audible".into(),
        product_id: product.into(),
        metadata: CatalogMetadata {
            title: format!("Title {product}"),
            authors: Some("Author".into()),
            ..CatalogMetadata::default()
        },
        primary_key: key.clone(),
        pdf_key: None,
        files: vec![CatalogFile {
            key,
            size,
            content_type: Some("audio/mp4".into()),
        }],
    }
}

fn reference(product: &str) -> CatalogRef {
    CatalogRef {
        store: "audible".into(),
        product_id: product.into(),
    }
}

async fn open(store: &LibraryStore, share: bool, include: bool) -> CatalogSessionOpened {
    match begin_catalog(store, BRANCH, share, include).await.unwrap() {
        CatalogBegin::Opened(opened) => opened,
        other => panic!("expected a session: {other:?}"),
    }
}

async fn accepted(
    store: &LibraryStore,
    fence: i64,
    cursor: &str,
    done: bool,
    titles: &[CatalogTitle],
) -> (u64, u64, Vec<CatalogRef>) {
    match accept_catalog_batch(store, BRANCH, fence, cursor, done, titles)
        .await
        .unwrap()
    {
        CatalogAccept::Accepted {
            upserted,
            skipped,
            wanted,
        } => (upserted, skipped, wanted),
        other => panic!("expected the batch accepted: {other:?}"),
    }
}

#[tokio::test]
async fn shared_titles_land_as_the_branch_owners_and_deduplicate() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let (store, member) = world(dir.path()).await;

    let session = open(&store, true, true).await;
    assert_eq!(session.fence, 1);
    assert_eq!(session.cursor, "");
    let (upserted, skipped, wanted) = accepted(
        &store,
        session.fence,
        "c1",
        false,
        &[title("p1", 10), title("p2", 20)],
    )
    .await;
    assert_eq!((upserted, skipped), (2, 0));
    assert_eq!(wanted, vec![reference("p1"), reference("p2")]);

    // The same titles again: still one row each.
    accepted(&store, session.fence, "c2", false, &[title("p1", 10)]).await;
    let rows = list_branch_titles(&store, Some(BRANCH), 100).await.unwrap();
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(row.owner_user_id, Some(member), "attributed to the owner");
        assert_eq!(row.provenance, format!("node:{BRANCH}"));
        assert_eq!(row.state, BRANCH_TITLE_SHARED);
        assert!(!row.delivered);
    }

    // Reopening resumes after the cursor and asks again for the files.
    let again = open(&store, true, true).await;
    assert_eq!(again.fence, 2);
    assert_eq!(again.cursor, "c2");
    assert_eq!(again.wanted, vec![reference("p1"), reference("p2")]);
    // A batch under the old fence is lost and writes nothing.
    assert_eq!(
        accept_catalog_batch(&store, BRANCH, 1, "c3", false, &[title("p3", 1)])
            .await
            .unwrap(),
        CatalogAccept::Lost
    );
    assert_eq!(
        list_branch_titles(&store, Some(BRANCH), 100)
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn only_a_trusted_branch_may_share() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let (store, member) = world(dir.path()).await;
    add_node(&store, "clerk", "owner", Some(member)).await;
    for node in ["clerk", "nobody"] {
        assert_eq!(
            begin_catalog(&store, node, true, true).await.unwrap(),
            CatalogBegin::Refused("not_a_branch"),
            "{node}"
        );
    }
    let session = open(&store, true, true).await;
    // A malformed title refuses the whole batch.
    let mut bad = title("p2", 1);
    bad.primary_key = "elsewhere.m4b".into();
    assert_eq!(
        accept_catalog_batch(
            &store,
            BRANCH,
            session.fence,
            "c",
            false,
            &[title("p1", 1), bad]
        )
        .await
        .unwrap(),
        CatalogAccept::Refused("invalid_manifest")
    );
    assert_eq!(
        accept_catalog_batch(
            &store,
            BRANCH,
            session.fence,
            "c",
            false,
            &[title("p1", 1), title("p1", 1)]
        )
        .await
        .unwrap(),
        CatalogAccept::Refused("duplicate_title")
    );
    assert!(list_branch_titles(&store, None, 10)
        .await
        .unwrap()
        .is_empty());
    // A disabled owner makes the fence useless.
    store
        .set_user_status(member, crate::models::UserStatus::Disabled)
        .await
        .unwrap();
    assert_eq!(
        accept_catalog_batch(&store, BRANCH, session.fence, "c", false, &[title("p1", 1)])
            .await
            .unwrap(),
        CatalogAccept::Lost
    );
}

#[tokio::test]
async fn a_title_the_library_holds_for_that_user_is_not_echoed_back() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let (store, member) = world(dir.path()).await;
    store
        .upsert_account(PLUGIN, "riley-acct", "us", None, true, "audible")
        .await
        .unwrap();
    let identity = store
        .ensure_local_portal_identity(member, None)
        .await
        .unwrap();
    store
        .link_account(identity.id, PLUGIN, "riley-acct", "test")
        .await
        .unwrap();
    store
        .scope(PLUGIN, "audible")
        .upsert_book(&NewBook {
            uuid: None,
            product_id: "mine".into(),
            plugin_key: PLUGIN.into(),
            source: "audible".into(),
            account_id: "riley-acct".into(),
            asin: None,
            isbn: None,
            marketplace: "us".into(),
            title: "Mine".into(),
            authors: None,
            narrators: None,
            series: None,
            series_index: None,
            series_asin: None,
            purchased_at: None,
            publisher: None,
            length_minutes: None,
            is_abridged: false,
            content_kind: "book".into(),
            categories: None,
            subtitle: None,
            published_at: None,
        })
        .await
        .unwrap();
    let session = open(&store, true, true).await;
    let (upserted, skipped, wanted) = accepted(
        &store,
        session.fence,
        "c",
        false,
        &[title("mine", 1), title("theirs", 1)],
    )
    .await;
    assert_eq!((upserted, skipped), (1, 1));
    assert_eq!(wanted, vec![reference("theirs")]);
    let rows = list_branch_titles(&store, None, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].product_id, "theirs");
}

#[tokio::test]
async fn stopping_sharing_or_leaving_marks_the_titles_and_the_owner_removes_them() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let (store, _member) = world(dir.path()).await;
    let session = open(&store, true, true).await;
    accepted(&store, session.fence, "c", false, &[title("p1", 1)]).await;

    let stopped = open(&store, false, true).await;
    assert!(stopped.wanted.is_empty());
    let rows = list_branch_titles(&store, None, 10).await.unwrap();
    assert_eq!(rows[0].state, BRANCH_TITLE_DETACHED);
    assert_eq!(
        rows[0].detached_reason.as_deref(),
        Some(DETACHED_STOPPED_SHARING)
    );
    // While sharing is off, nothing new is recorded.
    assert_eq!(
        accept_catalog_batch(&store, BRANCH, stopped.fence, "c", false, &[title("p2", 1)])
            .await
            .unwrap(),
        CatalogAccept::Lost
    );

    // Sharing again restores the title the branch names again.
    let resumed = open(&store, true, true).await;
    assert_eq!(resumed.cursor, "", "a settings change restarts the pass");
    accepted(&store, resumed.fence, "c", false, &[title("p1", 1)]).await;
    assert_eq!(
        branch_title_counts(&store, BRANCH).await.unwrap(),
        BranchTitleCounts {
            shared: 1,
            delivered: 0,
            detached: 0,
        }
    );

    // Leaving marks everything "from a branch that left".
    assert!(super::super::record_node_left(&store, BRANCH)
        .await
        .unwrap());
    let rows = list_branch_titles(&store, None, 10).await.unwrap();
    assert_eq!(rows[0].detached_reason.as_deref(), Some(DETACHED_LEFT));
    assert_eq!(
        begin_catalog(&store, BRANCH, true, true).await.unwrap(),
        CatalogBegin::Refused("not_a_branch")
    );
    assert_eq!(
        remove_detached_branch_titles(&store, Some(BRANCH))
            .await
            .unwrap(),
        1
    );
    assert!(list_branch_titles(&store, None, 10)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn a_full_pass_withdraws_what_it_no_longer_names() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let (store, _member) = world(dir.path()).await;
    let first = open(&store, true, true).await;
    accepted(
        &store,
        first.fence,
        "c",
        true,
        &[title("own", 1), title("from-below", 1)],
    )
    .await;
    // "Include books from my own branches" turned off: a new pass from
    // the start names only the branch's own title.
    let second = open(&store, true, false).await;
    assert_eq!(second.cursor, "");
    accepted(&store, second.fence, "c", true, &[title("own", 1)]).await;
    let rows = list_branch_titles(&store, None, 10).await.unwrap();
    let state = |product: &str| {
        rows.iter()
            .find(|row| row.product_id == product)
            .map(|row| (row.state.clone(), row.detached_reason.clone()))
            .unwrap()
    };
    assert_eq!(state("own"), (BRANCH_TITLE_SHARED.into(), None));
    assert_eq!(
        state("from-below"),
        (
            BRANCH_TITLE_DETACHED.into(),
            Some(DETACHED_WITHDRAWN.into())
        )
    );
}

/// A batch of `p1` and `p2`, read and checked under `fence` but not yet
/// written.
async fn prepared_batch(store: &LibraryStore, fence: i64) -> PreparedCatalogBatch {
    match prepare_catalog_batch(
        store,
        BRANCH,
        fence,
        "b",
        true,
        &[title("p1", 1), title("p2", 1)],
    )
    .await
    .unwrap()
    {
        Ok(prepared) => prepared,
        Err(other) => panic!("expected a prepared batch: {other:?}"),
    }
}

/// Only `p1`, detached with `reason`.
fn only_p1_detached(rows: &[BranchTitle], reason: &str) {
    assert_eq!(rows.len(), 1, "no new title: {rows:?}");
    assert_eq!(rows[0].product_id, "p1");
    assert_eq!(rows[0].state, BRANCH_TITLE_DETACHED);
    assert_eq!(rows[0].detached_reason.as_deref(), Some(reason));
}

/// A batch read under a live fence whose branch stops sharing or leaves
/// before it is written records nothing: titles marked detached stay
/// detached, no new title appears, and the cursor does not move.
#[tokio::test]
async fn a_batch_overtaken_by_a_detach_records_nothing() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let (store, _member) = world(dir.path()).await;
    let session = open(&store, true, true).await;
    accepted(&store, session.fence, "a", false, &[title("p1", 1)]).await;

    // Sharing turned off between the read and the write.
    let prepared = prepared_batch(&store, session.fence).await;
    open(&store, false, true).await;
    assert_eq!(
        apply_catalog_batch(&store, BRANCH, prepared).await.unwrap(),
        CatalogAccept::Lost
    );
    only_p1_detached(
        &list_branch_titles(&store, None, 10).await.unwrap(),
        DETACHED_STOPPED_SHARING,
    );

    // The branch left between the read and the write.
    let resumed = open(&store, true, true).await;
    accepted(&store, resumed.fence, "a", false, &[title("p1", 1)]).await;
    let prepared = prepared_batch(&store, resumed.fence).await;
    assert!(super::super::record_node_left(&store, BRANCH)
        .await
        .unwrap());
    assert_eq!(
        apply_catalog_batch(&store, BRANCH, prepared).await.unwrap(),
        CatalogAccept::Lost
    );
    only_p1_detached(
        &list_branch_titles(&store, None, 10).await.unwrap(),
        DETACHED_LEFT,
    );
    let node = nodes::Entity::find_by_id(BRANCH.to_string())
        .one(store.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(node.catalog_cursor, "a", "the cursor did not move");
    assert!(
        node.catalog_pass_started_at.is_some(),
        "the pass did not end"
    );
}

/// Sessions resume after a completed pass, but a day after it the library
/// asks for a full pass again, so titles a branch no longer has are
/// withdrawn; a pass still under way is never restarted.
#[tokio::test]
async fn a_full_pass_is_asked_for_again_a_day_later() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let (store, _member) = world(dir.path()).await;
    let first = open(&store, true, true).await;
    accepted(&store, first.fence, "a", false, &[title("gone", 1)]).await;
    // Under way: a reconnect resumes it.
    assert_eq!(open(&store, true, true).await.cursor, "a");
    let resumed = open(&store, true, true).await;
    accepted(&store, resumed.fence, "b", true, &[]).await;
    assert_eq!(open(&store, true, true).await.cursor, "b");

    let long_ago = (chrono::Utc::now() - FULL_CATALOG_PASS_EVERY - chrono::Duration::minutes(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    nodes::Entity::update_many()
        .col_expr(
            nodes::Column::CatalogFullPassAt,
            sea_orm::sea_query::Expr::value(long_ago),
        )
        .filter(nodes::Column::NodeId.eq(BRANCH))
        .exec(store.db())
        .await
        .unwrap();
    let full = open(&store, true, true).await;
    assert_eq!(full.cursor, "", "a day later the pass starts over");
    accepted(&store, full.fence, "c", true, &[]).await;
    let rows = list_branch_titles(&store, None, 10).await.unwrap();
    assert_eq!(rows[0].state, BRANCH_TITLE_DETACHED);
    assert_eq!(rows[0].detached_reason.as_deref(), Some(DETACHED_WITHDRAWN));
    assert_eq!(open(&store, true, true).await.cursor, "c");
}

#[tokio::test]
async fn a_catalog_upload_commits_only_under_the_live_fence() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let (store, _member) = world(dir.path()).await;
    let session = open(&store, true, true).await;
    accepted(&store, session.fence, "c", false, &[title("p1", 3)]).await;
    let destinations = vec!["local".to_string(), "s3".to_string()];
    let file = |key: &str, size: u64| UploadFile {
        copies: destinations
            .iter()
            .map(|destination| FileCopy {
                destination: destination.clone(),
                key: key.into(),
            })
            .collect(),
        size,
        sha256: "ab".repeat(32),
        content_type: None,
    };
    let key = "Author/p1/p1.m4b";
    let grant = |files: Vec<UploadFile>, fence: i64| {
        let store = store.clone();
        let destinations = destinations.clone();
        async move {
            grant_catalog_upload(
                &store,
                BRANCH,
                fence,
                "owner-node",
                "audible",
                "p1",
                &files,
                &destinations,
            )
            .await
            .unwrap()
        }
    };
    assert!(matches!(
        grant(vec![file(key, 4)], session.fence).await,
        UploadGrant::Refused("manifest_changed")
    ));
    assert!(matches!(
        grant(vec![file("other.m4b", 3)], session.fence).await,
        UploadGrant::Refused("manifest_changed")
    ));
    assert!(matches!(
        grant(vec![file(key, 3)], session.fence + 1).await,
        UploadGrant::Lost
    ));
    assert!(matches!(
        grant_catalog_upload(
            &store,
            BRANCH,
            session.fence,
            "owner-node",
            "audible",
            "nope",
            &[file(key, 3)],
            &destinations
        )
        .await
        .unwrap(),
        UploadGrant::Refused("unknown_title")
    ));
    let UploadGrant::Granted {
        upload_id, token, ..
    } = grant(vec![file(key, 3)], session.fence).await
    else {
        panic!("expected a grant");
    };
    let upload = authorize_upload(&store, &upload_id, &token, false)
        .await
        .unwrap()
        .expect("the token opens the upload");
    assert!(upload.is_catalog());
    assert_eq!(upload.job_id, CATALOG_ITEM);
    assert!(upload_fence_live(&store, &upload).await.unwrap());
    assert!(record_upload_file(&store, &upload_id, 0).await.unwrap());
    let limits = JobClaimLimits {
        class: 1,
        plugin_instance: 1,
    };
    assert_eq!(
        commit_upload(&store, &upload_id, None, limits)
            .await
            .unwrap(),
        UploadCommit::Committed {
            job_completed: false
        }
    );
    let rows = list_branch_titles(&store, None, 10).await.unwrap();
    assert!(rows[0].delivered, "the files arrived");
    assert!(pending_deliveries(&store, BRANCH, 10)
        .await
        .unwrap()
        .is_empty());

    // A second upload whose fence moves on before the commit is lost.
    accepted(&store, session.fence, "c", false, &[title("p1", 5)]).await;
    let UploadGrant::Granted { upload_id, .. } = grant(vec![file(key, 5)], session.fence).await
    else {
        panic!("expected a grant");
    };
    assert!(record_upload_file(&store, &upload_id, 0).await.unwrap());
    let reopened = open(&store, true, true).await;
    assert_eq!(reopened.wanted, vec![reference("p1")]);
    assert_eq!(
        commit_upload(&store, &upload_id, None, limits)
            .await
            .unwrap(),
        UploadCommit::Lost
    );
    assert!(!list_branch_titles(&store, None, 10).await.unwrap()[0].delivered);
}

#[tokio::test]
async fn ancestry_lists_this_library_then_every_parent_and_refuses_cycles() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path()).await;
    assert!(library_ancestry(&store).await.unwrap().is_empty());
    assert_eq!(join_cycle(&store, &["x".into()]).await.unwrap(), None);
    super::super::align_cluster_root(&store, dir.path(), None)
        .await
        .unwrap();
    let own = super::super::load_cluster_row(&store)
        .await
        .unwrap()
        .unwrap()
        .cluster_id;
    assert_eq!(library_ancestry(&store).await.unwrap(), vec![own.clone()]);
    record_cluster_link(
        &store,
        &NewClusterLink {
            owner_cluster_id: "parent".into(),
            owner_origin: "https://parent.example".into(),
            remote_node_id: "node-1".into(),
            spki_pin: None,
        },
        &ClusterLinkKeys::generate(),
    )
    .await
    .unwrap();
    set_cluster_link_ancestry(&store, "parent", &["parent".into(), "root".into()])
        .await
        .unwrap();
    assert!(
        set_cluster_link_ancestry(&store, "parent", &["not ok".into()])
            .await
            .is_err()
    );
    assert_eq!(
        library_ancestry(&store).await.unwrap(),
        vec![own.clone(), "parent".to_string(), "root".to_string()]
    );
    assert_eq!(
        join_cycle(&store, &["c".into(), own.clone(), "root".into()])
            .await
            .unwrap(),
        Some(own.clone())
    );
    assert_eq!(join_cycle(&store, &["root".into()]).await.unwrap(), None);
    // The settings default on and are kept.
    let link = load_cluster_link(&store, "parent").await.unwrap().unwrap();
    assert!(link.share_books && link.include_branches);
    assert!(set_cluster_link_sharing(&store, "parent", false, true)
        .await
        .unwrap());
    let link = load_cluster_link(&store, "parent").await.unwrap().unwrap();
    assert!(!link.share_books && link.include_branches);
    // A left link no longer counts.
    leave_cluster_link(&store, "parent").await.unwrap();
    assert_eq!(library_ancestry(&store).await.unwrap(), vec![own]);
}

/// A branch's files never land on a key another title here uses: one of
/// this library's own books (its audio, or a file beside it with the same
/// stem) or another branch's title; `%` and `_` in a key match only
/// themselves.
#[tokio::test]
async fn a_catalog_upload_never_overwrites_another_titles_files() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let (store, member) = world(dir.path()).await;
    add_node(&store, "branch-c", "host", Some(member)).await;
    store
        .upsert_account(PLUGIN, "someone-else", "us", None, true, "test")
        .await
        .unwrap();
    let own = store
        .scope(PLUGIN, "store")
        .upsert_book(&NewBook {
            uuid: None,
            product_id: "own".into(),
            plugin_key: PLUGIN.into(),
            source: "store".into(),
            account_id: "someone-else".into(),
            asin: None,
            isbn: None,
            marketplace: "us".into(),
            title: "Own".into(),
            authors: None,
            narrators: None,
            series: None,
            series_index: None,
            series_asin: None,
            purchased_at: None,
            publisher: None,
            length_minutes: None,
            is_abridged: false,
            content_kind: "book".into(),
            categories: None,
            subtitle: None,
            published_at: None,
        })
        .await
        .unwrap();
    store
        .set_acquire_status(
            &own.uuid,
            "someone-else",
            crate::models::AcquireStatus::Acquired,
            Some("Author/own/own.mp3"),
            None,
        )
        .await
        .unwrap();
    let keyed = |product: &str, key: &str| CatalogTitle {
        primary_key: key.into(),
        files: vec![CatalogFile {
            key: key.into(),
            size: 1,
            content_type: None,
        }],
        ..title(product, 1)
    };
    let titles = [
        keyed("same-stem", "Author/own/own.m4b"),
        keyed("wild", "Author/o_n/o_n.m4b"),
        keyed("free", "Author/free/free.m4b"),
    ];
    let session = open(&store, true, true).await;
    accepted(&store, session.fence, "c", false, &titles).await;
    let destinations = vec!["local".to_string()];
    let grant = |branch: &'static str, fence: i64, product: &'static str, key: &'static str| {
        let store = store.clone();
        let destinations = destinations.clone();
        async move {
            let files = vec![UploadFile {
                copies: vec![FileCopy {
                    destination: "local".into(),
                    key: key.into(),
                }],
                size: 1,
                sha256: "ab".repeat(32),
                content_type: None,
            }];
            grant_catalog_upload(
                &store,
                branch,
                fence,
                "owner-node",
                "audible",
                product,
                &files,
                &destinations,
            )
            .await
            .unwrap()
        }
    };
    assert!(matches!(
        grant(BRANCH, session.fence, "same-stem", "Author/own/own.m4b").await,
        UploadGrant::Refused("key_in_use")
    ));
    assert!(matches!(
        grant(BRANCH, session.fence, "wild", "Author/o_n/o_n.m4b").await,
        UploadGrant::Granted { .. }
    ));
    assert!(matches!(
        grant(BRANCH, session.fence, "free", "Author/free/free.m4b").await,
        UploadGrant::Granted { .. }
    ));

    // Another branch offering a key a first branch's title holds.
    let other = match begin_catalog(&store, "branch-c", true, true).await.unwrap() {
        CatalogBegin::Opened(opened) => opened,
        other => panic!("expected a session: {other:?}"),
    };
    match accept_catalog_batch(
        &store,
        "branch-c",
        other.fence,
        "c",
        false,
        &[keyed("free", "Author/free/free.m4b")],
    )
    .await
    .unwrap()
    {
        CatalogAccept::Accepted { upserted: 1, .. } => {}
        other => panic!("expected the batch accepted: {other:?}"),
    }
    assert!(matches!(
        grant("branch-c", other.fence, "free", "Author/free/free.m4b").await,
        UploadGrant::Refused("key_in_use")
    ));
}
