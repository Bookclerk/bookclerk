//! Books in other libraries: the library answers a trusted branch with its
//! books, the owner's marked, page by page within the frame budget; the
//! reading install stores each library's pages, drops what a complete read
//! no longer names, and merges every link's books by local work and
//! identifier.

use bookclerk_node_proto::{
    window_page_problem, Availability, CatalogMetadata, LookupEdition, LookupFormat,
    LookupIdentifier, LookupScheme, WindowCopy, WindowCursor, WindowEdition, WindowWork,
    MAX_FRAME_BYTES, MAX_LOOKUP_EDITIONS, MAX_WINDOW_WORKS,
};
use sea_orm::{ActiveModelTrait, ActiveValue::Set};

use super::*;
use crate::control_plane::{
    leave_library_link, record_library_link, set_library_link_keep_copies, LibraryLinkKeys,
    NewLibraryLink,
};
use crate::entities::node_credentials;
use crate::master_key::master_key_test_lock_async;
use crate::models::{AcquireStatus, UserRole};
use crate::store::NewBook;

pub(crate) const PLUGIN: &str = "path:file:///tmp/store#audible";
pub(crate) const BRANCH: &str = "window-w";

async fn store_in(dir: &std::path::Path) -> LibraryStore {
    let db = bookclerk_plugin_database_sqlite::open(&dir.join("library.db"))
        .await
        .expect("open sqlite file");
    crate::apply_host_schema(&db).await.expect("schema");
    crate::configure_master_key(dir).expect("master key");
    LibraryStore::from_connection(db)
}

