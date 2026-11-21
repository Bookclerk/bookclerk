//! Tests of the works search document.

use crate::store::holdings::HoldingSelection;
use crate::store::{LibraryStore, NewBook};
use crate::test_support::Profile;

/// A store under `profile` with one account.
async fn store(profile: Profile) -> LibraryStore {
    let store = crate::test_support::sqlite::memory(profile)
        .await
        .into_store();
    store
        .upsert_account("audible", "acct", "us", None, true, "audible")
        .await
        .unwrap();
    store
}

#[tokio::test]
async fn a_held_work_reads_its_titles_people_series_and_ids() {
    for profile in Profile::ALL {
        let store = store(profile).await;
        let mut book = NewBook::minimal("B0DUNE", "acct", "us", "Dune");
        book.subtitle = Some("Deluxe Edition".into());
        book.authors = Some("Frank Herbert, Brian Herbert".into());
        book.narrators = Some("Scott Brick, Orlagh Cassidy".into());
        book.series = Some("Dune Chronicles".into());
        book.isbn = Some("9780000000001".into());
        let saved = store.upsert_book(&book).await.unwrap();
        let work = store
            .work_of_holding(&saved.holding_id)
            .await
            .unwrap()
            .unwrap();

        let (top, doc) = store.work_search_document(&work).await.unwrap();
        assert_eq!(top, work, "{profile:?}");
        let doc = doc.expect("a held work has a document");
        assert_eq!(doc.title, "Dune");
        assert!(
            doc.other_titles.contains(&"Deluxe Edition".to_string()),
            "{doc:?}"
        );
        assert!(!doc.other_titles.contains(&"Dune".to_string()), "{doc:?}");
        // Two author credits read in pages under the one-row profile.
        assert_eq!(
            doc.authors,
            ["Brian Herbert", "Frank Herbert"],
            "{profile:?}"
        );
        assert_eq!(doc.narrators, ["Orlagh Cassidy", "Scott Brick"]);
        assert!(
            doc.series.contains(&"Dune Chronicles".to_string()),
            "{doc:?}"
        );
        assert!(doc.identifiers.contains(&"b0dune".to_string()), "{doc:?}");
    }
}

#[tokio::test]
async fn an_unheld_or_missing_work_has_no_document() {
    let store = store(Profile::D1).await;
    let saved = store
        .upsert_book(&NewBook::minimal("B0GONE", "acct", "us", "Gone"))
        .await
        .unwrap();
    let work = store
        .work_of_holding(&saved.holding_id)
        .await
        .unwrap()
        .unwrap();
    store
        .remove_account_holdings("audible", "acct", HoldingSelection::All)
        .await
        .unwrap();
    assert_eq!(
        store.work_search_document(&work).await.unwrap(),
        (work.clone(), None)
    );
    assert_eq!(
        store.work_search_document("no-such-work").await.unwrap(),
        ("no-such-work".to_string(), None)
    );
}
