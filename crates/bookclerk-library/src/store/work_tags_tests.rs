//! Library tag tests: storage, the page index facet and a scope on a tag,
//! under every database profile.

use super::{normalize_work_tags, WorkTag, WorkTagSource, MAX_WORK_TAGS, MAX_WORK_TAG_BYTES};
use crate::error::LibraryError;
use crate::store::library_index::{facet_key, FacetValuesOf, WorkFacet, WorkFilter};
use crate::store::library_index::{WorkPageQuery, WorkScope};
use crate::store::{LibraryStore, NewBook};
use crate::test_support::Profile;

/// A store under `profile` holding one account.
async fn store_under(profile: Profile) -> LibraryStore {
    let store = crate::test_support::sqlite::memory(profile)
        .await
        .into_store();
    store
        .upsert_account("audible", "acct-1", "us", None, true, "audible")
        .await
        .unwrap();
    store
}

/// Scans `title` onto the account; returns its work id.
async fn scan(store: &LibraryStore, asin: &str, title: &str) -> String {
    let saved = store
        .upsert_book(&NewBook::minimal(asin, "acct-1", "us", title))
        .await
        .unwrap();
    store
        .work_id_for_book(&saved.uuid)
        .await
        .unwrap()
        .expect("a scanned title has a work")
}

/// Titles of every row of `query`, page by page.
async fn titles(store: &LibraryStore, mut query: WorkPageQuery) -> Vec<String> {
    let mut out = Vec::new();
    loop {
        let page = store.library_works_page(&query).await.unwrap();
        out.extend(page.works.into_iter().map(|w| w.title));
        match page.next {
            Some(next) => query.after = Some(next),
            None => return out,
        }
    }
}

/// Strings from `&str`s.
fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| (*v).to_string()).collect()
}

#[test]
fn tags_are_trimmed_deduplicated_and_bounded() {
    assert_eq!(
        normalize_work_tags(&strings(&[" Kids ", "kids", "", "Bedtime"])).unwrap(),
        [
            ("kids".to_string(), "Kids".to_string()),
            ("bedtime".to_string(), "Bedtime".to_string())
        ]
    );
    let too_many: Vec<String> = (0..=MAX_WORK_TAGS).map(|n| format!("t{n}")).collect();
    assert!(matches!(
        normalize_work_tags(&too_many),
        Err(LibraryError::Invalid(_))
    ));
    assert!(matches!(
        normalize_work_tags(&["x".repeat(MAX_WORK_TAG_BYTES + 1)]),
        Err(LibraryError::Invalid(_))
    ));
    assert!(normalize_work_tags(&["x".repeat(MAX_WORK_TAG_BYTES)]).is_ok());
}

#[tokio::test]
async fn library_tags_filter_the_library_and_scopes() {
    for profile in Profile::ALL {
        library_tags_filter_the_library_and_scopes_under(profile).await;
    }
}

/// [`library_tags_filter_the_library_and_scopes`] under one profile.
async fn library_tags_filter_the_library_and_scopes_under(profile: Profile) {
    let store = store_under(profile).await;
    let gruffalo = scan(&store, "B001", "The Gruffalo").await;
    let dune = scan(&store, "B002", "Dune").await;
    store
        .refresh_library_works(&[gruffalo.clone(), dune.clone()])
        .await
        .unwrap();

    let set = store
        .set_work_tags(&gruffalo, &strings(&["Kids", " kids", "Bedtime"]))
        .await
        .unwrap();
    assert_eq!(
        set,
        [
            WorkTag {
                value: "Bedtime".into(),
                source: WorkTagSource::Admin
            },
            WorkTag {
                value: "Kids".into(),
                source: WorkTagSource::Admin
            },
        ],
        "{profile:?}"
    );
    assert_eq!(store.work_tags(&gruffalo).await.unwrap(), set);

    // The refresh ran with the write: the Library filters by the tag at once.
    let tagged = titles(
        &store,
        WorkPageQuery {
            filter: Some(WorkFilter::Facet {
                facet: WorkFacet::Tag,
                value: "KIDS".into(),
            }),
            limit: 10,
            ..WorkPageQuery::default()
        },
    )
    .await;
    assert_eq!(tagged, ["The Gruffalo"], "{profile:?}: filter");
    let mut names: Vec<String> = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = store
            .library_facet_values(
                FacetValuesOf::Facet(WorkFacet::Tag),
                after.as_deref(),
                10,
                None,
            )
            .await
            .unwrap();
        names.extend(page.values.into_iter().map(|v| v.value));
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert_eq!(names, ["Bedtime", "Kids"], "{profile:?}: values");

    // A scope can hide or keep a tag.
    let scoped = |scope: WorkScope| WorkPageQuery {
        limit: 10,
        scope: Some(scope),
        ..WorkPageQuery::default()
    };
    let kids_only = WorkScope {
        include: vec![(WorkFacet::Tag, facet_key("Kids"))],
        ..WorkScope::default()
    };
    assert_eq!(
        titles(&store, scoped(kids_only)).await,
        ["The Gruffalo"],
        "{profile:?}: include"
    );
    let no_kids = WorkScope {
        exclude: vec![(WorkFacet::Tag, facet_key("kids"))],
        ..WorkScope::default()
    };
    assert_eq!(
        titles(&store, scoped(no_kids)).await,
        ["Dune"],
        "{profile:?}: exclude"
    );

    // Sidecar tags stay when an Administrator replaces theirs.
    {
        use sea_orm::ConnectionTrait as _;
        store
            .db()
            .execute_unprepared(&format!(
                "INSERT INTO work_tags (work_id, source, key, value, created_at) \
                 VALUES ('{gruffalo}', 'sidecar', 'picture book', 'Picture Book', '2026-01-01')"
            ))
            .await
            .unwrap();
    }
    let after = store.set_work_tags(&gruffalo, &[]).await.unwrap();
    assert_eq!(
        after,
        [WorkTag {
            value: "Picture Book".into(),
            source: WorkTagSource::Sidecar
        }],
        "{profile:?}: sidecar kept"
    );
    let kids_now = titles(
        &store,
        WorkPageQuery {
            filter: Some(WorkFilter::Facet {
                facet: WorkFacet::Tag,
                value: "Kids".into(),
            }),
            limit: 10,
            ..WorkPageQuery::default()
        },
    )
    .await;
    assert!(kids_now.is_empty(), "{profile:?}: cleared");
}

#[tokio::test]
async fn tags_go_on_top_level_works_only() {
    let store = store_under(Profile::FirstParty).await;
    assert!(matches!(
        store.set_work_tags("no-such-work", &strings(&["x"])).await,
        Err(LibraryError::NotFound(_))
    ));
    assert!(matches!(
        store.work_tags("no-such-work").await,
        Err(LibraryError::NotFound(_))
    ));
}
