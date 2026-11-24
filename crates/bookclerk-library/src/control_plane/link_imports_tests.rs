//! Tests for importing the branch owner's books from a joined library and
//! for which books the catalog sync sends it.

use super::*;
use crate::models::UserRole;
use crate::test_support::Profile;
use crate::{CopyHolder, UnlabelledAudio};

/// The plugin every test book is on.
const PLUGIN: &str = "audible";

/// An empty store that behaves like one on an external plugin advertising
/// `profile`.
async fn kit(profile: Profile) -> LibraryStore {
    crate::test_support::sqlite::memory(profile)
        .await
        .into_store()
}

/// Connects `account` on `store`, linked to `user` when one is given.
async fn connect(store: &LibraryStore, account: &str, user: Option<i64>) {
    store
        .upsert_account(PLUGIN, account, "us", Some(account), true, PLUGIN)
        .await
        .unwrap();
    if let Some(user) = user {
        let identity = store
            .ensure_local_portal_identity(user, None)
            .await
            .unwrap();
        store
            .link_account(identity.id, PLUGIN, account, PLUGIN)
            .await
            .unwrap();
    }
}

/// The library's title `product` on `account`, as the branch is handed it.
fn library_row(account: &str, product: &str) -> NewBook {
    NewBook::minimal(product, account, "us", format!("Title {product}"))
}

/// The branch owner's book acquired for a library joins the branch's own
/// library: the branch's own holding of the title acquired at the
/// copy's key, its file recorded, and the book recorded as imported from
/// that library. A book on an account the branch does not have for its
/// Owner, or on a branch with no single Owner, stays out.
#[tokio::test]
async fn the_owners_book_from_a_library_joins_the_branch() {
    for profile in Profile::ALL {
        let store = kit(profile).await;
        let name = profile.name();
        let row = library_row("own-acct", "P1");
        let import = LinkImport {
            library_id: "lib-a",
            book: &row,
            primary_key: "Author/Title P1/Title P1.m4b",
            pdf_key: None,
            files: &[("local", "Author/Title P1/Title P1.m4b")],
        };
        assert!(
            matches!(
                import_link_title(&store, import).await.unwrap(),
                LinkImportOutcome::NoPerson
            ),
            "{name}"
        );
        let owner = store
            .create_user(UserRole::Owner, Some("Riley"), None)
            .await
            .unwrap()
            .id;
        assert!(
            matches!(
                import_link_title(&store, import).await.unwrap(),
                LinkImportOutcome::AccountNotHere
            ),
            "{name}"
        );
        connect(&store, "own-acct", Some(owner)).await;
        let LinkImportOutcome::Imported(book) = import_link_title(&store, import).await.unwrap()
        else {
            panic!("{name}: the owner's book is imported");
        };
        assert_eq!(book.status, AcquireStatus::Acquired, "{name}");
        assert_eq!(
            book.storage_key.as_deref(),
            Some("Author/Title P1/Title P1.m4b"),
            "{name}"
        );
        let owner_of = store
            .file_owner_of_holding(&book.holding_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .edition_file("local", "Author/Title P1/Title P1.m4b")
                .await
                .unwrap()
                .map(|file| file.owner),
            Some(owner_of),
            "{name}"
        );
        let imported = books_imported_from_link(&store, "lib-a").await.unwrap();
        assert_eq!(
            imported,
            BTreeSet::from([(PLUGIN.into(), "us".into(), "P1".into(), "own-acct".into())]),
            "{name}"
        );
        assert!(
            books_imported_from_link(&store, "lib-b")
                .await
                .unwrap()
                .is_empty(),
            "{name}"
        );

        // Someone else's account here is not the owner's.
        let member = store
            .create_user(UserRole::Member, Some("Sam"), None)
            .await
            .unwrap()
            .id;
        connect(&store, "sam-acct", Some(member)).await;
        let sams = library_row("sam-acct", "P2");
        assert!(
            matches!(
                import_link_title(
                    &store,
                    LinkImport {
                        book: &sams,
                        ..import
                    }
                )
                .await
                .unwrap(),
                LinkImportOutcome::AccountNotHere
            ),
            "{name}"
        );
    }
}

