//! The player API's catalog reads, under every database profile.

use super::*;
use crate::control_plane::default_work_scope;
use crate::models::UserRole;
use crate::store::library_index::{WorkFilter, WorkSort};
use crate::store::NewBook;
use crate::test_support::{sqlite::memory, Profile};
use crate::NewPlaybackSession;
use crate::PlayerKind;

/// Scans `asin` onto `acct` with `author` and `narrator`, acquires it when
/// `acquired`, and answers its work and edition.
async fn title(
    store: &LibraryStore,
    asin: &str,
    name: &str,
    author: &str,
    acquired: bool,
) -> (String, String) {
    let mut book = NewBook::minimal(asin, "acct", "us", name);
    book.authors = Some(author.into());
    book.narrators = Some("Nia Narrator".into());
    let saved = store.upsert_book(&book).await.unwrap();
    if acquired {
        store
            .set_acquire_status(
                &saved.uuid,
                "acct",
                AcquireStatus::Acquired,
                Some("k.m4b"),
                None,
            )
            .await
            .unwrap();
    }
    let work = store.work_id_for_book(&saved.uuid).await.unwrap().unwrap();
    store
        .refresh_library_works(std::slice::from_ref(&work))
        .await
        .unwrap();
    let edition = store.edition_of_book(&saved.uuid).await.unwrap().unwrap();
    (work, edition)
}

/// A store under `profile` with one store account.
async fn store_under(profile: Profile) -> LibraryStore {
    let store = memory(profile).await.into_store();
    store
        .upsert_account("audible", "acct", "us", None, true, "audible")
        .await
        .unwrap();
    store
}

#[tokio::test]
async fn items_are_acquired_audio_editions_with_index_values() {
    for profile in Profile::ALL {
        let store = store_under(profile).await;
        let (work, edition) = title(&store, "B001", "The Hobbit", "J. R. R. Tolkien", true).await;
        let (other_work, other) = title(&store, "B002", "Unread", "Someone Else", false).await;
        let picked = store
            .player_editions_of_works(&[work.clone(), other_work.clone()])
            .await
            .unwrap();
        assert_eq!(picked.get(&work), Some(&edition), "{}", profile.name());
        assert!(!picked.contains_key(&other_work));

        let items = store
            .player_items(&[other.clone(), edition.clone(), "nope".into()])
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.edition_id, edition);
        assert_eq!(item.work_id, work);
        assert_eq!(item.title, "The Hobbit");
        assert_eq!(item.authors.len(), 1);
        assert_eq!(item.authors[0].key, "j. r. r. tolkien");
        assert_eq!(item.authors[0].value, "J. R. R. Tolkien");
        assert_eq!(item.narrators[0].value, "Nia Narrator");
        assert!(!item.abridged);
    }
}

#[tokio::test]
async fn pages_are_read_by_position_with_a_total() {
    for profile in Profile::ALL {
        let store = store_under(profile).await;
        for (asin, name) in [("B1", "Alpha"), ("B2", "Beta"), ("B3", "Gamma")] {
            title(&store, asin, name, "Ann Author", true).await;
        }
        title(&store, "B4", "Delta", "Ann Author", false).await;
        let query = WorkPageQuery {
            sort: WorkSort::Title,
            status: Some(AcquireStatus::Acquired),
            ..WorkPageQuery::default()
        };
        let (first, total) = store.player_items_page(&query, 0, 2, false).await.unwrap();
        assert_eq!(total, 3, "{}", profile.name());
        let names: Vec<&str> = first.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(names, ["Alpha", "Beta"]);
        let (rest, _) = store.player_items_page(&query, 2, 2, false).await.unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].title, "Gamma");
        let (reversed, _) = store.player_items_page(&query, 0, 1, true).await.unwrap();
        assert_eq!(reversed[0].title, "Gamma");
        let (past, total) = store.player_items_page(&query, 9, 2, false).await.unwrap();
        assert!(past.is_empty());
        assert_eq!(total, 3);

        // A filter on an index value and the library's default scope.
        let scope = default_work_scope(&store).await.unwrap();
        let filtered = WorkPageQuery {
            filter: Some(WorkFilter::Facet {
                facet: WorkFacet::Author,
                value: "ann author".into(),
            }),
            scope,
            ..query.clone()
        };
        let (_, total) = store
            .player_items_page(&filtered, 0, 10, false)
            .await
            .unwrap();
        assert_eq!(total, 3);
    }
}

#[tokio::test]
async fn sessions_list_newest_first_with_a_total() {
    for profile in Profile::ALL {
        let store = store_under(profile).await;
        let (_, edition) = title(&store, "B9", "Sessions", "Ann Author", true).await;
        let user = store
            .create_user(UserRole::Member, Some("Riley"), None)
            .await
            .unwrap()
            .id;
        let mut opened = Vec::new();
        for _ in 0..3 {
            let session = store
                .open_playback_session(NewPlaybackSession {
                    user_id: user,
                    edition_id: &edition,
                    serving_node_id: "node-1",
                    destination: "local",
                    files_digest: "digest",
                    client_kind: PlayerKind::App,
                    client_name: "Lissen",
                })
                .await
                .unwrap();
            opened.push(session.id);
        }
        let (page, total) = store.list_playback_sessions(user, 0, 2).await.unwrap();
        assert_eq!(total, 3, "{}", profile.name());
        assert_eq!(page.len(), 2);
        let (rest, _) = store.list_playback_sessions(user, 2, 2).await.unwrap();
        assert_eq!(rest.len(), 1);
        let mut seen: Vec<String> = page.into_iter().chain(rest).map(|s| s.id).collect();
        seen.sort();
        opened.sort();
        assert_eq!(seen, opened);
        let (none, total) = store.list_playback_sessions(user + 1, 0, 2).await.unwrap();
        assert!(none.is_empty());
        assert_eq!(total, 0);
    }
}
