//! Tests for importing the branch owner's books from a joined library and
//! for which books the catalog sync sends it.

use super::*;
use crate::models::UserRole;
use crate::test_support::Profile;

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

/// The library's row of `product` on `account`, as the branch is handed
/// it: written to a store of its own (the library's).
async fn library_row(profile: Profile, account: &str, product: &str) -> BookRecord {
    let library = kit(profile).await;
    library
        .upsert_account(PLUGIN, account, "us", Some(account), true, PLUGIN)
        .await
        .unwrap();
    let new = NewBook::minimal(product, account, "us", format!("Title {product}"));
    library.upsert_book(&new).await.unwrap()
}

/// The branch owner's book acquired for a library joins the branch's own
/// library: the flat row (under the branch's own uuid) acquired at the
/// copy's key, its file recorded, and the book recorded as imported from
/// that library. A book on an account the branch does not have for its
/// Owner, or on a branch with no single Owner, stays out.
#[tokio::test]
async fn the_owners_book_from_a_library_joins_the_branch() {
    for profile in Profile::ALL {
        let store = kit(profile).await;
        let name = profile.name();
        let row = library_row(profile, "own-acct", "P1").await;
        let import = LinkImport {
            cluster_id: "lib-a",
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
        assert_ne!(book.uuid, row.uuid, "{name}: the branch's own uuid");
        assert_eq!(book.acquire_status, AcquireStatus::Acquired, "{name}");
        assert_eq!(
            book.storage_key.as_deref(),
            Some("Author/Title P1/Title P1.m4b"),
            "{name}"
        );
        let owner_of = store.file_owner_of_book(&book.uuid).await.unwrap().unwrap();
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
        let sams = library_row(profile, "sam-acct", "P2").await;
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
/// same library (though it is sent to another library).
#[tokio::test]
async fn only_the_owners_own_books_are_shared_upward() {
    for profile in Profile::ALL {
        let store = kit(profile).await;
        let name = profile.name();
        assert!(
            OwnerShareFilter::load(&store, "lib-a")
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
        connect(&store, "sam-acct", Some(member)).await;
        connect(&store, "loose-acct", None).await;
        for (account, product) in [
            ("own-acct", "OWN"),
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
        let imported = library_row(profile, "own-acct", "FROM-A").await;
        let LinkImportOutcome::Imported(_) = import_link_title(
            &store,
            LinkImport {
                cluster_id: "lib-a",
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

        let keys = store.acquired_book_keys_page(None, 100).await.unwrap();
        let sent = |filter: &OwnerShareFilter| -> BTreeSet<String> {
            keys.iter()
                .filter(|key| filter.admits(key))
                .map(|key| key.product_id.clone())
                .collect()
        };
        let to_a = OwnerShareFilter::load(&store, "lib-a").await.unwrap();
        assert_eq!(sent(&to_a), BTreeSet::from(["OWN".into()]), "{name}");
        let to_b = OwnerShareFilter::load(&store, "lib-b").await.unwrap();
        assert_eq!(
            sent(&to_b),
            BTreeSet::from(["OWN".into(), "FROM-A".into()]),
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
            rows.push(library_row(profile, "own-acct", product).await);
        }
        for (cluster_id, row) in rows
            .iter()
            .map(|row| ("lib-a", row))
            .chain(std::iter::once(("lib-b", &rows[0])))
        {
            let LinkImportOutcome::Imported(book) = import_link_title(
                &store,
                LinkImport {
                    cluster_id,
                    book: row,
                    primary_key: "k.m4b",
                    pdf_key: Some("k.pdf"),
                    files: &[],
                },
            )
            .await
            .unwrap() else {
                panic!("{name}: {cluster_id} {} imported", row.product_id);
            };
            assert_eq!(book.plugin_key, PLUGIN, "{name}");
            assert_eq!(book.acquire_status, AcquireStatus::Acquired, "{name}");
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
            .file_owner_of_book(&other.uuid)
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

        let row = library_row(profile, "own-acct", "P9").await;
        assert!(
            matches!(
                import_link_title(
                    &store,
                    LinkImport {
                        cluster_id: "lib-a",
                        book: &row,
                        primary_key: "shared.m4b",
                        pdf_key: None,
                        files: &[("local", "shared.m4b")],
                    },
                )
                .await
                .unwrap(),
                LinkImportOutcome::KeyHeldElsewhere
            ),
            "{name}"
        );
        let books = store.acquired_book_keys_page(None, 100).await.unwrap();
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
            Some(other_owner),
            "{name}"
        );
    }
}
