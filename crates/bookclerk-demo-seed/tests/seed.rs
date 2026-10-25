//! The demo seed against a fresh library at this commit's schema: it adds
//! Sam, Sam's books and wishlist, and an offline branch that shared titles,
//! and a second run changes nothing. A schema or library change that breaks
//! the seed fails here instead of in the demo.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};

use bookclerk_demo_seed::{
    best_match, distinct_records, lookups, seed, Lookup, SeedReport, Seeded, StoreRecords,
    SAMPLE_BRANCH, SAMPLE_LOGIN, STOREFRONTS,
};
use bookclerk_library::control_plane::branch_title_counts;
use bookclerk_library::entities::nodes;
use bookclerk_library::{AcquireStatus, LibraryStore, UserRole};
use bookclerk_source::CatalogHit;
use sea_orm::EntityTrait;

/// Plugin keys the test pretends the storefronts were enrolled under.
fn plugin_keys() -> BTreeMap<&'static str, String> {
    STOREFRONTS
        .iter()
        .map(|front| {
            (
                front.id,
                format!("path:file:///opt/bookclerk/storefronts/{0}#{0}", front.id),
            )
        })
        .collect()
}

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
    let keys = plugin_keys();
    let now = chrono::Utc::now();

    let Seeded::Wrote(report) = seed(&store, "cluster", &keys, &StoreRecords::new(), now)
        .await
        .unwrap()
    else {
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
        .all(|b| b.acquire_status == AcquireStatus::Acquired
            && keys.get(b.source.as_str()) == Some(&b.plugin_key)
            && b.narrators.is_some()));
    // Every storefront the demo ships has titles in the library.
    let sources: BTreeSet<&str> = books.iter().map(|b| b.source.as_str()).collect();
    assert_eq!(sources, keys.keys().copied().collect());
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
        seed(&store, "cluster", &keys, &StoreRecords::new(), now)
            .await
            .unwrap(),
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

#[tokio::test]
async fn leaves_out_the_titles_of_a_storefront_that_is_not_enrolled() {
    let dir = tempfile::tempdir().unwrap();
    let store = fresh(dir.path()).await;
    let mut keys = plugin_keys();
    // Audible has three library titles and two branch titles.
    keys.remove("audible");

    let Seeded::Wrote(report) = seed(
        &store,
        "cluster",
        &keys,
        &StoreRecords::new(),
        chrono::Utc::now(),
    )
    .await
    .unwrap() else {
        panic!("a fresh library should be seeded");
    };
    assert_eq!((report.books, report.branch_titles), (7, 2));
    let books = store.list_books(Some("sam@example.com")).await.unwrap();
    assert!(books.iter().all(|b| b.source != "audible"));
}

/// A storefront search hit with the fields the seed reads.
fn hit(product_id: &str, title: &str, authors: &str) -> CatalogHit {
    CatalogHit {
        product_id: product_id.into(),
        title: title.into(),
        authors: Some(authors.into()),
        ..CatalogHit::default()
    }
}

/// The lookup for `key`.
fn lookup(key: &str) -> Lookup {
    lookups().into_iter().find(|l| l.key == key).unwrap()
}

#[test]
fn matches_the_store_record_of_the_title_and_author() {
    let dune = lookup("demo-dune");
    let hits = [
        hit("B1", "Dune Messiah", "Frank Herbert"),
        hit("B2", "Dune", "Brian Herbert, Kevin J. Anderson"),
        hit("B3", "Dune: Book One", "Frank Herbert"),
        hit("B4", "Dune", "Frank Herbert"),
    ];
    // Exact title beats the subtitle form; the sequel and a namesake by
    // other authors never match.
    assert_eq!(best_match(&dune, &hits).unwrap().product_id, "B4");
    assert_eq!(best_match(&dune, &hits[..3]).unwrap().product_id, "B3");
    assert!(best_match(&dune, &hits[..1]).is_none());

    let mistborn = lookup("demo-mistborn-1-part-2");
    let parts = [
        hit(
            "GA1",
            "Mistborn 1: The Final Empire (1 of 3)",
            "Brandon Sanderson",
        ),
        hit(
            "GA2",
            "Mistborn 1: The Final Empire (2 of 3)",
            "Brandon Sanderson",
        ),
    ];
    assert_eq!(best_match(&mistborn, &parts).unwrap().product_id, "GA2");

    // The store's first-ranked hit wins a tie.
    let twice = [
        hit("A", "Circe", "Madeline Miller"),
        hit("B", "Circe", "Madeline Miller"),
    ];
    assert_eq!(
        best_match(&lookup("demo-circe"), &twice)
            .unwrap()
            .product_id,
        "A"
    );
}

