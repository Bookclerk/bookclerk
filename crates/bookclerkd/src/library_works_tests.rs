//! Tests of the library works routes, driven through their handlers.

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use bookclerk_library::{LibraryStore, NewBook};
use chrono::Utc;

use super::*;

/// Scans `title` (product `asin`) onto account `acct` and returns its work.
async fn scan(library: &LibraryStore, asin: &str, title: &str, author: &str) -> String {
    let mut book = NewBook::minimal(asin, "acct", "us", title);
    book.authors = Some(author.into());
    let saved = library.upsert_book(&book).await.expect("book");
    library
        .work_id_for_book(&saved.uuid)
        .await
        .expect("work read")
        .expect("a scanned title has a work")
}

/// A daemon state whose library holds three indexed works.
async fn library_with_three_works(
    files: &tempfile::TempDir,
) -> (Arc<AppState>, LibraryStore, Vec<String>) {
    let (state, library) = crate::node_api::tests::test_state(files).await;
    library
        .upsert_account("audible", "acct", "us", None, true, "audible")
        .await
        .expect("account");
    let ids = vec![
        scan(&library, "B001", "The Hobbit", "J. R. R. Tolkien").await,
        scan(&library, "B002", "Dune", "Frank Herbert").await,
        scan(&library, "B003", "Anathem", "Neal Stephenson").await,
    ];
    library.refresh_library_works(&ids).await.expect("refresh");
    (state, library, ids)
}

/// Lists works with `query`, unwrapping a success.
async fn list(state: &Arc<AppState>, query: WorksQuery) -> WorksPage {
    list_works(State(state.clone()), HeaderMap::new(), Query(query))
        .await
        .map(|Json(page)| page)
        .map_err(|(status, Json(body))| (status, body.error))
        .expect("page")
}

#[tokio::test]
async fn a_work_with_a_stored_cover_lists_a_cover_path() {
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
    let files = tempfile::tempdir().expect("tempdir");
    let (state, library, ids) = library_with_three_works(&files).await;
    // A title's enrichment stores its cover on the work, after the link
    // that indexed it; the list shows it without another refresh.
    let work = bookclerk_library::entities::works::Entity::find_by_id(ids[1].clone())
        .one(library.db())
        .await
        .expect("read")
        .expect("work");
    let mut am: bookclerk_library::entities::works::ActiveModel = work.into();
    am.cover_url = Set(Some("https://img.example/dune.jpg".into()));
    am.update(library.db()).await.expect("cover");
    let page = list(&state, WorksQuery::default()).await;
    let dune = page.works.iter().find(|w| w.title == "Dune").expect("Dune");
    let path = dune
        .cover_path
        .as_deref()
        .expect("the work's cover is listed");
    assert!(path.starts_with("/api/covers/"), "{path}");
    let hobbit = page
        .works
        .iter()
        .find(|w| w.title == "The Hobbit")
        .expect("The Hobbit");
    assert!(hobbit.cover_path.is_none(), "no cover is known for it");
}

