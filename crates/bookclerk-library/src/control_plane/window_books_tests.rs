//! Window mode: the library answers a trusted branch with the books on its
//! owner's accounts only, page by page within the frame budget; the window
//! stores each library's pages, drops what a complete read no longer names,
//! and merges every window link's books by local work and identifier.

use bookclerk_node_proto::{
    window_page_problem, Availability, CatalogMetadata, LookupEdition, LookupFormat,
    LookupIdentifier, LookupScheme, WindowWork, MAX_FRAME_BYTES, MAX_LOOKUP_EDITIONS,
    MAX_WINDOW_WORKS,
};
use sea_orm::{ActiveModelTrait, ActiveValue::Set};

use super::*;
use crate::control_plane::{
    record_cluster_link, set_cluster_link_download, ClusterLinkKeys, NewClusterLink,
};
use crate::entities::node_credentials;
use crate::master_key::master_key_test_lock_async;
use crate::models::UserRole;
use crate::store::NewBook;

const PLUGIN: &str = "path:file:///tmp/store#audible";
const BRANCH: &str = "window-w";

async fn store_in(dir: &std::path::Path) -> LibraryStore {
    let db = bookclerk_plugin_database_sqlite::open(&dir.join("library.db"))
        .await
        .expect("open sqlite file");
    crate::apply_host_schema(&db).await.expect("schema");
    crate::configure_master_key(dir).expect("master key");
    LibraryStore::from_connection(db)
}

async fn add_branch(store: &LibraryStore, node_id: &str, owner: i64) {
    let now = Utc::now().to_rfc3339();
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
        kind: Set("host".into()),
        owner_user_id: Set(Some(owner)),
        ..Default::default()
    }
    .insert(store.db())
    .await
    .unwrap();
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

fn book(account: &str, asin: &str) -> NewBook {
    NewBook {
        uuid: None,
        product_id: asin.into(),
        plugin_key: PLUGIN.into(),
        source: "audible".into(),
        account_id: account.into(),
        asin: Some(asin.into()),
        isbn: None,
        marketplace: "us".into(),
        title: format!("Title {asin}"),
        authors: Some("Ann Author".into()),
        narrators: None,
        series: None,
        series_index: None,
        series_asin: None,
        purchased_at: None,
        publisher: None,
        length_minutes: Some(600),
        is_abridged: false,
        content_kind: "book".into(),
        categories: None,
        subtitle: None,
        published_at: None,
    }
}

/// Connects `account` for `user` and records `asins` on it; answers the
/// work id of each.
async fn hold(store: &LibraryStore, user: i64, account: &str, asins: &[String]) -> Vec<String> {
    store
        .upsert_account(PLUGIN, account, "us", None, true, "audible")
        .await
        .unwrap();
    let identity = store
        .ensure_local_portal_identity(user, None)
        .await
        .unwrap();
    store
        .link_account(identity.id, PLUGIN, account, "test")
        .await
        .unwrap();
    let mut works = Vec::new();
    for asin in asins {
        let record = store.upsert_book(&book(account, asin)).await.unwrap();
        works.push(store.work_id_for_book(&record.uuid).await.unwrap().unwrap());
    }
    works
}

/// Every page of `node`'s window books, checked against the frame rules
/// and budget as it goes.
async fn read_all(store: &LibraryStore, node: &str) -> Vec<WindowWork> {
    let mut after = 0;
    let mut works = Vec::new();
    for _ in 0..1000 {
        let (page, next) = window_books_page(store, node, after)
            .await
            .unwrap()
            .expect("a trusted branch is answered");
        assert_eq!(window_page_problem(after, &page, next), None);
        assert!(serde_json::to_vec(&page).unwrap().len() <= MAX_FRAME_BYTES / 2);
        works.extend(page);
        match next {
            Some(next) => after = next,
            None => return works,
        }
    }
    panic!("paging never ended");
}

#[tokio::test]
async fn the_library_lists_only_its_persons_books_to_a_trusted_branch() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path()).await;
    let riley = store
        .create_user(UserRole::Member, Some("Riley"), None)
        .await
        .unwrap()
        .id;
    let sam = store
        .create_user(UserRole::Member, Some("Sam"), None)
        .await
        .unwrap()
        .id;
    let works = hold(&store, riley, "riley-acct", &["B0RILEY001".into()]).await;
    hold(&store, sam, "sam-acct", &["B0SAM00001".into()]).await;

    assert_eq!(window_books_page(&store, BRANCH, 0).await.unwrap(), None);
    add_branch(&store, BRANCH, riley).await;

    let listed = read_all(&store, BRANCH).await;
    assert_eq!(listed.len(), 1, "only Riley's book: {listed:?}");
    assert_eq!(listed[0].work, works[0]);
    assert_eq!(listed[0].editions[0].product_id, "B0RILEY001");
    assert_eq!(listed[0].editions[0].availability, Availability::Owned);
    assert!(listed[0].identifiers.iter().any(|id| {
        id.scheme == LookupScheme::Asin && id.value == "B0RILEY001" && id.marketplace == "us"
    }));
}

