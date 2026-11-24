//! Tests for the record of a branch's own copies (#314).

use super::*;
use crate::store::{FileRole, NewBook};
use crate::test_support::Profile;

/// An empty store that behaves like one on an external plugin advertising
/// `profile`.
async fn kit(profile: Profile) -> LibraryStore {
    crate::test_support::sqlite::memory(profile)
        .await
        .into_store()
}

/// An attempt of the title `title_ref` for library `lib-a`.
fn attempt<'a>(title_ref: &'a str, writer: &'a str) -> LinkCopyAttempt<'a> {
    LinkCopyAttempt {
        library_id: "lib-a",
        title_ref,
        writer,
    }
}

/// The record of `key` on `local`, if any.
async fn record(store: &LibraryStore, key: &str) -> Option<link_copy_files::Model> {
    link_copy_files::Entity::find_by_id(("local".to_string(), key.to_string()))
        .one(store.db())
        .await
        .unwrap()
}

/// A key is claimed fresh, taken over by a later attempt of the same title
/// (and kept again when that attempt fails), and refused to another title.
#[tokio::test]
async fn a_copy_holds_its_key_against_another_title() {
    for profile in Profile::ALL {
        let store = kit(profile).await;
        let name = profile.name();
        let one = link_copy_title_ref(Some("aaaa1111"), "h1");
        let two = link_copy_title_ref(Some("bbbb2222"), "h2");
        assert_eq!(
            claim_link_copy(
                &store,
                attempt(&one, "n/i/1"),
                "local",
                "A/T.m4b",
                CopyTarget::New
            )
            .await
            .unwrap(),
            LinkCopyClaim::Fresh,
            "{name}"
        );
        assert_eq!(
            claim_link_copy(
                &store,
                attempt(&two, "n/i/2"),
                "local",
                "A/T.m4b",
                CopyTarget::New
            )
            .await
            .unwrap(),
            LinkCopyClaim::HeldByAnotherTitle,
            "{name}"
        );
        keep_link_copies(&store, "n/i/1").await.unwrap();
        assert_eq!(record(&store, "A/T.m4b").await.unwrap().state, "kept");
        assert_eq!(
            claim_link_copy(
                &store,
                attempt(&one, "n/i/3"),
                "local",
                "A/T.m4b",
                CopyTarget::New
            )
            .await
            .unwrap(),
            LinkCopyClaim::Replaced,
            "{name}"
        );
        // The later attempt fails: the earlier kept copy stays recorded.
        release_link_copies(&store, "n/i/3").await.unwrap();
        let kept = record(&store, "A/T.m4b").await.unwrap();
        assert_eq!(kept.state, "kept", "{name}");
        assert_eq!(kept.writer, "n/i/3", "{name}");
        // A fresh claim that fails is forgotten.
        claim_link_copy(
            &store,
            attempt(&two, "n/i/4"),
            "local",
            "B/U.m4b",
            CopyTarget::New,
        )
        .await
        .unwrap();
        release_link_copies(&store, "n/i/4").await.unwrap();
        assert!(record(&store, "B/U.m4b").await.is_none(), "{name}");
    }
}

/// A key this install's own library recorded for another edition is
/// refused; one recorded for the title's own edition (same call number)
/// is not.
#[tokio::test]
async fn a_copy_refuses_a_file_another_edition_here_holds() {
    for profile in Profile::ALL {
        let store = kit(profile).await;
        let name = profile.name();
        store
            .upsert_account("audible", "acct", "us", Some("acct"), true, "audible")
            .await
            .unwrap();
        let other = store
            .upsert_book(&NewBook::minimal("OTHER", "acct", "us", "Other"))
            .await
            .unwrap();
        let owner = store
            .file_owner_of_holding(&other.holding_id)
            .await
            .unwrap()
            .unwrap();
        assert!(store
            .record_edition_file("local", "Other.m4b", &owner, FileRole::Audio)
            .await
            .unwrap());
        let FileOwner::Edition(edition) = &owner;
        let same = link_copy_title_ref(call_number(edition).as_deref(), "h1");
        let different = link_copy_title_ref(Some("zzzz9999"), "h2");
        assert_eq!(
            claim_link_copy(
                &store,
                attempt(&different, "n/i/1"),
                "local",
                "Other.m4b",
                CopyTarget::New
            )
            .await
            .unwrap(),
            LinkCopyClaim::HeldByAnotherTitle,
            "{name}"
        );
        assert_eq!(
            claim_link_copy(
                &store,
                attempt(&same, "n/i/2"),
                "local",
                "Other.m4b",
                CopyTarget::New
            )
            .await
            .unwrap(),
            LinkCopyClaim::Fresh,
            "{name}"
        );
    }
}

/// Only this node's copies of an earlier incarnation, old enough and still
/// copying, are stale.
#[tokio::test]
async fn only_an_earlier_processs_unfinished_copies_are_stale() {
    for profile in Profile::ALL {
        let store = kit(profile).await;
        let name = profile.name();
        let title = link_copy_title_ref(None, "h1");
        for (key, writer) in [
            ("old.m4b", "node-a/old/1"),
            ("live.m4b", "node-a/now/2"),
            ("other.m4b", "node-b/old/3"),
            ("kept.m4b", "node-a/old/4"),
        ] {
            claim_link_copy(
                &store,
                attempt(&title, writer),
                "local",
                key,
                CopyTarget::New,
            )
            .await
            .unwrap();
        }
        keep_link_copies(&store, "node-a/old/4").await.unwrap();
        let later = Utc::now() + Duration::seconds(STALE_LINK_COPY_AGE_SECS + 60);
        assert!(stale_link_copies(&store, "node-a", "now", Utc::now())
            .await
            .unwrap()
            .is_empty());
        let stale = stale_link_copies(&store, "node-a", "now", later)
            .await
            .unwrap();
        assert_eq!(
            stale,
            vec![StaleLinkCopy {
                destination: "local".into(),
                key: "old.m4b".into(),
                writer: "node-a/old/1".into(),
                target: CopyTarget::New,
            }],
            "{name}"
        );
    }
}