pub(crate) async fn add_branch(store: &LibraryStore, node_id: &str, owner: i64) {
    let now = Utc::now().to_rfc3339();
    nodes::ActiveModel {
        node_id: Set(node_id.into()),
        library_id: Set("library".into()),
        created_at: Set(now.clone()),
        incarnation: Set("test".into()),
        heartbeat_at: Set(now.clone()),
        software_version: Set("1.0.0".into()),
        schema_state: Set("current".into()),
        compatible: Set(1),
        target: Set(String::new()),
        labels_json: Set("[]".into()),
        kind: Set("branch".into()),
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

pub(crate) fn book(account: &str, asin: &str) -> NewBook {
    NewBook {
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
pub(crate) async fn hold(
    store: &LibraryStore,
    user: i64,
    account: &str,
    asins: &[String],
) -> Vec<String> {
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
        works.push(
            store
                .work_of_holding(&record.holding_id)
                .await
                .unwrap()
                .unwrap(),
        );
    }
    works
}

/// Every page of `node`'s window books, checked against the frame rules
/// and budget as it goes.
async fn read_all(store: &LibraryStore, node: &str) -> Vec<WindowWork> {
    let mut after = WindowCursor::start();
    let mut works = Vec::new();
    for _ in 0..1000 {
        let (page, next) = window_books_page(store, node, &after)
            .await
            .unwrap()
            .expect("a trusted branch is answered");
        assert_eq!(window_page_problem(&after, &page, next.as_ref()), None);
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
async fn the_library_lists_every_book_to_a_trusted_branch_marking_its_persons() {
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
    // Riley's second account holds the same edition: audio on one copy, the
    // PDF on the other, so the library has both.
    hold(&store, riley, "riley-acct-2", &["B0RILEY001".into()]).await;
    store
        .set_acquire_status(
            "B0RILEY001",
            "riley-acct",
            AcquireStatus::Acquired,
            None,
            None,
        )
        .await
        .unwrap();
    store
        .set_pdf_status("B0RILEY001", "riley-acct-2", AcquireStatus::Acquired, None)
        .await
        .unwrap();

    assert_eq!(
        window_books_page(&store, BRANCH, &WindowCursor::start())
            .await
            .unwrap(),
        None
    );
    add_branch(&store, BRANCH, riley).await;

    let listed = read_all(&store, BRANCH).await;
    assert_eq!(listed.len(), 2, "every book of the library: {listed:?}");
    let riley_work = listed
        .iter()
        .find(|w| w.work == works[0])
        .expect("Riley's book is listed");
    assert_eq!(riley_work.editions.len(), 1, "one entry per edition");
    let riley_book = &riley_work.editions[0];
    assert_eq!(riley_book.edition.product_id, "B0RILEY001");
    assert_eq!(riley_book.edition.availability, Availability::Owned);
    assert!(riley_book.yours, "on Riley's own accounts");
    assert_eq!(
        riley_book.edition.formats,
        vec![LookupFormat::Audio, LookupFormat::Pdf]
    );
    assert!(riley_work.identifiers.iter().any(|id| {
        id.scheme == LookupScheme::Asin && id.value == "B0RILEY001" && id.marketplace == "us"
    }));
    let sam_book = listed
        .iter()
        .flat_map(|w| &w.editions)
        .find(|e| e.edition.product_id == "B0SAM00001")
        .expect("Sam's book is listed too");
    assert!(!sam_book.yours, "only Sam's account holds it");
}

/// A clerk's library card reads the same page for its person, with no
/// branch node involved: every book, the person's marked.
#[tokio::test]
async fn a_persons_page_lists_every_book_marking_that_persons() {
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
    let nobody = store
        .create_user(UserRole::Member, Some("Nobody"), None)
        .await
        .unwrap()
        .id;
    let works = hold(&store, riley, "riley-acct", &["B0RILEY001".into()]).await;
    hold(&store, sam, "sam-acct", &["B0SAM00001".into()]).await;
    store
        .set_acquire_status(
            "B0RILEY001",
            "riley-acct",
            AcquireStatus::Acquired,
            None,
            None,
        )
        .await
        .unwrap();

    let (page, next) = person_books_page(&store, riley, &WindowCursor::start())
        .await
        .unwrap();
    assert_eq!(
        window_page_problem(&WindowCursor::start(), &page, next.as_ref()),
        None
    );
    assert_eq!(next, None);
    let yours = |page: &[WindowWork]| -> Vec<(String, bool)> {
        let mut books: Vec<(String, bool)> = page
            .iter()
            .flat_map(|w| &w.editions)
            .map(|e| (e.edition.product_id.clone(), e.yours))
            .collect();
        books.sort();
        books
    };
    assert_eq!(
        yours(&page),
        vec![("B0RILEY001".into(), true), ("B0SAM00001".into(), false)]
    );
    assert!(page.iter().any(|w| w.work == works[0]));
    let (page, next) = person_books_page(&store, nobody, &WindowCursor::start())
        .await
        .unwrap();
    assert_eq!(next, None);
    assert_eq!(
        yours(&page),
        vec![("B0RILEY001".into(), false), ("B0SAM00001".into(), false)],
        "a person with no store accounts sees every book, none theirs"
    );
}

/// A scoped person's pages list only the books their patron scope shows
/// them, and always the ones on their own store accounts, under every
/// profile: a clerk signed in with their card never learns of the rest.
#[tokio::test]
async fn a_persons_pages_follow_their_patron_scope_under_every_profile() {
    use crate::control_plane::{set_patron_scope, PatronScope, RuleEffect, ScopeField, ScopeRule};
    for profile in crate::test_support::Profile::ALL {
        let name = profile.name();
        let store = crate::test_support::sqlite::memory(profile)
            .await
            .into_store();
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
        let mut works = hold(&store, riley, "riley-acct", &["B0RILEY001".into()]).await;
        let mut other = book("sam-acct", "B0SAM00001");
        other.authors = Some("Bea Writer".into());
        store
            .upsert_account(PLUGIN, "sam-acct", "us", None, true, "audible")
            .await
            .unwrap();
        let rec = store.upsert_book(&other).await.unwrap();
        works.push(
            store
                .work_of_holding(&rec.holding_id)
                .await
                .unwrap()
                .unwrap(),
        );
        store.refresh_library_works(&works).await.unwrap();
        let no_ann = PatronScope {
            rules: vec![ScopeRule {
                field: ScopeField::Author,
                value: "Ann Author".into(),
                effect: RuleEffect::Exclude,
            }],
            explicit: Default::default(),
        };
        for person in [riley, sam] {
            set_patron_scope(&store, person, Some(no_ann.clone()))
                .await
                .unwrap();
        }
        let books = |works: Vec<WindowWork>| -> Vec<String> {
            let mut ids: Vec<String> = works
                .iter()
                .flat_map(|w| &w.editions)
                .map(|e| e.edition.product_id.clone())
                .collect();
            ids.sort();
            ids
        };
        assert_eq!(
            books(read_person(&store, sam).await),
            ["B0SAM00001"],
            "{name}: Ann Author's book is outside Sam's scope"
        );
        assert_eq!(
            books(read_person(&store, riley).await),
            ["B0RILEY001", "B0SAM00001"],
            "{name}: Riley's own book passes whatever the scope says"
        );
        assert!(
            read_person(&store, 9999).await.is_empty(),
            "{name}: a person the library does not have learns of nothing"
        );
    }
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
        hold(&store, owner, "owner-acct", &asins[..BOOKS - 10]).await;
        // A second account on the same identity, and a third on a second
        // sign-in identity: every one is read, whatever the row limit.
        hold(&store, owner, "owner-acct-2", &asins[BOOKS - 10..BOOKS - 5]).await;
        let second = store
            .link_portal_identity_to_user("oidc", "owner-sso", owner, None)
            .await
            .unwrap();
        store
            .upsert_account(PLUGIN, "owner-acct-3", "us", None, true, "audible")
            .await
            .unwrap();
        store
            .link_account(second.id, PLUGIN, "owner-acct-3", "test")
            .await
            .unwrap();
        for asin in &asins[BOOKS - 5..] {
            store
                .upsert_book(&book("owner-acct-3", asin))
                .await
                .unwrap();
        }
        let listed = read_all(&store, BRANCH).await;
        let mut products: Vec<&str> = listed
            .iter()
            .flat_map(|w| w.editions.iter().map(|e| e.edition.product_id.as_str()))
            .collect();
        products.sort_unstable();
        products.dedup();
        assert_eq!(products.len(), BOOKS, "{name}");

        // The same store as a reading branch: every page stored, merged and kept by
        // a complete read, all within the profile's limits.
        crate::entities::library_links::ActiveModel {
            parent_library_id: Set("lib".into()),
            owner_origin: Set("https://lib.example".into()),
            remote_node_id: Set("node-lib".into()),
            spki_pin: Set(None),
            state: Set("active".into()),
            enrolled_at: Set(Utc::now().to_rfc3339()),
            share_books: Set(1),
            include_branches: Set(1),
            ancestry_json: Set("[]".into()),
            list_members_books: Set(1),
            work_for_members: Set(0),
            keep_member_copies: Set(0),
            keep_copies: Set(0),
            person_user_id: Set(None),
            max_jobs: Set(None),
        }
        .insert(store.db())
        .await
        .unwrap();
        // A second link: listing the links pages too.
        crate::entities::library_links::ActiveModel {
            parent_library_id: Set("other".into()),
            owner_origin: Set("https://other.example".into()),
            remote_node_id: Set("node-other".into()),
            spki_pin: Set(None),
            state: Set("active".into()),
            enrolled_at: Set(Utc::now().to_rfc3339()),
            share_books: Set(1),
            include_branches: Set(1),
            ancestry_json: Set("[]".into()),
            list_members_books: Set(1),
            work_for_members: Set(0),
            keep_member_copies: Set(0),
            keep_copies: Set(1),
            person_user_id: Set(None),
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

/// `edition` as a page lists it for its person: theirs.
fn yours(edition: LookupEdition) -> WindowEdition {
    WindowEdition {
        edition,
        yours: true,
        copy: None,
    }
}

fn asin(value: &str) -> LookupIdentifier {
    LookupIdentifier {
        scheme: LookupScheme::Asin,
        value: value.into(),
        marketplace: "us".into(),
    }
}

async fn link(store: &LibraryStore, library: &str) {
    record_library_link(
        store,
        &NewLibraryLink {
            parent_library_id: library.into(),
            owner_origin: format!("https://{library}.example"),
            remote_node_id: format!("node-{library}"),
            spki_pin: None,
        },
        &LibraryLinkKeys::generate(),
    )
    .await
    .unwrap();
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
    link(&store, "alpha").await;
    link(&store, "beta").await;
    link(&store, "gamma").await;
    // Keeping copies off changes nothing about the view.
    assert!(set_library_link_keep_copies(&store, "gamma", false)
        .await
        .unwrap());
    assert!(!set_library_link_keep_copies(&store, "nowhere", false)
        .await
        .unwrap());

    let first = window_read_stamp();
    let alpha = vec![
        WindowWork {
            work: "a-1".into(),
            identifiers: vec![asin("B0SHARED01")],
            editions: vec![yours(edition("B0SHARED01", "Shared"))],
        },
        WindowWork {
            work: "a-2".into(),
            identifiers: vec![asin("B0ALPHA001")],
            editions: vec![yours(edition("B0ALPHA001", "Alpha only"))],
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
            editions: vec![yours(edition("B0BETAED01", "Shared (beta edition)"))],
        },
        WindowWork {
            work: "book-4".into(),
            identifiers: Vec::new(),
            editions: vec![yours(edition("B0NOIDS001", "No identifiers"))],
        },
    ];
    record_window_page(&store, "beta", &first, &beta)
        .await
        .unwrap();
    // Every active link is part of the view, keeping copies or not.
    record_window_page(
        &store,
        "gamma",
        &first,
        &[WindowWork {
            work: "g".into(),
            identifiers: Vec::new(),
            editions: vec![yours(edition("B0GAMMA001", "Gamma"))],
        }],
    )
    .await
    .unwrap();

    let view = window_view(&store).await.unwrap();
    let titles: Vec<&str> = view.iter().map(WindowEntry::title).collect();
    assert_eq!(
        titles,
        vec!["Alpha only", "Gamma", "No identifiers", "Shared"]
    );
    let shared = &view[3];
    assert_eq!(shared.work_id.as_deref(), Some(local[0].as_str()));
    assert_eq!(shared.key, format!("work:{}", local[0]));
    let libraries: Vec<&str> = shared
        .editions
        .iter()
        .map(|e| e.parent_library_id.as_str())
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
    assert_eq!(titles, vec!["Gamma", "No identifiers", "Shared"]);

    // Leaving beta hides its books; the daemon then drops its rows.
    leave_library_link(&store, "beta", None).await.unwrap();
    let titles: Vec<String> = window_view(&store)
        .await
        .unwrap()
        .iter()
        .map(|e| e.title().to_string())
        .collect();
    assert_eq!(titles, vec!["Gamma", "Shared"]);
    assert_eq!(clear_window_books(&store, "beta").await.unwrap(), 2);
}

/// Two accounts holding one edition with their books on different pages of
/// one read: the second copy adds its formats instead of replacing them,
/// and a later complete read still drops a format no longer held.
#[tokio::test]
async fn one_edition_on_two_pages_of_a_read_keeps_every_format() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path()).await;
    link(&store, "alpha").await;
    let held = |formats: Vec<LookupFormat>| {
        let mut copy = edition("B0TWICE001", "Twice");
        copy.formats = formats;
        vec![WindowWork {
            work: "w-1".into(),
            identifiers: vec![asin("B0TWICE001")],
            editions: vec![yours(copy)],
        }]
    };
    let formats = |view: Vec<WindowEntry>| -> Vec<LookupFormat> {
        view.into_iter()
            .flat_map(|entry| entry.editions)
            .flat_map(|held| held.edition.formats)
            .collect()
    };

    let read = window_read_stamp();
    record_window_page(&store, "alpha", &read, &held(vec![LookupFormat::Audio]))
        .await
        .unwrap();
    record_window_page(&store, "alpha", &read, &held(vec![LookupFormat::Pdf]))
        .await
        .unwrap();
    record_window_page(&store, "alpha", &read, &held(Vec::new()))
        .await
        .unwrap();
    finish_window_read(&store, "alpha", &read).await.unwrap();
    assert_eq!(
        formats(window_view(&store).await.unwrap()),
        vec![LookupFormat::Audio, LookupFormat::Pdf]
    );

    std::thread::sleep(std::time::Duration::from_millis(2));
    let next = window_read_stamp();
    assert_ne!(next, read);
    record_window_page(&store, "alpha", &next, &held(vec![LookupFormat::Pdf]))
        .await
        .unwrap();
    finish_window_read(&store, "alpha", &next).await.unwrap();
    assert_eq!(
        formats(window_view(&store).await.unwrap()),
        vec![LookupFormat::Pdf]
    );
}

/// The page states the copy the library holds of an edition: the probe of
/// its acquired primary audio file, the media type its name implies when
/// it was never probed, and nothing for an edition that is not acquired or
/// whose file has no row. The version changes when the file is rewritten.
#[tokio::test]
async fn a_page_states_the_copy_the_library_holds() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path()).await;
    let riley = store
        .create_user(UserRole::Member, Some("Riley"), None)
        .await
        .unwrap()
        .id;
    let asins: Vec<String> = [
        "B0PROBED01",
        "B0UNPROBED",
        "B0UNKNOWN1",
        "B0NOROW001",
        "B0NOTYET01",
    ]
    .map(String::from)
    .to_vec();
    hold(&store, riley, "riley-acct", &asins).await;
    let acquire = |asin: &'static str, key: &'static str, recorded: bool| {
        let store = store.clone();
        async move {
            store
                .set_acquire_status(asin, "riley-acct", AcquireStatus::Acquired, Some(key), None)
                .await
                .unwrap();
            let book = store.held_title(asin, "riley-acct").await.unwrap().unwrap();
            let owner = store
                .file_owner_of_holding(&book.holding_id)
                .await
                .unwrap()
                .unwrap();
            if recorded {
                assert!(store
                    .record_edition_file("local", key, &owner, FileRole::Audio)
                    .await
                    .unwrap());
            }
            owner
        }
    };
    let probed = acquire("B0PROBED01", "A/Probed/Probed.m4b", true).await;
    assert!(store
        .set_edition_file_probe(
            "local",
            "A/Probed/Probed.m4b",
            &probed,
            &crate::FileProbe {
                content_type: "audio/flac".into(),
                bitrate_kbps: Some(900),
                lossless: true,
                source_store: Some("libro".into()),
            },
        )
        .await
        .unwrap());
    // The same file on a second destination, never probed: the probed row
    // is the one described.
    assert!(store
        .record_edition_file("a-first", "A/Probed/Probed.m4b", &probed, FileRole::Audio)
        .await
        .unwrap());
    acquire("B0UNPROBED", "A/Unprobed/Unprobed.mp3", true).await;
    acquire("B0UNKNOWN1", "A/Unknown/Unknown.wma", true).await;
    acquire("B0NOROW001", "A/NoRow/NoRow.m4b", false).await;

    let copies = |works: Vec<WindowWork>| -> BTreeMap<String, Option<WindowCopy>> {
        works
            .into_iter()
            .flat_map(|work| work.editions)
            .map(|held| (held.edition.product_id, held.copy))
            .collect()
    };
    let (works, next) = person_books_page(&store, riley, &WindowCursor::start())
        .await
        .unwrap();
    assert_eq!(next, None);
    assert_eq!(
        window_page_problem(&WindowCursor::start(), &works, None),
        None
    );
    let first = copies(works);
    let version = |asin: &str| first[asin].as_ref().map(|copy| copy.version.clone());
    assert_eq!(
        first["B0PROBED01"],
        Some(WindowCopy {
            source_store: Some("libro".into()),
            content_type: "audio/flac".into(),
            bitrate_kbps: Some(900),
            lossless: true,
            version: version("B0PROBED01").unwrap(),
        })
    );
    assert_eq!(
        first["B0UNPROBED"],
        Some(WindowCopy {
            source_store: None,
            content_type: "audio/mpeg".into(),
            bitrate_kbps: None,
            lossless: false,
            version: version("B0UNPROBED").unwrap(),
        })
    );
    assert_ne!(version("B0PROBED01"), version("B0UNPROBED"));
    for none in ["B0UNKNOWN1", "B0NOROW001", "B0NOTYET01"] {
        assert_eq!(first[none], None, "{none}");
    }

    // Read again: the same file is the same version. Written again by an
    // upload (a new writer and digest, the probe not yet taken): a new one.
    let (works, _) = person_books_page(&store, riley, &WindowCursor::start())
        .await
        .unwrap();
    assert_eq!(copies(works), first);
    {
        use crate::entities::edition_files;
        use sea_orm::sea_query::Expr;
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
        let none: Option<String> = None;
        edition_files::Entity::update_many()
            .col_expr(edition_files::Column::Writer, Expr::value("upload-2"))
            .col_expr(edition_files::Column::Sha256, Expr::value("ab12"))
            .col_expr(edition_files::Column::ProbedAt, Expr::value(none.clone()))
            .col_expr(
                edition_files::Column::ContentType,
                Expr::value(none.clone()),
            )
            .col_expr(
                edition_files::Column::BitrateKbps,
                Expr::value(Option::<i32>::None),
            )
            .col_expr(edition_files::Column::SourceStore, Expr::value(none))
            .col_expr(edition_files::Column::Lossless, Expr::value(0))
            .filter(edition_files::Column::StorageKey.eq("A/Probed/Probed.m4b"))
            .exec(store.db())
            .await
            .unwrap();
    }
    let (works, _) = person_books_page(&store, riley, &WindowCursor::start())
        .await
        .unwrap();
    let after = copies(works);
    let replaced = after["B0PROBED01"].clone().expect("still held");
    assert_ne!(Some(replaced.version.clone()), version("B0PROBED01"));
    assert_eq!(
        (
            replaced.content_type.as_str(),
            replaced.bitrate_kbps,
            replaced.lossless
        ),
        ("audio/mp4", None, false),
        "the new file was not probed yet"
    );
}

/// The window keeps the copy a library states for an edition: a later
/// page of the same read that states none does not drop it, and the next
/// read's word replaces it.
#[tokio::test]
async fn the_window_keeps_the_copy_a_library_states() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path()).await;
    link(&store, "alpha").await;
    let copy = WindowCopy {
        source_store: Some("audible".into()),
        content_type: "audio/mp4".into(),
        bitrate_kbps: Some(125),
        lossless: false,
        version: "v1".into(),
    };
    let held = |copy: Option<WindowCopy>| {
        vec![WindowWork {
            work: "w-1".into(),
            identifiers: vec![asin("B0COPY0001")],
            editions: vec![WindowEdition {
                copy,
                ..yours(edition("B0COPY0001", "Copied"))
            }],
        }]
    };
    let stated = |view: Vec<WindowEntry>| -> Vec<Option<WindowCopy>> {
        view.into_iter()
            .flat_map(|entry| entry.editions)
            .map(|held| held.copy)
            .collect()
    };
    let read = window_read_stamp();
    record_window_page(&store, "alpha", &read, &held(Some(copy.clone())))
        .await
        .unwrap();
    record_window_page(&store, "alpha", &read, &held(None))
        .await
        .unwrap();
    finish_window_read(&store, "alpha", &read).await.unwrap();
    assert_eq!(stated(window_view(&store).await.unwrap()), vec![Some(copy)]);

    std::thread::sleep(std::time::Duration::from_millis(2));
    let next = window_read_stamp();
    record_window_page(&store, "alpha", &next, &held(None))
        .await
        .unwrap();
    finish_window_read(&store, "alpha", &next).await.unwrap();
    assert_eq!(stated(window_view(&store).await.unwrap()), vec![None]);
}

/// A work's overflow groups (`<work>-x<n>`) are stored under the work's own
/// grouping key, so the window shows one entry even with no identifier to
/// join them.
/// Whether an edition is the person's survives the store: it stays theirs
/// when any copy in a read is, and a later read where no copy is theirs
/// any more drops the mark. A row stored before the library sent the mark
/// reads as not theirs.
#[tokio::test]
async fn whether_a_book_is_yours_is_kept_per_read() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path()).await;
    link(&store, "alpha").await;
    let page = |asin_value: &str, mine: bool| {
        vec![WindowWork {
            work: format!("w-{asin_value}"),
            identifiers: vec![asin(asin_value)],
            editions: vec![WindowEdition {
                edition: edition(asin_value, asin_value),
                yours: mine,
                copy: None,
            }],
        }]
    };
    let marks = |view: Vec<WindowEntry>| -> Vec<(String, bool)> {
        view.into_iter()
            .flat_map(|entry| entry.editions)
            .map(|held| (held.edition.product_id, held.yours))
            .collect()
    };

    let read = window_read_stamp();
    for (asin_value, mine) in [
        ("B0MINE0001", false),
        ("B0MINE0001", true),
        ("B0MINE0001", false),
        ("B0OTHER001", false),
    ] {
        record_window_page(&store, "alpha", &read, &page(asin_value, mine))
            .await
            .unwrap();
    }
    finish_window_read(&store, "alpha", &read).await.unwrap();
    assert_eq!(
        marks(window_view(&store).await.unwrap()),
        vec![("B0MINE0001".into(), true), ("B0OTHER001".into(), false)]
    );

    let later = window_read_stamp();
    for asin_value in ["B0MINE0001", "B0OTHER001"] {
        record_window_page(&store, "alpha", &later, &page(asin_value, false))
            .await
            .unwrap();
    }
    finish_window_read(&store, "alpha", &later).await.unwrap();
    assert_eq!(
        marks(window_view(&store).await.unwrap()),
        vec![("B0MINE0001".into(), false), ("B0OTHER001".into(), false)]
    );

    let before: StoredBook = serde_json::from_str(
        r#"{"identifiers":[],"edition":{"store":"audible","product_id":"B0OLD00001","availability":"owned","metadata":{"title":"Old"}}}"#,
    )
    .unwrap();
    assert!(!before.yours);
}

