//! Page index tests against a store under the D1-shaped kit.

use bookclerk_resolve::{MediaKind, WorkKind};

use super::{WorkCursor, WorkPageQuery, WorkSort};
use crate::models::AcquireStatus;
use crate::store::holdings::HoldingSelection;
use crate::store::{LibraryStore, NewBook};

/// A store under the kit.
async fn store() -> LibraryStore {
    let store = crate::test_support::sqlite::store().await;
    for account in ["acct-1", "acct-2"] {
        store
            .upsert_account("audible", account, "us", None, true, "audible")
            .await
            .unwrap();
    }
    store
}

/// Scans `title` (product `asin`) onto `account` with `author`; returns
/// the work id.
async fn scan(
    store: &LibraryStore,
    asin: &str,
    account: &str,
    title: &str,
    author: &str,
) -> String {
    let mut book = NewBook::minimal(asin, account, "us", title);
    book.authors = Some(author.into());
    let saved = store.upsert_book(&book).await.unwrap();
    store
        .work_id_for_book(&saved.uuid)
        .await
        .unwrap()
        .expect("a scanned title has a work")
}

/// Every row of `query`, page by page, as work titles.
async fn walk(store: &LibraryStore, mut query: WorkPageQuery) -> Vec<String> {
    let mut titles = Vec::new();
    loop {
        let page = store.library_works_page(&query).await.unwrap();
        titles.extend(page.works.into_iter().map(|w| w.title));
        match page.next {
            Some(next) => query.after = Some(next),
            None => return titles,
        }
    }
}

#[tokio::test]
async fn a_refresh_writes_one_row_per_held_work_and_pages_by_title() {
    let store = store().await;
    let hobbit = scan(&store, "B001", "acct-1", "The Hobbit", "J. R. R. Tolkien").await;
    let again = scan(&store, "B001", "acct-2", "The Hobbit", "J. R. R. Tolkien").await;
    assert_eq!(hobbit, again, "one product on two accounts is one work");
    let dune = scan(&store, "B002", "acct-1", "Dune", "Frank Herbert").await;
    let anathem = scan(&store, "B003", "acct-2", "Anathem", "Neal Stephenson").await;
    let changed = store
        .refresh_library_works(&[hobbit.clone(), dune, anathem])
        .await
        .unwrap();
    assert_eq!(changed, 3);

    let first = store
        .library_works_page(&WorkPageQuery {
            limit: 2,
            ..WorkPageQuery::default()
        })
        .await
        .unwrap();
    let titles: Vec<&str> = first.works.iter().map(|w| w.title.as_str()).collect();
    assert_eq!(titles, ["Anathem", "Dune"]);
    let row = &store
        .library_works_page(&WorkPageQuery {
            limit: 10,
            ..WorkPageQuery::default()
        })
        .await
        .unwrap()
        .works[2];
    assert_eq!(row.work_id, hobbit);
    assert_eq!(row.kind, WorkKind::Book);
    assert_eq!(row.media_kinds, vec![MediaKind::Audio]);
    assert_eq!(row.holding_count, 2);
    assert_eq!(row.acquire_status, AcquireStatus::NotAcquired);
    assert!(
        row.authors
            .as_deref()
            .is_some_and(|a| a.contains("Tolkien")),
        "{row:?}"
    );

    let all = walk(
        &store,
        WorkPageQuery {
            limit: 1,
            ..WorkPageQuery::default()
        },
    )
    .await;
    assert_eq!(
        all,
        ["Anathem", "Dune", "The Hobbit"],
        "the hobbit sorts under h"
    );
    let newest = walk(
        &store,
        WorkPageQuery {
            sort: WorkSort::Added,
            limit: 2,
            ..WorkPageQuery::default()
        },
    )
    .await;
    assert_eq!(newest.len(), 3);
    assert_eq!(newest[0], "Anathem", "the newest holding comes first");
    let by_author = walk(
        &store,
        WorkPageQuery {
            sort: WorkSort::Author,
            limit: 1,
            ..WorkPageQuery::default()
        },
    )
    .await;
    assert_eq!(by_author, ["Dune", "Anathem", "The Hobbit"]);
}

