//! Collection tests: validation, storage, the rebuild they trigger and the
//! page index facet, under every database profile.

use super::{
    library_collections, parse_collections_request, set_library_collections, LibraryCollection,
    LibraryCollections, MAX_COLLECTIONS, MAX_COLLECTION_PREFIXES,
};
use crate::control_plane::ConfigActor;
use crate::error::LibraryError;
use crate::store::library_index::{facet_key, WorkFacet, WorkFilter, WorkPageQuery, WorkScope};
use crate::store::{FileRole, LibraryStore, NewBook};
use crate::test_support::Profile;

/// The operator, as the CLI writes.
fn operator() -> ConfigActor {
    ConfigActor::Operator { id: "test".into() }
}

/// One collection.
fn collection(name: &str, prefixes: &[&str]) -> LibraryCollection {
    LibraryCollection {
        name: name.into(),
        prefixes: prefixes.iter().map(|p| (*p).to_string()).collect(),
    }
}

/// A set of collections.
fn collections(list: Vec<LibraryCollection>) -> LibraryCollections {
    LibraryCollections { collections: list }
}

#[test]
fn collections_are_normalized_and_bounded() {
    let parsed = parse_collections_request(
        br#"{"collections":[{"name":" Kids ","prefixes":[" kids","kids/","children/"]}]}"#,
    )
    .unwrap();
    assert_eq!(
        parsed,
        collections(vec![collection("Kids", &["kids/", "children/"])])
    );
    for body in [
        r#"{"collections":[{"name":"","prefixes":["a"]}]}"#,
        r#"{"collections":[{"name":"A","prefixes":[]}]}"#,
        r#"{"collections":[{"name":"A","prefixes":["/abs"]}]}"#,
        r#"{"collections":[{"name":"A","prefixes":["a"]},{"name":"a","prefixes":["b"]}]}"#,
        r#"{"collections":[{"name":"A","prefixes":["a"],"x":1}]}"#,
        r#"{"collections":[["A",["a"]]]}"#,
        r#"[]"#,
    ] {
        assert!(
            matches!(
                parse_collections_request(body.as_bytes()),
                Err(LibraryError::Invalid(_))
            ),
            "{body}"
        );
    }
    let too_many = collections(
        (0..=MAX_COLLECTIONS)
            .map(|n| collection(&format!("c{n}"), &["x"]))
            .collect(),
    );
    assert!(too_many.validate().is_err());
    let prefixes: Vec<String> = (0..=MAX_COLLECTION_PREFIXES)
        .map(|n| format!("p{n}"))
        .collect();
    let refs: Vec<&str> = prefixes.iter().map(String::as_str).collect();
    assert!(collections(vec![collection("A", &refs)])
        .validate()
        .is_err());
}

#[test]
fn a_key_is_in_a_collection_by_folder_prefix() {
    let set = collections(vec![
        collection("Kids", &["kids/"]),
        collection("Classics", &["classics/", "old/"]),
    ])
    .validate()
    .unwrap();
    assert_eq!(set.names_for_keys(["kids/Gruffalo/a.m4b"]), ["Kids"]);
    assert!(set.names_for_keys(["kidsbooks/a.m4b"]).is_empty());
    assert_eq!(
        set.names_for_keys(["old/x.mp3", "kids/y.mp3"]),
        ["Kids", "Classics"]
    );
}

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

/// Scans `title`, records a file at `key` for its edition when given, and
/// returns its work id.
async fn scan(store: &LibraryStore, asin: &str, title: &str, key: Option<&str>) -> String {
    let saved = store
        .upsert_book(&NewBook::minimal(asin, "acct-1", "us", title))
        .await
        .unwrap();
    if let Some(key) = key {
        let owner = store
            .file_owner_of_book(&saved.uuid)
            .await
            .unwrap()
            .unwrap();
        assert!(store
            .record_edition_file("local", key, &owner, FileRole::Audio)
            .await
            .unwrap());
    }
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

#[tokio::test]
async fn collections_filter_the_library_and_scopes() {
    for profile in Profile::ALL {
        collections_filter_the_library_and_scopes_under(profile).await;
    }
}

/// [`collections_filter_the_library_and_scopes`] under one profile.
async fn collections_filter_the_library_and_scopes_under(profile: Profile) {
    let store = store_under(profile).await;
    let gruffalo = scan(&store, "B001", "The Gruffalo", Some("kids/Gruffalo.m4b")).await;
    let dune = scan(&store, "B002", "Dune", Some("scifi/Dune.m4b")).await;
    let unheld = scan(&store, "B003", "Anathem", None).await;
    let ids = [gruffalo, dune, unheld];
    store.refresh_library_works(&ids).await.unwrap();
    store.finish_library_index_build().await.unwrap();

    let stored = set_library_collections(
        &store,
        &operator(),
        collections(vec![collection("Kids", &["kids"])]),
    )
    .await
    .unwrap();
    assert_eq!(
        stored.body.collections[0].prefixes,
        ["kids/"],
        "{profile:?}"
    );
    assert_eq!(
        library_collections(&store).await.unwrap(),
        stored.body,
        "{profile:?}"
    );
    assert!(
        !store.library_index_built().await.unwrap(),
        "{profile:?}: a change drops the finished build"
    );
    // What the rebuild does, work by work.
    store.refresh_library_works(&ids).await.unwrap();

    let in_kids = titles(
        &store,
        WorkPageQuery {
            filter: Some(WorkFilter::Facet {
                facet: WorkFacet::Collection,
                value: "kids".into(),
            }),
            limit: 10,
            ..WorkPageQuery::default()
        },
    )
    .await;
    assert_eq!(in_kids, ["The Gruffalo"], "{profile:?}: filter");
    let kids_only = WorkPageQuery {
        limit: 10,
        scope: Some(WorkScope {
            include: vec![(WorkFacet::Collection, facet_key("Kids"))],
            ..WorkScope::default()
        }),
        ..WorkPageQuery::default()
    };
    assert_eq!(
        titles(&store, kids_only).await,
        ["The Gruffalo"],
        "{profile:?}: a book with no file is in no collection"
    );

    // Writing the same collections again changes nothing.
    store.finish_library_index_build().await.unwrap();
    set_library_collections(
        &store,
        &operator(),
        collections(vec![collection("Kids", &["kids/"])]),
    )
    .await
    .unwrap();
    assert!(store.library_index_built().await.unwrap(), "{profile:?}");
}

#[tokio::test]
async fn a_member_cannot_write_collections() {
    let store = store_under(Profile::FirstParty).await;
    let member = ConfigActor::Member {
        id: "user:9".into(),
    };
    assert!(set_library_collections(
        &store,
        &member,
        collections(vec![collection("Kids", &["kids/"])])
    )
    .await
    .is_err());
}