#[tokio::test]
async fn a_works_overflow_group_joins_the_work_without_identifiers() {
    assert_eq!(grouping_key("0b1e-4c2a-x2"), "0b1e-4c2a");
    assert_eq!(grouping_key("book-12"), "book-12");
    assert_eq!(grouping_key("book-12-x1"), "book-12");
    assert_eq!(grouping_key("-x1"), "-x1");
    assert_eq!(grouping_key("w-x"), "w-x");
    assert_eq!(grouping_key(&overflow_key("w", 3)), "w");

    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path()).await;
    link(&store, "alpha").await;
    let read = window_read_stamp();
    let page = vec![
        WindowWork {
            work: "book-7".into(),
            identifiers: Vec::new(),
            editions: vec![yours(edition("B0OVERFL01", "Overflowing"))],
        },
        WindowWork {
            work: overflow_key("book-7", 1),
            identifiers: Vec::new(),
            editions: vec![yours(edition("B0OVERFL02", "Overflowing"))],
        },
    ];
    record_window_page(&store, "alpha", &read, &page)
        .await
        .unwrap();
    let view = window_view(&store).await.unwrap();
    assert_eq!(view.len(), 1, "{view:?}");
    assert_eq!(view[0].editions.len(), 2);
}

/// One product listed in several marketplaces (several listings, and a
/// work with several identifiers) is read in full under every profile,
/// even where the database answers one row per read.
#[tokio::test]
async fn a_product_in_several_marketplaces_is_read_under_every_profile() {
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
        hold(&store, owner, "owner-acct", &["B0MULTI001".into()]).await;
        for market in ["uk", "de"] {
            let account = format!("owner-acct-{market}");
            hold(&store, owner, &account, &[]).await;
            let mut b = book(&account, "B0MULTI001");
            b.marketplace = market.into();
            store.upsert_book(&b).await.unwrap();
        }
        let listed = read_all(&store, BRANCH).await;
        let mut markets: Vec<&str> = listed
            .iter()
            .flat_map(|w| w.editions.iter().map(|e| &e.edition))
            .filter(|e| e.product_id == "B0MULTI001")
            .map(|e| e.marketplace.as_str())
            .collect();
        markets.sort_unstable();
        markets.dedup();
        assert_eq!(markets, ["de", "uk", "us"], "{name}: {listed:?}");
        assert!(
            listed.iter().all(|w| !w.identifiers.is_empty()),
            "{name}: every work keeps its identifiers"
        );

        // Two work-level identifiers on the one work are both read.
        let work = listed[0].work.clone();
        for raw in ["OL1W", "OL2W"] {
            store
                .add_work_identifier(
                    &work,
                    bookclerk_resolve::IdentifierScheme::OpenLibraryWork,
                    raw,
                )
                .await
                .unwrap();
        }
        let listed = read_all(&store, BRANCH).await;
        let mut works: Vec<&str> = listed
            .iter()
            .flat_map(|w| w.identifiers.iter())
            .filter(|i| i.scheme == LookupScheme::OpenlibraryWork)
            .map(|i| i.value.as_str())
            .collect();
        works.sort_unstable();
        works.dedup();
        assert_eq!(works, ["OL1W", "OL2W"], "{name}: {listed:?}");
    }
}