#[tokio::test]
async fn filters_narrow_the_page() {
    let store = store().await;
    let a = scan(&store, "B010", "acct-1", "Alpha", "A. Author").await;
    let b = scan(&store, "B011", "acct-2", "Beta", "B. Author").await;
    store.refresh_library_works(&[a, b]).await.unwrap();
    let held_by = |account: &str| WorkPageQuery {
        account: Some(account.into()),
        limit: 10,
        ..WorkPageQuery::default()
    };
    assert_eq!(walk(&store, held_by("acct-1")).await, ["Alpha"]);
    assert_eq!(walk(&store, held_by("acct-2")).await, ["Beta"]);
    assert!(walk(&store, held_by("nobody")).await.is_empty());
    let audio = WorkPageQuery {
        media_kind: Some(MediaKind::Audio),
        limit: 10,
        ..WorkPageQuery::default()
    };
    assert_eq!(walk(&store, audio).await.len(), 2);
    let ebooks = WorkPageQuery {
        media_kind: Some(MediaKind::Ebook),
        limit: 10,
        ..WorkPageQuery::default()
    };
    assert!(walk(&store, ebooks).await.is_empty());
    let podcasts = WorkPageQuery {
        kind: Some(WorkKind::Podcast),
        limit: 10,
        ..WorkPageQuery::default()
    };
    assert!(walk(&store, podcasts).await.is_empty());
    let acquired = WorkPageQuery {
        status: Some(AcquireStatus::Acquired),
        limit: 10,
        ..WorkPageQuery::default()
    };
    assert!(walk(&store, acquired).await.is_empty());
}