/// The catalog sync sends a library only the branch owner's books: not a
/// member's, not an unlinked account's, and not one imported from that
/// same library, through any account holding its edition (though it is
/// sent to another library).
#[tokio::test]
async fn only_the_owners_own_books_are_shared_upward() {
    for profile in Profile::ALL {
        let store = kit(profile).await;
        let name = profile.name();
        assert!(
            OwnerShareFilter::load(&store, "lib-a", false)
                .await
                .unwrap()
                .accounts
                .is_empty(),
            "{name}: no Owner, nothing goes up"
        );
        let owner = store
            .create_user(UserRole::Owner, Some("Riley"), None)
            .await
            .unwrap()
            .id;
        let member = store
            .create_user(UserRole::Member, Some("Sam"), None)
            .await
            .unwrap()
            .id;
        connect(&store, "own-acct", Some(owner)).await;
        connect(&store, "own-acct-2", Some(owner)).await;
        connect(&store, "sam-acct", Some(member)).await;
        connect(&store, "loose-acct", None).await;
        for (account, product) in [
            ("own-acct", "OWN"),
            ("own-acct-2", "OWN2"),
            ("sam-acct", "SAM"),
            ("loose-acct", "LOOSE"),
        ] {
            store
                .upsert_book(&NewBook::minimal(product, account, "us", product))
                .await
                .unwrap();
            store
                .set_acquire_status(product, account, AcquireStatus::Acquired, Some("k"), None)
                .await
                .unwrap();
        }
        let imported = library_row("own-acct", "FROM-A");
        let LinkImportOutcome::Imported(_) = import_link_title(
            &store,
            LinkImport {
                library_id: "lib-a",
                book: &imported,
                primary_key: "a/FROM-A.m4b",
                pdf_key: None,
                files: &[],
            },
        )
        .await
        .unwrap() else {
            panic!("{name}: imported");
        };

        // The owner's second account holds the imported edition too; its
        // holding must not send library A's copy back either.
        store
            .upsert_book(&NewBook::minimal("FROM-A", "own-acct-2", "us", "FROM-A"))
            .await
            .unwrap();
        let keys = store.all_acquired_title_keys().await;
        assert_eq!(
            keys.iter().filter(|key| key.product_id == "FROM-A").count(),
            2,
            "{name}: both owner accounts hold the imported edition"
        );
        let sent = |filter: &OwnerShareFilter| -> BTreeSet<String> {
            filter
                .admit_editions(keys.clone())
                .into_iter()
                .map(|key| key.product_id)
                .collect()
        };
        let to_a = OwnerShareFilter::load(&store, "lib-a", false)
            .await
            .unwrap();
        assert_eq!(
            sent(&to_a),
            BTreeSet::from(["OWN".into(), "OWN2".into()]),
            "{name}"
        );
        let to_b = OwnerShareFilter::load(&store, "lib-b", false)
            .await
            .unwrap();
        assert_eq!(
            sent(&to_b),
            BTreeSet::from(["OWN".into(), "OWN2".into(), "FROM-A".into()]),
            "{name}"
        );
    }
}

/// A book imported from two libraries stays recorded for both, so neither
/// gets it back; several imports from one library are all read back, in
/// pages no larger than every profile's result cap; and a title whose
/// product id the same account also has on another store is imported by
/// the exact row written, not refused as ambiguous.
#[tokio::test]
async fn imports_are_kept_per_library_and_read_in_pages() {
    for profile in Profile::ALL {
        let store = kit(profile).await;
        let name = profile.name();
        let owner = store
            .create_user(UserRole::Owner, Some("Riley"), None)
            .await
            .unwrap()
            .id;
        connect(&store, "own-acct", Some(owner)).await;
        // The same account and product id on another store, not the owner's
        // book being imported.
        store
            .upsert_account("libro", "own-acct", "us", Some("own-acct"), true, "libro")
            .await
            .unwrap();
        store
            .upsert_book(&NewBook {
                plugin_key: "libro".into(),
                source: "libro".into(),
                ..NewBook::minimal("P1", "own-acct", "us", "Other store")
            })
            .await
            .unwrap();

        let mut rows = Vec::new();
        for product in ["P1", "P2", "P3"] {
            rows.push(library_row("own-acct", product));
        }
        for (library_id, row) in rows
            .iter()
            .map(|row| ("lib-a", row))
            .chain(std::iter::once(("lib-b", &rows[0])))
        {
            let LinkImportOutcome::Imported(book) = import_link_title(
                &store,
                LinkImport {
                    library_id,
                    book: row,
                    primary_key: "k.m4b",
                    pdf_key: Some("k.pdf"),
                    files: &[],
                },
            )
            .await
            .unwrap() else {
                panic!("{name}: {library_id} {} imported", row.product_id);
            };
            assert_eq!(book.plugin_key, PLUGIN, "{name}");
            assert_eq!(book.status, AcquireStatus::Acquired, "{name}");
        }

        let key = |product: &str| {
            (
                PLUGIN.into(),
                "us".into(),
                product.into(),
                "own-acct".into(),
            )
        };
        assert_eq!(
            books_imported_from_link(&store, "lib-a").await.unwrap(),
            BTreeSet::from([key("P1"), key("P2"), key("P3")]),
            "{name}"
        );
        assert_eq!(
            books_imported_from_link(&store, "lib-b").await.unwrap(),
            BTreeSet::from([key("P1")]),
            "{name}: importing P1 from lib-b keeps lib-a's record"
        );
    }
}