/// An edition whose metadata together is wider than one database cell
/// (each field fits on its own) is stored in pieces and read back whole,
/// under every profile; a newer read replaces the pieces and a finished
/// read drops the old ones.
#[tokio::test]
async fn an_edition_wider_than_a_cell_is_kept_whole_under_every_profile() {
    for profile in crate::test_support::Profile::ALL {
        let name = profile.name();
        let store = crate::test_support::sqlite::memory(profile)
            .await
            .into_store();
        crate::entities::library_links::ActiveModel {
            parent_library_id: Set("lib".into()),
            owner_origin: Set("https://lib.example".into()),
            remote_node_id: Set("node-lib".into()),
            spki_pin: Set(None),
            state: Set("active".into()),
            enrolled_at: Set(Utc::now().to_rfc3339()),
            share_books: Set(1),
            include_branches: Set(1),
            ancestry_json: Set("[]".into()),
            list_members_books: Set(1),
            work_for_members: Set(0),
            keep_member_copies: Set(0),
            keep_copies: Set(0),
            person_user_id: Set(None),
            max_jobs: Set(None),
        }
        .insert(store.db())
        .await
        .unwrap();
        let mut wide = edition("B0WIDE0001", &"T\u{e9}".repeat(200));
        wide.metadata.authors = Some("A\u{2014}".repeat(150));
        wide.metadata.narrators = Some("N".repeat(600));
        assert!(wide.problem().is_none(), "{name}");
        let work = WindowWork {
            work: "w1".into(),
            identifiers: vec![asin("B0WIDE0001")],
            editions: vec![yours(wide.clone())],
        };
        let first = window_read_stamp();
        record_window_page(&store, "lib", &first, std::slice::from_ref(&work))
            .await
            .unwrap();
        finish_window_read(&store, "lib", &first).await.unwrap();
        let view = window_view(&store).await.unwrap();
        assert_eq!(view.len(), 1, "{name}");
        assert_eq!(view[0].editions[0].edition, wide, "{name}");

        // A write cut short (pieces in, its row never pointed at them)
        // leaves the row on its previous version.
        window_book_parts::ActiveModel {
            parent_library_id: Set("lib".into()),
            store: Set("audible".into()),
            marketplace: Set("us".into()),
            product_id: Set("B0WIDE0001".into()),
            version: Set("cut-short".into()),
            seq: Set(0),
            body: Set("{\"identifiers\":[".into()),
        }
        .insert(store.db())
        .await
        .unwrap();
        let view = window_view(&store).await.unwrap();
        assert_eq!(view[0].editions[0].edition, wide, "{name}: still whole");

        // A later read with a shorter title replaces every piece.
        let mut short = wide.clone();
        short.metadata.title = "Short".into();
        let later = window_read_stamp();
        let work = WindowWork {
            editions: vec![yours(short.clone())],
            ..work
        };
        record_window_page(&store, "lib", &later, &[work])
            .await
            .unwrap();
        finish_window_read(&store, "lib", &later).await.unwrap();
        let view = window_view(&store).await.unwrap();
        assert_eq!(view[0].editions[0].edition, short, "{name}");
        let versions: BTreeSet<String> =
            crate::test_support::uncapped(window_book_parts::Entity::find().all(store.db()))
                .await
                .unwrap()
                .into_iter()
                .map(|part| part.version)
                .collect();
        assert_eq!(versions.len(), 1, "{name}: only the named version stays");
        assert_eq!(
            clear_window_books(&store, "lib").await.unwrap(),
            1,
            "{name}"
        );
        assert!(window_view(&store).await.unwrap().is_empty(), "{name}");
    }
}