#[tokio::test]
async fn works_page_by_cursor_in_title_order() {
    let files = tempfile::tempdir().expect("tempdir");
    let (state, _library, _ids) = library_with_three_works(&files).await;
    let mut titles = Vec::new();
    let mut cursor = None;
    loop {
        let page = list(
            &state,
            WorksQuery {
                limit: Some(2),
                cursor: cursor.take(),
                ..WorksQuery::default()
            },
        )
        .await;
        titles.extend(page.works.iter().map(|w| w.title.clone()));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(titles, ["Anathem", "Dune", "The Hobbit"]);

    let page = list(
        &state,
        WorksQuery {
            sort: Some(WorkSortParam::Author),
            media_kind: Some(MediaKindJson::Audio),
            kind: Some(WorkKindJson::Book),
            status: Some(AcquireStatus::NotAcquired),
            account: Some("acct".into()),
            ..WorksQuery::default()
        },
    )
    .await;
    let first = &page.works[0];
    assert_eq!(first.title, "Dune", "Herbert sorts before Stephenson");
    assert_eq!(first.kind, WorkKindJson::Book);
    assert_eq!(first.media_kinds, [MediaKindJson::Audio]);
    assert_eq!(first.holding_count, 1);
    assert!(first.cover_path.is_none(), "a scanned edition has no cover");
    assert!(page.next_cursor.is_none());

    let none = list(
        &state,
        WorksQuery {
            kind: Some(WorkKindJson::Podcast),
            ..WorksQuery::default()
        },
    )
    .await;
    assert!(none.works.is_empty());
}

#[tokio::test]
async fn a_bad_cursor_or_one_of_another_order_is_a_400() {
    let files = tempfile::tempdir().expect("tempdir");
    let (state, _library, _ids) = library_with_three_works(&files).await;
    let refused = list_works(
        State(state.clone()),
        HeaderMap::new(),
        Query(WorksQuery {
            cursor: Some("not a cursor".into()),
            ..WorksQuery::default()
        }),
    )
    .await
    .err()
    .map(|(status, Json(body))| (status, body.error));
    assert_eq!(
        refused,
        Some((StatusCode::BAD_REQUEST, "invalid page cursor".into()))
    );

    let title_cursor = list(
        &state,
        WorksQuery {
            limit: Some(1),
            ..WorksQuery::default()
        },
    )
    .await
    .next_cursor
    .expect("more pages");
    let status = list_works(
        State(state.clone()),
        HeaderMap::new(),
        Query(WorksQuery {
            sort: Some(WorkSortParam::Added),
            cursor: Some(title_cursor),
            ..WorksQuery::default()
        }),
    )
    .await
    .err()
    .map(|(status, _)| status);
    assert_eq!(status, Some(StatusCode::BAD_REQUEST));
}

/// Builds the works search index for `library` under `state`'s files.
async fn index_works(state: &Arc<AppState>, library: &LibraryStore) {
    let dir = state.config.read().await.paths().search_index_dir.clone();
    let index = WorkSearchIndex::open(&dir).expect("works index");
    index.rebuild(library).await.expect("rebuild");
}

/// Lists works with `query`, unwrapping a refusal into its status and message.
async fn refused(state: &Arc<AppState>, query: WorksQuery) -> Option<(StatusCode, String)> {
    list_works(State(state.clone()), HeaderMap::new(), Query(query))
        .await
        .err()
        .map(|(status, Json(body))| (status, body.error))
}

#[tokio::test]
async fn a_search_pages_its_matches_and_applies_the_filters() {
    let files = tempfile::tempdir().expect("tempdir");
    let (state, library, ids) = library_with_three_works(&files).await;
    index_works(&state, &library).await;

    let page = list(
        &state,
        WorksQuery {
            q: Some("dune".into()),
            ..WorksQuery::default()
        },
    )
    .await;
    assert_eq!(page.works.len(), 1);
    assert_eq!(page.works[0].id, ids[1]);
    assert!(page.next_cursor.is_none());

    // Every title has an author, so an author-or-title query matches all
    // three; paging one at a time walks each exactly once.
    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = list(
            &state,
            WorksQuery {
                q: Some("tolkien OR herbert OR stephenson".into()),
                limit: Some(1),
                cursor: cursor.take(),
                ..WorksQuery::default()
            },
        )
        .await;
        seen.extend(page.works.iter().map(|w| w.id.clone()));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    seen.sort();
    let mut all = ids.clone();
    all.sort();
    assert_eq!(seen, all);

    let filtered = list(
        &state,
        WorksQuery {
            q: Some("dune".into()),
            kind: Some(WorkKindJson::Podcast),
            ..WorksQuery::default()
        },
    )
    .await;
    assert!(filtered.works.is_empty(), "the filters apply to matches");

    // The title search's shortcuts are plain words here, and store ids
    // match whatever their case.
    for q in [
        "dune finished",
        "dune -acquired",
        "productid:B002",
        "asin:b002",
    ] {
        let page = list(
            &state,
            WorksQuery {
                q: Some(q.into()),
                ..WorksQuery::default()
            },
        )
        .await;
        assert_eq!(page.works.len(), 1, "{q}");
        assert_eq!(page.works[0].id, ids[1], "{q}");
    }
}

#[tokio::test]
async fn a_filtered_search_skips_matches_the_filters_drop() {
    let files = tempfile::tempdir().expect("tempdir");
    let (state, library, _ids) = library_with_three_works(&files).await;
    library
        .upsert_account("audible", "other", "us", None, true, "audible")
        .await
        .expect("account");
    let mut book = NewBook::minimal("B004", "other", "us", "Neuromancer");
    book.authors = Some("William Gibson".into());
    let saved = library.upsert_book(&book).await.expect("book");
    let other = library
        .work_id_for_book(&saved.uuid)
        .await
        .expect("work read")
        .expect("a work");
    library
        .refresh_library_works(std::slice::from_ref(&other))
        .await
        .expect("refresh");
    index_works(&state, &library).await;

    // All four match; only one is held on the other account, wherever it
    // ranks. Paging one at a time finds it exactly once.
    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = list(
            &state,
            WorksQuery {
                q: Some("tolkien OR herbert OR stephenson OR gibson".into()),
                account: Some("other".into()),
                limit: Some(1),
                cursor: cursor.take(),
                ..WorksQuery::default()
            },
        )
        .await;
        seen.extend(page.works.iter().map(|w| w.id.clone()));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, [other]);
}