/// A copy whose primary file key another live edition here already holds
/// is not that title's: the title is left unacquired and not recorded as
/// imported, and the other edition keeps its file.
#[tokio::test]
async fn a_primary_key_another_edition_holds_is_not_imported() {
    for profile in Profile::ALL {
        let store = kit(profile).await;
        let name = profile.name();
        let owner = store
            .create_user(UserRole::Owner, Some("Riley"), None)
            .await
            .unwrap()
            .id;
        connect(&store, "own-acct", Some(owner)).await;
        let other = store
            .upsert_book(&NewBook::minimal("OTHER", "own-acct", "us", "Other"))
            .await
            .unwrap();
        let other_owner = store
            .file_owner_of_holding(&other.holding_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            store
                .record_edition_file("local", "shared.m4b", &other_owner, FileRole::Audio)
                .await
                .unwrap(),
            "{name}"
        );

        let row = library_row("own-acct", "P9");
        assert!(
            matches!(
                import_link_title(
                    &store,
                    LinkImport {
                        library_id: "lib-a",
                        book: &row,
                        primary_key: "shared.m4b",
                        pdf_key: None,
                        // The primary is checked first: the cover listed
                        // before it is not claimed.
                        files: &[("local", "P9/cover.jpg"), ("local", "shared.m4b")],
                    },
                )
                .await
                .unwrap(),
                LinkImportOutcome::KeyHeldElsewhere
            ),
            "{name}"
        );
        let books = store.all_acquired_title_keys().await;
        assert!(
            books.iter().all(|key| key.product_id != "P9"),
            "{name}: not acquired"
        );
        assert!(
            books_imported_from_link(&store, "lib-a")
                .await
                .unwrap()
                .is_empty(),
            "{name}"
        );
        assert_eq!(
            store
                .edition_file("local", "shared.m4b")
                .await
                .unwrap()
                .map(|file| file.owner),
            Some(other_owner.clone()),
            "{name}"
        );
        assert!(
            store
                .edition_file("local", "P9/cover.jpg")
                .await
                .unwrap()
                .is_none(),
            "{name}: nothing else claimed"
        );

        // A PDF key another edition holds is not marked acquired, while the
        // title itself is.
        assert!(
            store
                .record_edition_file("local", "shared.pdf", &other_owner, FileRole::Pdf)
                .await
                .unwrap(),
            "{name}"
        );
        let LinkImportOutcome::Imported(book) = import_link_title(
            &store,
            LinkImport {
                library_id: "lib-a",
                book: &row,
                primary_key: "P9/P9.m4b",
                pdf_key: Some("shared.pdf"),
                files: &[("local", "P9/P9.m4b"), ("local", "shared.pdf")],
            },
        )
        .await
        .unwrap() else {
            panic!("{name}: imported");
        };
        assert_eq!(book.status, AcquireStatus::Acquired, "{name}");
        assert_ne!(book.pdf_status, AcquireStatus::Acquired, "{name}");
        assert_eq!(book.pdf_storage_key, None, "{name}");
    }
}

/// A copy this install took from a library is recorded for that library
/// alone, once however often it is taken, so the catalog sync never sends
/// the book back to it.
#[tokio::test]
async fn a_copy_taken_from_a_library_is_recorded_once_for_that_library() {
    for profile in Profile::ALL {
        let store = kit(profile).await;
        let name = profile.name();
        connect(&store, "own-acct", None).await;
        let book = TakenCopy {
            plugin_key: PLUGIN,
            account_id: "own-acct",
            marketplace: "us",
            product_id: "P9",
        };
        for _ in 0..2 {
            record_copy_taken_from_link(&store, "lib-a", book)
                .await
                .unwrap();
        }
        assert_eq!(
            books_imported_from_link(&store, "lib-a").await.unwrap(),
            BTreeSet::from([(PLUGIN.into(), "us".into(), "P9".into(), "own-acct".into())]),
            "{name}"
        );
        assert!(
            books_imported_from_link(&store, "lib-b")
                .await
                .unwrap()
                .is_empty(),
            "{name}"
        );
    }
}

