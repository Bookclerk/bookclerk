//! The demo seed against a fresh library at this commit's schema: it adds
//! Sam, Sam's books and wishlist, and an offline branch that shared titles,
//! and a second run changes nothing. A schema or library change that breaks
//! the seed fails here instead of in the demo.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use bookclerk_demo_seed::{seed, SeedReport, Seeded, SAMPLE_BRANCH, SAMPLE_LOGIN};
use bookclerk_library::control_plane::branch_title_counts;
use bookclerk_library::entities::nodes;
use bookclerk_library::{AcquireStatus, LibraryStore, UserRole};
use sea_orm::EntityTrait;

/// Plugin key the test pretends the Libro.fm plugin was enrolled under.
const PLUGIN: &str = "path:file:///opt/bookclerk/storefronts/libro#libro";

/// A fresh library in `dir`, schema applied.
async fn fresh(dir: &std::path::Path) -> LibraryStore {
    let db = bookclerk_plugin_database_sqlite::open(&dir.join("library.db"))
        .await
        .expect("open sqlite file");
    bookclerk_library::apply_host_schema(&db)
        .await
        .expect("schema");
    LibraryStore::from_connection(db)
}

#[tokio::test]
async fn seeds_a_member_with_books_wishlist_and_an_offline_branch() {
    let dir = tempfile::tempdir().unwrap();
    let store = fresh(dir.path()).await;

    let Seeded::Wrote(report) = seed(&store, "cluster", PLUGIN).await.unwrap() else {
        panic!("a fresh library should be seeded");
    };
    assert_eq!(
        report,
        SeedReport {
            user_id: report.user_id,
            books: 10,
            branch_titles: 4,
            wishlist: 4
        }
    );

    let sam = store
        .get_user_by_login_name(SAMPLE_LOGIN)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sam.role, UserRole::Member);
    // Sam cannot sign in: no password was set.
    assert!(!sam.has_password);

    let books = store.list_books(Some("sam@example.com")).await.unwrap();
    assert_eq!(books.len(), 10);
    assert!(books
        .iter()
        .all(|b| b.plugin_key == PLUGIN && b.acquire_status == AcquireStatus::Acquired));
    assert!(books.iter().all(|b| b.product_id.starts_with("demo-")));

    let identity = store
        .ensure_local_portal_identity(sam.id, None)
        .await
        .unwrap();
    assert_eq!(
        store.list_wishlist(Some(identity.id)).await.unwrap().len(),
        4
    );

    let counts = branch_title_counts(&store, SAMPLE_BRANCH).await.unwrap();
    assert_eq!(
        (counts.shared, counts.delivered, counts.detached),
        (4, 0, 0)
    );
    let node = nodes::Entity::find_by_id(SAMPLE_BRANCH.to_owned())
        .one(store.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(node.owner_user_id, Some(sam.id));
    assert_eq!(node.kind, "host");
    let heartbeat = chrono::DateTime::parse_from_rfc3339(&node.heartbeat_at).unwrap();
    assert!(chrono::Utc::now() - heartbeat.to_utc() > chrono::Duration::days(2));

    // No Owner: the demo makes the visitor the Owner after this runs.
    assert_eq!(store.count_owners().await.unwrap(), 0);

    assert_eq!(
        seed(&store, "cluster", PLUGIN).await.unwrap(),
        Seeded::AlreadySeeded
    );
    assert_eq!(
        store
            .list_books(Some("sam@example.com"))
            .await
            .unwrap()
            .len(),
        10
    );
}