#[tokio::test]
async fn a_search_refuses_a_bad_query_or_another_querys_cursor() {
    let files = tempfile::tempdir().expect("tempdir");
    let (state, library, _ids) = library_with_three_works(&files).await;
    index_works(&state, &library).await;

    let bad = refused(
        &state,
        WorksQuery {
            q: Some("title:(".into()),
            ..WorksQuery::default()
        },
    )
    .await;
    assert!(
        matches!(&bad, Some((StatusCode::BAD_REQUEST, m)) if m.starts_with("invalid search query")),
        "{bad:?}"
    );

    let cursor = list(
        &state,
        WorksQuery {
            q: Some("tolkien OR herbert".into()),
            limit: Some(1),
            ..WorksQuery::default()
        },
    )
    .await
    .next_cursor
    .expect("more matches");
    assert_eq!(
        refused(
            &state,
            WorksQuery {
                q: Some("tolkien OR herbert".into()),
                account: Some("acct".into()),
                cursor: Some(cursor.clone()),
                ..WorksQuery::default()
            },
        )
        .await,
        Some((
            StatusCode::BAD_REQUEST,
            "page cursor belongs to another query or other filters".into()
        )),
        "a cursor is bound to its filters"
    );
    assert_eq!(
        refused(
            &state,
            WorksQuery {
                q: Some("dune".into()),
                cursor: Some(cursor),
                ..WorksQuery::default()
            },
        )
        .await,
        Some((
            StatusCode::BAD_REQUEST,
            "page cursor belongs to another query or other filters".into()
        ))
    );
    assert_eq!(
        refused(
            &state,
            WorksQuery {
                q: Some("dune".into()),
                cursor: Some("not a cursor".into()),
                ..WorksQuery::default()
            },
        )
        .await,
        Some((StatusCode::BAD_REQUEST, "invalid page cursor".into()))
    );
}

#[tokio::test]
async fn a_search_before_the_works_index_is_built_is_a_503() {
    let files = tempfile::tempdir().expect("tempdir");
    let (state, _library, _ids) = library_with_three_works(&files).await;
    let status = refused(
        &state,
        WorksQuery {
            q: Some("dune".into()),
            ..WorksQuery::default()
        },
    )
    .await
    .map(|(status, _)| status);
    assert_eq!(status, Some(StatusCode::SERVICE_UNAVAILABLE));
}

/// Fills a page from `window` with chunks of `chunk`, keeping the ids in
/// `kept` as the filters would, and counts the lookups.
async fn fill(
    window: &[&str],
    kept: &[&str],
    limit: usize,
    chunk: usize,
) -> (Vec<String>, usize, usize) {
    let window: Vec<String> = window.iter().map(|s| (*s).to_string()).collect();
    let mut lookups = 0;
    let (rows, used) = fill_search_page(
        &window,
        limit,
        chunk,
        |id: &String| id.as_str(),
        |part| {
            lookups += 1;
            let found: Vec<String> = part
                .into_iter()
                .filter(|id| kept.contains(&id.as_str()))
                .collect();
            async move { Ok::<_, std::convert::Infallible>(found) }
        },
    )
    .await
    .expect("infallible");
    (rows, used, lookups)
}

