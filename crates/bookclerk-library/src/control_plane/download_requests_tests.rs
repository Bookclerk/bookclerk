//! Download requests: the body parse and the plan, under every database
//! profile.

use super::*;
use crate::control_plane::window_books::tests::hold;
use crate::models::{AcquireStatus, UserRole};
use crate::FileRole;

/// The body parse keeps a trimmed work id and refuses everything else.
#[test]
fn the_body_names_one_work() {
    assert_eq!(
        parse_download_request(br#"{"work_id":"  w-1  "}"#).unwrap(),
        DownloadRequestAsk {
            work_id: "w-1".into()
        }
    );
    for body in [
        &br#"{"work_id":"w-1","node_id":"n"}"#[..],
        br#"{"work_id":""}"#,
        br#"{"work_id":"a b"}"#,
        br#"["w-1"]"#,
        br#"{}"#,
        br#"{"work_id":"w-1","work_id":"w-2"}"#,
    ] {
        assert!(
            matches!(parse_download_request(body), Err(LibraryError::Invalid(_))),
            "{}",
            String::from_utf8_lossy(body)
        );
    }
}

/// A work held on an account with no copy is planned through its
/// holding; one with a copy is a conflict, and one the library does not
/// hold (or does not know) is not found, under every profile.
#[tokio::test]
async fn a_held_work_without_a_copy_is_planned() {
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
        let asins: Vec<String> = ["B0WANTED01", "B0HAVEIT01"].map(String::from).to_vec();
        let works = hold(&store, riley, "riley-acct", &asins).await;

        let plan = plan_download_request(&store, &works[0]).await.unwrap();
        let book = store
            .get_book("B0WANTED01", "riley-acct")
            .await
            .unwrap()
            .unwrap();
        let held = store
            .held_titles_of_edition(&plan.edition_id)
            .await
            .unwrap();
        assert_eq!(held.len(), 1, "{name}");
        assert_eq!(plan.title, held[0].holding_id, "{name}");
        assert_eq!(held[0].product_id, book.product_id, "{name}");

        store
            .set_acquire_status(
                "B0HAVEIT01",
                "riley-acct",
                AcquireStatus::Acquired,
                Some("A/Have/Have.m4b"),
                None,
            )
            .await
            .unwrap();
        let have = store
            .get_book("B0HAVEIT01", "riley-acct")
            .await
            .unwrap()
            .unwrap();
        let owner = store.file_owner_of_book(&have.uuid).await.unwrap().unwrap();
        assert!(store
            .record_edition_file("local", "A/Have/Have.m4b", &owner, FileRole::Audio)
            .await
            .unwrap());
        assert!(
            matches!(
                plan_download_request(&store, &works[1]).await,
                Err(LibraryError::Conflict(_))
            ),
            "{name}"
        );
        assert!(
            matches!(
                plan_download_request(&store, "no-such-work").await,
                Err(LibraryError::NotFound(_))
            ),
            "{name}"
        );
    }
}