/// The same edition sent by two joined libraries is one title: the second
/// library's attempt takes over the first's kept copy. A title named by
/// its holding is another library's title, and is refused.
#[tokio::test]
async fn two_libraries_share_a_copy_of_one_edition() {
    for profile in Profile::ALL {
        let store = kit(profile).await;
        let name = profile.name();
        let edition = link_copy_title_ref(Some("aaaa1111"), "h1");
        claim_link_copy(
            &store,
            attempt(&edition, "n/i/1"),
            "local",
            "A/T.m4b",
            CopyTarget::New,
        )
        .await
        .unwrap();
        keep_link_copies(&store, "n/i/1").await.unwrap();
        let other_library = LinkCopyAttempt {
            library_id: "lib-b",
            title_ref: &edition,
            writer: "n/i/2",
        };
        assert_eq!(
            claim_link_copy(
                &store,
                other_library,
                "local",
                "A/T.m4b",
                CopyTarget::Existing
            )
            .await
            .unwrap(),
            LinkCopyClaim::Replaced,
            "{name}"
        );
        let holding = link_copy_title_ref(None, "h1");
        claim_link_copy(
            &store,
            attempt(&holding, "n/i/3"),
            "local",
            "B/U.m4b",
            CopyTarget::New,
        )
        .await
        .unwrap();
        keep_link_copies(&store, "n/i/3").await.unwrap();
        let elsewhere = LinkCopyAttempt {
            library_id: "lib-b",
            title_ref: &holding,
            writer: "n/i/4",
        };
        assert_eq!(
            claim_link_copy(&store, elsewhere, "local", "B/U.m4b", CopyTarget::Existing)
                .await
                .unwrap(),
            LinkCopyClaim::HeldByAnotherTitle,
            "{name}"
        );
    }
}

/// A title's own kept copy that this install's library recorded since (the
/// owner's title, imported once committed) stays the title's, even when
/// the title has no call number to match the edition by.
#[tokio::test]
async fn an_imported_copy_stays_its_titles() {
    for profile in Profile::ALL {
        let store = kit(profile).await;
        let name = profile.name();
        store
            .upsert_account("audible", "acct", "us", Some("acct"), true, "audible")
            .await
            .unwrap();
        let book = store
            .upsert_book(&NewBook::minimal("MINE", "acct", "us", "Mine"))
            .await
            .unwrap();
        let owner = store
            .file_owner_of_holding(&book.holding_id)
            .await
            .unwrap()
            .unwrap();
        let title = link_copy_title_ref(None, "h1");
        claim_link_copy(
            &store,
            attempt(&title, "n/i/1"),
            "local",
            "Mine.m4b",
            CopyTarget::New,
        )
        .await
        .unwrap();
        keep_link_copies(&store, "n/i/1").await.unwrap();
        assert!(store
            .record_edition_file("local", "Mine.m4b", &owner, FileRole::Audio)
            .await
            .unwrap());
        assert_eq!(
            claim_link_copy(
                &store,
                attempt(&title, "n/i/2"),
                "local",
                "Mine.m4b",
                CopyTarget::Existing
            )
            .await
            .unwrap(),
            LinkCopyClaim::Replaced,
            "{name}"
        );
    }
}

/// A stale copy written over a file already there is kept again, one
/// written to an empty key is forgotten, each key on its own.
#[tokio::test]
async fn a_stale_copy_is_settled_by_its_own_key() {
    for profile in Profile::ALL {
        let store = kit(profile).await;
        let name = profile.name();
        let title = link_copy_title_ref(None, "h1");
        claim_link_copy(
            &store,
            attempt(&title, "n/old/1"),
            "local",
            "a.m4b",
            CopyTarget::Existing,
        )
        .await
        .unwrap();
        claim_link_copy(
            &store,
            attempt(&title, "n/old/1"),
            "local",
            "a.cue",
            CopyTarget::New,
        )
        .await
        .unwrap();
        let later = Utc::now() + Duration::seconds(STALE_LINK_COPY_AGE_SECS + 60);
        let stale = stale_link_copies(&store, "n", "now", later).await.unwrap();
        assert_eq!(stale.len(), 2, "{name}");
        let existing = stale.iter().find(|copy| copy.key == "a.m4b").unwrap();
        assert_eq!(existing.target, CopyTarget::Existing, "{name}");
        settle_stale_link_copy(&store, existing).await.unwrap();
        assert_eq!(
            record(&store, "a.m4b").await.unwrap().state,
            "kept",
            "{name}"
        );
        // The other key's delete failed: it stays for the next sweep.
        assert_eq!(
            record(&store, "a.cue").await.unwrap().state,
            "copying",
            "{name}"
        );
        let new = stale.iter().find(|copy| copy.key == "a.cue").unwrap();
        settle_stale_link_copy(&store, new).await.unwrap();
        assert!(record(&store, "a.cue").await.is_none(), "{name}");
    }
}