#[tokio::test]
async fn a_search_page_fills_across_chunks_and_stops_past_its_last_match() {
    let window = ["a", "b", "c", "d", "e", "f", "g"];
    // The filters drop a, b and d: the page of two fills in the second
    // chunk, at e, so the next page starts at f.
    assert_eq!(
        fill(&window, &["c", "e", "f", "g"], 2, 3).await,
        (vec!["c".into(), "e".into()], 5, 2)
    );
    // Nothing passes: the whole window is used, one lookup per chunk, and
    // the page is empty (the route then puts its cursor past the window).
    assert_eq!(fill(&window, &[], 2, 3).await, (Vec::new(), 7, 3));
    // Too few pass to fill the page: a short page over the whole window.
    assert_eq!(fill(&window, &["g"], 2, 3).await, (vec!["g".into()], 7, 3));
    // The page fills exactly at a chunk's end.
    assert_eq!(
        fill(&window, &["a", "b", "c"], 3, 3).await,
        (vec!["a".into(), "b".into(), "c".into()], 3, 1)
    );
}

#[tokio::test]
async fn a_work_reads_with_its_editions_or_is_a_404() {
    let files = tempfile::tempdir().expect("tempdir");
    let (state, _library, ids) = library_with_three_works(&files).await;
    let Json(detail) = get_work(
        State(state.clone()),
        HeaderMap::new(),
        AxumPath(ids[0].clone()),
    )
    .await
    .map_err(|(status, _)| status)
    .expect("detail");
    assert_eq!(detail.title, "The Hobbit");
    assert_eq!(detail.authors, ["J. R. R. Tolkien"]);
    assert!(detail.state.is_none(), "the operator has no own state");
    let edition = &detail.editions[0];
    assert_eq!(edition.media_kind, MediaKindJson::Audio);
    assert_eq!(edition.listings[0].product_id, "B001");
    assert!(matches!(
        &edition.listings[0].holdings[0].holder,
        HolderJson::Account { account_id, .. } if account_id == "acct"
    ));

    let missing = get_work(
        State(state.clone()),
        HeaderMap::new(),
        AxumPath("no-such-work".into()),
    )
    .await
    .err()
    .map(|(status, Json(body))| (status, body.error));
    assert_eq!(
        missing,
        Some((StatusCode::NOT_FOUND, "work no-such-work not found".into()))
    );
}

/// The cover route's refusal for `edition`, as status and message.
async fn cover_refusal(state: &Arc<AppState>, edition: &str) -> (StatusCode, String) {
    match get_edition_cover(
        State(state.clone()),
        HeaderMap::new(),
        AxumPath(edition.to_string()),
    )
    .await
    {
        Ok(res) => panic!("expected a refusal, got {}", res.status()),
        Err((status, Json(body))) => (status, body.error),
    }
}

#[tokio::test]
async fn an_edition_cover_is_a_404_unless_this_node_holds_one() {
    let files = tempfile::tempdir().expect("tempdir");
    let (state, library, ids) = library_with_three_works(&files).await;
    assert_eq!(
        cover_refusal(&state, "no-such-edition").await,
        (
            StatusCode::NOT_FOUND,
            "edition no-such-edition not found".into()
        )
    );
    let uuids = library
        .book_uuids_for_work(&ids[0])
        .await
        .expect("the scanned title");
    let edition = library
        .edition_of_book(&uuids[0])
        .await
        .expect("read")
        .expect("edition");
    let no_cover = (
        StatusCode::NOT_FOUND,
        format!("edition {edition} has no cover on this node"),
    );
    // Never acquired, then acquired on a node with no local destination.
    assert_eq!(cover_refusal(&state, &edition).await, no_cover);
    library
        .set_acquire_status(
            "B001",
            "acct",
            AcquireStatus::Acquired,
            Some("Tolkien/The Hobbit.m4b"),
            None,
        )
        .await
        .expect("acquired");
    assert!(state.destinations.read().await.local().is_none());
    assert_eq!(cover_refusal(&state, &edition).await, no_cover);
}