/// A branch keeping its own copy of a title asks whether a key belongs to
/// another edition here: an unrecorded key, one the title's own edition
/// holds, or one of an edition no longer here is free; another live
/// edition's is not, and neither is any live edition's when the title has
/// no edition here yet.
#[tokio::test]
async fn a_key_is_held_only_by_another_live_edition() {
    for profile in Profile::ALL {
        let store = kit(profile).await;
        let name = profile.name();
        connect(&store, "own-acct", None).await;
        let ours = store
            .upsert_book(&library_row("own-acct", "P1"))
            .await
            .unwrap();
        let theirs = store
            .upsert_book(&library_row("own-acct", "P2"))
            .await
            .unwrap();
        let edition = store
            .edition_of_listing(PLUGIN, "US", "P1")
            .await
            .unwrap()
            .expect("the listing's edition");
        assert_eq!(edition, ours.edition_id, "{name}");
        assert_eq!(
            store.edition_of_listing(PLUGIN, "us", "P9").await.unwrap(),
            None,
            "{name}"
        );
        let their_owner = store
            .file_owner_of_holding(&theirs.holding_id)
            .await
            .unwrap()
            .unwrap();
        let our_owner = store
            .file_owner_of_holding(&ours.holding_id)
            .await
            .unwrap()
            .unwrap();
        store
            .record_edition_file("local", "T/theirs.m4b", &their_owner, FileRole::Audio)
            .await
            .unwrap();
        store
            .record_edition_file("local", "T/ours.m4b", &our_owner, FileRole::Audio)
            .await
            .unwrap();
        let held = |key: &'static str, edition: Option<String>| {
            let store = store.clone();
            async move {
                store
                    .key_held_by_another_edition("local", key, edition.as_deref())
                    .await
                    .unwrap()
            }
        };
        assert!(held("T/theirs.m4b", Some(edition.clone())).await, "{name}");
        assert!(!held("T/ours.m4b", Some(edition.clone())).await, "{name}");
        assert!(!held("T/free.m4b", Some(edition.clone())).await, "{name}");
        assert!(held("T/ours.m4b", None).await, "{name}");
    }
}

/// A copy asks whether another copy of its edition has a file at a key, on
/// any destination: only a copy the library still makes counts, and the
/// copy's own file, another edition's, or an unrecorded key never does.
#[tokio::test]
async fn a_key_is_held_only_by_another_copy_the_library_makes() {
    let copy = |text: &str| bookclerk_config::OutputCopy::parse(text).unwrap();
    for profile in Profile::ALL {
        let store = kit(profile).await;
        let name = profile.name();
        connect(&store, "own-acct", None).await;
        let ours = store
            .upsert_book(&library_row("own-acct", "P1"))
            .await
            .unwrap();
        let theirs = store
            .upsert_book(&library_row("own-acct", "P2"))
            .await
            .unwrap();
        let owner = store
            .file_owner_of_holding(&ours.holding_id)
            .await
            .unwrap()
            .unwrap();
        let their_owner = store
            .file_owner_of_holding(&theirs.holding_id)
            .await
            .unwrap()
            .unwrap();
        let (main, phone, old) = (copy("m4b:original"), copy("mp3:64k"), copy("mp3:128k"));
        store
            .record_edition_copy_file("local", "T/T.m4b", &owner, FileRole::Audio, Some(&main))
            .await
            .unwrap();
        store
            .record_edition_copy_file("local", "T/old.mp3", &owner, FileRole::Audio, Some(&old))
            .await
            .unwrap();
        store
            .record_edition_copy_file(
                "local",
                "T/theirs.mp3",
                &their_owner,
                FileRole::Audio,
                Some(&main),
            )
            .await
            .unwrap();
        store
            .record_edition_file("local", "T/upload.m4b", &owner, FileRole::Audio)
            .await
            .unwrap();
        store
            .record_edition_file("local", "T/upload.pdf", &owner, FileRole::Pdf)
            .await
            .unwrap();
        let guarded = [main.clone(), phone.clone()];
        let held_as = |key: &'static str,
                       writing: &bookclerk_config::OutputCopy,
                       unlabelled: UnlabelledAudio| {
            let (store, owner, writing, guarded) = (
                store.clone(),
                owner.clone(),
                writing.clone(),
                guarded.clone(),
            );
            async move {
                store
                    .key_held_by_another_copy(&owner, key, &writing, &guarded, unlabelled)
                    .await
                    .unwrap()
            }
        };
        let held = |key: &'static str, writing: &bookclerk_config::OutputCopy| {
            held_as(key, writing, UnlabelledAudio::Replaceable)
        };
        assert_eq!(
            held("T/T.m4b", &phone).await,
            Some(CopyHolder::Copy(main.clone())),
            "{name}"
        );
        assert_eq!(held("T/T.m4b", &main).await, None, "{name}");
        assert_eq!(held("T/old.mp3", &phone).await, None, "{name}");
        assert_eq!(held("T/theirs.mp3", &phone).await, None, "{name}");
        assert_eq!(held("T/free.mp3", &phone).await, None, "{name}");
        assert_eq!(
            store
                .key_held_by_another_copy(
                    &owner,
                    "T/T.m4b",
                    &phone,
                    &[],
                    UnlabelledAudio::Replaceable
                )
                .await
                .unwrap(),
            None,
            "{name}"
        );
        // Audio recorded with no copy holds its key only against a convert.
        assert_eq!(held("T/upload.m4b", &phone).await, None, "{name}");
        assert_eq!(
            held_as("T/upload.m4b", &phone, UnlabelledAudio::Guarded).await,
            Some(CopyHolder::Unlabelled),
            "{name}"
        );
        assert_eq!(
            held_as("T/upload.pdf", &phone, UnlabelledAudio::Guarded).await,
            None,
            "{name}"
        );
    }
}