#[test]
fn json_is_cut_on_character_boundaries_and_joined_in_order() {
    let json = "\u{e9}a\u{1f600}bc";
    let pieces = cut(json, 1);
    assert!(pieces.iter().all(|p| p.len() <= 4));
    assert_eq!(pieces.concat(), json);
    let part = |seq: i64, body: &str| window_book_parts::Model {
        parent_library_id: "lib".into(),
        store: "audible".into(),
        marketplace: "us".into(),
        product_id: "B0X".into(),
        version: "v1".into(),
        seq,
        body: body.into(),
    };
    let stored = serde_json::to_string(&StoredBook {
        identifiers: vec![],
        edition: edition("B0X", "Title"),
        yours: false,
        copy: None,
    })
    .unwrap();
    let (a, b) = stored.split_at(10);
    // Out of order in, joined in order.
    assert_eq!(join_parts(vec![part(1, b), part(0, a)]).len(), 1);
    // A gap spoils the book.
    assert!(join_parts(vec![part(0, a), part(2, b)]).is_empty());
}

/// An ISBN alone never places a window book on a work of this library, so
/// it cannot join another library's book through that work.
#[tokio::test]
async fn an_isbn_alone_does_not_place_a_window_book() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path()).await;
    let me = store
        .create_user(UserRole::Owner, Some("Me"), None)
        .await
        .unwrap()
        .id;
    hold(&store, me, "my-acct", &[]).await;
    let mut local = book("my-acct", "B0LOCAL001");
    local.isbn = Some("9780306406157".into());
    store.upsert_book(&local).await.unwrap();
    link(&store, "alpha").await;
    let isbn = LookupIdentifier {
        scheme: LookupScheme::Isbn,
        value: "9780306406157".into(),
        marketplace: String::new(),
    };
    assert!(isbn.is_well_formed());
    let seen = window_read_stamp();
    record_window_page(
        &store,
        "alpha",
        &seen,
        &[WindowWork {
            work: "remote".into(),
            identifiers: vec![isbn],
            editions: vec![yours(edition("B0REMOTE01", "Remote"))],
        }],
    )
    .await
    .unwrap();
    let view = window_view(&store).await.unwrap();
    assert_eq!(view.len(), 1);
    assert_eq!(view[0].work_id, None, "not placed by the ISBN");
}

/// Stands in for an adapter that refuses a read whose rows carry more than
/// `cap` bytes of text, the way a plugin refuses a reply past its
/// `maxResultBytes` (the profile kit does not size ORM reads).
#[derive(Debug)]
struct TextCapped {
    /// The engine every statement goes to.
    inner: bookclerk_plugin_database_sqlite::sqlite::SqliteProxy,
    /// Most bytes of text one reply may carry, once set (0 until then, so
    /// the fixture can store a book the host's own writes read back whole).
    cap: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Most binds one read may carry (0: no limit), the way a plugin
    /// refuses a statement past its `maxBinds`.
    max_binds: usize,
    /// Most bytes one text cell of a reply may carry, once set (0 until
    /// then), the way a plugin refuses a cell past its `maxCellBytes`.
    max_cell: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl sea_orm::ProxyDatabaseTrait for TextCapped {
    async fn query(
        &self,
        statement: sea_orm::Statement,
    ) -> std::result::Result<Vec<sea_orm::ProxyRow>, sea_orm::DbErr> {
        let binds = statement.values.as_ref().map_or(0, |values| values.0.len());
        if self.max_binds > 0 && binds > self.max_binds {
            return Err(sea_orm::DbErr::Custom(
                bookclerk_plugin_abi::LimitExceeded::new(
                    "maxBinds",
                    u64::try_from(self.max_binds).unwrap(),
                    u64::try_from(binds).unwrap(),
                )
                .to_message(),
            ));
        }
        let rows = self.inner.query(statement).await?;
        let max_cell = self.max_cell.load(std::sync::atomic::Ordering::SeqCst);
        let widest = rows
            .iter()
            .flat_map(|row| row.values.values())
            .map(|value| match value {
                sea_orm::Value::String(Some(text)) => text.len(),
                _ => 0,
            })
            .max()
            .unwrap_or(0);
        if max_cell > 0 && widest > max_cell {
            return Err(sea_orm::DbErr::Custom(
                bookclerk_plugin_abi::LimitExceeded::new(
                    "maxCellBytes",
                    u64::try_from(max_cell).unwrap(),
                    u64::try_from(widest).unwrap(),
                )
                .to_message(),
            ));
        }
        let used: usize = rows
            .iter()
            .flat_map(|row| row.values.values())
            .map(|value| match value {
                sea_orm::Value::String(Some(text)) => text.len(),
                _ => 0,
            })
            .sum();
        let cap = self.cap.load(std::sync::atomic::Ordering::SeqCst);
        if cap > 0 && used > cap {
            return Err(sea_orm::DbErr::Custom(
                bookclerk_plugin_abi::LimitExceeded::new(
                    "maxResultBytes",
                    u64::try_from(cap).unwrap(),
                    u64::try_from(used).unwrap(),
                )
                .to_message(),
            ));
        }
        Ok(rows)
    }

    async fn execute(
        &self,
        statement: sea_orm::Statement,
    ) -> std::result::Result<sea_orm::ProxyExecResult, sea_orm::DbErr> {
        self.inner.execute(statement).await
    }

    async fn begin(&self) {
        self.inner.begin().await;
    }

    async fn commit(&self) {
        self.inner.commit().await;
    }

    async fn rollback(&self) {
        self.inner.rollback().await;
    }

    async fn ping(&self) -> std::result::Result<(), sea_orm::DbErr> {
        self.inner.ping().await
    }
}

/// A title whose listing metadata together is wider than one database
/// reply (each field fits a cell) does not stop the window: the page's
/// titles are read again one at a time, that title is left out, and the
/// titles around it are listed. (The book is stored before the cap applies, since the host's
/// own write reads a book back whole.)
#[tokio::test]
async fn a_title_wider_than_one_reply_is_left_out_of_the_window() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let (store, caps) = capped_store(dir.path()).await;
    let cap = caps.reply;
    let riley = store
        .create_user(UserRole::Member, Some("Riley"), None)
        .await
        .unwrap()
        .id;
    hold(&store, riley, "riley-acct", &["B0NARROW01".into()]).await;
    let mut wide = book("riley-acct", "B0WIDE0001");
    wide.title = "T".repeat(900);
    wide.subtitle = Some("S".repeat(900));
    wide.authors = Some("A".repeat(900));
    wide.narrators = Some("N".repeat(900));
    wide.series = Some("R".repeat(900));
    store.upsert_book(&wide).await.unwrap();
    hold(&store, riley, "riley-acct", &["B0NARROW02".into()]).await;
    add_branch(&store, BRANCH, riley).await;
    cap.store(4096, std::sync::atomic::Ordering::SeqCst);