#[tokio::test]
async fn the_detail_carries_the_callers_state_and_every_holder() {
    let files = tempfile::tempdir().expect("tempdir");
    let (app, _library) = crate::node_api::tests::test_state(&files).await;
    let detail = WorkDetail {
        id: "w".into(),
        kind: WorkKind::Podcast,
        title: "Show".into(),
        description: None,
        authors: vec!["Host".into()],
        series: None,
        series_position: None,
        editions: vec![EditionDetail {
            id: "e".into(),
            work_id: "episode".into(),
            expression_id: "x".into(),
            expression_kind: ExpressionKind::OriginalAudio,
            language: Some("en".into()),
            abridgement: Abridgement::Unknown,
            media_kind: MediaKind::Audio,
            title: "Episode 1".into(),
            subtitle: None,
            published_at: None,
            duration_minutes: Some(42),
            cover_url: Some("http://insecure.example/c.jpg".into()),
            acquire_status: AcquireStatus::Acquired,
            listings: vec![EditionListing {
                id: "l".into(),
                plugin_key: "podcasts".into(),
                marketplace: String::new(),
                product_id: "ep-1".into(),
                url: None,
                holdings: vec![ListingHolding {
                    id: "h".into(),
                    holder: HoldingHolder::Node {
                        node_id: "n1".into(),
                    },
                    entitlement: Entitlement::Free,
                    purchased_at: None,
                    added_at: "2026-11-14T00:00:00Z".into(),
                }],
            }],
        }],
    };
    let state = UserTitleState {
        user_id: 7,
        work_id: "w".into(),
        is_finished: true,
        rating_overall: Some(4.5),
        rating_performance: None,
        rating_story: None,
        tags: None,
        updated_at: Utc::now(),
    };
    let json =
        serde_json::to_value(detail_json(detail, Some(state), true, &app.covers)).expect("json");
    assert_eq!(json["kind"], "podcast");
    assert_eq!(json["state"]["is_finished"], true);
    assert_eq!(json["state"]["rating_overall"], 4.5);
    assert_eq!(json["can_edit_state"], true);
    let edition = &json["editions"][0];
    assert_eq!(edition["expression_kind"], "original_audio");
    assert_eq!(edition["abridgement"], "unknown");
    assert_eq!(edition["acquire_status"], "acquired");
    assert!(
        edition["cover_path"].is_null(),
        "only https covers are proxied"
    );
    let holding = &edition["listings"][0]["holdings"][0];
    assert_eq!(holding["holder"]["kind"], "node");
    assert_eq!(holding["holder"]["node_id"], "n1");
    assert_eq!(holding["entitlement"], "free");
}

#[test]
fn wire_enums_round_trip_every_stored_value() {
    for kind in WorkKind::ALL {
        assert_eq!(WorkKind::from(WorkKindJson::from(*kind)), *kind);
    }
    for kind in MediaKind::ALL {
        assert_eq!(MediaKind::from(MediaKindJson::from(*kind)), *kind);
    }
    for kind in ExpressionKind::ALL {
        let json = serde_json::to_value(ExpressionKindJson::from(*kind)).expect("json");
        assert_eq!(json, kind.as_str());
    }
    for value in Abridgement::ALL {
        let json = serde_json::to_value(AbridgementJson::from(*value)).expect("json");
        assert_eq!(json, value.as_str());
    }
    for value in Entitlement::ALL {
        let json = serde_json::to_value(EntitlementJson::from(*value)).expect("json");
        assert_eq!(json, value.as_str());
    }
}

#[test]
fn a_facet_and_value_make_one_filter() {
    let refusal = |facet, value| {
        work_filter(Some(facet), value, Some(7))
            .err()
            .map(|(status, Json(body))| (status, body.error))
    };
    assert_eq!(work_filter(None, Some("x"), None).ok(), Some(None));
    assert_eq!(
        refusal(WorkFacetParam::Genre, Some("  ")),
        Some((StatusCode::BAD_REQUEST, "`facet` needs a `value`".into()))
    );
    assert_eq!(
        refusal(WorkFacetParam::Progress, Some("halfway")).map(|(status, _)| status),
        Some(StatusCode::BAD_REQUEST)
    );
    assert_eq!(
        work_filter(Some(WorkFacetParam::Series), None, None).ok(),
        Some(Some(WorkFilter::Facet {
            facet: WorkFacet::Series,
            value: String::new(),
        })),
        "an empty series is works in no series"
    );
    assert_eq!(
        work_filter(Some(WorkFacetParam::Tag), Some(" reread "), Some(7)).ok(),
        Some(Some(WorkFilter::Tag {
            user_id: Some(7),
            tag: "reread".into(),
        }))
    );
    assert_eq!(
        work_filter(
            Some(WorkFacetParam::Progress),
            Some("not_finished"),
            Some(7)
        )
        .ok(),
        Some(Some(WorkFilter::Finished {
            user_id: Some(7),
            finished: false,
        }))
    );
    for (param, facet) in [
        (WorkFacetParam::Author, WorkFacet::Author),
        (WorkFacetParam::Narrator, WorkFacet::Narrator),
        (WorkFacetParam::Genre, WorkFacet::Genre),
        (WorkFacetParam::Publisher, WorkFacet::Publisher),
        (WorkFacetParam::Source, WorkFacet::Source),
    ] {
        assert_eq!(param.indexed(), Some(facet));
    }
    assert_eq!(WorkFacetParam::Tag.indexed(), None);
}