#[tokio::test]
async fn a_cursor_of_another_order_is_refused() {
    let store = store().await;
    let err = store
        .library_works_page(&WorkPageQuery {
            sort: WorkSort::Added,
            after: Some(WorkCursor {
                sort: WorkSort::Title,
                key: "a".into(),
                title: None,
                work_id: "w".into(),
            }),
            limit: 10,
            ..WorkPageQuery::default()
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("cursor"), "{err}");
}

#[tokio::test]
async fn a_work_that_loses_its_last_holding_leaves_the_list() {
    let store = store().await;
    let work = scan(&store, "B020", "acct-1", "Gone Soon", "C. Author").await;
    store
        .refresh_library_works(std::slice::from_ref(&work))
        .await
        .unwrap();
    assert_eq!(
        store
            .library_works_page(&WorkPageQuery {
                limit: 5,
                ..WorkPageQuery::default()
            })
            .await
            .unwrap()
            .works
            .len(),
        1
    );
    store
        .remove_account_holdings("audible", "acct-1", HoldingSelection::All)
        .await
        .unwrap();
    assert_eq!(
        store
            .refresh_library_works(std::slice::from_ref(&work))
            .await
            .unwrap(),
        1
    );
    assert!(store
        .library_works_page(&WorkPageQuery {
            limit: 5,
            ..WorkPageQuery::default()
        })
        .await
        .unwrap()
        .works
        .is_empty());
    assert_eq!(
        store
            .refresh_library_works(&["no-such-work".into()])
            .await
            .unwrap(),
        0,
        "an unknown work has no row to delete"
    );
}

#[tokio::test]
async fn an_acquire_moves_the_works_status_and_a_rebuild_walks_every_work() {
    let store = store().await;
    let mut book = NewBook::minimal("B030", "acct-1", "us", "Rebuilt");
    book.authors = Some("D. Author".into());
    let saved = store.upsert_book(&book).await.unwrap();
    let other = scan(&store, "B031", "acct-1", "Also Rebuilt", "E. Author").await;
    store
        .set_acquire_status(
            &saved.uuid,
            "acct-1",
            AcquireStatus::Acquired,
            Some("k.m4b"),
            None,
        )
        .await
        .unwrap();
    assert!(!store.library_index_built().await.unwrap());
    let mut after = None;
    while let Some(last) = store
        .rebuild_library_index_page(after.as_deref())
        .await
        .unwrap()
    {
        after = Some(last);
    }
    store.finish_library_index_build().await.unwrap();
    assert!(store.library_index_built().await.unwrap());
    store.finish_library_index_build().await.unwrap();
    let page = store
        .library_works_page(&WorkPageQuery {
            status: Some(AcquireStatus::Acquired),
            limit: 10,
            ..WorkPageQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(page.works.len(), 1);
    assert_eq!(page.works[0].title, "Rebuilt");
    let all = walk(
        &store,
        WorkPageQuery {
            limit: 10,
            ..WorkPageQuery::default()
        },
    )
    .await;
    assert_eq!(all, ["Also Rebuilt", "Rebuilt"]);
    assert_eq!(
        store.work_of_listing("no-such-listing").await.unwrap(),
        None
    );
    assert_eq!(
        store.work_of_edition("no-such-edition").await.unwrap(),
        None
    );
    let edition = store.edition_of_book(&saved.uuid).await.unwrap().unwrap();
    let work = store.work_of_edition(&edition).await.unwrap().unwrap();
    assert_ne!(work, other);
}

#[tokio::test]
async fn a_work_detail_lists_its_editions_listings_and_holdings() {
    let store = store().await;
    let hobbit = scan(&store, "B001", "acct-1", "The Hobbit", "J. R. R. Tolkien").await;
    scan(&store, "B001", "acct-2", "The Hobbit", "J. R. R. Tolkien").await;

    let detail = store
        .library_work_detail(&hobbit)
        .await
        .unwrap()
        .expect("the work has a detail");
    assert_eq!(detail.title, "The Hobbit");
    assert_eq!(detail.kind, WorkKind::Book);
    assert_eq!(detail.authors, ["J. R. R. Tolkien"]);
    assert_eq!(detail.editions.len(), 1);
    let edition = &detail.editions[0];
    assert_eq!(edition.work_id, hobbit);
    assert_eq!(edition.media_kind, MediaKind::Audio);
    assert_eq!(edition.acquire_status, AcquireStatus::NotAcquired);
    assert_eq!(edition.listings.len(), 1);
    let listing = &edition.listings[0];
    assert_eq!(listing.plugin_key, "audible");
    assert_eq!(listing.product_id, "B001");
    let mut accounts: Vec<String> = listing
        .holdings
        .iter()
        .map(|h| match &h.holder {
            super::super::work_detail::HoldingHolder::Account { account_id, .. } => {
                account_id.clone()
            }
            super::super::work_detail::HoldingHolder::Node { node_id } => node_id.clone(),
        })
        .collect();
    accounts.sort();
    assert_eq!(accounts, ["acct-1", "acct-2"]);
    assert!(
        store
            .edition_cover_urls(std::slice::from_ref(&edition.id))
            .await
            .unwrap()
            .is_empty(),
        "a scanned edition without a cover has no cover URL"
    );

    assert!(store
        .library_work_detail("no-such-work")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn a_users_state_for_one_work_reads_back() {
    let store = store().await;
    let hobbit = scan(&store, "B001", "acct-1", "The Hobbit", "J. R. R. Tolkien").await;
    let user = store
        .create_user(crate::models::UserRole::Member, Some("Reader"), None)
        .await
        .unwrap();
    assert!(store
        .user_title_state(user.id, &hobbit)
        .await
        .unwrap()
        .is_none());
    store
        .set_user_title_state(
            user.id,
            &hobbit,
            &crate::store::UserBookFields {
                is_finished: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let state = store
        .user_title_state(user.id, &hobbit)
        .await
        .unwrap()
        .expect("the state was written");
    assert!(state.is_finished);
}

#[tokio::test]
async fn a_stale_refresh_cannot_bring_back_a_work_that_lost_its_holdings() {
    let store = store().await;
    let work = scan(&store, "B030", "acct-1", "Briefly Held", "D. Author").await;
    // A refresh reads the work while it is still held, then stalls.
    let stale_stamp = "2000-01-01T00:00:00.000000Z";
    let stale = super::build_work_row(&store.work_row_inputs(&work).await.unwrap(), stale_stamp)
        .expect("a held work has a row");
    assert_eq!(stale.holding_count, 1);
    // Meanwhile the holding goes and a later refresh records it.
    store
        .remove_account_holdings("audible", "acct-1", HoldingSelection::All)
        .await
        .unwrap();
    store
        .refresh_library_works(std::slice::from_ref(&work))
        .await
        .unwrap();
    // The stalled refresh now writes: the fence row refuses it.
    assert_eq!(
        store
            .write_work_row(&work, Some(stale), stale_stamp)
            .await
            .unwrap(),
        0
    );
    assert!(walk(
        &store,
        WorkPageQuery {
            limit: 5,
            ..WorkPageQuery::default()
        }
    )
    .await
    .is_empty());
}