    // Pages follow holding ids, which are random: compare as sets.
    let mut listed: Vec<String> = read_all(&store, BRANCH)
        .await
        .into_iter()
        .flat_map(|work| work.editions)
        .map(|edition| edition.edition.product_id)
        .collect();
    listed.sort();
    assert_eq!(listed, vec!["B0NARROW01", "B0NARROW02"]);

    // One holding per read (the Minimum profile's `maxResultRows`): the
    // page that only skips the wide title still moves the cursor past it.
    let mut after = WindowCursor::start();
    let mut listed = Vec::new();
    loop {
        let (page, next) = window_books_page_by(&store, BRANCH, &after, 1)
            .await
            .unwrap()
            .unwrap();
        listed.extend(
            page.into_iter()
                .flat_map(|work| work.editions)
                .map(|edition| edition.edition.product_id),
        );
        match next {
            Some(next) => after = next,
            None => break,
        }
    }
    listed.sort();
    assert_eq!(listed, vec!["B0NARROW01", "B0NARROW02"]);
}

/// The caps of a [`capped_store`], set once its fixture is stored.
pub(crate) struct Caps {
    /// Most bytes of text one reply may carry (0: no cap).
    pub(crate) reply: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Most bytes one text cell may carry (0: no cap).
    pub(crate) cell: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

/// A library over [`TextCapped`] with no caps yet, and the caps to set once
/// the fixture is stored.
pub(crate) async fn capped_store(dir: &std::path::Path) -> (LibraryStore, Caps) {
    let conn = rusqlite::Connection::open(dir.join("library.db")).unwrap();
    let caps = Caps {
        reply: std::sync::Arc::default(),
        cell: std::sync::Arc::default(),
    };
    let engine = TextCapped {
        inner: bookclerk_plugin_database_sqlite::sqlite::SqliteProxy::new(conn).unwrap(),
        cap: std::sync::Arc::clone(&caps.reply),
        max_binds: 0,
        max_cell: std::sync::Arc::clone(&caps.cell),
    };
    let db = sea_orm::Database::connect_proxy(
        sea_orm::DbBackend::Sqlite,
        std::sync::Arc::new(Box::new(engine)),
    )
    .await
    .unwrap();
    crate::apply_host_schema(&db).await.unwrap();
    crate::configure_master_key(dir).unwrap();
    (LibraryStore::from_connection(db), caps)
}

/// Runs `sql` on the library as is, for a fixture the host's writes would
/// not produce.
pub(crate) async fn raw(store: &LibraryStore, sql: &str) {
    use sea_orm::ConnectionTrait;
    store.db().execute_unprepared(sql).await.unwrap();
}

/// Listing text that fits one listing at a time but not a chunk of them
/// is read a listing at a time: every title is listed with its language.
#[tokio::test]
async fn listing_text_wider_than_one_reply_together_is_read_per_listing() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let (store, caps) = capped_store(dir.path()).await;
    let cap = caps.reply;
    let riley = store
        .create_user(UserRole::Member, Some("Riley"), None)
        .await
        .unwrap()
        .id;
    hold(
        &store,
        riley,
        "riley-acct",
        &["B0LANG0001".into(), "B0LANG0002".into()],
    )
    .await;
    add_branch(&store, BRANCH, riley).await;
    let language = "l".repeat(2500);
    raw(
        &store,
        &format!("UPDATE store_listings SET language = '{language}'"),
    )
    .await;
    cap.store(4096, std::sync::atomic::Ordering::SeqCst);

    let titles = listed_titles(
        &store,
        titles_within_bytes(&store, find_holdings().all(store.db()).await.unwrap())
            .await
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(titles.len(), 2);
    assert!(titles
        .iter()
        .all(|t| t.language.as_deref() == Some(language.as_str())));
}

/// A listing cell wider than the plugin's `maxCellBytes` (stored before a
/// smaller cap) leaves that title out of the window; the page and the
/// titles around it are still listed.
#[tokio::test]
async fn a_listing_cell_wider_than_the_cell_cap_leaves_only_its_title_out() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let (store, caps) = capped_store(dir.path()).await;
    let riley = store
        .create_user(UserRole::Member, Some("Riley"), None)
        .await
        .unwrap()
        .id;
    hold(
        &store,
        riley,
        "riley-acct",
        &["B0CELL0001".into(), "B0CELL0002".into()],
    )
    .await;
    add_branch(&store, BRANCH, riley).await;
    let wide = "l".repeat(2000);
    raw(
        &store,
        &format!("UPDATE store_listings SET language = '{wide}' WHERE product_id = 'B0CELL0002'"),
    )
    .await;
    caps.cell.store(1024, std::sync::atomic::Ordering::SeqCst);

    let listed: Vec<String> = read_all(&store, BRANCH)
        .await
        .into_iter()
        .flat_map(|work| work.editions)
        .map(|edition| edition.edition.product_id)
        .collect();
    assert_eq!(listed, vec!["B0CELL0001"]);
}

/// A holdings read refused for its bytes is read again with fewer rows per
/// read, and every title is still listed once.
#[tokio::test]
async fn a_holdings_read_wider_than_one_reply_reads_fewer_rows() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let (store, caps) = capped_store(dir.path()).await;
    let cap = caps.reply;
    let riley = store
        .create_user(UserRole::Member, Some("Riley"), None)
        .await
        .unwrap()
        .id;
    let asins: Vec<String> = (0..60).map(|n| format!("B0MANY{n:04}")).collect();
    hold(&store, riley, "riley-acct", &asins).await;
    add_branch(&store, BRANCH, riley).await;
    let one_read: usize = find_holdings()
        .all(store.db())
        .await
        .unwrap()
        .iter()
        .map(|h| {
            h.id.len()
                + h.listing_id.len()
                + h.plugin_key.as_ref().map_or(0, String::len)
                + h.account_id.as_ref().map_or(0, String::len)
        })
        .sum();
    cap.store(one_read / 3, std::sync::atomic::Ordering::SeqCst);

    let mut listed: Vec<String> = read_all(&store, BRANCH)
        .await
        .into_iter()
        .flat_map(|work| work.editions)
        .map(|edition| edition.edition.product_id)
        .collect();
    listed.sort();
    assert_eq!(listed, asins);
}