#[tokio::test]
async fn works_filter_by_a_facet_value() {
    let files = tempfile::tempdir().expect("tempdir");
    let (state, _library, _ids) = library_with_three_works(&files).await;
    let titles = |page: WorksPage| page.works.into_iter().map(|w| w.title).collect::<Vec<_>>();
    let by_author = list(
        &state,
        WorksQuery {
            facet: Some(WorkFacetParam::Author),
            value: Some("frank herbert".into()),
            ..WorksQuery::default()
        },
    )
    .await;
    assert_eq!(titles(by_author), ["Dune"]);
    // The operator has no own state: nothing is finished, everything is not.
    let finished = list(
        &state,
        WorksQuery {
            facet: Some(WorkFacetParam::Progress),
            value: Some("finished".into()),
            ..WorksQuery::default()
        },
    )
    .await;
    assert!(finished.works.is_empty());
    let unfinished = list(
        &state,
        WorksQuery {
            facet: Some(WorkFacetParam::Progress),
            value: Some("not_finished".into()),
            sort: Some(WorkSortParam::Length),
            ..WorksQuery::default()
        },
    )
    .await;
    assert_eq!(unfinished.works.len(), 3);
    assert_eq!(
        refused(
            &state,
            WorksQuery {
                facet: Some(WorkFacetParam::Tag),
                ..WorksQuery::default()
            },
        )
        .await
        .map(|(status, _)| status),
        Some(StatusCode::BAD_REQUEST)
    );
}

/// Lists `facet`'s values, unwrapping the answer into status or page.
async fn facet_values(
    state: &Arc<AppState>,
    facet: WorkFacetParam,
    query: FacetValuesQuery,
) -> Result<FacetValuesJson, (StatusCode, String)> {
    list_facet_values(
        State(state.clone()),
        HeaderMap::new(),
        AxumPath(facet),
        Query(query),
    )
    .await
    .map(|Json(page)| page)
    .map_err(|(status, Json(body))| (status, body.error))
}