/// An acquire run records under a writer of its own, and releases only a
/// record it made: not one the edition held before, nor one another run
/// recorded since.
#[tokio::test]
async fn a_run_releases_only_its_own_new_record() {
    for profile in Profile::ALL {
        let store = kit(profile).await;
        let name = profile.name();
        connect(&store, "own-acct", None).await;
        let book = store
            .upsert_book(&library_row("own-acct", "P1"))
            .await
            .unwrap();
        let owner = store
            .file_owner_of_holding(&book.holding_id)
            .await
            .unwrap()
            .unwrap();
        let (one, two) = (
            crate::store::AcquireWriter::for_run(),
            crate::store::AcquireWriter::for_run(),
        );
        assert_ne!(one, two, "{name}");
        assert!(crate::store::AcquireWriter::is_acquire(one.as_str()));
        assert!(crate::store::AcquireWriter::is_acquire("acquire"));
        assert!(!crate::store::AcquireWriter::is_acquire("acquire:"));
        assert!(!crate::store::AcquireWriter::is_acquire("upload-1"));
        let record = |key: &'static str, writer: &crate::store::AcquireWriter| {
            let (store, owner, writer) = (store.clone(), owner.clone(), writer.clone());
            async move {
                store
                    .record_run_edition_file("local", key, &owner, FileRole::Audio, None, &writer)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(
            record("T/a.m4b", &one).await,
            crate::store::RecordedFile::New,
            "{name}"
        );
        // A second run takes the key over; the first run's release then
        // leaves the second run's record.
        assert_eq!(
            record("T/a.m4b", &two).await,
            crate::store::RecordedFile::TakenOver,
            "{name}"
        );
        assert!(
            !store
                .release_run_edition_file("local", "T/a.m4b", &one)
                .await
                .unwrap(),
            "{name}"
        );
        assert!(
            store
                .release_run_edition_file("local", "T/a.m4b", &two)
                .await
                .unwrap(),
            "{name}"
        );
        assert_eq!(store.edition_file("local", "T/a.m4b").await.unwrap(), None);
        // Another live edition's key is refused.
        let other = store
            .upsert_book(&library_row("own-acct", "P2"))
            .await
            .unwrap();
        let other_owner = store
            .file_owner_of_holding(&other.holding_id)
            .await
            .unwrap()
            .unwrap();
        store
            .record_edition_file("local", "T/b.m4b", &other_owner, FileRole::Audio)
            .await
            .unwrap();
        assert_eq!(
            record("T/b.m4b", &one).await,
            crate::store::RecordedFile::Refused,
            "{name}"
        );
    }
}
