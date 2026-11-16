//! Page index tests against a store under the kit; the paging and
//! rebuild tests run under every database profile.

use bookclerk_resolve::{MediaKind, WorkKind};

use super::{FacetValuesOf, WorkCursor, WorkFacet, WorkFilter, WorkPageQuery, WorkSort};
use crate::models::AcquireStatus;
use crate::store::holdings::HoldingSelection;
use crate::store::{LibraryStore, NewBook};
use crate::test_support::Profile;

/// A store under the kit.
async fn store() -> LibraryStore {
    store_under(Profile::D1).await
}

/// A store under the kit with `profile`'s capability advertisement, holding
/// the two test accounts.
async fn store_under(profile: Profile) -> LibraryStore {
    let store = crate::test_support::sqlite::memory(profile)
        .await
        .into_store();
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

/// Every row of `query`, page by page.
async fn walk_rows(store: &LibraryStore, mut query: WorkPageQuery) -> Vec<super::LibraryWorkRow> {
    let mut rows = Vec::new();
    loop {
        let page = store.library_works_page(&query).await.unwrap();
        rows.extend(page.works);
        match page.next {
            Some(next) => query.after = Some(next),
            None => return rows,
        }
    }
}

/// Every row of `query`, page by page, as work titles.
async fn walk(store: &LibraryStore, query: WorkPageQuery) -> Vec<String> {
    walk_rows(store, query)
        .await
        .into_iter()
        .map(|w| w.title)
        .collect()
}

#[tokio::test]
async fn a_refresh_writes_one_row_per_held_work_and_pages_by_title() {
    for profile in Profile::ALL {
        a_refresh_writes_one_row_per_held_work_and_pages_by_title_under(profile).await;
    }
}

/// [`a_refresh_writes_one_row_per_held_work_and_pages_by_title`] under one profile.
async fn a_refresh_writes_one_row_per_held_work_and_pages_by_title_under(profile: Profile) {
    let store = store_under(profile).await;
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
    // A database that caps result rows below the asked limit serves a
    // shorter page, still in order and still with a next cursor.
    assert!(
        !titles.is_empty() && ["Anathem", "Dune"].starts_with(&titles),
        "{profile:?}: {titles:?}"
    );
    assert!(first.next.is_some(), "{profile:?}");
    let rows = walk_rows(
        &store,
        WorkPageQuery {
            limit: 10,
            ..WorkPageQuery::default()
        },
    )
    .await;
    let row = &rows[2];
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
    for profile in Profile::ALL {
        an_acquire_moves_the_works_status_and_a_rebuild_walks_every_work_under(profile).await;
    }
}

/// [`an_acquire_moves_the_works_status_and_a_rebuild_walks_every_work`] under one profile.
async fn an_acquire_moves_the_works_status_and_a_rebuild_walks_every_work_under(profile: Profile) {
    let store = store_under(profile).await;
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
            .write_work_rows(&[(work.clone(), Some(stale), Vec::new())], stale_stamp)
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

#[tokio::test]
async fn rows_by_id_keep_the_given_order_and_apply_the_filters() {
    let store = store().await;
    let hobbit = scan(&store, "B040", "acct-1", "The Hobbit", "J. R. R. Tolkien").await;
    let dune = scan(&store, "B041", "acct-2", "Dune", "Frank Herbert").await;
    store
        .refresh_library_works(&[hobbit.clone(), dune.clone()])
        .await
        .unwrap();
    let ids = vec![dune.clone(), "no-such-work".into(), hobbit.clone()];
    let all = store
        .library_works_by_ids(&ids, &WorkPageQuery::default())
        .await
        .unwrap();
    let order: Vec<&str> = all.iter().map(|w| w.work_id.as_str()).collect();
    assert_eq!(order, [dune.as_str(), hobbit.as_str()]);
    let narrowed = store
        .library_works_by_ids(
            &ids,
            &WorkPageQuery {
                account: Some("acct-1".into()),
                ..WorkPageQuery::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(narrowed.len(), 1);
    assert_eq!(narrowed[0].work_id, hobbit);
    assert_eq!(
        store.top_level_work_ids_page(None, 1).await.unwrap().len(),
        1
    );
}

/// What [`scan_full`] scans besides the title and author.
#[derive(Default)]
struct Extras {
    /// Narrator text.
    narrators: Option<&'static str>,
    /// Series name and position.
    series: Option<(&'static str, &'static str)>,
    /// Publisher.
    publisher: Option<&'static str>,
    /// Category text.
    categories: Option<&'static str>,
    /// Runtime in minutes.
    length: Option<i64>,
    /// Purchase time (RFC 3339).
    purchased: Option<&'static str>,
}

/// Scans `title` (product `asin`) onto `acct-1` with `author` and
/// `extras`; returns the work id.
async fn scan_full(
    store: &LibraryStore,
    asin: &str,
    title: &str,
    author: &str,
    extras: Extras,
) -> String {
    let mut book = NewBook::minimal(asin, "acct-1", "us", title);
    book.authors = Some(author.into());
    book.narrators = extras.narrators.map(Into::into);
    if let Some((name, position)) = extras.series {
        book.series = Some(name.into());
        book.series_index = Some(position.into());
    }
    book.publisher = extras.publisher.map(Into::into);
    book.categories = extras.categories.map(Into::into);
    book.length_minutes = extras.length;
    book.purchased_at = extras.purchased.map(|at| {
        chrono::DateTime::parse_from_rfc3339(at)
            .unwrap()
            .with_timezone(&chrono::Utc)
    });
    let saved = store.upsert_book(&book).await.unwrap();
    store
        .work_id_for_book(&saved.uuid)
        .await
        .unwrap()
        .expect("a scanned title has a work")
}

/// Four works with series, narrators, publishers, genres, runtimes and
/// purchase times, refreshed into the index.
async fn faceted_library(store: &LibraryStore) -> Vec<String> {
    let ids = vec![
        scan_full(
            store,
            "B101",
            "Dune Messiah",
            "Frank Herbert",
            Extras {
                narrators: Some("Scott Brick, Katherine Kellgren"),
                series: Some(("Dune", "2")),
                publisher: Some("Macmillan Audio"),
                categories: Some("Science Fiction; Classics"),
                length: Some(530),
                purchased: Some("2024-05-01T00:00:00Z"),
            },
        )
        .await,
        scan_full(
            store,
            "B102",
            "Dune",
            "Frank Herbert",
            Extras {
                narrators: Some("Scott Brick"),
                series: Some(("Dune", "1")),
                publisher: Some("Macmillan Audio"),
                categories: Some("science fiction"),
                length: Some(1260),
                purchased: Some("2023-01-01T00:00:00Z"),
            },
        )
        .await,
        scan_full(
            store,
            "B103",
            "Children of Dune",
            "Frank Herbert",
            Extras {
                series: Some(("Dune", "10")),
                categories: Some("Science Fiction"),
                ..Extras::default()
            },
        )
        .await,
        scan_full(
            store,
            "B104",
            "Piranesi",
            "Susanna Clarke",
            Extras {
                narrators: Some("Chiwetel Ejiofor"),
                publisher: Some("Bloomsbury"),
                categories: Some("Fantasy"),
                length: Some(500),
                ..Extras::default()
            },
        )
        .await,
    ];
    store.refresh_library_works(&ids).await.unwrap();
    ids
}

/// Titles of every row of `query` with `filter`, by title.
async fn filtered(store: &LibraryStore, filter: WorkFilter) -> Vec<String> {
    walk(
        store,
        WorkPageQuery {
            filter: Some(filter),
            limit: 10,
            ..WorkPageQuery::default()
        },
    )
    .await
}

/// A facet filter.
fn facet(facet: WorkFacet, value: &str) -> WorkFilter {
    WorkFilter::Facet {
        facet,
        value: value.into(),
    }
}

#[tokio::test]
async fn facets_filter_works_and_list_their_values() {
    for profile in Profile::ALL {
        facets_filter_works_and_list_their_values_under(profile).await;
    }
}

/// [`facets_filter_works_and_list_their_values`] under one profile.
async fn facets_filter_works_and_list_their_values_under(profile: Profile) {
    let store = store_under(profile).await;
    faceted_library(&store).await;
    assert_eq!(
        filtered(&store, facet(WorkFacet::Narrator, "scott brick")).await,
        ["Dune", "Dune Messiah"],
        "{profile:?}"
    );
    assert_eq!(
        filtered(&store, facet(WorkFacet::Genre, "Science Fiction")).await,
        ["Children of Dune", "Dune", "Dune Messiah"],
        "{profile:?}"
    );
    assert_eq!(
        filtered(&store, facet(WorkFacet::Genre, "classics")).await,
        ["Dune Messiah"],
        "{profile:?}"
    );
    assert_eq!(
        filtered(&store, facet(WorkFacet::Publisher, "Bloomsbury")).await,
        ["Piranesi"],
        "{profile:?}"
    );
    assert_eq!(
        filtered(&store, facet(WorkFacet::Author, "FRANK HERBERT")).await,
        ["Children of Dune", "Dune", "Dune Messiah"],
        "{profile:?}"
    );
    assert_eq!(
        filtered(&store, facet(WorkFacet::Series, "dune")).await,
        ["Children of Dune", "Dune", "Dune Messiah"],
        "{profile:?}"
    );
    assert_eq!(
        filtered(&store, facet(WorkFacet::Series, "")).await,
        ["Piranesi"],
        "{profile:?}: an empty series value is works in no series"
    );
    assert_eq!(
        filtered(&store, facet(WorkFacet::Source, "audible"))
            .await
            .len(),
        4,
        "{profile:?}"
    );

    // Values page by key; a database capped at one row a read still walks
    // them all.
    let mut values = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = store
            .library_facet_values(FacetValuesOf::Facet(WorkFacet::Genre), after.as_deref(), 2)
            .await
            .unwrap();
        values.extend(page.values);
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    let listed: Vec<(&str, u64)> = values.iter().map(|v| (v.key.as_str(), v.works)).collect();
    assert_eq!(
        listed,
        [("classics", 1), ("fantasy", 1), ("science fiction", 3)],
        "{profile:?}"
    );
}

#[tokio::test]
async fn series_purchase_and_length_orders_page_by_cursor() {
    for profile in Profile::ALL {
        series_purchase_and_length_orders_page_by_cursor_under(profile).await;
    }
}

/// [`series_purchase_and_length_orders_page_by_cursor`] under one profile.
async fn series_purchase_and_length_orders_page_by_cursor_under(profile: Profile) {
    let store = store_under(profile).await;
    faceted_library(&store).await;
    let by = |sort| WorkPageQuery {
        sort,
        limit: 1,
        ..WorkPageQuery::default()
    };
    assert_eq!(
        walk(&store, by(WorkSort::Series)).await,
        ["Dune", "Dune Messiah", "Children of Dune", "Piranesi"],
        "{profile:?}: by position, 10 after 2, no series last"
    );
    let purchased = walk_rows(&store, by(WorkSort::Purchased)).await;
    assert_eq!(purchased[0].title, "Dune Messiah", "{profile:?}");
    assert_eq!(purchased[1].title, "Dune", "{profile:?}");
    assert!(
        purchased[2..].iter().all(|w| w.purchased_at.is_none()),
        "{profile:?}: works with no purchase time last"
    );
    assert_eq!(
        walk(&store, by(WorkSort::Length)).await[..3],
        ["Dune", "Dune Messiah", "Piranesi"],
        "{profile:?}: longest first, unknown last"
    );
    let length = walk_rows(&store, by(WorkSort::Length)).await;
    assert_eq!(length[0].length_minutes, Some(1260));
    assert_eq!(length[3].length_minutes, None);
}

#[tokio::test]
async fn tags_and_progress_filter_by_the_users_own_state() {
    for profile in Profile::ALL {
        tags_and_progress_filter_by_the_users_own_state_under(profile).await;
    }
}

/// [`tags_and_progress_filter_by_the_users_own_state`] under one profile.
async fn tags_and_progress_filter_by_the_users_own_state_under(profile: Profile) {
    use crate::models::UserRole;
    use crate::store::UserBookFields;
    let store = store_under(profile).await;
    let ids = faceted_library(&store).await;
    let (messiah, piranesi) = (&ids[0], &ids[3]);
    let ana = store
        .create_user(UserRole::Owner, Some("Ana"), None)
        .await
        .unwrap();
    let ben = store
        .create_user(UserRole::Member, Some("Ben"), None)
        .await
        .unwrap();
    let tag = |tags: &str| UserBookFields {
        tags: Some(tags.into()),
        ..UserBookFields::default()
    };
    store
        .set_user_title_state(ana.id, messiah, &tag("Reread  favourite reread"))
        .await
        .unwrap();
    store
        .set_user_title_state(ana.id, piranesi, &tag("favourite"))
        .await
        .unwrap();
    store
        .set_user_title_state(
            ana.id,
            piranesi,
            &UserBookFields {
                is_finished: Some(true),
                ..UserBookFields::default()
            },
        )
        .await
        .unwrap();
    store
        .set_user_title_state(ben.id, messiah, &tag("dnf"))
        .await
        .unwrap();

    let tagged = |user_id, tag: &str| WorkFilter::Tag {
        user_id,
        tag: tag.into(),
    };
    assert_eq!(
        filtered(&store, tagged(Some(ana.id), "FAVOURITE")).await,
        ["Dune Messiah", "Piranesi"],
        "{profile:?}: a finished flag write keeps the tags"
    );
    assert_eq!(
        filtered(&store, tagged(Some(ben.id), "favourite")).await,
        Vec::<String>::new(),
        "{profile:?}: tags are per person"
    );
    assert!(filtered(&store, tagged(None, "favourite")).await.is_empty());

    // Retagging replaces the work's tags.
    store
        .set_user_title_state(ana.id, messiah, &tag("reread"))
        .await
        .unwrap();
    assert_eq!(
        filtered(&store, tagged(Some(ana.id), "favourite")).await,
        ["Piranesi"],
        "{profile:?}"
    );
    let mut tags = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = store
            .library_facet_values(FacetValuesOf::Tags(ana.id), after.as_deref(), 10)
            .await
            .unwrap();
        tags.extend(page.values);
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    let listed: Vec<(&str, &str, u64)> = tags
        .iter()
        .map(|v| (v.key.as_str(), v.value.as_str(), v.works))
        .collect();
    assert_eq!(
        listed,
        [("favourite", "favourite", 1), ("reread", "reread", 1)],
        "{profile:?}"
    );

    let finished = |user_id, finished| WorkFilter::Finished { user_id, finished };
    assert_eq!(
        filtered(&store, finished(Some(ana.id), true)).await,
        ["Piranesi"],
        "{profile:?}"
    );
    assert_eq!(
        filtered(&store, finished(Some(ana.id), false)).await,
        ["Children of Dune", "Dune", "Dune Messiah"],
        "{profile:?}"
    );
    assert!(filtered(&store, finished(None, true)).await.is_empty());
    assert_eq!(filtered(&store, finished(None, false)).await.len(), 4);
}

#[tokio::test]
async fn one_refresh_of_many_works_writes_each_row_and_its_values() {
    for profile in Profile::ALL {
        one_refresh_of_many_works_writes_each_row_and_its_values_under(profile).await;
    }
}

/// [`one_refresh_of_many_works_writes_each_row_and_its_values`] under one
/// profile: the rows and values of many works share batches, cut to the
/// profile's limits, and each row is still counted once and fenced alone.
async fn one_refresh_of_many_works_writes_each_row_and_its_values_under(profile: Profile) {
    let store = store_under(profile).await;
    let mut works = Vec::new();
    for i in 0..40 {
        let mut book = NewBook::minimal(format!("M{i:03}"), "acct-1", "us", format!("Many {i:03}"));
        book.authors = Some(format!("Author {i:03}, Shared Author"));
        book.narrators = Some(format!("Narrator {i:03}"));
        book.series = Some(format!("Series {}", i % 4));
        book.series_index = Some(format!("{i}"));
        let saved = store.upsert_book(&book).await.unwrap();
        works.push(
            store
                .work_id_for_book(&saved.uuid)
                .await
                .unwrap()
                .expect("a scanned title has a work"),
        );
    }
    works.push("no-such-work".to_string());
    assert_eq!(
        store.refresh_library_works(&works).await.unwrap(),
        40,
        "{profile:?}: every held work's row is written once"
    );
    let rows = walk_rows(
        &store,
        WorkPageQuery {
            limit: 7,
            ..WorkPageQuery::default()
        },
    )
    .await;
    assert_eq!(rows.len(), 40, "{profile:?}");
    let first = &rows[0];
    assert_eq!(first.title, "Many 000", "{profile:?}");
    assert_eq!(
        first.authors.as_deref(),
        Some("Author 000, Shared Author"),
        "{profile:?}"
    );
    assert_eq!(first.series.as_deref(), Some("Series 0"), "{profile:?}");
    let values = |facet| {
        let store = store.clone();
        async move {
            let mut values = Vec::new();
            let mut after: Option<String> = None;
            loop {
                let page = store
                    .library_facet_values(FacetValuesOf::Facet(facet), after.as_deref(), 500)
                    .await
                    .unwrap();
                values.extend(page.values);
                match page.next {
                    Some(next) => after = Some(next),
                    None => return values,
                }
            }
        }
    };
    let authors = values(WorkFacet::Author).await;
    assert_eq!(
        authors.len(),
        41,
        "{profile:?}: 40 own authors and one shared"
    );
    let shared = authors
        .iter()
        .find(|v| v.value == "Shared Author")
        .expect("the shared author is a value");
    assert_eq!(shared.works, 40, "{profile:?}");
    assert_eq!(values(WorkFacet::Narrator).await.len(), 40, "{profile:?}");
    assert_eq!(values(WorkFacet::Series).await.len(), 4, "{profile:?}");
    // A refresh stamped earlier than the rows now writes none of them.
    let stale_stamp = "2000-01-01T00:00:00.000000Z";
    let stale: Vec<_> = works[..40]
        .iter()
        .map(|work| (work.clone(), None, Vec::new()))
        .collect();
    assert_eq!(
        store.write_work_rows(&stale, stale_stamp).await.unwrap(),
        0,
        "{profile:?}: the fence refuses an older refresh"
    );
    assert_eq!(walk(&store, WorkPageQuery::default()).await.len(), 40);
}

/// The page index row of `work`, when it has one.
async fn row_of(store: &LibraryStore, work: &str) -> Option<super::LibraryWorkRow> {
    walk_rows(
        store,
        WorkPageQuery {
            limit: 50,
            ..WorkPageQuery::default()
        },
    )
    .await
    .into_iter()
    .find(|row| row.work_id == work)
}

#[tokio::test]
async fn a_works_cover_comes_from_its_edition_listing_or_work() {
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
    let store = store().await;
    let work = scan(&store, "B0C1", "acct-1", "Covered", "A. Author").await;
    store
        .refresh_library_works(std::slice::from_ref(&work))
        .await
        .unwrap();
    // A held work names its cover edition even before any cover is known.
    let edition = row_of(&store, &work)
        .await
        .expect("the held work has a row")
        .cover_edition_id
        .expect("a held work names a cover edition");
    let covers = |store: &LibraryStore, edition: &str| {
        let ids = [edition.to_string()];
        let store = store.clone();
        async move {
            store
                .edition_cover_urls(&ids)
                .await
                .unwrap()
                .get(&ids[0])
                .cloned()
        }
    };
    assert_eq!(covers(&store, &edition).await, None);

    // The work's cover (a title's enrichment) shows once it is stored,
    // without the row being refreshed.
    let stored = crate::entities::works::Entity::find_by_id(work.clone())
        .one(store.db())
        .await
        .unwrap()
        .unwrap();
    let mut am: crate::entities::works::ActiveModel = stored.into();
    am.cover_url = Set(Some("https://img.example/work.jpg".into()));
    am.update(store.db()).await.unwrap();
    assert_eq!(
        covers(&store, &edition).await.as_deref(),
        Some("https://img.example/work.jpg")
    );

    // A store's cover on the edition's listing wins over the work's.
    let detail = store.library_work_detail(&work).await.unwrap().unwrap();
    let listing = detail.editions[0].listings[0].id.clone();
    store
        .record_listing_detail(
            &listing,
            &crate::ListingDetail {
                cover_url: Some("https://img.example/store.jpg".into()),
                ..crate::ListingDetail::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        covers(&store, &edition).await.as_deref(),
        Some("https://img.example/store.jpg")
    );
    let detail = store.library_work_detail(&work).await.unwrap().unwrap();
    assert_eq!(
        detail.editions[0].cover_url.as_deref(),
        Some("https://img.example/store.jpg")
    );
    assert_eq!(covers(&store, "no-such-edition").await, None);

    // The store's cover became the edition's own; a later store text,
    // written by store key, updates the listing but keeps the edition's.
    let edition_cover = |store: &LibraryStore, id: &str| {
        let id = id.to_owned();
        let db = store.db().clone();
        async move {
            crate::entities::editions::Entity::find_by_id(id)
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .cover_url
        }
    };
    assert_eq!(
        edition_cover(&store, &edition).await.as_deref(),
        Some("https://img.example/store.jpg")
    );
    let row = crate::entities::store_listings::Entity::find_by_id(listing.clone())
        .one(store.db())
        .await
        .unwrap()
        .unwrap();
    let key = bookclerk_resolve::ListingKey {
        plugin_key: row.plugin_key,
        marketplace: row.marketplace,
        product_id: row.product_id,
    };
    let missing = bookclerk_resolve::ListingKey {
        product_id: "not-listed".into(),
        ..key.clone()
    };
    let later = crate::ListingDetail {
        cover_url: Some("https://img.example/later.jpg".into()),
        description: Some("From the store.".into()),
        ..crate::ListingDetail::default()
    };
    let named = store
        .record_store_text(&[(key, later.clone()), (missing, later)])
        .await
        .unwrap();
    assert_eq!(named, 1, "a key with no listing is skipped");
    let row = crate::entities::store_listings::Entity::find_by_id(listing)
        .one(store.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.cover_url.as_deref(),
        Some("https://img.example/later.jpg")
    );
    assert_eq!(row.description.as_deref(), Some("From the store."));
    assert_eq!(
        edition_cover(&store, &edition).await.as_deref(),
        Some("https://img.example/store.jpg")
    );
}