#[tokio::test]
async fn facet_values_page_by_cursor() {
    let files = tempfile::tempdir().expect("tempdir");
    let (state, _library, _ids) = library_with_three_works(&files).await;
    let mut authors = Vec::new();
    let mut cursor = None;
    loop {
        let page = facet_values(
            &state,
            WorkFacetParam::Author,
            FacetValuesQuery {
                cursor: cursor.take(),
                limit: Some(1),
            },
        )
        .await
        .expect("values");
        authors.extend(page.values.into_iter().map(|v| (v.value, v.works)));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(
        authors,
        [
            ("Frank Herbert".to_string(), 1),
            ("J. R. R. Tolkien".to_string(), 1),
            ("Neal Stephenson".to_string(), 1),
        ]
    );
    let sources = facet_values(&state, WorkFacetParam::Source, FacetValuesQuery::default())
        .await
        .expect("values");
    assert_eq!(sources.values.len(), 1);
    assert_eq!(sources.values[0].works, 3);

    let tags = facet_values(&state, WorkFacetParam::Tag, FacetValuesQuery::default())
        .await
        .expect("values");
    assert!(tags.values.is_empty(), "the operator has no tags");
    assert_eq!(
        facet_values(
            &state,
            WorkFacetParam::Progress,
            FacetValuesQuery::default()
        )
        .await
        .err()
        .map(|(status, _)| status),
        Some(StatusCode::BAD_REQUEST)
    );
    assert_eq!(
        facet_values(
            &state,
            WorkFacetParam::Genre,
            FacetValuesQuery {
                cursor: Some("not a cursor!".into()),
                limit: None,
            },
        )
        .await
        .err(),
        Some((StatusCode::BAD_REQUEST, "invalid page cursor".into()))
    );
}

/// Sends `PUT /api/library/works/{work}/state` with `body` and `cookie`
/// through the router; returns the status and decoded body.
async fn put_state(
    app: &axum::Router,
    cookie: Option<&str>,
    work: &str,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let mut request = axum::http::Request::builder()
        .method("PUT")
        .uri(format!("/api/library/works/{work}/state"))
        .header(header::CONTENT_TYPE, "application/json");
    request = match cookie {
        Some(cookie) => request.header(header::COOKIE, cookie),
        None => request.header(header::AUTHORIZATION, "Bearer op-token-state"),
    };
    let response = app
        .clone()
        .oneshot(
            request
                .body(axum::body::Body::from(body.to_string()))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn a_member_edits_their_own_work_state() {
    let (_state, app, library) = crate::auth::tests::phase2_harness("op-token-state").await;
    library
        .upsert_account("audible", "acct", "us", None, true, "audible")
        .await
        .expect("account");
    let work = scan(&library, "B001", "The Hobbit", "J. R. R. Tolkien").await;
    let cookie = crate::auth::tests::portal_cookie_for(&library, "test", "member-ext").await;

    let (status, json) = put_state(
        &app,
        Some(&cookie),
        &work,
        r#"{"is_finished": true, "rating_overall": 4.5, "tags": "  classic\treread "}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["is_finished"], true);
    assert_eq!(json["rating_overall"], 4.5);
    assert_eq!(json["tags"], "classic reread");
    let detail = {
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/api/library/works/{work}"))
                    .header(header::COOKIE, &cookie)
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        serde_json::from_slice::<serde_json::Value>(&bytes).expect("json")
    };
    assert_eq!(detail["can_edit_state"], true);
    assert_eq!(detail["state"]["tags"], "classic reread");

    let (status, json) = put_state(
        &app,
        Some(&cookie),
        &work,
        r#"{"rating_overall": null, "rating_story": 5}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["is_finished"], true, "left out, kept");
    assert!(json["rating_overall"].is_null(), "null clears");
    assert_eq!(json["rating_story"], 5.0);
    assert_eq!(json["tags"], "classic reread", "left out, kept");

    let (status, json) = put_state(&app, Some(&cookie), &work, r#"{"rating_story": 6}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["error"], "rating_story must be from 0 to 5");
    let many: Vec<String> = (0..33).map(|n| format!("t{n}")).collect();
    let (status, _) = put_state(
        &app,
        Some(&cookie),
        &work,
        &serde_json::json!({ "tags": many.join(" ") }).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "33 tags are too many");
    let (status, _) = put_state(
        &app,
        Some(&cookie),
        &work,
        &serde_json::json!({ "tags": "x".repeat(65) }).to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a 65-byte tag is too long");

    let (status, json) = put_state(&app, Some(&cookie), "no-such-work", "{}").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["error"], "work no-such-work not found");

    let (status, json) = put_state(&app, None, &work, r#"{"is_finished": false}"#).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the operator token has no user"
    );
    assert_eq!(json["error"], "this session has no library user");
}

#[test]
fn tags_collapse_whitespace_and_clear_when_empty() {
    use bookclerk_library::FieldEdit;
    assert_eq!(tags_edit(None).expect("keep"), FieldEdit::Keep);
    assert_eq!(tags_edit(Some(None)).expect("null"), FieldEdit::Clear);
    assert_eq!(
        tags_edit(Some(Some(" \t ".into()))).expect("blank"),
        FieldEdit::Clear
    );
    assert_eq!(
        tags_edit(Some(Some("a  b\nA".into()))).expect("set"),
        FieldEdit::Set("a b A".into())
    );
    assert!(tags_edit(Some(Some("bad\u{7}tag".into()))).is_err());
    assert!(tags_edit(Some(Some("y".repeat(1025)))).is_err());
    assert_eq!(rating_edit("r", None).expect("keep"), FieldEdit::Keep);
    assert!(rating_edit("r", Some(Some(f64::NAN))).is_err());
    assert!(rating_edit("r", Some(Some(-0.5))).is_err());
}

/// Sends `GET uri` with `cookie` (or the operator token) through the
/// router; returns the status and decoded body.
async fn get_json(
    app: &axum::Router,
    cookie: Option<&str>,
    uri: &str,
) -> (StatusCode, serde_json::Value) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let request = axum::http::Request::builder().uri(uri);
    let request = match cookie {
        Some(cookie) => request.header(header::COOKIE, cookie),
        None => request.header(header::AUTHORIZATION, "Bearer op-token-scope"),
    };
    let response = app
        .clone()
        .oneshot(request.body(axum::body::Body::empty()).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// The titles of a `GET /api/library/works` answer.
fn titles(page: &serde_json::Value) -> Vec<String> {
    page["works"]
        .as_array()
        .expect("works")
        .iter()
        .map(|w| w["title"].as_str().expect("title").to_string())
        .collect()
}

#[tokio::test]
async fn a_scoped_member_sees_only_their_scope_on_every_library_screen() {
    use bookclerk_library::control_plane::{
        set_patron_scope, PatronScope, RuleEffect, ScopeField, ScopeRule,
    };
    let (_state, app, library) = crate::auth::tests::phase2_harness("op-token-scope").await;
    library
        .upsert_account("audible", "acct", "us", None, true, "audible")
        .await
        .expect("account");
    let ids = vec![
        scan(&library, "B001", "The Hobbit", "J. R. R. Tolkien").await,
        scan(&library, "B002", "Dune", "Frank Herbert").await,
        scan(&library, "B003", "Anathem", "Neal Stephenson").await,
    ];
    library.refresh_library_works(&ids).await.expect("refresh");
    let cookie = crate::auth::tests::portal_cookie_for(&library, "test", "member-ext").await;
    let identity = library
        .get_portal_identity("test", "member-ext")
        .await
        .expect("read")
        .expect("identity");
    let member = identity.user_id.expect("a member user");
    let dune = &ids[1];
    let (_, detail) = get_json(&app, None, &format!("/api/library/works/{dune}")).await;
    let dune_edition = detail["editions"][0]["id"]
        .as_str()
        .expect("an edition")
        .to_string();

    // Out of the box nothing is hidden.
    let (status, page) = get_json(&app, Some(&cookie), "/api/library/works").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(titles(&page).len(), 3);

    set_patron_scope(
        &library,
        member,
        Some(PatronScope {
            rules: vec![ScopeRule {
                field: ScopeField::Author,
                value: "J. R. R. Tolkien".into(),
                effect: RuleEffect::Include,
            }],
        }),
    )
    .await
    .expect("scope");

    let (_, page) = get_json(&app, Some(&cookie), "/api/library/works").await;
    assert_eq!(titles(&page), ["The Hobbit"], "list");
    let (_, page) = get_json(&app, None, "/api/library/works").await;
    assert_eq!(titles(&page).len(), 3, "the operator is never scoped");
    let (status, body) = get_json(&app, Some(&cookie), &format!("/api/library/works/{dune}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "work page: {body}");
    assert_eq!(body["error"], format!("work {dune} not found"));
    let (status, _) = get_json(
        &app,
        Some(&cookie),
        &format!("/api/library/editions/{dune_edition}/cover"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "cover");
    let (status, body) = get_json(
        &app,
        Some(&cookie),
        "/api/library/works/facets/author?limit=50",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let authors: Vec<&str> = body["values"]
        .as_array()
        .expect("values")
        .iter()
        .filter_map(|v| v["value"].as_str())
        .collect();
    assert_eq!(authors, ["J. R. R. Tolkien"], "facet values");

    // Books on the member's own store account always pass.
    library
        .link_account(identity.id, "audible", "acct", "audible")
        .await
        .expect("link");
    let (_, page) = get_json(&app, Some(&cookie), "/api/library/works").await;
    assert_eq!(titles(&page).len(), 3, "own books");
    let (status, _) = get_json(&app, Some(&cookie), &format!("/api/library/works/{dune}")).await;
    assert_eq!(status, StatusCode::OK, "own book's page");
}
