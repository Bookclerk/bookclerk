//! The demo seed against a fresh library at this commit's schema: it adds
//! Sam, Sam's books and wishlist, and an offline branch that shared titles,
//! and a second run changes nothing. A schema or library change that breaks
//! the seed fails here instead of in the demo.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};

use bookclerk_demo_seed::{
    best_match, clear_install_files, committed_records, deploy_on_node, distinct_records, lookups,
    merge_records, parse_mode, parse_records, records_json, search_query, seed, Lookup, Mode,
    SeedReport, Seeded, StoreRecords, SAMPLE_BRANCH, SAMPLE_LOGIN, STOREFRONTS,
};
use bookclerk_library::control_plane::branch_title_counts;
use bookclerk_library::entities::nodes;
use bookclerk_library::{AcquireStatus, LibraryStore, UserRole};
use bookclerk_source::records::{self, CreditRole, IdentifierScheme};
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
        credits: records::credits_from_names([authors], CreditRole::Author),
        work_kind: bookclerk_source::records::WorkKind::Book,
        expression_kind: bookclerk_source::records::ExpressionKind::Narration,
        media_kind: bookclerk_source::records::MediaKind::Audio,
        ..CatalogHit::default()
    }
}

/// One identifier list holding `value` in `scheme`.
fn ids(scheme: IdentifierScheme, value: &str) -> Vec<records::Identifier> {
    records::identifier(scheme, value, None)
        .into_iter()
        .collect()
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
    assert_eq!(best_match(&dune, &hits, now()).unwrap().product_id, "B4");
    assert_eq!(
        best_match(&dune, &hits[..3], now()).unwrap().product_id,
        "B3"
    );
    assert!(best_match(&dune, &hits[..1], now()).is_none());

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
    assert_eq!(
        best_match(&mistborn, &parts, now()).unwrap().product_id,
        "GA2"
    );

    // GraphicAudio returns no authors; its titles add a series number.
    let authorless = |id: &str, title: &str| CatalogHit {
        product_id: id.into(),
        title: title.into(),
        work_kind: bookclerk_source::records::WorkKind::Book,
        expression_kind: bookclerk_source::records::ExpressionKind::Narration,
        media_kind: bookclerk_source::records::MediaKind::Audio,
        ..CatalogHit::default()
    };
    let ga = [
        authorless("GA1", "Mistborn 1: The Final Empire 1 of 3"),
        authorless("GA2", "Mistborn 1: The Final Empire 2 of 3"),
    ];
    assert_eq!(best_match(&mistborn, &ga, now()).unwrap().product_id, "GA2");
    assert_eq!(
        best_match(&lookup("demo-mistborn-1-part-1"), &ga, now())
            .unwrap()
            .product_id,
        "GA1"
    );

    // The store's first-ranked hit wins a tie.
    let twice = [
        hit("A", "Circe", "Madeline Miller"),
        hit("B", "Circe", "Madeline Miller"),
    ];
    assert_eq!(
        best_match(&lookup("demo-circe"), &twice, now())
            .unwrap()
            .product_id,
        "A"
    );
}