/// A page repeating many editions stored earlier in the same read reads
/// their pieces in chunks that leave room for the owner filter and the
/// page's limit and offset, under a plugin with few binds and a large
/// result page.
#[tokio::test]
async fn many_editions_repeated_in_one_read_stay_within_the_bind_limit() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let conn = rusqlite::Connection::open(dir.path().join("library.db")).unwrap();
    let engine = TextCapped {
        inner: bookclerk_plugin_database_sqlite::sqlite::SqliteProxy::new(conn).unwrap(),
        cap: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        max_binds: 32,
        max_cell: std::sync::Arc::default(),
    };
    let db = sea_orm::Database::connect_proxy(
        sea_orm::DbBackend::Sqlite,
        std::sync::Arc::new(Box::new(engine)),
    )
    .await
    .unwrap();
    crate::apply_host_schema(&db).await.unwrap();
    crate::configure_master_key(dir.path()).unwrap();
    let mut caps = bookclerk_plugin_abi::DbCapabilities::advertised_sqlite();
    caps.max_binds = 32;
    caps.max_result_rows = 500;
    let store = LibraryStore::from_connection(db).with_db_capabilities(caps);
    link(&store, "alpha").await;
    let works: Vec<WindowWork> = (0..40)
        .map(|n| {
            let id = format!("B0REPEAT{n:02}");
            WindowWork {
                work: format!("w-{n}"),
                identifiers: vec![asin(&id)],
                editions: vec![yours(edition(&id, &format!("Book {n}")))],
            }
        })
        .collect();
    let read = window_read_stamp();
    record_window_page(&store, "alpha", &read, &works)
        .await
        .unwrap();
    record_window_page(&store, "alpha", &read, &works)
        .await
        .unwrap();
    finish_window_read(&store, "alpha", &read).await.unwrap();
    assert_eq!(window_view(&store).await.unwrap().len(), 40);
}

/// A title a later scan dropped (its holding removed, its book row kept)
/// is no longer listed to the branch.
#[tokio::test]
async fn a_title_a_scan_dropped_is_not_listed() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path()).await;
    let riley = store
        .create_user(UserRole::Member, Some("Riley"), None)
        .await
        .unwrap()
        .id;
    hold(
        &store,
        riley,
        "riley-acct",
        &["B0KEPT0001".into(), "B0GONE0001".into()],
    )
    .await;
    add_branch(&store, BRANCH, riley).await;
    let listed = |works: Vec<WindowWork>| -> Vec<String> {
        let mut products: Vec<String> = works
            .into_iter()
            .flat_map(|work| work.editions)
            .map(|edition| edition.edition.product_id)
            .collect();
        products.sort();
        products
    };
    assert_eq!(
        listed(read_all(&store, BRANCH).await),
        vec!["B0GONE0001", "B0KEPT0001"]
    );

    store
        .remove_account_holdings(
            PLUGIN,
            "riley-acct",
            crate::store::HoldingSelection::Products(&["B0GONE0001".into()]),
        )
        .await
        .unwrap();
    assert_eq!(listed(read_all(&store, BRANCH).await), vec!["B0KEPT0001"]);
}

/// Every page of `person`'s books, as a clerk reads them, checked against
/// the frame rules and budget as it goes.
async fn read_person(store: &LibraryStore, person: i64) -> Vec<WindowWork> {
    let mut after = WindowCursor::start();
    let mut works = Vec::new();
    for _ in 0..1000 {
        let (page, next) = person_books_page(store, person, &after).await.unwrap();
        assert_eq!(window_page_problem(&after, &page, next.as_ref()), None);
        assert!(serde_json::to_vec(&page).unwrap().len() <= MAX_FRAME_BYTES / 2);
        works.extend(page);
        match next {
            Some(next) => after = next,
            None => return works,
        }
    }
    panic!("paging never ended");
}

/// Drops the account holdings of `asin`, leaving its edition and work in
/// the library (as for a book the library only knows from its pool).
async fn drop_holdings_of(store: &LibraryStore, asin: &str) {
    use sea_orm::QueryFilter as _;
    let listings: Vec<String> =
        crate::test_support::uncapped(store_listings::Entity::find().all(store.db()))
            .await
            .unwrap()
            .into_iter()
            .filter(|l| l.product_id.ends_with(asin))
            .map(|l| l.id)
            .collect();
    holdings::Entity::delete_many()
        .filter(holdings::Column::ListingId.is_in(listings))
        .exec(store.db())
        .await
        .unwrap();
}

#[tokio::test]
async fn a_clerks_pages_end_with_the_books_only_branches_hold() {
    use crate::control_plane::{ask_branches, pending_questions, record_answers, AnswersRecorded};
    use bookclerk_node_proto::LookupAnswer;

    for profile in crate::test_support::Profile::ALL {
        let name = profile.name();
        let store = crate::test_support::sqlite::memory(profile)
            .await
            .into_store();
        let riley = store
            .create_user(UserRole::Member, Some("Riley"), None)
            .await
            .unwrap()
            .id;
        let branch = "branch-b";
        add_branch(&store, branch, riley).await;
        let works = hold(
            &store,
            riley,
            "riley-acct",
            &["B0HELD0001".into(), "B0BRANCH01".into()],
        )
        .await;
        drop_holdings_of(&store, "B0BRANCH01").await;

        // Before any branch lists anything, the pages end with the library's
        // own books.
        let own = read_person(&store, riley).await;
        assert_eq!(own.len(), 1, "{name}");
        assert_eq!(own[0].work, works[0], "{name}");

        // The branch lists both: the one the library holds stays its own,
        // the other is offered as only a branch holds it.
        ask_branches(&store, &works).await.unwrap();
        pending_questions(&store, branch).await.unwrap().unwrap();
        let answer = |work: &str, asin: &str| LookupAnswer {
            work: work.into(),
            editions: vec![LookupEdition {
                store: "audible".into(),
                marketplace: "us".into(),
                product_id: asin.into(),
                availability: Availability::Owned,
                formats: vec![LookupFormat::Audio],
                metadata: CatalogMetadata {
                    title: format!("Branch {asin}"),
                    authors: Some("Ann Author".into()),
                    asin: Some(asin.into()),
                    ..CatalogMetadata::default()
                },
            }],
        };
        let answers = [
            answer(&works[0], "B0HELD0001"),
            answer(&works[1], "B0BRANCH01"),
        ];
        match record_answers(&store, branch, &answers, &[]).await.unwrap() {
            AnswersRecorded::Recorded(report) => assert_eq!(report.listings, 2, "{name}"),
            other => panic!("{name}: expected the answers recorded: {other:?}"),
        }
        // A second branch lists the same book: it still goes on the page once.
        let other = "branch-c";
        add_branch(&store, other, riley).await;
        ask_branches(&store, &works).await.unwrap();
        pending_questions(&store, other).await.unwrap().unwrap();
        match record_answers(&store, other, &answers[1..], &[])
            .await
            .unwrap()
        {
            AnswersRecorded::Recorded(report) => assert_eq!(report.listings, 1, "{name}"),
            other => panic!("{name}: expected the answer recorded: {other:?}"),
        }

        let all = read_person(&store, riley).await;
        let editions: Vec<(&str, &str, Availability, bool, bool)> = all
            .iter()
            .flat_map(|w| {
                w.editions.iter().map(move |e| {
                    (
                        w.work.as_str(),
                        e.edition.product_id.as_str(),
                        e.edition.availability,
                        e.yours,
                        e.edition.formats.is_empty(),
                    )
                })
            })
            .collect();
        assert_eq!(
            editions,
            vec![
                (
                    works[0].as_str(),
                    "B0HELD0001",
                    Availability::Owned,
                    true,
                    true
                ),
                (
                    works[1].as_str(),
                    "B0BRANCH01",
                    Availability::Reachable,
                    false,
                    true
                ),
            ],
            "{name}"
        );
        let reachable = &all[1].editions[0].edition;
        assert_eq!(reachable.metadata.title, "Branch B0BRANCH01", "{name}");
        assert_eq!(reachable.metadata.authors.as_deref(), Some("Ann Author"));
        // The page names no branch.
        let text = serde_json::to_string(&all).unwrap();
        assert!(!text.contains(branch), "{name}: {text}");
        assert!(!text.contains(other), "{name}: {text}");

        // A branch reading the same library sees only the library's own.
        let as_branch = read_all(&store, branch).await;
        assert!(
            as_branch
                .iter()
                .flat_map(|w| &w.editions)
                .all(|e| e.edition.availability == Availability::Owned),
            "{name}"
        );

        // One edition at a time still pages through every branch book.
        let mut after = WindowCursor::start();
        let mut seen = Vec::new();
        for _ in 0..10 {
            let (page, next) = person_books_page_by(&store, riley, &after, 1, BranchBooks::Include)
                .await
                .unwrap();
            assert_eq!(window_page_problem(&after, &page, next.as_ref()), None);
            seen.extend(page.into_iter().map(|w| w.work));
            match next {
                Some(next) => after = next,
                None => break,
            }
        }
        assert_eq!(seen, works, "{name}");

        // A branch no longer trusted takes its books off the pages.
        node_credentials::Entity::update_many()
            .col_expr(
                node_credentials::Column::RevokedAt,
                sea_orm::sea_query::Expr::value(Utc::now().to_rfc3339()),
            )
            .exec(store.db())
            .await
            .unwrap();
        let after_revoke = read_person(&store, riley).await;
        assert_eq!(after_revoke.len(), 1, "{name}");
    }
}

