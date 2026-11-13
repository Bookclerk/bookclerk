//! "Acquire all of this branch's members' books" on the library: the
//! choice reaches the branch with its next catalog session, a change
//! loses the pass under way and restarts it from the start, setting the
//! same value again changes nothing, and only a branch takes it.

use bookclerk_node_proto::{CatalogFile, CatalogMetadata, CatalogTitle};
use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};

use super::*;
use crate::control_plane::*;
use crate::entities::{node_credentials, nodes};
use crate::models::UserRole;
use crate::store::LibraryStore;
use crate::test_support::Profile;

const BRANCH: &str = "branch-b";

async fn store(profile: Profile) -> LibraryStore {
    crate::test_support::sqlite::memory(profile)
        .await
        .into_store()
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

async fn open(store: &LibraryStore) -> CatalogSessionOpened {
    match begin_catalog(store, BRANCH, true, false).await.unwrap() {
        CatalogBegin::Opened(opened) => opened,
        other => panic!("expected a session: {other:?}"),
    }
}

async fn node(store: &LibraryStore, node_id: &str) -> nodes::Model {
    nodes::Entity::find_by_id(node_id.to_string())
        .one(store.db())
        .await
        .unwrap()
        .unwrap()
}

fn title(product: &str) -> CatalogTitle {
    let key = format!("Author/{product}/{product}.m4b");
    CatalogTitle {
        store: "audible".into(),
        product_id: product.into(),
        marketplace: "us".into(),
        metadata: CatalogMetadata {
            title: format!("Title {product}"),
            authors: Some("Author".into()),
            ..CatalogMetadata::default()
        },
        primary_key: key.clone(),
        pdf_key: None,
        files: vec![CatalogFile {
            key,
            size: 10,
            content_type: Some("audio/mp4".into()),
        }],
    }
}

#[tokio::test]
async fn the_choice_reaches_the_branch_and_restarts_its_pass() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = store(profile).await;
        let member = store
            .create_user(UserRole::Member, Some("Riley"), None)
            .await
            .unwrap();
        add_node(&store, BRANCH, "host", Some(member.id)).await;

        // Off by default.
        let first = open(&store).await;
        assert!(!first.members_books, "{name}");
        match accept_catalog_batch(&store, BRANCH, first.fence, "c1", false, &[title("B1")])
            .await
            .unwrap()
        {
            CatalogAccept::Accepted { .. } => {}
            other => panic!("expected the batch accepted: {other:?}"),
        }
        assert_eq!(node(&store, BRANCH).await.catalog_cursor, "c1");

        // Turning it on loses the pass under way and clears the cursor.
        assert_eq!(
            set_branch_members_books(&store, BRANCH, true)
                .await
                .unwrap(),
            Some(true)
        );
        assert!(!catalog_fence_live(&store, BRANCH, first.fence)
            .await
            .unwrap());
        let row = node(&store, BRANCH).await;
        assert_eq!(row.acquire_members_books, 1, "{name}");
        assert_eq!(row.catalog_cursor, "");

        // The next session carries it and starts from the start.
        let second = open(&store).await;
        assert!(second.members_books, "{name}");
        assert_eq!(second.cursor, "");
        assert!(second.fence > first.fence);

        // The same value again changes nothing: the session stays live.
        assert_eq!(
            set_branch_members_books(&store, BRANCH, true)
                .await
                .unwrap(),
            Some(true)
        );
        assert!(catalog_fence_live(&store, BRANCH, second.fence)
            .await
            .unwrap());

        // Off again restarts the pass, so the members' books are withdrawn
        // when it ends.
        assert_eq!(
            set_branch_members_books(&store, BRANCH, false)
                .await
                .unwrap(),
            Some(false)
        );
        assert!(!catalog_fence_live(&store, BRANCH, second.fence)
            .await
            .unwrap());
        let third = open(&store).await;
        assert!(!third.members_books, "{name}");
        assert_eq!(third.cursor, "");
    }
}

#[tokio::test]
async fn only_a_branch_takes_the_choice() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = store(profile).await;
        add_node(&store, "owner-node", "owner", None).await;

        assert_eq!(
            set_branch_members_books(&store, "owner-node", true)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            node(&store, "owner-node").await.acquire_members_books,
            0,
            "{name}"
        );
        assert_eq!(
            set_branch_members_books(&store, "nowhere", true)
                .await
                .unwrap(),
            None
        );
    }
}