/// More books than one read takes are paged out with a cursor that moves
/// forward, each page within the frame budget, under every profile.
#[tokio::test]
async fn many_books_page_out_within_every_profiles_limits() {
    const BOOKS: usize = 210;
    for profile in crate::test_support::Profile::ALL {
        let name = profile.name();
        let store = crate::test_support::sqlite::memory(profile)
            .await
            .into_store();
        let owner = store
            .create_user(UserRole::Owner, Some("Owner"), None)
            .await
            .unwrap()
            .id;
        add_branch(&store, BRANCH, owner).await;
        let asins: Vec<String> = (0..BOOKS).map(|n| format!("B0PAGE{n:04}")).collect();
        hold(&store, owner, "owner-acct", &asins).await;
        let listed = read_all(&store, BRANCH).await;
        let mut products: Vec<&str> = listed
            .iter()
            .flat_map(|w| w.editions.iter().map(|e| e.product_id.as_str()))
            .collect();
        products.sort_unstable();
        products.dedup();
        assert_eq!(products.len(), BOOKS, "{name}");

        // The same store as a window: every page stored, merged and kept by
        // a complete read, all within the profile's limits.
        crate::entities::cluster_links::ActiveModel {
            owner_cluster_id: Set("lib".into()),
            owner_origin: Set("https://lib.example".into()),
            remote_node_id: Set("node-lib".into()),
            spki_pin: Set(None),
            state: Set("active".into()),
            enrolled_at: Set(Utc::now().to_rfc3339()),
            share_books: Set(1),
            include_branches: Set(1),
            ancestry_json: Set("[]".into()),
            list_members_books: Set(1),
            download: Set(0),
            max_jobs: Set(None),
        }
        .insert(store.db())
        .await
        .unwrap();
        let seen = window_read_stamp();
        for page in listed.chunks(MAX_WINDOW_WORKS) {
            record_window_page(&store, "lib", &seen, page)
                .await
                .unwrap();
        }
        assert_eq!(
            finish_window_read(&store, "lib", &seen).await.unwrap(),
            0,
            "{name}"
        );
        let view = window_view(&store).await.unwrap();
        assert_eq!(view.len(), BOOKS, "{name}");
        assert!(
            view.iter().all(|e| e.work_id.is_some()),
            "{name}: placed on local works"
        );
        let later = window_read_stamp();
        record_window_page(&store, "lib", &later, &listed[..1])
            .await
            .unwrap();
        assert_eq!(
            finish_window_read(&store, "lib", &later).await.unwrap(),
            u64::try_from(BOOKS - 1).unwrap(),
            "{name}"
        );
        assert_eq!(
            clear_window_books(&store, "lib").await.unwrap(),
            1,
            "{name}"
        );
    }
}

/// A work with more editions than one frame allows goes on under extra
/// keys of the page, which the window merges back by identifier.
#[tokio::test]
async fn a_work_with_many_editions_spills_into_extra_keys() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path()).await;
    let owner = store
        .create_user(UserRole::Owner, Some("Owner"), None)
        .await
        .unwrap()
        .id;
    add_branch(&store, BRANCH, owner).await;
    // Ten accounts in ten marketplaces, each holding the same ASIN: ten
    // distinct (store, marketplace, product) editions.
    let mut accounts = Vec::new();
    for n in 0..10 {
        let account = format!("acct-{n}");
        let market = format!("m{n}");
        store
            .upsert_account(PLUGIN, &account, &market, None, true, "audible")
            .await
            .unwrap();
        let identity = store
            .ensure_local_portal_identity(owner, None)
            .await
            .unwrap();
        store
            .link_account(identity.id, PLUGIN, &account, "test")
            .await
            .unwrap();
        let mut b = book(&account, "B0SAMEWORK");
        b.marketplace = market;
        store.upsert_book(&b).await.unwrap();
        accounts.push(account);
    }
    let listed = read_all(&store, BRANCH).await;
    let editions: usize = listed.iter().map(|w| w.editions.len()).sum();
    assert_eq!(editions, 10, "{listed:?}");
    assert!(listed
        .iter()
        .all(|w| w.editions.len() <= MAX_LOOKUP_EDITIONS));
}