#[tokio::test]
async fn the_window_keeps_a_book_only_a_branch_holds_with_its_work() {
    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let reader = store_in(dir.path()).await;
    link(&reader, "lib").await;
    let mut listed = edition("B0BRANCH01", "Branch Book");
    listed.availability = Availability::Reachable;
    listed.formats.clear();
    let work = |work: &str, edition: WindowEdition| WindowWork {
        work: work.into(),
        identifiers: vec![asin(&edition.edition.product_id)],
        editions: vec![edition],
    };
    let reachable = WindowEdition {
        edition: listed.clone(),
        yours: false,
        copy: None,
    };
    let stamp = window_read_stamp();
    record_window_page(&reader, "lib", &stamp, &[work("work-7", reachable.clone())])
        .await
        .unwrap();
    finish_window_read(&reader, "lib", &stamp).await.unwrap();
    let view = window_view(&reader).await.unwrap();
    assert_eq!(view.len(), 1);
    let held = &view[0].editions[0];
    assert_eq!(held.edition.availability, Availability::Reachable);
    assert_eq!(held.work_ref, "work-7");

    // Held on an account on another page of the same read wins.
    let stamp = window_read_stamp();
    let mut owned = listed.clone();
    owned.availability = Availability::Owned;
    record_window_page(
        &reader,
        "lib",
        &stamp,
        &[work(
            "work-7",
            WindowEdition {
                edition: owned,
                yours: false,
                copy: None,
            },
        )],
    )
    .await
    .unwrap();
    record_window_page(&reader, "lib", &stamp, &[work("work-7", reachable.clone())])
        .await
        .unwrap();
    finish_window_read(&reader, "lib", &stamp).await.unwrap();
    let view = window_view(&reader).await.unwrap();
    assert_eq!(
        view[0].editions[0].edition.availability,
        Availability::Owned
    );

    // So it does when both reports sit on one page, under two works: the
    // later branch report keeps the held one's formats and copy.
    let stamp = window_read_stamp();
    let mut owned = listed;
    owned.availability = Availability::Owned;
    owned.formats = vec![LookupFormat::Audio];
    let held = WindowEdition {
        edition: owned,
        yours: true,
        copy: None,
    };
    record_window_page(
        &reader,
        "lib",
        &stamp,
        &[work("work-7", held), work("work-8", reachable)],
    )
    .await
    .unwrap();
    finish_window_read(&reader, "lib", &stamp).await.unwrap();
    let view = window_view(&reader).await.unwrap();
    let kept = &view[0].editions[0];
    assert_eq!(kept.edition.availability, Availability::Owned);
    assert_eq!(kept.edition.formats, vec![LookupFormat::Audio]);
    assert!(kept.yours);
}

/// A branch listing wider than the plugin's `maxCellBytes` (stored before
/// a smaller cap) leaves only that edition out of a clerk's branch pages:
/// the listing ids are read without their metadata, the rows in chunks,
/// and paging moves past the edition whose row could not be read.
#[tokio::test]
async fn a_branch_listing_wider_than_the_cell_cap_leaves_only_its_edition_out() {
    use crate::control_plane::{ask_branches, pending_questions, record_answers, AnswersRecorded};
    use bookclerk_node_proto::LookupAnswer;

    let _lock = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let (store, caps) = capped_store(dir.path()).await;
    let riley = store
        .create_user(UserRole::Member, Some("Riley"), None)
        .await
        .unwrap()
        .id;
    let branch = "branch-b";
    add_branch(&store, branch, riley).await;
    let works = hold(
        &store,
        riley,
        "riley-acct",
        &["B0NARROW01".into(), "B0WIDE0001".into()],
    )
    .await;
    drop_holdings_of(&store, "B0NARROW01").await;
    drop_holdings_of(&store, "B0WIDE0001").await;
    ask_branches(&store, &works).await.unwrap();
    pending_questions(&store, branch).await.unwrap().unwrap();
    let answers: Vec<LookupAnswer> = works
        .iter()
        .zip(["B0NARROW01", "B0WIDE0001"])
        .map(|(work, asin)| LookupAnswer {
            work: work.clone(),
            editions: vec![LookupEdition {
                store: "audible".into(),
                marketplace: "us".into(),
                product_id: asin.into(),
                availability: Availability::Owned,
                formats: vec![LookupFormat::Audio],
                metadata: CatalogMetadata {
                    title: format!("Branch {asin}"),
                    asin: Some(asin.into()),
                    ..CatalogMetadata::default()
                },
            }],
        })
        .collect();
    match record_answers(&store, branch, &answers, &[]).await.unwrap() {
        AnswersRecorded::Recorded(report) => assert_eq!(report.listings, 2),
        other => panic!("expected the answers recorded: {other:?}"),
    }
    let wide = "l".repeat(2000);
    raw(
        &store,
        &format!(
            "UPDATE store_listings SET language = '{wide}' \
             WHERE plugin_key LIKE 'branch:%' AND product_id LIKE '%B0WIDE0001'"
        ),
    )
    .await;
    caps.cell.store(1024, std::sync::atomic::Ordering::SeqCst);

    let listed: Vec<String> = read_person(&store, riley)
        .await
        .into_iter()
        .flat_map(|work| work.editions)
        .map(|edition| edition.edition.product_id)
        .collect();
    assert_eq!(listed, vec!["B0NARROW01"]);

    // One edition per read: the page that only skips the wide listing
    // still moves the cursor past it.
    let mut after = WindowCursor::start();
    let mut pages = 0;
    loop {
        let (_, next) = person_books_page_by(&store, riley, &after, 1, BranchBooks::Include)
            .await
            .unwrap();
        pages += 1;
        assert!(pages < 10, "paging never ended");
        match next {
            Some(next) => after = next,
            None => break,
        }
    }
}
