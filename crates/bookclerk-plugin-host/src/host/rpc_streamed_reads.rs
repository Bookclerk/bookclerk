//! Library reads past the inline result caps through the production
//! `RpcDatabaseProxy` and a spawned first-party guest.
//!
//! The in-crate `books_page` tests drive an in-process typed executor; these
//! tests spawn the real sqlite guest and read a page whose rows weigh far
//! more than one scalar, which only the streamed path (`executeStreamed`,
//! collected under `maxStreamedResultBytes`) can return whole.

#![allow(clippy::missing_docs_in_private_items)]

use bookclerk_library::{LibraryStore, NewBook};
use sea_orm::{ConnectionTrait, Value};

use super::database::{open_library_store, DatabaseRegistry, ExternalDatabase};
use super::rpc_proxy_like::{
    approve_guest, guest_config, missing_guest, stage_first_party_guest, StagedGuest,
};

/// Books on the oversized page; the page limit the UI asks for.
const BOOKS: u32 = 64;

/// Text per wide column; six columns make each row about 48 KiB, so the
/// page is roughly 3 MiB: twelve times the inline cap and impossible to
/// return in one `execute` reply.
const WIDE_TEXT_BYTES: usize = 8 * 1024;

/// Spawns the staged guest and opens the host library through it.
async fn open_store(staged: &StagedGuest) -> LibraryStore {
    let config = guest_config(staged, "sqlite", None);
    approve_guest(&config, &staged.plugin);
    let ext = ExternalDatabase::spawn(&staged.plugin, &config)
        .await
        .unwrap_or_else(|err| panic!("spawn sqlite: {err}"));
    open_library_store(&config, &DatabaseRegistry::with_active(ext))
        .await
        .unwrap_or_else(|err| panic!("open library store through sqlite: {err}"))
}

/// Inserts `BOOKS` books and widens six text columns of every row to
/// `WIDE_TEXT_BYTES`, through the same proxy the daemon writes with.
async fn seed_wide_books(store: &LibraryStore) {
    store
        .upsert_account("audible", "stream-account", "us", None, false, "audible")
        .await
        .unwrap_or_else(|err| panic!("upsert account: {err}"));
    for i in 0..BOOKS {
        let mut book = NewBook::minimal(
            format!("wide-{i:03}"),
            "stream-account",
            "us",
            format!("Wide {i:03}"),
        );
        book.uuid = Some(format!("wide-uuid-{i:03}"));
        store
            .upsert_book(&book)
            .await
            .unwrap_or_else(|err| panic!("upsert book {i}: {err}"));
    }
    let wide = "w".repeat(WIDE_TEXT_BYTES);
    for column in [
        "description",
        "authors",
        "narrators",
        "categories",
        "subjects",
        "publisher",
    ] {
        store
            .db()
            .execute_raw(bookclerk_db_exec::canonical_statement(
                format!("UPDATE books SET {column} = ?"),
                [Value::from(wide.clone())],
            ))
            .await
            .unwrap_or_else(|err| panic!("widen {column}: {err}"));
    }
}

/// A 64-row page whose rows weigh about 3 MiB returns whole through the
/// guest, which is only possible when the host reads it as a streamed
/// result instead of paging inline.
#[tokio::test]
async fn oversized_enriched_page_reads_whole_through_sqlite_guest() {
    let _live = crate::authority::test_live_lock();
    let Some(staged) = stage_first_party_guest("sqlite") else {
        missing_guest("bookclerk-plugin-database-sqlite");
        return;
    };
    let store = open_store(&staged).await;
    seed_wide_books(&store).await;

    let page = store
        .list_books_filtered_page(None, None, u64::from(BOOKS), 0)
        .await
        .unwrap_or_else(|err| panic!("oversized page must stream through the guest: {err}"));
    assert_eq!(page.total, BOOKS as usize);
    assert_eq!(page.books.len(), BOOKS as usize);
    assert!(
        page.books
            .iter()
            .all(|book| book.title.starts_with("Wide ")),
        "every seeded book is on the page"
    );

    let keyset = store
        .list_books_page(Some("stream-account"), None, u64::from(BOOKS))
        .await
        .unwrap_or_else(|err| panic!("oversized keyset page must stream through the guest: {err}"));
    assert_eq!(keyset.len(), BOOKS as usize);

    let hits = (0..BOOKS)
        .map(|i| format!("wide-uuid-{i:03}"))
        .collect::<Vec<_>>();
    let by_uuid = store
        .list_books_by_uuid_page(&hits, None, None, u64::from(BOOKS), 0)
        .await
        .unwrap_or_else(|err| panic!("oversized uuid page must stream through the guest: {err}"));
    assert_eq!(by_uuid.total, BOOKS as usize);
    assert_eq!(by_uuid.books.len(), BOOKS as usize);
}