fn edition(asin: &str, title: &str) -> LookupEdition {
    LookupEdition {
        store: "audible".into(),
        marketplace: "us".into(),
        product_id: asin.into(),
        availability: Availability::Owned,
        formats: vec![LookupFormat::Audio],
        metadata: CatalogMetadata {
            title: title.into(),
            ..CatalogMetadata::default()
        },
    }
}

fn asin(value: &str) -> LookupIdentifier {
    LookupIdentifier {
        scheme: LookupScheme::Asin,
        value: value.into(),
        marketplace: "us".into(),
    }
}

async fn link(store: &LibraryStore, cluster: &str, download: bool) {
    record_cluster_link(
        store,
        &NewClusterLink {
            owner_cluster_id: cluster.into(),
            owner_origin: format!("https://{cluster}.example"),
            remote_node_id: format!("node-{cluster}"),
            spki_pin: None,
        },
        &ClusterLinkKeys::generate(),
    )
    .await
    .unwrap();
    assert!(set_cluster_link_download(store, cluster, download)
        .await
        .unwrap());
}

#[tokio::test]
async fn the_window_merges_its_libraries_by_work_and_drops_what_they_stop_holding() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path()).await;
    let me = store
        .create_user(UserRole::Owner, Some("Me"), None)
        .await
        .unwrap()
        .id;
    // The window's own library knows one of the works.
    let local = hold(&store, me, "my-acct", &["B0SHARED01".into()]).await;
    link(&store, "alpha", false).await;
    link(&store, "beta", false).await;
    link(&store, "gamma", true).await;
    assert!(!set_cluster_link_download(&store, "nowhere", false)
        .await
        .unwrap());

    let first = window_read_stamp();
    let alpha = vec![
        WindowWork {
            work: "a-1".into(),
            identifiers: vec![asin("B0SHARED01")],
            editions: vec![edition("B0SHARED01", "Shared")],
        },
        WindowWork {
            work: "a-2".into(),
            identifiers: vec![asin("B0ALPHA001")],
            editions: vec![edition("B0ALPHA001", "Alpha only")],
        },
    ];
    assert_eq!(
        record_window_page(&store, "alpha", &first, &alpha)
            .await
            .unwrap(),
        2
    );
    // Beta names the same work by another edition's ASIN plus the shared one.
    let beta = vec![
        WindowWork {
            work: "b-9".into(),
            identifiers: vec![asin("B0SHARED01"), asin("B0BETAED01")],
            editions: vec![edition("B0BETAED01", "Shared (beta edition)")],
        },
        WindowWork {
            work: "book-4".into(),
            identifiers: Vec::new(),
            editions: vec![edition("B0NOIDS001", "No identifiers")],
        },
    ];
    record_window_page(&store, "beta", &first, &beta)
        .await
        .unwrap();
    // A link that downloads is not part of the view.
    record_window_page(
        &store,
        "gamma",
        &first,
        &[WindowWork {
            work: "g".into(),
            identifiers: Vec::new(),
            editions: vec![edition("B0GAMMA001", "Gamma")],
        }],
    )
    .await
    .unwrap();

    let view = window_view(&store).await.unwrap();
    let titles: Vec<&str> = view.iter().map(WindowEntry::title).collect();
    assert_eq!(titles, vec!["Alpha only", "No identifiers", "Shared"]);
    let shared = &view[2];
    assert_eq!(shared.work_id.as_deref(), Some(local[0].as_str()));
    assert_eq!(shared.key, format!("work:{}", local[0]));
    let libraries: Vec<&str> = shared
        .editions
        .iter()
        .map(|e| e.owner_cluster_id.as_str())
        .collect();
    assert_eq!(libraries, vec!["alpha", "beta"]);
    assert_eq!(shared.editions[0].owner_origin, "https://alpha.example");
    assert_eq!(shared.formats(), vec![LookupFormat::Audio]);
    assert_eq!(view[0].work_id, None);

    // A later complete read of alpha that no longer names a-2 drops it.
    let second = window_read_stamp();
    record_window_page(&store, "alpha", &second, &alpha[..1])
        .await
        .unwrap();
    assert_eq!(
        finish_window_read(&store, "alpha", &second).await.unwrap(),
        1
    );
    let titles: Vec<String> = window_view(&store)
        .await
        .unwrap()
        .iter()
        .map(|e| e.title().to_string())
        .collect();
    assert_eq!(titles, vec!["No identifiers", "Shared"]);

    // Turning download back on hides beta; clearing drops its rows.
    set_cluster_link_download(&store, "beta", true)
        .await
        .unwrap();
    assert_eq!(window_view(&store).await.unwrap().len(), 1);
    assert_eq!(clear_window_books(&store, "beta").await.unwrap(), 2);
}