#[test]
fn searches_suit_each_store() {
    assert_eq!(
        search_query(&lookup("demo-mistborn-1-part-1")),
        "mistborn the final empire"
    );
    assert_eq!(
        search_query(&lookup("demo-project-hail-mary")),
        "Project Hail Mary weir"
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
            identifiers: ids(IdentifierScheme::Asin, "B08G9PRS1K"),
            credits: [
                records::credit("Andy Weir", CreditRole::Author),
                records::credit("Ray Porter", CreditRole::Narrator),
            ]
            .into_iter()
            .flatten()
            .collect(),
            cover_url: Some("https://m.media-amazon.com/images/I/phm.jpg".into()),
            description: Some("A lone astronaut.".into()),
            published_at: Some("2021-05-04".into()),
            ..hit("B08G9PRS1K", "Project Hail Mary", "Andy Weir")
        },
    );
    records.insert(
        "demo-wish-james",
        CatalogHit {
            identifiers: ids(IdentifierScheme::Asin, "B0CJ6Q8CZ4"),
            cover_url: Some("https://m.media-amazon.com/images/I/james.jpg".into()),
            ..hit("B0CJ6Q8CZ4", "James", "Percival Everett")
        },
    );
    records.insert(
        "demo-circe",
        CatalogHit {
            identifiers: ids(IdentifierScheme::Isbn, "9781549140396"),
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

#[test]
fn the_committed_records_parse_and_are_real_store_records() {
    let records = committed_records().unwrap();
    for (key, hit) in &records {
        let lookup = lookup(key);
        assert!(
            !hit.product_id.starts_with("demo-"),
            "{key}: a committed record must carry the store's product id"
        );
        assert!(
            best_match(&lookup, std::slice::from_ref(hit), now()).is_some(),
            "{key}: the committed record must match its title and author"
        );
    }
}

#[test]
fn a_records_file_round_trips_and_drops_unknown_titles() {
    let mut records = StoreRecords::new();
    records.insert("demo-dune", hit("B0HQ4V4BGY", "Dune", "Frank Herbert"));
    let json = records_json(&records).unwrap();
    assert!(json.ends_with('\n'));
    let back = parse_records(&json).unwrap();
    assert_eq!(back["demo-dune"].product_id, "B0HQ4V4BGY");

    let with_stray = json.replacen(
        '{',
        r#"{"demo-no-such-title": {"product_id": "x", "title": "x", "origin": "search"},"#,
        1,
    );
    let parsed = parse_records(&with_stray).unwrap();
    assert_eq!(
        parsed.keys().copied().collect::<Vec<_>>(),
        vec!["demo-dune"]
    );
    assert!(parse_records("[]").is_err());
}

#[test]
fn live_records_win_and_committed_ones_fill_the_gaps() {
    let mut live = StoreRecords::new();
    live.insert("demo-dune", hit("LIVE-DUNE", "Dune", "Frank Herbert"));
    let mut committed = StoreRecords::new();
    committed.insert("demo-dune", hit("OLD-DUNE", "Dune", "Frank Herbert"));
    committed.insert(
        "demo-lessons-in-chemistry",
        hit("9780593395042", "Lessons in Chemistry", "Bonnie Garmus"),
    );
    let merged = merge_records(live, committed);
    assert_eq!(merged["demo-dune"].product_id, "LIVE-DUNE");
    assert_eq!(
        merged["demo-lessons-in-chemistry"].product_id,
        "9780593395042"
    );
}

#[test]
fn a_stale_committed_record_never_pushes_out_a_live_one() {
    // The live lookup found the later title under a product id an earlier
    // title's committed record also holds, and missed the earlier title.
    let mut live = StoreRecords::new();
    live.insert(
        "demo-wish-iron-flame",
        hit("B0SHARED", "Iron Flame", "Rebecca Yarros"),
    );
    let mut committed = StoreRecords::new();
    committed.insert(
        "demo-wish-wind-and-truth",
        hit("B0SHARED", "Wind and Truth", "Brandon Sanderson"),
    );
    let merged = merge_records(live, committed);
    assert_eq!(merged["demo-wish-iron-flame"].product_id, "B0SHARED");
    assert!(!merged.contains_key("demo-wish-wind-and-truth"));
}

#[test]
fn prefers_a_released_edition_over_a_pre_order() {
    let dune = lookup("demo-dune");
    let mut pre_order = hit("B0PREORDER", "Dune", "Frank Herbert");
    pre_order.published_at = Some("2999-11-24".into());
    let mut classic = hit("B002V1OF70", "Dune", "Frank Herbert");
    classic.published_at = Some("2007-03-01".into());
    let undated = hit("B0UNDATED", "Dune", "Frank Herbert");
    let hits = [pre_order.clone(), classic, undated];
    assert_eq!(
        best_match(&dune, &hits, now()).unwrap().product_id,
        "B002V1OF70"
    );
    // With only the pre-order, it is still the match.
    assert_eq!(
        best_match(&dune, std::slice::from_ref(&pre_order), now())
            .unwrap()
            .product_id,
        "B0PREORDER"
    );
}

#[test]
fn reads_each_mode_and_refuses_anything_else() {
    let args = |list: &[&str]| {
        list.iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    };
    assert_eq!(parse_mode(args(&[])).unwrap(), Mode::Seed);
    assert_eq!(
        parse_mode(args(&["--write-records", "out.json"])).unwrap(),
        Mode::WriteRecords(std::path::PathBuf::from("out.json"))
    );
    assert_eq!(
        parse_mode(args(&["--bake-finish"])).unwrap(),
        Mode::BakeFinish
    );
    for bad in [
        &["--write-records"][..],
        &["--write-records", "out.json", "extra"][..],
        &["--bake-finish", "extra"][..],
        &["--seed"][..],
    ] {
        assert!(parse_mode(args(bad)).is_err(), "{bad:?}");
    }
}

/// A fixed "today" for matching, so which edition is out does not depend
/// on when the tests run.
fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-11-06T00:00:00Z")
        .unwrap()
        .to_utc()
}

/// A seeded files dir as bookclerkd leaves it, with every per-install file
/// in place; returns the files and cache dirs.
fn baked_dir(root: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let files = root.join("files");
    let cache = files.join("cache");
    for dir in [
        files.join("logs"),
        files.join("plugin-state/libro/data"),
        files.join("plugin-state/libro/session-1/tmp"),
        files.join("plugin-state/covers/session-2"),
        cache.join("covers"),
    ] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(bookclerk_library::master_key_path(&files), b"key").unwrap();
    std::fs::write(cache.join("covers/token.key"), b"signing").unwrap();
    std::fs::write(files.join("logs/bookclerkd.log"), b"line").unwrap();
    std::fs::write(files.join("plugin-state/libro/session-1/tmp/x"), b"x").unwrap();
    std::fs::write(files.join("library.db"), b"db").unwrap();
    std::fs::write(files.join("library.db-shm"), b"").unwrap();
    (files, cache)
}

#[test]
fn finishing_a_bake_clears_the_per_install_files() {
    let root = tempfile::tempdir().unwrap();
    let (files, cache) = baked_dir(root.path());

    clear_install_files(&files, &cache).unwrap();

    assert!(!bookclerk_library::master_key_path(&files).exists());
    assert!(!cache.join("covers/token.key").exists());
    assert!(!files.join("logs").exists());
    assert!(!files.join("plugin-state/libro/session-1").exists());
    assert!(!files.join("plugin-state/covers/session-2").exists());
    assert!(!files.join("library.db-shm").exists());
    // The library and each plugin's own state stay.
    assert!(files.join("library.db").exists());
    assert!(files.join("plugin-state/libro/data").is_dir());
    // A second pass finds nothing to remove.
    clear_install_files(&files, &cache).unwrap();
}

#[test]
fn finishing_a_bake_refuses_a_side_file_with_data() {
    let root = tempfile::tempdir().unwrap();
    let (files, cache) = baked_dir(root.path());
    std::fs::write(files.join("library.db-wal"), b"pending").unwrap();

    let err = clear_install_files(&files, &cache).unwrap_err();
    assert!(err.to_string().contains("library.db-wal"), "{err}");
    assert!(files.join("library.db-wal").exists());
}

#[test]
fn finishing_a_bake_reports_a_file_it_cannot_remove() {
    let root = tempfile::tempdir().unwrap();
    let files = root.path().join("files");
    // A directory where master.key belongs cannot be removed as a file.
    std::fs::create_dir_all(bookclerk_library::master_key_path(&files)).unwrap();
    let err = clear_install_files(&files, &files.join("cache")).unwrap_err();
    assert!(err.to_string().contains("remove"), "{err}");
}

#[tokio::test]
async fn deploys_a_default_instance_on_the_node_once() {
    use bookclerk_library::control_plane::{
        ensure_plugin_deployment, ensure_plugin_instance, load_deployment_for_node, ConfigActor,
        DesiredState,
    };
    let dir = tempfile::tempdir().unwrap();
    let store = fresh(dir.path()).await;
    let key = "path:file:///opt/bookclerk/storefronts/libro#libro";

    let err = deploy_on_node(&store, "node-a", key).await.unwrap_err();
    assert!(err.to_string().contains("no default instance"), "{err}");

    let instance = ensure_plugin_instance(&store, &ConfigActor::Bootstrap, key)
        .await
        .unwrap();
    deploy_on_node(&store, "node-a", key).await.unwrap();
    let first = load_deployment_for_node(&store, &instance.id, "node-a")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.desired, DesiredState::operator(true));
    // Already present: nothing changes.
    deploy_on_node(&store, "node-a", key).await.unwrap();
    let again = load_deployment_for_node(&store, &instance.id, "node-a")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.revision, first.revision);

    // A deployment an operator set to absent is turned back on.
    let operator = ConfigActor::Operator { id: "test".into() };
    ensure_plugin_deployment(
        &store,
        &operator,
        &instance.id,
        "node-b",
        DesiredState::operator(false),
    )
    .await
    .unwrap();
    deploy_on_node(&store, "node-b", key).await.unwrap();
    let flipped = load_deployment_for_node(&store, &instance.id, "node-b")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(flipped.desired, DesiredState::operator(true));
}