#[test]
fn two_titles_never_share_one_store_record() {
    let mut records = StoreRecords::new();
    records.insert(
        "demo-mistborn-1-part-1",
        hit("GA1", "Mistborn", "Brandon Sanderson"),
    );
    records.insert(
        "demo-mistborn-1-part-2",
        hit("GA1", "Mistborn", "Brandon Sanderson"),
    );
    let records = distinct_records(records);
    assert!(records.contains_key("demo-mistborn-1-part-1"));
    assert!(!records.contains_key("demo-mistborn-1-part-2"));
}

#[tokio::test]
async fn writes_the_store_record_when_there_is_one() {
    let dir = tempfile::tempdir().unwrap();
    let store = fresh(dir.path()).await;
    let keys = plugin_keys();
    let mut records = StoreRecords::new();
    records.insert(
        "demo-project-hail-mary",
        CatalogHit {
            asin: Some("B08G9PRS1K".into()),
            narrators: Some("Ray Porter".into()),
            cover_url: Some("https://m.media-amazon.com/images/I/phm.jpg".into()),
            description: Some("A lone astronaut.".into()),
            published_at: Some("2021-05-04".into()),
            ..hit("B08G9PRS1K", "Project Hail Mary", "Andy Weir")
        },
    );
    records.insert(
        "demo-wish-james",
        CatalogHit {
            asin: Some("B0CJ6Q8CZ4".into()),
            cover_url: Some("https://m.media-amazon.com/images/I/james.jpg".into()),
            ..hit("B0CJ6Q8CZ4", "James", "Percival Everett")
        },
    );
    records.insert(
        "demo-circe",
        CatalogHit {
            isbn: Some("9781549140396".into()),
            ..hit("9781549140396", "Circe", "Madeline Miller")
        },
    );
    let Seeded::Wrote(report) = seed(&store, "cluster", &keys, &records, chrono::Utc::now())
        .await
        .unwrap()
    else {
        panic!("a fresh library should be seeded");
    };
    assert_eq!((report.books, report.branch_titles), (10, 4));

    let books = store.list_books(Some("sam@example.com")).await.unwrap();
    let phm = books
        .iter()
        .find(|b| b.title == "Project Hail Mary")
        .unwrap();
    assert_eq!(phm.product_id, "B08G9PRS1K");
    assert_eq!(phm.asin.as_deref(), Some("B08G9PRS1K"));
    assert_eq!(
        phm.cover_url.as_deref(),
        Some("https://m.media-amazon.com/images/I/phm.jpg")
    );
    assert_eq!(phm.description.as_deref(), Some("A lone astronaut."));
    assert!(phm.published_at.is_some());
    // Titles without a record keep their sample fields.
    assert!(books
        .iter()
        .filter(|b| b.title != "Project Hail Mary")
        .all(|b| b.product_id.starts_with("demo-") && b.cover_url.is_none()));

    let sam = store
        .get_user_by_login_name(SAMPLE_LOGIN)
        .await
        .unwrap()
        .unwrap();
    let identity = store
        .ensure_local_portal_identity(sam.id, None)
        .await
        .unwrap();
    let wishes = store.list_wishlist(Some(identity.id)).await.unwrap();
    let james = wishes.iter().find(|w| w.title == "James").unwrap();
    assert_eq!(
        james.cover_url.as_deref(),
        Some("https://m.media-amazon.com/images/I/james.jpg")
    );
}
